"""Discovered synchronization situations (matrix section N of scenarios/matrix/sync.md,
rows SYNC-201 upward): push path edge cases, script naming variants, file-system
events under Live Sync, store and Studio-side edge cases, identity alignment."""

from __future__ import annotations

import json
import os
import shutil
import stat
import subprocess
import sys
import time
from pathlib import Path

from run import Context, Scenario

MAIN = "src/ServerScriptService/Main.server.luau"
UTIL = "src/ReplicatedStorage/Shared/Util.luau"
WORKSPACE = "instances/Workspace.renium"
LIGHTING = "instances/Lighting.renium"


def _live(ctx: Context):
    ctx.open_studio()
    ctx.rbx("lon", "--wait", "60", timeout=200)


def _offline(ctx: Context):
    """Studio open with Live Sync stopped: in session mode a previous row may have left it running."""
    ctx.open_studio()
    ctx.rbx("lof", ok=None, timeout=60)


def _tree(root: Path) -> set:
    paths = set()
    for dirpath, dirnames, filenames in os.walk(root):
        if ".renium" in Path(dirpath).relative_to(root).parts:
            continue
        for name in dirnames + filenames:
            paths.add(Path(dirpath) / name)
    return paths


def _tidy(run):
    """Removes every file or directory a row created, so the shuffled session order never sees a
    stray script, symlink or nested project from an earlier row."""
    def wrapped(ctx: Context):
        before = _tree(ctx.root)
        try:
            run(ctx)
        finally:
            extras = sorted((p for p in _tree(ctx.root) if p not in before), key=lambda p: len(p.parts), reverse=True)
            for path in extras:
                try:
                    if path.is_symlink():
                        # chmod would follow the link and strip the target
                        # directory's execute bit, leaving it unreadable.
                        path.unlink()
                    elif path.is_file():
                        path.chmod(stat.S_IREAD | stat.S_IWRITE)
                        path.unlink()
                    elif path.is_dir():
                        shutil.rmtree(path, ignore_errors=True)
                except OSError:
                    pass
    wrapped.__name__ = run.__name__
    wrapped.__doc__ = run.__doc__
    return wrapped


def _source(ctx: Context, path: str) -> str:
    return ctx.luau(f"return {path}.Source")


def _studio_edit(ctx: Context, code: str):
    ctx.luau(code + " return true")


def _settled(ctx: Context, wait: int = 60):
    status = ctx.rbx("lst", "--wait", str(wait), timeout=wait + 60)
    ctx.expect(status.pointer("/daemon/pendingCount", 0) == 0, f"pending after settle: {status.json}")
    return status


def _counts(ctx: Context):
    status = ctx.rbx("lst", ok=None, timeout=60)
    daemon = status.get("daemon") or {}
    return daemon.get("pushes", 0), daemon.get("pulls", 0)


def _error_of(status) -> str:
    return str(status.pointer("/daemon/error") or status.get("error") or "")


def _wait_file(ctx: Context, relative: str, needle: str, seconds: int = 60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline and needle not in ctx.read(relative):
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect_in(needle, ctx.read(relative), f"{relative} after the Studio edit")


def _wait_error(ctx: Context, seconds: int = 40):
    deadline = time.monotonic() + seconds
    status = None
    while time.monotonic() < deadline:
        status = ctx.rbx("lst", ok=None, timeout=60)
        if _error_of(status):
            return status
        time.sleep(1)
    raise AssertionError(f"no Live Sync error was reported: {status.json if status else None}")


def _wait_source(ctx: Context, path: str, needle: str, seconds: int = 60):
    deadline = time.monotonic() + seconds
    text = ""
    while time.monotonic() < deadline:
        text = ctx.luau(f"local i = {path} return i and i.Source or ''") or ""
        if needle in text:
            return text
        time.sleep(2)
    raise AssertionError(f"{path} never held {needle!r}: {text[:200]!r}")


def _rename(source: Path, target: Path):
    """A directory rename right after another rename of the same tree can be refused while the
    watcher still holds the old handle; retry briefly."""
    for attempt in range(20):
        try:
            os.rename(source, target)
            return
        except PermissionError:
            if attempt == 19:
                raise
            time.sleep(0.25)


def _count(ctx: Context, path: str) -> int:
    return int(ctx.luau(f"local i = {path} return i and #i:GetChildren() or -1"))


def _ok_or_clear_error(ctx: Context, result, label: str):
    ctx.expect(result.ok or result.error, f"{label}: neither ok nor an error: {result.stdout[:300]}")


# ---------------------------------------------------------------- A. push paths


def ps_from_a_subdirectory(ctx: Context):
    """SYNC-201: with -r naming the project, paths and -f lists resolve against the invoking
    directory; comments and blank lines in the list are skipped. A bare ps from a subdirectory
    is refused (N5: the relative project root never walks upward)."""
    _offline(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 201\n')
    bare = ctx.rbx("ps", "ServerScriptService/Main.server.luau", "--yes", ok=None, timeout=120, cwd=ctx.root / "src")
    ctx.log.append(f"bare ps from src: ok={bare.ok} {bare.error[:200]}")
    ctx.rbx("ps", "-r", "..", "ServerScriptService/Main.server.luau", "--yes", timeout=120, cwd=ctx.root / "src")
    ctx.expect_in("return 201", _source(ctx, "game.ServerScriptService.Main"), "relative path from src")
    ctx.write(UTIL, "return { version = 201 }\n")
    ctx.write("src/list.txt", "# comment\n\n  ReplicatedStorage/Shared/Util.luau  \n")
    ctx.rbx("ps", "-r", "..", "-f", "list.txt", "--yes", timeout=120, cwd=ctx.root / "src")
    ctx.expect_in("version = 201", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "list relative to src")


def ps_of_unmapped_paths_pushes_nothing(ctx: Context):
    """SYNC-202: a misspelled script path and a non-script file under src map to no instance; the
    push changes nothing in Studio and says so."""
    _offline(ctx)
    before = _source(ctx, "game.ServerScriptService.Main")
    ctx.write("src/ServerScriptService/notes.txt", "not a script\n")
    for path in ("src/ServerScriptService/Mian.server.luau", "src/ServerScriptService/notes.txt"):
        result = ctx.rbx("ps", path, "--yes", ok=None, timeout=120)
        _ok_or_clear_error(ctx, result, path)
        ctx.expect(result.pointer("/scripts/updated", 0) == 0 and result.pointer("/scripts/created", 0) == 0, f"{path}: {result.json}")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main") == before, "Main untouched")
    ctx.expect(_count(ctx, "game.ServerScriptService") == 1, "no instance created for notes.txt")


def ps_with_an_unknown_target_id(ctx: Context):
    """SYNC-203: -i naming an id the store does not hold pushes nothing from that store."""
    _offline(ctx)
    ctx.rbx("ba", "Workspace", "-n", "Extra", "-c", "Part", "--no-parent")
    result = ctx.rbx("ps", WORKSPACE, "-i", "editor:999999", "--yes", ok=None, timeout=120)
    _ok_or_clear_error(ctx, result, "unknown id")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Extra") == nil') is True, "an instance outside the unknown id was created")


def target_property_limits_the_push(ctx: Context):
    """SYNC-204: -t PROPERTY pushes only that property of the targeted instance."""
    _offline(ctx)
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Anchored", "--bool", "true")
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Transparency", "--num", "0.5")
    ctx.rbx("ps", WORKSPACE, "-i", "editor:2", "-t", "Anchored", "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True, "targeted property applied")
    ctx.expect(ctx.luau("return workspace.Props.Crate.Transparency") == 0, "untargeted property not applied")


def deleted_script_path_removes_the_instance(ctx: Context):
    """SYNC-205: pushing the path of a deleted script file removes that instance and nothing else."""
    _offline(ctx)
    (ctx.root / UTIL).unlink()
    ctx.rbx("ps", UTIL, "--yes", timeout=120)
    ctx.expect(ctx.luau('return game.ReplicatedStorage.Shared:FindFirstChild("Util") == nil') is True, "Util removed")
    ctx.expect(ctx.luau('return game.ReplicatedStorage:FindFirstChild("Shared") ~= nil') is True, "Shared folder kept")


def deleted_init_demotes_to_folder(ctx: Context):
    """SYNC-206: deleting init.luau demotes the script to a Folder and keeps its children (the
    module and its child are created in two pushes; one push is SYNC-257)."""
    _offline(ctx)
    ctx.write("src/ReplicatedStorage/Shared/Lib/init.luau", "return { lib = true }\n")
    ctx.rbx("ps", "src/ReplicatedStorage/Shared/Lib/init.luau", "--yes", timeout=120)
    ctx.write("src/ReplicatedStorage/Shared/Lib/Child.luau", "return { child = true }\n")
    ctx.rbx("ps", "src/ReplicatedStorage/Shared/Lib/Child.luau", "--yes", timeout=120)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Lib.ClassName") == "ModuleScript", "Lib is a ModuleScript")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Lib.Child.ClassName") == "ModuleScript", "Child is a ModuleScript")
    (ctx.root / "src/ReplicatedStorage/Shared/Lib/init.luau").unlink()
    ctx.rbx("ps", "src/ReplicatedStorage/Shared/Lib/init.luau", "--yes", timeout=120)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Lib.ClassName") == "Folder", "Lib demoted to a Folder")
    ctx.expect_in("child = true", _source(ctx, "game.ReplicatedStorage.Shared.Lib.Child"), "Child kept")


def new_init_module_with_a_child_in_one_push(ctx: Context):
    """SYNC-257: a new DIR/init.luau with a child file pushes in one directory push (D12 decides)."""
    _offline(ctx)
    ctx.write("src/ReplicatedStorage/Shared/Lib/init.luau", "return { lib = true }\n")
    ctx.write("src/ReplicatedStorage/Shared/Lib/Child.luau", "return { child = true }\n")
    pushed = ctx.rbx("ps", "src/ReplicatedStorage", "--yes", ok=None, timeout=120)
    ctx.expect(pushed.ok, f"one push of init + child refused: {pushed.error[:300]}")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Lib.ClassName .. game.ReplicatedStorage.Shared.Lib.Child.ClassName") == "ModuleScriptModuleScript", "Lib and Child created")


def script_naming_variants(ctx: Context):
    """SYNC-207: .server.lua, .client.luau, .run-client.luau, .plugin.luau and init.server.luau map
    to their classes and run contexts; a pull keeps a .lua file's extension."""
    _offline(ctx)
    ctx.write("src/ServerStorage/Legacy.server.lua", "return 1\n")
    ctx.write("src/ServerStorage/Local.client.luau", "return 2\n")
    ctx.write("src/ServerStorage/Ctx.run-client.luau", "return 3\n")
    ctx.write("src/ServerStorage/Plug.plugin.luau", "return 4\n")
    ctx.write("src/ServerStorage/Boot/init.server.luau", "return 5\n")
    ctx.write("src/ServerStorage/Boot/Helper.luau", "return 6\n")
    ctx.rbx("ps", "src/ServerStorage", "--yes", timeout=120)
    classes = ctx.luau('local s = game.ServerStorage return table.concat({s.Legacy.ClassName, s.Local.ClassName, s.Ctx.ClassName, s.Ctx.RunContext.Name, s.Plug.ClassName, s.Plug.RunContext.Name, s.Boot.ClassName, s.Boot.Helper.ClassName}, ",")')
    ctx.expect(classes == "Script,LocalScript,Script,Client,Script,Plugin,Script,ModuleScript", f"classes: {classes}")
    _studio_edit(ctx, 'game.ServerStorage.Legacy.Source = "return 11"')
    ctx.rbx("pl", timeout=120)
    ctx.expect((ctx.root / "src/ServerStorage/Legacy.server.lua").exists(), "the .lua file is kept by the pull")
    ctx.expect(not (ctx.root / "src/ServerStorage/Legacy.server.luau").exists(), "no .luau twin was written")
    ctx.expect_in("return 11", ctx.read("src/ServerStorage/Legacy.server.lua"), "pulled content")


def script_gains_a_child_moves_to_init(ctx: Context):
    """SYNC-208: a script that gains a child in Studio is pulled as DIR/init.server.luau and the
    leaf file goes away; the init file pushes back."""
    _offline(ctx)
    _studio_edit(ctx, 'local h = Instance.new("ModuleScript") h.Name = "Helper" h.Source = "return 9" h.Parent = game.ServerScriptService.Main')
    ctx.rbx("pl", timeout=120)
    ctx.expect((ctx.root / "src/ServerScriptService/Main/init.server.luau").exists(), "init.server.luau written")
    ctx.expect((ctx.root / "src/ServerScriptService/Main/Helper.luau").exists(), "child file written")
    ctx.expect(not (ctx.root / MAIN).exists(), "the leaf file was removed")
    ctx.write("src/ServerScriptService/Main/init.server.luau", 'print("main")\nreturn 208\n')
    ctx.rbx("ps", "src/ServerScriptService/Main/init.server.luau", "--yes", timeout=120)
    ctx.expect_in("return 208", _source(ctx, "game.ServerScriptService.Main"), "init pushed")
    ctx.expect(ctx.luau("return game.ServerScriptService.Main.Helper.Source") == "return 9", "child kept")


def sanitized_file_names_round_trip(ctx: Context):
    """SYNC-209: instance names a file system refuses (colon, reserved name, trailing dot) are
    sanitized on pull and still address their instances on push."""
    _offline(ctx)
    _studio_edit(ctx, 'for _, p in ipairs({{"Bad:Name", "return 1"}, {"CON", "return 2"}, {"Trail.", "return 3"}}) do local s = Instance.new("Script") s.Name = p[1] s.Source = p[2] s.Parent = game.ServerStorage end')
    ctx.rbx("pl", timeout=120)
    files = sorted(p.name for p in (ctx.root / "src/ServerStorage").iterdir())
    ctx.expect(files == ["Bad_Name.server.luau", "Trail.server.luau", "_CON.server.luau"], f"pulled file names: {files}")
    for name in files:
        ctx.write(f"src/ServerStorage/{name}", f"-- {name}\nreturn 209\n")
    ctx.rbx("ps", "src/ServerStorage", "--yes", timeout=120)
    names = ctx.luau('local t = {} for _, c in ipairs(game.ServerStorage:GetChildren()) do table.insert(t, c.Name .. "=" .. (c.Source:find("209") and "new" or "old")) end table.sort(t) return table.concat(t, ",")')
    ctx.expect(names == "Bad:Name=new,CON=new,Trail.=new", f"Studio names after the push: {names}")


def same_named_sibling_scripts(ctx: Context):
    """SYNC-210: two same-named sibling scripts pull as NAME and NAME_2 files and each file pushes
    to its own instance."""
    _offline(ctx)
    _studio_edit(ctx, 'local m = Instance.new("ModuleScript") m.Name = "Util" m.Source = "return 2" m.Parent = game.ReplicatedStorage.Shared')
    ctx.rbx("pl", timeout=120)
    files = sorted(p.name for p in (ctx.root / "src/ReplicatedStorage/Shared").iterdir())
    ctx.expect(files == ["Util.luau", "Util_2.luau"], f"pulled files: {files}")
    ctx.write("src/ReplicatedStorage/Shared/Util_2.luau", "return 22\n")
    ctx.rbx("ps", "src/ReplicatedStorage", "--yes", timeout=120)
    sources = ctx.luau('local t = {} for _, c in ipairs(game.ReplicatedStorage.Shared:GetChildren()) do table.insert(t, (c.Source:gsub("%s+$", ""))) end table.sort(t) return table.concat(t, "|")')
    ctx.expect(sources == "return 22|return { version = 1 }", f"sibling sources: {sources}")


# ---------------------------------------------------------------- B. file contents


def crlf_file_under_live_sync_does_not_bounce(ctx: Context):
    """SYNC-211: a CRLF file pushes, verifies, and is not pulled back as an LF rewrite."""
    _live(ctx)
    _, pulls = _counts(ctx)
    crlf = b'print("main")\r\nreturn 211\r\n'
    (ctx.root / MAIN).write_bytes(crlf)
    _settled(ctx)
    ctx.expect_in("return 211", _source(ctx, "game.ServerScriptService.Main"), "CRLF file pushed")
    verify = ctx.rbx("ps", MAIN, "--verify", timeout=120)
    ctx.expect(verify.get("sourceVerified") == 1 and not verify.get("sourceVerifyFailed"), f"verify: {verify.json}")
    time.sleep(5)
    _settled(ctx)
    ctx.expect((ctx.root / MAIN).read_bytes() == crlf, "the CRLF file was rewritten")
    ctx.expect(_counts(ctx)[1] == pulls, "the pushed file was pulled back")


def utf8_source_round_trip(ctx: Context):
    """SYNC-212: multibyte UTF-8 source survives push, verification and pull byte for byte."""
    _offline(ctx)
    text = 'print("héllo – 日本語 🎉")\n'
    ctx.write(MAIN, text)
    result = ctx.rbx("ps", MAIN, "--verify-sources", "--yes", timeout=120)
    ctx.expect(result.get("sourceVerified") == 1 and not result.get("sourceVerifyFailed"), f"verify: {result.json}")
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = game.ServerScriptService.Main.Source .. "-- ümlaut ✓\\n"')
    ctx.rbx("pl", timeout=120)
    ctx.expect(ctx.read(MAIN) == text + "-- ümlaut ✓\n", f"pulled bytes: {ctx.read(MAIN)!r}")


def non_utf8_file_is_refused_and_recovers(ctx: Context):
    """SYNC-213: a script file that is not UTF-8 blocks its path under Live Sync with an error
    naming it and syncs after the fix and rp; ps refuses it naming the file; lon refuses to
    start on it naming the file."""
    _live(ctx)
    before = _source(ctx, "game.ServerScriptService.Main")
    (ctx.root / MAIN).write_bytes(b'print("x")\n\xff\xfe\n')
    status = _wait_error(ctx)
    ctx.expect_in("Main.server.luau", _error_of(status), "Live Sync names the unreadable file")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main") == before, "Studio untouched")
    ctx.write(MAIN, 'print("main")\nreturn 213\n')
    ctx.rbx("rp", ok=None, timeout=60)
    status = _settled(ctx, 90)
    ctx.expect(status.pointer("/daemon/running") is True, f"Live Sync stopped on an unreadable file: {status.json}")
    ctx.expect(not _error_of(status), f"error after the fix: {status.json}")
    ctx.expect_in("return 213", _source(ctx, "game.ServerScriptService.Main"), "fixed file pushed")
    ctx.rbx("lof")
    (ctx.root / MAIN).write_bytes(b'print("x")\n\xff\xfe\n')
    refused = ctx.rbx("ps", MAIN, "--yes", ok=False, timeout=120)
    ctx.expect_in("Main.server.luau", refused.error, "ps refusal names the file")
    started = ctx.rbx("lon", ok=None, timeout=200)
    ctx.expect_in("Main.server.luau", started.error + _error_of(started), "lon names the unreadable file")
    ctx.expect_in("return 213", _source(ctx, "game.ServerScriptService.Main"), "Studio untouched by the refused start")
    ctx.write(MAIN, 'print("main")\nreturn 213\n')


def empty_script_file(ctx: Context):
    """SYNC-214: a 0-byte script pushes an empty Source, a pull keeps the file empty, and a later
    Studio edit is pulled."""
    _offline(ctx)
    ctx.write(MAIN, "")
    result = ctx.rbx("ps", MAIN, "--verify-sources", "--yes", timeout=120)
    ctx.expect(not result.get("sourceVerifyFailed"), f"verify: {result.json}")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main") == "", "empty Source in Studio")
    ctx.rbx("pl", timeout=120)
    ctx.expect((ctx.root / MAIN).exists() and (ctx.root / MAIN).stat().st_size == 0, "the empty file survives a pull")
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 214"')
    ctx.rbx("pl", timeout=120)
    ctx.expect(ctx.read(MAIN) == "return 214", "later edit pulled")


def large_sources(ctx: Context):
    """SYNC-215: a 1 MiB script pushes and verifies; a 9 MiB script is refused before Studio changes."""
    _offline(ctx)
    line = "local a = '" + "x" * 1000 + "'\n"
    big = line * 1050
    ctx.write(MAIN, big)
    result = ctx.rbx("ps", MAIN, "--verify-sources", "--yes", timeout=240)
    ctx.expect(result.get("sourceVerified") == 1 and not result.get("sourceVerifyFailed"), f"1 MiB verify: {result.json}")
    ctx.expect(ctx.luau("return #game.ServerScriptService.Main.Source") == len(big), "1 MiB source length")
    ctx.write(MAIN, line * 9500)
    refused = ctx.rbx("ps", MAIN, "--yes", ok=False, timeout=240)
    ctx.expect("exceed" in refused.error.lower() or "limit" in refused.error.lower() or "large" in refused.error.lower(), f"refusal text: {refused.error[:300]}")
    ctx.expect(ctx.luau("return #game.ServerScriptService.Main.Source") == len(big), "Studio keeps the 1 MiB source")


def pull_over_a_read_only_file(ctx: Context):
    """SYNC-216: a pull onto a read-only script either replaces it or refuses without publishing
    anything else; never a partial publish."""
    _offline(ctx)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 216"; workspace.Props.Sign.Value = "ro 216"')
    main_before = ctx.read(MAIN)
    store_before = (ctx.root / WORKSPACE).read_bytes()
    path = ctx.root / MAIN
    path.chmod(stat.S_IREAD)
    try:
        result = ctx.rbx("pl", ok=None, timeout=120)
    finally:
        path.chmod(stat.S_IREAD | stat.S_IWRITE)
    if result.ok:
        ctx.expect(ctx.read(MAIN) == "return 216", "the read-only file holds the Studio source")
        ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "ro 216", "store published")
    else:
        ctx.expect(ctx.read(MAIN) == main_before, f"refused pull changed the file: {result.error[:200]}")
        ctx.expect((ctx.root / WORKSPACE).read_bytes() == store_before, f"refused pull published the store (partial publish): {result.error[:200]}")


def symlinked_script_and_directory(ctx: Context):
    """SYNC-217 (N8): symlinks under src are ignored by directory walks and by an explicit path push
    (the canonical path leaves src); the pushes stay no-ops without an error and Studio is
    unchanged. The row records the behavior; nothing promises symlink support."""
    _offline(ctx)
    ctx.write("ext/Linked.server.luau", "return 217\n")
    ctx.write("ext/Mods/Mod.luau", "return { mod = 217 }\n")
    os.symlink(ctx.root / "ext/Linked.server.luau", ctx.root / "src/ServerScriptService/Linked.server.luau")
    os.symlink(ctx.root / "ext/Mods", ctx.root / "src/ServerStorage/Mods", target_is_directory=True)
    walked = ctx.rbx("ps", "src", "--yes", timeout=120)
    direct = ctx.rbx("ps", "src/ServerScriptService/Linked.server.luau", "--yes", timeout=120)
    linked = ctx.luau('return game.ServerScriptService:FindFirstChild("Linked") ~= nil')
    mods = ctx.luau('return game.ServerStorage:FindFirstChild("Mods") ~= nil')
    ctx.log.append(f"symlinks: walk {walked.json}; direct {direct.json}; Linked={linked} Mods={mods}")
    ctx.expect(_count(ctx, "game.ServerScriptService") == 1 and ctx.luau("return #game.ServerStorage:GetChildren()") == 0, "symlink pushes changed Studio unexpectedly")
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "symlink 217"')
    result = ctx.rbx("pl", ok=None, timeout=120)
    ctx.expect(result.ok, f"pull with symlinks in src: {result.error[:300]}")
    ctx.expect((ctx.root / "src/ServerScriptService/Linked.server.luau").is_symlink() and ctx.read("ext/Linked.server.luau").strip() == "return 217", "the pull disturbed the symlink or its target")


# ---------------------------------------------------------------- C. watcher events


def case_only_rename_under_live_sync(ctx: Context):
    """SYNC-220: renaming a script file to a different letter case leaves one script with its
    source and no error."""
    _live(ctx)
    os.rename(ctx.root / MAIN, ctx.root / "src/ServerScriptService/main.server.luau")
    status = _settled(ctx)
    ctx.expect(not _error_of(status) and status.pointer("/daemon/resolutionRequired") is not True, f"after the case rename: {status.json}")
    ctx.expect(_count(ctx, "game.ServerScriptService") == 1, "exactly one script after a case-only rename")
    ctx.expect_in('print("main")', ctx.luau("return game.ServerScriptService:GetChildren()[1].Source"), "source intact")


def script_rename_under_live_sync(ctx: Context):
    """SYNC-221: renaming a script file replaces the instance by the new name."""
    _live(ctx)
    os.rename(ctx.root / MAIN, ctx.root / "src/ServerScriptService/Boot.server.luau")
    status = _settled(ctx)
    ctx.expect(not _error_of(status), f"after the rename: {status.json}")
    ctx.expect(ctx.luau('return game.ServerScriptService:FindFirstChild("Main") == nil') is True, "Main gone")
    ctx.expect_in('print("main")', _source(ctx, "game.ServerScriptService.Boot"), "Boot has the source")
    ctx.expect(_count(ctx, "game.ServerScriptService") == 1, "one script")


def directory_rename_under_live_sync(ctx: Context):
    """SYNC-222: renaming a directory of scripts moves its scripts in Studio."""
    _live(ctx)
    os.rename(ctx.root / "src/ReplicatedStorage/Shared", ctx.root / "src/ReplicatedStorage/Shared2")
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"after the directory rename: {status.json}")
    deadline = time.monotonic() + 30
    present = False
    while time.monotonic() < deadline and not present:
        probe = ctx.rbx("l", 'return game.ReplicatedStorage:FindFirstChild("Shared2") ~= nil', ok=None, timeout=60)
        present = (probe.get("results") or [None])[0] is True
        if not present:
            time.sleep(2)
    status = ctx.rbx("lst", "--wait", "30", ok=None, timeout=120)
    old_util = ctx.luau('local s = game.ReplicatedStorage:FindFirstChild("Shared") return s ~= nil and s:FindFirstChild("Util") ~= nil')
    ctx.expect(present, f"Shared2 never appeared in Studio (D18); old Shared.Util still present: {old_util}; status {status.json}")
    ctx.expect_in("version = 1", _source(ctx, "game.ReplicatedStorage.Shared2.Util"), "Shared2.Util")
    ctx.expect(ctx.luau('local s = game.ReplicatedStorage:FindFirstChild("Shared") return s == nil or s:FindFirstChild("Util") == nil') is True, "old Util gone")


def directory_swap_under_live_sync(ctx: Context):
    """SYNC-223 (D6 class): a directory swapped into place over the old one pushes the files it
    carries."""
    _live(ctx)
    ctx.write("src/ReplicatedStorage/Shared.new/Util.luau", "return { version = 223 }\n")
    _rename(ctx.root / "src/ReplicatedStorage/Shared", ctx.root / "src/ReplicatedStorage/Shared.old")
    _rename(ctx.root / "src/ReplicatedStorage/Shared.new", ctx.root / "src/ReplicatedStorage/Shared")
    shutil.rmtree(ctx.root / "src/ReplicatedStorage/Shared.old")
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"after the swap: {status.json}")
    ctx.expect_in("version = 223", _wait_source(ctx, "game.ReplicatedStorage.Shared.Util", "223"), "swapped content pushed")


def directory_delete_under_live_sync(ctx: Context):
    """SYNC-224: deleting a service directory removes its scripts in Studio."""
    _live(ctx)
    shutil.rmtree(ctx.root / "src/ReplicatedStorage")
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"after the delete: {status.json}")
    ctx.expect(ctx.luau('local s = game.ReplicatedStorage:FindFirstChild("Shared") return s == nil or s:FindFirstChild("Util") == nil') is True, "Util removed")


def delete_and_recreate_identical(ctx: Context):
    """SYNC-225: a file deleted and recreated with the same bytes leaves Studio's instance intact."""
    _live(ctx)
    content = ctx.read(MAIN)
    (ctx.root / MAIN).unlink()
    ctx.write(MAIN, content)
    time.sleep(3)
    status = _settled(ctx)
    ctx.expect(not _error_of(status), f"after delete+recreate: {status.json}")
    ctx.expect(ctx.luau('return game.ServerScriptService:FindFirstChild("Main") ~= nil') is True, "Main still exists")
    ctx.expect_in('print("main")', _source(ctx, "game.ServerScriptService.Main"), "source intact")


def non_script_files_under_live_sync_settle(ctx: Context):
    """SYNC-226: files the projection ignores (notes.txt, data.json) do not stay pending or wedge."""
    _live(ctx)
    ctx.write("src/ServerScriptService/notes.txt", "notes\n")
    ctx.write("src/ServerScriptService/data.json", '{"a": 1}\n')
    status = ctx.rbx("lst", "--wait", "30", ok=None, timeout=120)
    ctx.expect(status.pointer("/daemon/pendingCount", 0) == 0 and not _error_of(status), f"non-script files left Live Sync unsettled: {status.json}")
    ctx.expect(_count(ctx, "game.ServerScriptService") == 1, "no instance created")


def new_nested_directory_under_live_sync(ctx: Context):
    """SYNC-227: a script saved in a new nested directory creates the folders and the script."""
    _live(ctx)
    ctx.write("src/ServerStorage/New/Deep/Mod.luau", "return 227\n")
    _settled(ctx)
    ctx.expect(ctx.luau("return game.ServerStorage.New.ClassName .. game.ServerStorage.New.Deep.ClassName .. game.ServerStorage.New.Deep.Mod.ClassName") == "FolderFolderModuleScript", "folders and module created")
    ctx.expect(_source(ctx, "game.ServerStorage.New.Deep.Mod").strip() == "return 227", "source")


def file_move_between_services_under_live_sync(ctx: Context):
    """SYNC-228: moving a script file to another service's directory moves the instance."""
    _live(ctx)
    (ctx.root / "src/ServerStorage").mkdir(exist_ok=True)
    os.rename(ctx.root / UTIL, ctx.root / "src/ServerStorage/Util.luau")
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"after the move: {status.json}")
    ctx.expect_in("version = 1", _source(ctx, "game.ServerStorage.Util"), "Util in ServerStorage")
    ctx.expect(ctx.luau('return game.ReplicatedStorage.Shared:FindFirstChild("Util") == nil') is True, "Util left Shared")


def store_file_deleted_under_live_sync(ctx: Context):
    """SYNC-229: a deleted store file never empties its service in Studio; a pull recreates it."""
    _live(ctx)
    brightness = ctx.luau("return game.Lighting.Brightness")
    children = _count(ctx, "game.Lighting")
    (ctx.root / LIGHTING).unlink()
    status = ctx.rbx("lst", "--wait", "30", ok=None, timeout=120)
    ctx.expect(ctx.luau("return game.Lighting.Brightness") == brightness and _count(ctx, "game.Lighting") == children, f"Lighting changed after its store was deleted: {status.json}")
    ctx.rbx("pl", timeout=120)
    ctx.expect((ctx.root / LIGHTING).exists(), "pull recreated the store")
    status = _settled(ctx, 60)
    ctx.expect(not _error_of(status), f"after the pull: {status.json}")


def store_file_corrupted_under_live_sync_push(ctx: Context):
    """SYNC-230: an undecodable store blocks its push with an error naming it; restoring the bytes
    and rp recovers."""
    _live(ctx)
    good = (ctx.root / LIGHTING).read_bytes()
    brightness = ctx.luau("return game.Lighting.Brightness")
    (ctx.root / LIGHTING).write_bytes(b"not a store")
    status = _wait_error(ctx)
    details = ctx.rbx("lst", "--details", ok=None, timeout=60)
    ctx.expect("Lighting" in _error_of(status) + json.dumps(details.json) + details.error, f"neither the error nor --details names the store (N7): {_error_of(status)} / {details.json}")
    ctx.expect(ctx.luau("return game.Lighting.Brightness") == brightness, "Studio untouched")
    (ctx.root / LIGHTING).write_bytes(good)
    ctx.rbx("rp", ok=None, timeout=60)
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"error after the restore: {status.json}")


def _locker(ctx: Context, relative: str, content: str, seconds: float) -> subprocess.Popen:
    """Writes the file, then holds it open with share mode 0 (no sharing) for a while."""
    code = (
        "import ctypes,sys,time\n"
        "path,content,seconds=sys.argv[1],sys.argv[2],float(sys.argv[3])\n"
        "open(path,'w',encoding='utf-8',newline='\\n').write(content)\n"
        "k=ctypes.windll.kernel32\n"
        "h=k.CreateFileW(path,0x80000000|0x40000000,0,None,3,0x80,None)\n"
        "print('handle', h, flush=True)\n"
        "time.sleep(seconds)\n"
        "k.CloseHandle(h)\n"
        "print('released', flush=True)\n"
    )
    return subprocess.Popen([sys.executable, "-I", "-c", code, str(ctx.root / relative), content, str(seconds)], stdout=subprocess.PIPE, text=True)


def file_locked_during_the_watcher_read(ctx: Context):
    """SYNC-231: a file held without sharing while the watcher reads it does not stop Live Sync;
    once released the content reaches Studio."""
    _live(ctx)
    locker = _locker(ctx, MAIN, 'print("main")\nreturn 231\n', 6)
    locker.wait(timeout=60)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and "return 231" not in _source(ctx, "game.ServerScriptService.Main"):
        status = ctx.rbx("lst", ok=None, timeout=60)
        if _error_of(status):
            ctx.rbx("rp", ok=None, timeout=60)
        time.sleep(3)
    status = ctx.rbx("lst", "--wait", "30", ok=None, timeout=120)
    ctx.expect(status.pointer("/daemon/running") is True, f"Live Sync stopped: {status.json}")
    ctx.expect_in("return 231", _source(ctx, "game.ServerScriptService.Main"), "content pushed after the lock")
    ctx.expect(not _error_of(status), f"error after the release: {status.json}")


# ---------------------------------------------------------------- D. reconcile


def store_property_conflict_on_both_sides(ctx: Context):
    """SYNC-232: the same stored property edited on both sides while Live Sync is off is a conflict;
    --prefer editor resolves it."""
    _live(ctx)
    ctx.rbx("lof")
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "file")
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "studio"')
    started = ctx.rbx("lon", ok=False, timeout=200)
    ctx.expect_in("Sync needs review", started.error, "store conflict reported")
    ctx.expect("Workspace.renium" in started.error and "Sign" in started.error and "Value" in started.error, f"conflict names the store, instance and property: {started.error[:300]}")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "studio" and ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "file", "neither side written")
    ctx.rbx("lon", "--prefer", "editor", "--wait", "60", timeout=200)
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "file", "file side applied")


def different_properties_of_one_instance_merge(ctx: Context):
    """SYNC-233: different properties of one instance edited on each side merge on lon."""
    _live(ctx)
    ctx.rbx("lof")
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Anchored", "--bool", "true")
    _studio_edit(ctx, "workspace.Props.Crate.Transparency = 0.5")
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True and "both sides" not in started.error, f"merge produced a conflict: {started.json}")
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True and ctx.luau("return workspace.Props.Crate.Transparency") == 0.5, "Studio has both")
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Transparency").json == 0.5, "store has the Studio edit")


def studio_deletion_is_synced_not_resurrected(ctx: Context):
    """SYNC-234 (guide: missing from both sides means deleted in Studio and synced): a Studio
    deletion is pulled, survives a restart, and a deletion made while Live Sync is off is pulled."""
    _live(ctx)
    _studio_edit(ctx, "workspace.Props.Sign:Destroy()")
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and ctx.rbx("bg", "Workspace", "Props.Sign", "Value", ok=None).ok:
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect(not ctx.rbx("bg", "Workspace", "Props.Sign", "Value", ok=None).ok, "Sign removed from the store")
    ctx.rbx("lof")
    ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(ctx.luau('return workspace.Props:FindFirstChild("Sign") == nil') is True, "Sign not resurrected")
    ctx.rbx("lof")
    _studio_edit(ctx, "workspace.Props:FindFirstChild('Crate', true):Destroy()")
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True, f"offline deletion: {started.json}")
    ctx.expect(_count(ctx, "workspace.Props") == 1, "one Crate left in Studio")
    found = ctx.rbx("f", "Workspace", "-n", "Crate", "--all", "-l", "10")
    matches = found.json if isinstance(found.json, list) else (found.get("matches") or [])
    ctx.expect(len(matches) == 1, f"store Crates after the offline deletion: {len(matches)}")


def studio_script_rename_pulled(ctx: Context):
    """SYNC-235: a script renamed in Studio is pulled as a renamed file."""
    _live(ctx)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Name = "Boot"')
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and not (ctx.root / "src/ServerScriptService/Boot.server.luau").exists():
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect((ctx.root / "src/ServerScriptService/Boot.server.luau").exists(), "renamed file written")
    ctx.expect(not (ctx.root / MAIN).exists(), "old file removed")
    ctx.expect_in('print("main")', ctx.read("src/ServerScriptService/Boot.server.luau"), "content kept")


def studio_script_move_between_services_pulled(ctx: Context):
    """SYNC-236: a script reparented to another service in Studio moves its file."""
    _live(ctx)
    _studio_edit(ctx, "game.ReplicatedStorage.Shared.Util.Parent = game.ServerStorage")
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline and not (ctx.root / "src/ServerStorage/Util.luau").exists():
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect((ctx.root / "src/ServerStorage/Util.luau").exists(), "file moved to ServerStorage")
    ctx.expect(not (ctx.root / UTIL).exists(), "old file removed")


def studio_deletion_vs_file_edit_on_restart(ctx: Context):
    """SYNC-237: with Live Sync off, a Studio-deleted script whose file is unchanged is pulled as
    a deletion with history; a deleted script whose file was edited is a conflict and --prefer
    editor recreates it."""
    _live(ctx)
    ctx.rbx("lof")
    before = ctx.read(MAIN)
    _studio_edit(ctx, "game.ServerScriptService.Main:Destroy()")
    ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(not (ctx.root / MAIN).exists(), "deleted script's file removed")
    printed = ctx.rbx("rev", "--path", MAIN, "--print", json_mode=False, ok=None, timeout=60)
    ctx.expect(printed.code == 0 and before.strip() in printed.stdout, f"sync history holds the removed file: {printed.stderr[:200]}")
    ctx.rbx("lof")
    ctx.write(UTIL, "return { version = 237 }\n")
    _studio_edit(ctx, "game.ReplicatedStorage.Shared.Util:Destroy()")
    started = ctx.rbx("lon", ok=None, timeout=200)
    ctx.expect("both sides" in started.error or started.pointer("/daemon/resolutionRequired") is True, f"edit vs deletion was not a conflict: {started.json} {started.error[:200]}")
    ctx.expect_in("version = 237", ctx.read(UTIL), "file kept during the conflict")
    ctx.rbx("lon", "--prefer", "editor", "--wait", "60", timeout=200)
    ctx.expect_in("version = 237", _wait_source(ctx, "game.ReplicatedStorage.Shared:FindFirstChild('Util')", "237"), "Util recreated from the file")


def cli_push_then_lon_has_no_conflict(ctx: Context):
    """SYNC-238: a CLI push made while Live Sync is off leaves the next lon with nothing to move."""
    _live(ctx)
    ctx.rbx("lof")
    ctx.write(MAIN, 'print("main")\nreturn 238\n')
    ctx.rbx("ps", MAIN, "--yes", timeout=120)
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True and "both sides" not in started.error, f"lon after a CLI push: {started.json}")
    ctx.expect(started.pointer("/daemon/pushes", 0) == 0 and started.pointer("/daemon/pulls", 0) == 0, f"lon re-synced after a CLI push: {started.json}")
    ctx.expect_in("return 238", _source(ctx, "game.ServerScriptService.Main"), "Studio keeps the pushed source")


# ---------------------------------------------------------------- E. properties and services


def service_root_property_both_ways(ctx: Context):
    """SYNC-239: a service root property syncs from the store and back from Studio."""
    _offline(ctx)
    ctx.rbx("bs", "Workspace", "-p", "Gravity", "--num", "120")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Gravity") == 120, "Gravity pushed")
    _studio_edit(ctx, "workspace.Gravity = 150")
    ctx.rbx("pl", timeout=120)
    ctx.expect(ctx.rbx("bg", "Workspace", "-p", "Gravity").json == 150, "Gravity pulled")


def terrain_property_both_ways(ctx: Context):
    """SYNC-240: a Terrain property syncs both ways although Terrain is an engine-managed root."""
    _offline(ctx)
    ctx.rbx("bs", "Workspace", "Terrain", "-p", "WaterTransparency", "--num", "0.25")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(abs(float(ctx.luau("return workspace.Terrain.WaterTransparency")) - 0.25) < 1e-6, "WaterTransparency pushed")
    _studio_edit(ctx, "workspace.Terrain.WaterWaveSize = 0.4")
    ctx.rbx("pl", timeout=120)
    ctx.expect(abs(float(ctx.rbx("bg", "Workspace", "Terrain", "WaterWaveSize").json) - 0.4) < 1e-6, "WaterWaveSize pulled")


def camera_property_both_ways(ctx: Context):
    """SYNC-241: the Workspace viewport camera is Studio's (N9): bs refuses to edit it, a push after
    the refusal has nothing to do, and a Studio edit of it is not pulled into the store."""
    _offline(ctx)
    refused = ctx.rbx("bs", "Workspace", "Camera", "-p", "FieldOfView", "--num", "80", ok=False)
    ctx.expect("viewport camera" in refused.error, f"bs on the viewport camera was accepted (N9): {refused.json} {refused.error[:200]}")
    pushed = ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(pushed.pointer("/unchanged") is True, f"a refused camera edit changed the store: {pushed.json}")
    _studio_edit(ctx, "workspace.CurrentCamera.FieldOfView = 60")
    ctx.rbx("pl", timeout=120)
    stored = ctx.rbx("bg", "Workspace", "Camera", "FieldOfView", ok=None)
    ctx.expect(stored.json != 60, f"the viewport camera's Studio edit was pulled: {stored.json}")


def attributes_both_ways(ctx: Context):
    """SYNC-242: attributes set in the store reach Studio; attributes set and removed in Studio
    reach the store."""
    _offline(ctx)
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Reviewed", "--scope", "attribute", "--bool", "true")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau('return workspace.Props.Crate:GetAttribute("Reviewed")') is True, "attribute pushed")
    _studio_edit(ctx, 'workspace.Props.Crate:SetAttribute("Note", "hi")')
    ctx.rbx("pl", timeout=120)
    dump = json.dumps(ctx.rbx("in", "Workspace", "-i", "editor:2", "-F", "attrs").json)
    ctx.expect('"Note"' in dump and '"hi"' in dump, f"attribute pulled: {dump[:300]}")
    _studio_edit(ctx, 'workspace.Props.Crate:SetAttribute("Note", nil)')
    ctx.rbx("pl", timeout=120)
    dump = json.dumps(ctx.rbx("in", "Workspace", "-i", "editor:2", "-F", "attrs").json)
    ctx.expect('"Note"' not in dump, f"removed attribute still stored: {dump[:300]}")


def tags_both_ways(ctx: Context):
    """SYNC-243: CollectionService tags sync from the store and back."""
    _offline(ctx)
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Tags", "-j", '["a","b"]')
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau('local cs = game:GetService("CollectionService") return cs:HasTag(workspace.Props.Crate, "a") and cs:HasTag(workspace.Props.Crate, "b")') is True, "tags pushed")
    _studio_edit(ctx, 'game:GetService("CollectionService"):AddTag(workspace.Props.Crate, "c")')
    ctx.rbx("pl", timeout=120)
    tags = ctx.rbx("bg", "Workspace", "-i", "editor:2", "Tags").json
    ctx.expect(isinstance(tags, list) and set(tags) == {"a", "b", "c"}, f"tags pulled: {tags}")


def cross_service_reference_round_trip(ctx: Context):
    """SYNC-244: an ObjectValue pointing into another service is pulled as a path reference, kept
    by a later push, and set from the store."""
    _offline(ctx)
    ctx.rbx("ba", "Workspace", "-n", "Ref", "-c", "ObjectValue", "--no-parent")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    _studio_edit(ctx, "workspace.Ref.Value = game.ReplicatedStorage.Shared")
    ctx.rbx("pl", timeout=120)
    value = json.dumps(ctx.rbx("bg", "Workspace", "Ref", "Value").json)
    ctx.expect("ReplicatedStorage" in value and "Shared" in value, f"pulled reference: {value}")
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "244")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Ref.Value == game.ReplicatedStorage.Shared") is True, "reference kept by the push")
    ctx.rbx("bs", "Workspace", "Ref", "-p", "Value", "-j", '{"_type":"Ref","pathSegments":["ServerScriptService","Main"],"pathOrdinals":[1,1]}')
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Ref.Value == game.ServerScriptService.Main") is True, "store-set reference applied")


def reference_to_a_deleted_script_is_repaired(ctx: Context):
    """SYNC-245: deleting a referenced script's file removes the script and clears the reference
    without an error."""
    _offline(ctx)
    ctx.rbx("ba", "ServerScriptService", "-n", "Ref", "-c", "ObjectValue", "--no-parent")
    ctx.rbx("ps", "instances/ServerScriptService.renium", "--yes", timeout=120)
    _studio_edit(ctx, "game.ServerScriptService.Ref.Value = game.ServerScriptService.Main")
    ctx.rbx("pl", timeout=120)
    (ctx.root / MAIN).unlink()
    ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect(ctx.luau('return game.ServerScriptService:FindFirstChild("Main") == nil') is True, "Main removed")
    ctx.expect(ctx.luau("return game.ServerScriptService.Ref.Value == nil") is True, "reference cleared")
    stored = ctx.rbx("bg", "ServerScriptService", "Ref", "Value").json
    ctx.expect(stored in (None, "") or (isinstance(stored, dict) and set(stored) <= {"_type"}), f"store reference cleared: {stored}")


def service_outside_the_sync_set(ctx: Context):
    """SYNC-246: a service outside the default sync set is refused with a clear error by the store
    commands or the push, and never reaches Studio."""
    _offline(ctx)
    added = ctx.rbx("ba", "Chat", "-n", "X", "-c", "Folder", ok=None, timeout=60)
    if added.ok:
        pushed = ctx.rbx("ps", "instances/Chat.renium", "--yes", ok=False, timeout=120)
        ctx.expect_in("Chat", pushed.error, "push refusal names the service")
    else:
        ctx.expect_in("Chat", added.error, "ba refusal names the service")
    ctx.expect(ctx.luau('return game:GetService("Chat"):FindFirstChild("X") == nil') is True, "nothing created in Chat")
    pulled = ctx.rbx("pl", "-s", "Chat", ok=False, timeout=120)
    ctx.expect_in("Chat", pulled.error, "pull refusal names the service")


def pull_one_service_leaves_others(ctx: Context):
    """SYNC-247: pl -s SERVICE publishes only that service's files."""
    _offline(ctx)
    before = ctx.rbx("bg", "Lighting", "-p", "Brightness").json
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "w247"; game.Lighting.Brightness = 4')
    ctx.rbx("pl", "-s", "Workspace", timeout=120)
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "w247", "Workspace pulled")
    ctx.expect(ctx.rbx("bg", "Lighting", "-p", "Brightness").json == before, "Lighting untouched by a Workspace-only pull")
    ctx.rbx("pl", timeout=120)
    ctx.expect(ctx.rbx("bg", "Lighting", "-p", "Brightness").json == 4, "full pull brings Lighting")


def single_service_pull_under_live_sync_keeps_other_pending(ctx: Context):
    """SYNC-248: a one-service CLI pull while Live Sync runs does not drop pending edits of other
    services; everything settles into the store."""
    _live(ctx)
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "w248"; game.Lighting.Brightness = 5')
    ctx.rbx("pl", "-s", "Lighting", timeout=120)
    status = _settled(ctx, 90)
    ctx.expect(not _error_of(status), f"after the partial pull: {status.json}")
    ctx.expect(ctx.rbx("bg", "Lighting", "-p", "Brightness").json == 5, "Lighting stored")
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "w248", "Workspace edit still pulled")


def push_during_play_targets_edit(ctx: Context):
    """SYNC-249: a CLI push during solo Play edits the Edit runtime."""
    _offline(ctx)
    ctx.rbx("play", "-s", timeout=180)
    try:
        ctx.write(MAIN, 'print("main")\nreturn 249\n')
        ctx.rbx("ps", MAIN, "--yes", timeout=120)
    finally:
        ctx.rbx("play", "-x", ok=None, timeout=120)
    ctx.expect_in("return 249", _source(ctx, "game.ServerScriptService.Main"), "Edit runtime has the push")


def run_context_client_script_round_trip(ctx: Context):
    """SYNC-250: a Script with RunContext Client pulls as NAME.run-client.luau and keeps its run
    context on push."""
    _offline(ctx)
    _studio_edit(ctx, 'local s = Instance.new("Script") s.Name = "Ctx" s.RunContext = Enum.RunContext.Client s.Source = "return 1" s.Parent = game.ServerStorage')
    ctx.rbx("pl", timeout=120)
    ctx.expect((ctx.root / "src/ServerStorage/Ctx.run-client.luau").exists(), f"files: {sorted(p.name for p in (ctx.root / 'src/ServerStorage').iterdir())}")
    ctx.write("src/ServerStorage/Ctx.run-client.luau", "return 250\n")
    ctx.rbx("ps", "src/ServerStorage/Ctx.run-client.luau", "--yes", timeout=120)
    ctx.expect(ctx.luau("return game.ServerStorage.Ctx.RunContext.Name") == "Client", "run context kept")
    ctx.expect_in("return 250", _source(ctx, "game.ServerStorage.Ctx"), "source pushed")


# ---------------------------------------------------------------- F. identity


def pi_project_pulls_once_then_syncs(ctx: Context):
    """SYNC-251 (projects.md): a pi-imported project adopts Studio's ids with one pull, after which
    verification, pushes and Live Sync work."""
    _offline(ctx)
    ctx.rbx("pi", ctx.place, "-r", "imported", timeout=120)
    other = ctx.root / "imported"
    ctx.rbx("pl", timeout=120, cwd=other)
    verify = ctx.rbx("ps", MAIN, "--verify", timeout=120, cwd=other)
    ctx.expect(verify.get("sourceVerified") == 1 and not verify.get("sourceVerifyFailed"), f"verify from the imported project: {verify.json}")
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "pi 251", cwd=other)
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120, cwd=other)
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "pi 251", "store push from the imported project")
    ctx.expect(_count(ctx, "workspace.Props") == 3, "no duplicates")
    started = ctx.rbx("lon", "--wait", "60", timeout=200, cwd=other)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True and not _error_of(started), f"lon from the imported project: {started.json}")
    ctx.rbx("lof", ok=None, cwd=other)


def pi_project_push_without_pull_makes_no_duplicates(ctx: Context):
    """SYNC-252: a store push from a pi-imported project that skipped the pull matches instances
    by path instead of duplicating them."""
    _offline(ctx)
    ctx.rbx("pi", ctx.place, "-r", "imported", timeout=120)
    other = ctx.root / "imported"
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "pi 252", cwd=other)
    pushed = ctx.rbx("ps", WORKSPACE, "-u", "--yes", timeout=120, cwd=other)
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "pi 252", f"value pushed (N11: the push answered {pushed.json} and applied nothing)")
    ctx.expect(_count(ctx, "workspace.Props") == 3 and ctx.luau('local n = 0 for _, c in ipairs(workspace:GetChildren()) do if c.Name == "Props" then n += 1 end end return n') == 1, "no duplicate instances")


def studio_instance_without_identity_matches_store_by_path(ctx: Context):
    """SYNC-253: an instance created in Studio and the same path added to the store resolve to one
    instance on an upsert push."""
    _offline(ctx)
    _studio_edit(ctx, 'Instance.new("Folder", game.ServerStorage).Name = "Dup"')
    ctx.rbx("ba", "ServerStorage", "-n", "Dup", "-c", "Folder", "--no-parent")
    ctx.rbx("ps", "instances/ServerStorage.renium", "-u", "--yes", timeout=120)
    ctx.expect(ctx.luau('local n = 0 for _, c in ipairs(game.ServerStorage:GetChildren()) do if c.Name == "Dup" then n += 1 end end return n') == 1, "one Dup after the upsert")


def inline_source_store_survives_pushes(ctx: Context):
    """SYNC-254: a store whose script source is inlined (a packed link) keeps that script through a
    store push, a full push and Live Sync; the missing file is not a deletion."""
    _offline(ctx)
    ctx.rbx("lkp", "--link-folder", "links", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Shared"]', timeout=60)
    ctx.expect(not (ctx.root / UTIL).exists(), "lkp inlined the source")
    ctx.rbx("ps", "instances/ReplicatedStorage.renium", "--yes", timeout=120)
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Shared.Util.ClassName") == "ModuleScript" and "version = 1" in _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util kept by the store push")
    ctx.rbx("ps", "--yes", timeout=180)
    ctx.expect(ctx.luau('local u = game.ReplicatedStorage.Shared:FindFirstChild("Util") return u and u.ClassName or "missing"') == "ModuleScript", "Util kept by the full push")
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(not _error_of(started) and started.pointer("/daemon/resolutionRequired") is not True, f"lon with an inline store: {started.json}")
    ctx.expect(ctx.luau('local u = game.ReplicatedStorage.Shared:FindFirstChild("Util") return u and u.ClassName or "missing"') == "ModuleScript", "Util kept by Live Sync")


# ---------------------------------------------------------------- G. control commands


def control_commands_with_nothing_to_do(ctx: Context):
    """SYNC-255: dp and rp with nothing pending succeed; rv, rev --sync and rev --print with
    nothing to act on fail with an error."""
    _live(ctx)
    ctx.rbx("dp", timeout=60)
    ctx.rbx("rp", timeout=60)
    review = ctx.rbx("rv", "apply", ok=False, timeout=60)
    ctx.expect_in("review", review.error.lower(), "rv without a review")
    bogus = ctx.rbx("rev", "--sync", "bogus-id", ok=False, timeout=60)
    ctx.expect(bogus.error, "rev --sync with an unknown id")
    ctx.log.append(f"rev --sync bogus-id: {bogus.error[:200]} (N10: a raw OS error, not an unknown-id message)")
    printed = ctx.rbx("rev", "--path", "src/ServerScriptService/Nowhere.server.luau", "--print", json_mode=False, ok=None, timeout=60)
    ctx.expect(printed.code != 0, f"rev --print for a file no entry holds succeeded: {printed.stdout[:200]!r}")


def lon_without_studio_and_bad_flags(ctx: Context):
    """SYNC-256: lon with Studio closed fails with the reopen verdict and creates no marker;
    invalid --prefer and --initial-sync-priority values are refused."""
    _offline(ctx)
    ctx.close_studio(save=True)
    started = ctx.rbx("lon", ok=False, timeout=120)
    text = (started.error + json.dumps(started.json or {})).lower()
    ctx.expect("studio" in text, f"no Studio verdict: {text[:300]}")
    ctx.expect(not (ctx.root / ".renium" / "live-watch-state.enabled").exists(), "marker written without a session")
    ctx.reopen_studio()
    for args in (["lon", "--prefer", "bogus"], ["lon", "--initial-sync-priority", "bogus"]):
        result = ctx.rbx(*args, ok=None, timeout=120)
        ctx.expect(not result.ok, f"{' '.join(args)} was accepted: {result.json}")
    status = ctx.rbx("lst", ok=None, timeout=60)
    ctx.expect(status.pointer("/daemon/running") is not True, f"a refused lon started a session: {status.json}")


SCENARIOS = [
    Scenario("SYNC-201", "ps from a subdirectory", "small", _tidy(ps_from_a_subdirectory)),
    Scenario("SYNC-202", "ps of unmapped paths pushes nothing", "small", _tidy(ps_of_unmapped_paths_pushes_nothing)),
    Scenario("SYNC-203", "ps with an unknown target id", "small", _tidy(ps_with_an_unknown_target_id)),
    Scenario("SYNC-204", "-t limits the push to one property", "small", _tidy(target_property_limits_the_push)),
    Scenario("SYNC-205", "deleted script path removes the instance", "small", _tidy(deleted_script_path_removes_the_instance)),
    Scenario("SYNC-206", "deleted init demotes to a Folder", "small", _tidy(deleted_init_demotes_to_folder)),
    Scenario("SYNC-207", "script naming variants", "small", _tidy(script_naming_variants)),
    Scenario("SYNC-257", "new init module with a child in one push", "small", _tidy(new_init_module_with_a_child_in_one_push)),
    Scenario("SYNC-208", "script gains a child, moves to init", "small", _tidy(script_gains_a_child_moves_to_init)),
    Scenario("SYNC-209", "sanitized file names round-trip", "small", _tidy(sanitized_file_names_round_trip)),
    Scenario("SYNC-210", "same-named sibling scripts", "small", _tidy(same_named_sibling_scripts)),
    Scenario("SYNC-211", "CRLF file under Live Sync does not bounce", "small", _tidy(crlf_file_under_live_sync_does_not_bounce)),
    Scenario("SYNC-212", "UTF-8 source round-trip", "small", _tidy(utf8_source_round_trip)),
    Scenario("SYNC-213", "non-UTF-8 file is refused and recovers", "small", _tidy(non_utf8_file_is_refused_and_recovers)),
    Scenario("SYNC-214", "empty script file", "small", _tidy(empty_script_file)),
    Scenario("SYNC-215", "large sources", "small", _tidy(large_sources)),
    Scenario("SYNC-216", "pull over a read-only file", "small", _tidy(pull_over_a_read_only_file), platforms=("nt",)),
    Scenario("SYNC-217", "symlinked script and directory", "small", _tidy(symlinked_script_and_directory)),
    Scenario("SYNC-220", "case-only rename under Live Sync", "small", _tidy(case_only_rename_under_live_sync), platforms=("nt",)),
    Scenario("SYNC-221", "script rename under Live Sync", "small", _tidy(script_rename_under_live_sync)),
    Scenario("SYNC-222", "directory rename under Live Sync", "small", _tidy(directory_rename_under_live_sync)),
    Scenario("SYNC-223", "directory swap under Live Sync", "small", _tidy(directory_swap_under_live_sync)),
    Scenario("SYNC-224", "directory delete under Live Sync", "small", _tidy(directory_delete_under_live_sync)),
    Scenario("SYNC-225", "delete and recreate identical", "small", _tidy(delete_and_recreate_identical)),
    Scenario("SYNC-226", "non-script files under Live Sync settle", "small", _tidy(non_script_files_under_live_sync_settle)),
    Scenario("SYNC-227", "new nested directory under Live Sync", "small", _tidy(new_nested_directory_under_live_sync)),
    Scenario("SYNC-228", "file move between services under Live Sync", "small", _tidy(file_move_between_services_under_live_sync)),
    Scenario("SYNC-229", "store file deleted under Live Sync", "small", _tidy(store_file_deleted_under_live_sync)),
    Scenario("SYNC-230", "store file corrupted under Live Sync (push)", "small", _tidy(store_file_corrupted_under_live_sync_push)),
    Scenario("SYNC-231", "file locked during the watcher read", "small", _tidy(file_locked_during_the_watcher_read), platforms=("nt",)),
    Scenario("SYNC-232", "store property conflict on both sides", "small", _tidy(store_property_conflict_on_both_sides)),
    Scenario("SYNC-233", "different properties of one instance merge", "small", _tidy(different_properties_of_one_instance_merge)),
    Scenario("SYNC-234", "Studio deletion is synced, not resurrected", "small", _tidy(studio_deletion_is_synced_not_resurrected)),
    Scenario("SYNC-235", "Studio script rename pulled", "small", _tidy(studio_script_rename_pulled)),
    Scenario("SYNC-236", "Studio script move between services pulled", "small", _tidy(studio_script_move_between_services_pulled)),
    Scenario("SYNC-237", "Studio deletion vs file edit on restart", "small", _tidy(studio_deletion_vs_file_edit_on_restart)),
    Scenario("SYNC-238", "CLI push then lon has no conflict", "small", _tidy(cli_push_then_lon_has_no_conflict)),
    Scenario("SYNC-239", "service root property both ways", "small", _tidy(service_root_property_both_ways)),
    Scenario("SYNC-240", "Terrain property both ways", "small", _tidy(terrain_property_both_ways)),
    Scenario("SYNC-241", "viewport camera is Studio's", "small", _tidy(camera_property_both_ways)),
    Scenario("SYNC-242", "attributes both ways", "small", _tidy(attributes_both_ways)),
    Scenario("SYNC-243", "tags both ways", "small", _tidy(tags_both_ways)),
    Scenario("SYNC-244", "cross-service reference round-trip", "small", _tidy(cross_service_reference_round_trip)),
    Scenario("SYNC-245", "reference to a deleted script is repaired", "small", _tidy(reference_to_a_deleted_script_is_repaired)),
    Scenario("SYNC-246", "service outside the sync set", "small", _tidy(service_outside_the_sync_set)),
    Scenario("SYNC-247", "pull one service leaves others", "small", _tidy(pull_one_service_leaves_others)),
    Scenario("SYNC-248", "single-service pull under Live Sync keeps other pending", "small", _tidy(single_service_pull_under_live_sync_keeps_other_pending)),
    Scenario("SYNC-249", "push during Play targets Edit", "small", _tidy(push_during_play_targets_edit)),
    Scenario("SYNC-250", "RunContext Client script round-trip", "small", _tidy(run_context_client_script_round_trip)),
    Scenario("SYNC-251", "pi project pulls once then syncs", "small", _tidy(pi_project_pulls_once_then_syncs)),
    Scenario("SYNC-252", "pi project push without pull makes no duplicates", "small", _tidy(pi_project_push_without_pull_makes_no_duplicates)),
    Scenario("SYNC-253", "Studio instance without identity matches the store by path", "small", _tidy(studio_instance_without_identity_matches_store_by_path)),
    Scenario("SYNC-254", "inline-source store survives pushes", "small", _tidy(inline_source_store_survives_pushes)),
    Scenario("SYNC-255", "control commands with nothing to do", "small", _tidy(control_commands_with_nothing_to_do)),
    Scenario("SYNC-256", "lon without Studio and bad flags", "small", _tidy(lon_without_studio_and_bad_flags)),
]
