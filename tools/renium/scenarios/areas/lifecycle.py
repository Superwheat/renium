"""Studio lifecycle, place pinning, daemon, project, doctor/report/docs/setup, places,
configuration and global output scenarios (matrix/lifecycle.md, LIFE rows)."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
import zipfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from run import DATA, LOCAL, RBX, CommandResult, Context, Scenario, run_captured

MAIN = "src/ServerScriptService/Main.server.luau"
IGNORE = shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock")
NO_WINDOW = 0x08000000 if os.name == "nt" else 0


def _failure(ctx: Context, message: str) -> AssertionError:
    """The runner's own failure type (run.py may be loaded twice: as __main__ and as run)."""
    return sys.modules[type(ctx).__module__].ScenarioFailure(message)


def _parse(stdout: str, stderr: str):
    for stream in (stdout, stderr):
        for line in reversed(stream.strip().splitlines()):
            line = line.strip()
            if not line or line.startswith("[renium]"):
                continue
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                continue
    return None


def _env(ctx: Context, place: Path | str | None = None, unset: tuple = (), **extra) -> dict:
    env = dict(os.environ, RENIUM_PLACE=str(place if place is not None else ctx.root / ctx.place))
    for key in unset:
        env.pop(key, None)
    env.update({key: str(value) for key, value in extra.items()})
    return env


def _run(ctx: Context, *args: str, env: dict, cwd: Path | None = None, timeout: float = 300, json_mode: bool = True, ok: bool | None = True, stdin: str | None = None) -> CommandResult:
    """ctx.rbx with a caller-chosen environment (another pin, no pin, a scratch home)."""
    argv = [RBX] + (["--output-mode", "json"] if json_mode else []) + list(args)
    started = time.monotonic()
    try:
        completed = run_captured(argv, cwd=cwd or ctx.root, stdin=stdin, timeout=timeout, env=env)
    except subprocess.TimeoutExpired as error:
        raise _failure(ctx, f"{' '.join(argv)} did not finish within {timeout}s") from error
    seconds = time.monotonic() - started
    result = CommandResult(argv, completed.returncode, completed.stdout, completed.stderr, seconds, _parse(completed.stdout, completed.stderr))
    pin = env.get("RENIUM_PLACE", "-")
    ctx.log.append(f"$ {' '.join(args)}  [+{time.monotonic() - ctx.row_started:.0f}s {seconds:.1f}s exit {completed.returncode}; cwd {cwd or ctx.root}; pin {pin}]\n{completed.stdout.strip()[:1200]}\n{completed.stderr.strip()[:600]}")
    if ok is True and not result.ok:
        raise _failure(ctx, f"{' '.join(args)} failed: {result.error[:500]}")
    if ok is False and result.ok:
        raise _failure(ctx, f"{' '.join(args)} succeeded but was expected to fail: {result.stdout[:300]}")
    return result


def _sibling(ctx: Context, suffix: str, place: str | None = None) -> tuple[Path, Path]:
    """A second scratch copy beside this one; place renames its place file."""
    other = ctx.root.parent / f"{ctx.root.name}-{suffix}"
    shutil.rmtree(other, ignore_errors=True)
    shutil.copytree(ctx.root, other, ignore=IGNORE)
    file = other / ctx.place
    if place and place != ctx.place:
        file = file.rename(other / place)
    return other, file


_SLOTS: dict = {}


def _second_slot(ctx: Context, key, seconds: float = 90) -> None:
    """A second Studio while the row's own one is open takes one of the runner's machine-wide Studio slots; when
    none frees up in time the row fails instead of waiting on another runner's whole session."""
    if not ctx.studio_open or key in _SLOTS or getattr(ctx.scenario, "exclusive", False):
        return
    runner = sys.modules[type(ctx).__module__]
    if getattr(runner, "SHARD_COUNT", 0) >= runner.STUDIO_SLOTS:
        seconds = min(seconds, 4)
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        for index in range(runner.STUDIO_SLOTS):
            handle = open(runner.SESSION_ROOT / f".studio-slot-{index}.lock", "a+")
            if runner.lock_file(handle, False):
                _SLOTS[key] = (runner, handle)
                return
            handle.close()
        time.sleep(3)
    raise runner.ScenarioSkip(f"no free Studio slot within {seconds:.0f}s: this row needs a second Studio and the machine-wide cap of {runner.STUDIO_SLOTS} is in use; it runs again in the final serial pass")


def _release_slot(key) -> None:
    held = _SLOTS.pop(key, None)
    if held is not None:
        runner, handle = held
        runner.unlock_file(handle)
        handle.close()


def slotted(function):
    """In --isolated runs (no session) a row that opens Studio first takes one of the machine-wide Studio slots,
    as a session does, so the cap on open Studios holds."""
    def run(ctx: Context):
        if ctx.session is not None:
            return function(ctx)
        slot = sys.modules[type(ctx).__module__].StudioSlot()
        slot.acquire()
        try:
            return function(ctx)
        finally:
            slot.release()
    run.__name__ = function.__name__
    run.__doc__ = function.__doc__
    return run


def _open_sibling(ctx: Context, suffix: str, place: str | None = None) -> tuple[Path, Path, dict]:
    """H3: a second scratch copy opened in its own Studio, pinned to its absolute place file."""
    other, file = _sibling(ctx, suffix, place)
    env = _env(ctx, place=file)
    _second_slot(ctx, str(other))
    opened = _run(ctx, "so", file.name, env=env, cwd=other, timeout=200)
    ctx.expect(opened.get("connected") is True, f"second Studio: {opened.json}")
    return other, file, env


def _close_sibling(ctx: Context, other: Path, env: dict):
    _run(ctx, "sx", "--terminate", env=env, cwd=other, ok=None, timeout=90)
    time.sleep(1.5)
    _release_slot(str(other))
    shutil.rmtree(other, ignore_errors=True)


def _scratch_home(ctx: Context) -> dict:
    """H5: a scratch USERPROFILE/HOME/APPDATA with an agent folder, so user config and setup hints land there."""
    home = ctx.root / "home"
    (home / ".claude").mkdir(parents=True, exist_ok=True)
    roaming = home / "AppData" / "Roaming"
    roaming.mkdir(parents=True, exist_ok=True)
    return {"USERPROFILE": str(home), "HOME": str(home), "APPDATA": str(roaming), "XDG_CONFIG_HOME": str(home / ".config")}


def _outside(ctx: Context, suffix: str) -> Path:
    """A folder beside the scratch copy, outside any project."""
    folder = ctx.root.parent / f"{ctx.root.name}-{suffix}"
    shutil.rmtree(folder, ignore_errors=True)
    folder.mkdir()
    return folder


def _clients(result: CommandResult) -> list:
    return [c for c in result.get("clients") or [] if c.get("role") == "edit"]


def _files(listing: CommandResult) -> list[str]:
    return [str(Path(c.get("localFile")).resolve()) for c in listing.get("clients") or [] if c.get("localFile")]


def _wait_connected(ctx: Context, env: dict | None = None, seconds: int = 150) -> CommandResult:
    deadline = time.monotonic() + seconds
    status = None
    while time.monotonic() < deadline:
        status = _run(ctx, "status", env=env or _env(ctx), ok=None, timeout=60)
        if _clients(status):
            return status
        time.sleep(2)
    raise _failure(ctx, f"Studio did not connect within {seconds}s: {status.json if status else None}")


def _spawn(ctx: Context, args: list[str], log: Path, env: dict | None = None, cwd: Path | None = None) -> subprocess.Popen:
    handle = open(log, "w", encoding="utf-8")
    return subprocess.Popen([RBX] + args, cwd=cwd or ctx.root, env=env or _env(ctx), stdin=subprocess.DEVNULL, stdout=handle, stderr=subprocess.STDOUT, creationflags=NO_WINDOW)


def _kill_tree(process: subprocess.Popen):
    if process.poll() is not None:
        return
    if os.name == "nt":
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True, timeout=30)
    else:
        process.kill()
    try:
        process.wait(timeout=15)
    except subprocess.TimeoutExpired:
        pass


def _matches(result: CommandResult) -> list:
    if isinstance(result.json, list):
        return result.json
    return result.get("matches") or result.get("results") or []


def _target_file(root: Path) -> str:
    path = root / ".renium" / "studio-target.json"
    if not path.is_file():
        return ""
    return str(Path(json.loads(path.read_text(encoding="utf-8")).get("file") or "").resolve())


# ---------------------------------------------------------------- A. so / sx / ro


def second_so_reuses_the_studio(ctx: Context):
    """LIFE-001 (0.5.6): a repeated so FILE reuses the open Studio and reports it connected."""
    ctx.open_studio()
    again = ctx.rbx("so", ctx.place, json_mode=True, timeout=60)
    ctx.expect(again.get("alreadyOpen") is True and again.get("connected") is True, f"second so: {again.json}")
    mine = [f for f in _files(ctx.rbx("cs", json_mode=True)) if f == str((ctx.root / ctx.place).resolve())]
    ctx.expect(len(mine) == 1, f"one Studio on the scratch place, got {len(mine)}")


def busy_answer_within_five_seconds(ctx: Context):
    """LIFE-002 (0.5.7): a Luau call queued behind a long Edit operation answers busy after 5 s."""
    ctx.open_studio()
    env = dict(os.environ, RENIUM_PLACE=str(ctx.root / ctx.place))
    long_call = subprocess.Popen([RBX, "l", "task.wait(12) return 1", "-t", "20"], cwd=ctx.root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(1.0)
        started = time.monotonic()
        second = ctx.rbx("l", "return 2", ok=False, timeout=60)
        waited = time.monotonic() - started
        ctx.expect_in("Studio is busy", second.error, "busy answer")
        ctx.expect(4.0 <= waited <= 9.0, f"busy answer took {waited:.1f}s")
    finally:
        long_call.wait(timeout=30)


def another_studios_request_is_not_held_up(ctx: Context):
    """LIFE-080: a long Luau call in one Studio holds only that Studio's gate: `rbx l` on a sibling Studio answers
    within 2 s while it runs, and a second call on the held Studio still answers busy."""
    ctx.open_studio()
    other, file, env = _open_sibling(ctx, "b", "B.rbxl")
    try:
        long_call = subprocess.Popen([RBX, "l", "task.wait(12) return 1", "-t", "20"], cwd=ctx.root, env=_env(ctx), stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, creationflags=NO_WINDOW)
        try:
            time.sleep(1.0)
            started = time.monotonic()
            sibling = _run(ctx, "l", "return 2", env=env, cwd=other, timeout=60)
            waited = time.monotonic() - started
            ctx.expect((sibling.get("results") or [None])[0] == 2, f"sibling answer: {sibling.json}")
            ctx.expect(waited <= 2.0, f"the sibling's `rbx l` answered after {waited:.1f}s while the long call ran")
            held = ctx.rbx("l", "return 3", ok=False, timeout=60)
            ctx.expect_in("Studio is busy", held.error, "busy answer on the held Studio")
        finally:
            long_call.wait(timeout=30)
    finally:
        _close_sibling(ctx, other, env)


def fresh_launch_reports_the_connection(ctx: Context):
    """LIFE-003 (launch part): a fresh so FILE waits for the connection and reports pid, the absolute file and
    connectSeconds. The H12 recovery-file part is not run: it writes Studio's AutoSaves folder."""
    ctx.close_studio()
    opened = ctx.open_studio()
    expected = str((ctx.root / ctx.place).resolve())
    ctx.expect(isinstance(opened.get("pid"), int) and opened.get("connected") is True, f"so: {opened.json}")
    ctx.expect(str(Path(opened.get("file") or "").resolve()) == expected and Path(opened.get("file") or "").is_absolute(), f"so file: {opened.get('file')}")
    ctx.expect(isinstance(opened.get("connectSeconds"), (int, float)), f"connectSeconds: {opened.json}")


def so_refuses_missing_and_non_place_files(ctx: Context):
    """LIFE-004: so of a missing file or a non-place file is refused and launches nothing."""
    ctx.open_studio()
    missing = ctx.rbx("so", "Nope.rbxl", ok=False, timeout=60)
    ctx.expect_in(f"Studio file does not exist: {ctx.root / 'Nope.rbxl'}", missing.error, "missing file")
    wrong = ctx.rbx("so", "renium.project.jsonc", ok=False, timeout=60)
    ctx.expect_in("Studio files must end in .rbxl, .rbxlx, .rbxm, or .rbxmx", wrong.error, "non-place file")
    files = _files(ctx.rbx("cs", timeout=60))
    ctx.expect(files.count(str((ctx.root / ctx.place).resolve())) == 1 and not [f for f in files if Path(f).parent == ctx.root and Path(f).name != ctx.place], f"one Studio on the copy: {files}")


def so_without_a_file(ctx: Context):
    """LIFE-005: so without FILE refuses two top-level places, and in a project without a place file builds
    .renium/build/studio.rbxl and opens it."""
    ctx.close_studio()
    shutil.copy(ctx.root / ctx.place, ctx.root / "B.rbxl")
    two = ctx.rbx("so", ok=False, timeout=60)
    ctx.expect_in(f"More than one Studio file exists under {ctx.root}", two.error, "two places")
    (ctx.root / "B.rbxl").unlink()
    bare = ctx.root.parent / f"{ctx.root.name}-noplace"
    shutil.copytree(ctx.root, bare, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl", "*.rbxl.lock"))
    built = bare / ".renium" / "build" / "studio.rbxl"
    try:
        opened = _run(ctx, "so", env=_env(ctx, unset=("RENIUM_PLACE",)), cwd=bare, timeout=240)
        ctx.expect(opened.get("connected") is True and str(Path(opened.get("file") or "").resolve()) == str(built.resolve()), f"so in a project without a place: {opened.json}")
        ctx.expect(built.is_file(), "built place exists")
    finally:
        _close_sibling(ctx, bare, _env(ctx, place=built))


def so_check_launches_nothing(ctx: Context):
    """LIFE-006: so --check reports the executable and file; without a place file it is not resolvable; neither launches."""
    ctx.close_studio()
    checked = ctx.rbx("so", "--check", timeout=60)
    ctx.expect(checked.get("executable") and checked.get("exists") is True and str(Path(checked.get("file") or "").resolve()) == str((ctx.root / ctx.place).resolve()), f"so --check: {checked.json}")
    bare = ctx.root.parent / f"{ctx.root.name}-noplace"
    shutil.copytree(ctx.root, bare, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl", "*.rbxl.lock"))
    try:
        unresolved = _run(ctx, "so", "--check", env=_env(ctx, unset=("RENIUM_PLACE",)), cwd=bare, ok=False, timeout=60)
        ctx.expect(unresolved.get("resolvable") is False, f"resolvable:false: {unresolved.json}")
        ctx.expect_in("No Studio file exists and --check does not build one", unresolved.error or unresolved.stdout, "unresolvable check")
        ctx.expect(not (bare / ".renium" / "build").exists(), "--check built nothing")
    finally:
        shutil.rmtree(bare, ignore_errors=True)
    ctx.expect(not _clients(ctx.rbx("status", ok=None, timeout=60)), "no Studio launched")


def so_no_wait_reports_starting(ctx: Context):
    """LIFE-007 (L8): so --no-wait returns before the connection; status right after says the Studio is starting (or connected)."""
    ctx.close_studio()
    launched = ctx.rbx("so", ctx.place, "--no-wait", timeout=90)
    ctx.studio_open = True
    ctx.expect("connected" not in (launched.json or {}) and isinstance(launched.get("pid"), int), f"so --no-wait: {launched.json}")
    for _ in range(4):
        status = ctx.rbx("status", "-w", "0", ok=None, timeout=60)
        if "busy serving" not in (status.error or ""):
            break
        time.sleep(2)
    verdict = str(status.pointer("/diagnosis/verdict") or "")
    ctx.expect(_clients(status) or f"Studio (pid {launched.get('pid')}) started" in verdict and "has not connected yet" in verdict, f"a just-launched Studio reads as not matching (L8): {status.pointer('/diagnosis/verdict')}")
    _wait_connected(ctx)
    ctx.guard_binding()


def studio_not_installed(ctx: Context):
    """LIFE-008: with no Studio under LOCALAPPDATA or Program Files, so --check and so FILE both say Studio is not installed."""
    ctx.close_studio()
    empty = ctx.root / "no-studio"
    empty.mkdir()
    env = _env(ctx, LOCALAPPDATA=empty, ProgramFiles=empty, **{"ProgramFiles(x86)": empty})
    for args in (("so", "--check"), ("so", ctx.place)):
        refused = _run(ctx, *args, env=env, ok=False, timeout=60)
        ctx.expect_in("Roblox Studio is not installed in the local or system Versions directories", refused.error, " ".join(args))
    ctx.expect(not _clients(ctx.rbx("status", ok=None, timeout=60)), "no Studio launched")


def sx_needs_a_choice(ctx: Context):
    """LIFE-009: sx on a local place without --save or --terminate is refused before Live Sync stops."""
    ctx.open_studio()
    ctx.rbx("lon", "--wait", "60", timeout=200)
    refused = ctx.rbx("sx", ok=False, timeout=60)
    ctx.expect_in("Closing a local Studio place requires --save or --terminate", refused.error, "sx without a choice")
    ctx.expect(ctx.luau("return 1") == 1, "Studio still answers")
    ctx.expect(ctx.rbx("lst", timeout=60).pointer("/daemon/running") is True, "Live Sync still running")


def save_close_and_reopen(ctx: Context):
    """LIFE-010, LIFE-011, LIFE-012: sx --save writes an edit into the place and records the reopen target, ro reopens
    it with the edit; sx --terminate drops an unsaved edit and leaves the file alone; --save and --terminate conflict."""
    ctx.open_studio()
    file = ctx.root / ctx.place
    ctx.luau("workspace.Props.Sign.Value = 'saved' Instance.new('Folder', workspace).Name = 'Saved' return true")
    closed = ctx.rbx("sx", "--save", timeout=120)
    ctx.expect(closed.get("closed") is True and isinstance(closed.get("pid"), int) and closed.pointer("/reopenTarget/file"), f"sx --save: {closed.json}")
    ctx.studio_open = False
    time.sleep(1.5)
    ctx.expect(len(_matches(ctx.rbx("q", ctx.place, "-n", "Saved", timeout=60))) == 1, "saved place holds Folder Saved")
    ctx.expect(_target_file(ctx.root) == str(file.resolve()), f"studio-target.json names the place: {_target_file(ctx.root)}")
    reopened = ctx.reopen_studio()
    ctx.expect(reopened.get("connected") is True, f"ro: {reopened.json}")
    ctx.expect(_files(ctx.rbx("cs", timeout=60)).count(str(file.resolve())) == 1, "one edit client on the file")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "saved", "saved edit survived the reopen")
    ctx.expect(ctx.luau("return workspace:FindFirstChild('Saved') ~= nil") is True, "Folder Saved after ro")
    ctx.luau("workspace.Props.Sign.Value = 'unsaved' Instance.new('Folder', workspace).Name = 'Unsaved' return true")
    stamp = file.stat().st_mtime_ns
    ctx.guard_binding()
    terminated = ctx.rbx("sx", "--terminate", timeout=120)
    ctx.studio_open = False
    ctx.expect(terminated.get("closed") is True, f"sx --terminate: {terminated.json}")
    time.sleep(1.5)
    ctx.expect(file.stat().st_mtime_ns == stamp, "terminate left the place file alone")
    ctx.expect(len(_matches(ctx.rbx("q", ctx.place, "-n", "Unsaved", timeout=60))) == 0, "unsaved folder not in the file")
    both = ctx.rbx("sx", "--save", "--terminate", ok=False, timeout=60)
    ctx.expect(both.code == 2 and "cannot be used with" in both.stderr, f"clap conflict: {both.stderr[:200]}")
    ctx.reopen_studio()
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "saved", "terminate dropped the unsaved edit")


def ro_while_open_and_missing(ctx: Context):
    """LIFE-013: ro with the place open answers alreadyOpen with the same pid; ro of a missing file is refused and
    leaves the open Studio alone."""
    opened = ctx.open_studio()
    again = ctx.rbx("ro", timeout=120)
    ctx.expect(again.get("alreadyOpen") is True and again.get("pid") == opened.get("pid") and again.get("runtimeId"), f"ro while open has no runtimeId (L1: no remembered target): {again.json}")
    missing = ctx.rbx("ro", "Nope.rbxl", ok=False, timeout=60)
    ctx.expect_in("Studio file does not exist", missing.error, "ro of a missing file")
    ctx.expect(ctx.luau("return 1") == 1, "open Studio untouched")
    ctx.expect(_files(ctx.rbx("cs", timeout=60)).count(str((ctx.root / ctx.place).resolve())) == 1, "no new Studio")


def two_ro_no_wait_launch_once(ctx: Context):
    """LIFE-015: two ro --no-wait in a row after sx --save launch one Studio."""
    ctx.open_studio()
    ctx.rbx("sx", "--save", timeout=120)
    ctx.studio_open = False
    time.sleep(1.5)
    first = ctx.rbx("ro", "--no-wait", timeout=90)
    ctx.studio_open = True
    second = ctx.rbx("ro", "--no-wait", timeout=90)
    ctx.expect(first.get("pid") and first.get("pid") == second.get("pid"), f"same pid: {first.json} {second.json}")
    _wait_connected(ctx)
    ctx.guard_binding()
    ctx.expect(_files(ctx.rbx("cs", timeout=60)).count(str((ctx.root / ctx.place).resolve())) == 1, "one Studio")


# ---------------------------------------------------------------- B. status, cs


def status_of_a_connected_place(ctx: Context):
    """LIFE-016: status of a connected place lists one edit client, the selection and persistence; cs names its file."""
    ctx.open_studio()
    status = ctx.rbx("status", timeout=60)
    clients = _clients(status)
    ctx.expect(len(clients) == 1, f"one client: {status.json}")
    ctx.expect("diagnosis" not in status.json, f"no diagnosis: {status.json}")
    ctx.expect(status.get("persistence") == "unsaved", f"persistence: {status.get('persistence')}")
    ctx.expect(status.get("selected"), f"selected: {status.get('selected')}")
    ctx.expect(clients[0].get("pid") and clients[0].get("placeName") == ctx.place, f"client: {clients[0]}")
    ctx.expect(str((ctx.root / ctx.place).resolve()) in _files(ctx.rbx("cs", timeout=60)), "cs names the client's file")


def status_all_sees_other_projects(ctx: Context):
    """LIFE-017: status --all and cs list the Studios of other projects too; cs keeps bridgeBuildUnix and drops
    channels/ports."""
    ctx.open_studio()
    other, file, env = _open_sibling(ctx, "b", "B.rbxl")
    try:
        everything = ctx.rbx("status", "--all", timeout=60)
        listing = ctx.rbx("cs", timeout=60)
        wanted = {str((ctx.root / ctx.place).resolve()), str(file.resolve())}
        for name, result in (("status --all", everything), ("cs", listing)):
            entries = {str(Path(c.get("localFile")).resolve()): c for c in result.get("clients") or [] if c.get("localFile")}
            ctx.expect(wanted <= set(entries), f"{name} lists both: {sorted(entries)}")
            ctx.expect(all(isinstance(entries[w].get("pid"), int) for w in wanted), f"{name} pids")
        for client in listing.get("clients") or []:
            ctx.expect("bridgeBuildUnix" in client and "channels" not in client and "ports" not in client, f"cs entry: {client}")
    finally:
        _close_sibling(ctx, other, env)


def no_matching_studio(ctx: Context):
    """LIFE-018: with only another project's Studio open, status lists no client and names it; l fails with the verdict."""
    ctx.close_studio()
    other, file, env = _open_sibling(ctx, "b", "B.rbxl")
    try:
        # Another Studio still starting on the host (another shard's launch)
        # takes the verdict first; the no-match verdict follows once it connects.
        deadline = time.monotonic() + 150
        while True:
            status = ctx.rbx("status", ok=None, timeout=60)
            if not status.pointer("/diagnosis/starting") or time.monotonic() > deadline:
                break
            time.sleep(3)
        verdict = str(status.pointer("/diagnosis/verdict") or "")
        ctx.expect(status.get("clients") in (None, []), f"no clients: {status.json}")
        ctx.expect("B.rbxl" in verdict and "no connected Studio matches this project" in verdict, f"verdict: {verdict}")
        while True:
            luau = ctx.rbx("l", "return 1", "-w", "3", ok=False, timeout=60)
            if "has not connected yet" not in luau.error or time.monotonic() > deadline:
                break
            time.sleep(3)
        ctx.expect_in("No Studio runtime connected to this project within", luau.error, "l without a match")
        ctx.expect_in("no connected Studio matches this project", luau.error, "l carries the verdict")
    finally:
        _close_sibling(ctx, other, env)


def no_studio_answers(ctx: Context):
    """LIFE-019: with no Studio, so on a missing file, status and l each say what to do. The "Studio is not running"
    verdict needs H8 (no Studio on the host); while other Studios run, the row checks the verdict for that state."""
    ctx.close_studio()
    missing = ctx.rbx("so", "Missing.rbxl", ok=False, timeout=60)
    ctx.expect_in("does not exist", missing.error, "so missing file")
    status = ctx.rbx("status", ok=None, timeout=60)
    others = status.pointer("/diagnosis/studioProcesses") or []
    expected = "rbx so FILE" if not others else "no connected Studio matches this project"
    ctx.expect_in(expected, str(status.pointer("/diagnosis/verdict")), f"status verdict ({len(others)} other Studio processes)")
    luau = ctx.rbx("l", "return 1", "-w", "2", ok=False, timeout=60)
    ctx.expect_in(expected, luau.error, "l without Studio")


# ---------------------------------------------------------------- C. place pinning


def place_flag_selects_another_studio(ctx: Context):
    """LIFE-020, LIFE-022: --place B.rbxl answers from B's Studio, and the flag beats RENIUM_PLACE."""
    ctx.open_studio()
    other, file, env = _open_sibling(ctx, "b", "B.rbxl")
    try:
        by_name = ctx.rbx("--place", "B.rbxl", "l", "return game.Name", timeout=60)
        ctx.expect("B" in str((by_name.get("results") or [""])[0]), f"--place B.rbxl: {by_name.json}")
        own = ctx.rbx("l", "return game.Name", timeout=60)
        ctx.expect("B" not in str((own.get("results") or [""])[0]), f"pinned to A: {own.json}")
        flag = ctx.rbx("--place", str(file), "l", "return game.Name", timeout=60)
        ctx.expect("B" in str((flag.get("results") or [""])[0]), f"--place <B> with RENIUM_PLACE=<A>: {flag.json}")
    finally:
        _close_sibling(ctx, other, env)


def two_places_need_a_pin(ctx: Context):
    """LIFE-021: with two Studios on same-named places, an unpinned command from a third project and a bare
    --place name are refused as ambiguous; the project's own Studio and an absolute --place path answer;
    status --all lists both."""
    ctx.open_studio()
    other = ctx.root.parent / f"{ctx.root.name}-other"
    shutil.rmtree(other, ignore_errors=True)
    shutil.copytree(ctx.root, other, ignore=IGNORE)
    env = dict(os.environ, RENIUM_PLACE=str(other / ctx.place))
    _second_slot(ctx, str(other))
    try:
        opened = subprocess.run([RBX, "--output-mode", "json", "so", ctx.place], cwd=other, env=env, capture_output=True, text=True, timeout=180)
        ctx.expect('"connected":true' in opened.stdout, f"second Studio: {opened.stdout[-200:]} {opened.stderr[-200:]}")
        unpinned_env = dict(os.environ)
        unpinned_env.pop("RENIUM_PLACE", None)
        own = subprocess.run([RBX, "--output-mode", "json", "l", "return 1"], cwd=ctx.root, env=unpinned_env, capture_output=True, text=True, timeout=60)
        ctx.expect('"results":[1]' in own.stdout, f"a project prefers the Studio on its own place: {own.stdout[-200:]} {own.stderr[-200:]}")
        third = ctx.root.parent / f"{ctx.root.name}-third"
        shutil.copytree(ctx.root, third, ignore=IGNORE)
        try:
            ambiguous = subprocess.run([RBX, "--output-mode", "json", "l", "return 1"], cwd=third, env=unpinned_env, capture_output=True, text=True, timeout=60)
            answer = ambiguous.stdout + ambiguous.stderr
            ctx.expect("More than one Studio" in answer or "no connected Studio matches this project" in answer, f"a third project neither binds a foreign window nor stays silent: {ambiguous.stdout[-200:]} {ambiguous.stderr[-200:]}")
            bare = _run(ctx, "--place", "Scenario", "l", "return 1", env=unpinned_env, cwd=third, ok=False, timeout=60)
            ctx.expect_in("More than one Studio runtime matches this project", bare.error, "a bare name matching both")
        finally:
            shutil.rmtree(third, ignore_errors=True)
        pinned = ctx.rbx("l", "return 1", timeout=60)
        ctx.expect(pinned.get("results") == [1], f"pinned call: {pinned.json}")
        absolute = _run(ctx, "--place", str(ctx.root / ctx.place), "l", "return 1", env=unpinned_env, timeout=60)
        ctx.expect(absolute.get("results") == [1], f"absolute --place: {absolute.json}")
        everything = ctx.rbx("status", "--all", timeout=60)
        listed = {str(Path(c.get("localFile")).resolve()) for c in everything.get("clients") or [] if c.get("role") == "edit" and c.get("localFile")}
        ctx.expect({str((ctx.root / ctx.place).resolve()), str((other / ctx.place).resolve())} <= listed, f"status --all: {everything.json}")
    finally:
        subprocess.run([RBX, "sx", "--terminate"], cwd=other, env=env, capture_output=True, timeout=60)
        _release_slot(str(other))
        shutil.rmtree(other, ignore_errors=True)


def config_place_against_renium_place(ctx: Context):
    """LIFE-023 (L3): a user-scope `place` key must not beat RENIUM_PLACE."""
    ctx.open_studio()
    other, file, env = _open_sibling(ctx, "b", "B.rbxl")
    home = _scratch_home(ctx)
    try:
        _run(ctx, "cfg", "set", "place", str(file), "--scope", "user", env=_env(ctx, **home), timeout=60)
        answered = _run(ctx, "l", "return game.Name", env=_env(ctx, **home), timeout=60)
        name = str((answered.get("results") or [""])[0])
        ctx.expect("B" not in name, f"RENIUM_PLACE=<A> with cfg place=<B> answered from {name!r} (L3: the config key beats the environment)")
    finally:
        _close_sibling(ctx, other, env)


def place_env_variable_is_ignored(ctx: Context):
    """LIFE-024 (L3): the undocumented PLACE variable must not pin; an unpinned call answers from the project's Studio."""
    ctx.open_studio()
    answered = _run(ctx, "l", "return 1", env=_env(ctx, unset=("RENIUM_PLACE",), PLACE="nomatch"), ok=None, timeout=60)
    ctx.expect(answered.get("results") == [1], f"PLACE=nomatch pinned the call (L3): {answered.error[:300] or answered.json}")


def so_binds_the_last_opened_place(ctx: Context):
    """LIFE-025, LIFE-026 (L1): two places opened by so in one project: the guide says commands bind the place
    so opened last; ro FILE records the target and binds it."""
    ctx.open_studio()
    second = ctx.root / "B.rbxl"
    shutil.copy(ctx.root / ctx.place, second)
    unpinned = _env(ctx, unset=("RENIUM_PLACE",))
    _second_slot(ctx, str(second))
    try:
        opened = _run(ctx, "so", "B.rbxl", env=unpinned, timeout=200)
        ctx.expect(opened.get("connected") is True, f"so B.rbxl: {opened.json}")
        recorded = ctx.rbx("ro", "B.rbxl", ok=None, timeout=200)
        ctx.expect(recorded.ok and _target_file(ctx.root) == str(second.resolve()), f"ro B.rbxl records the target: {recorded.json} target {_target_file(ctx.root)}")
        after_ro = _run(ctx, "l", "return game.Name", env=unpinned, ok=None, timeout=60)
        ctx.expect("B" in str((after_ro.get("results") or [""])[0]), f"after ro B.rbxl: {after_ro.error[:300] or after_ro.json}")
        (ctx.root / ".renium" / "studio-target.json").unlink()
        _run(ctx, "so", "B.rbxl", env=unpinned, timeout=200)
        ctx.expect(_target_file(ctx.root) == str(second.resolve()), f"so B.rbxl recorded no studio-target.json (L1): {_target_file(ctx.root)!r}")
        last = _run(ctx, "l", "return game.Name", env=unpinned, ok=None, timeout=60)
        ctx.expect("B" in str((last.get("results") or [""])[0]), f"unpinned after two so (L1): {last.error[:300] or last.json}")
    finally:
        _run(ctx, "sx", "--terminate", env=_env(ctx, place=second), ok=None, timeout=90)
        _release_slot(str(second))


# ---------------------------------------------------------------- D. daemon


def daemon_listing_and_restart(ctx: Context):
    """LIFE-027: dm list/status name the default daemon; after dm stop no daemon is alive, an offline command
    works, and the next Studio command starts a daemon that lists the open Studio."""
    ctx.open_studio()
    listing = ctx.rbx("dm", "list", timeout=60)
    ctx.expect(any(d.get("name") == "default" and d.get("alive") for d in listing.get("daemons") or []), f"dm list: {listing.json}")
    before = ctx.rbx("dm", "status", timeout=60).get("pid")
    try:
        stopped = ctx.rbx("dm", "stop", ok=None, timeout=60)
        ctx.expect(stopped.get("stopped") == ["default"], f"dm stop: {stopped.json}")
        time.sleep(1.5)
        after = ctx.rbx("dm", "list", timeout=60)
        ctx.expect(not any(d.get("alive") for d in after.get("daemons") or [] if d.get("name") == "default"), f"dm list after stop: {after.json}")
        offline = ctx.rbx("cfg", "get", "bridgePorts", timeout=60)
        ctx.expect(offline.json == "8781,8782", f"an offline command works without the daemon: {offline.json}")
        status = ctx.rbx("status", "-w", "30", ok=None, timeout=90)
        ctx.expect(len(_clients(status)) == 1, f"status -w 30 after the restart returned without the client (L8): {status.pointer('/diagnosis/verdict')}")
    finally:
        ctx.rbx("dm", "status", ok=None, timeout=60)
    daemon = ctx.rbx("dm", "status", timeout=60)
    ctx.expect(daemon.get("alive") is True and daemon.get("pid") != before, f"daemon after stop: {daemon.json}")


def daemon_list_fields(ctx: Context):
    """LIFE-028: dm list carries name, pid, alive, responsive, ports and the discovery file; an unknown name is refused."""
    listing = ctx.rbx("dm", "list", timeout=60)
    default = next((d for d in listing.get("daemons") or [] if d.get("name") == "default"), None)
    ctx.expect(default is not None, f"default daemon: {listing.json}")
    for key in ("name", "pid", "alive", "responsive", "controlPort", "bridgePorts", "discoveryFile"):
        ctx.expect(key in default, f"dm list lacks {key}: {default}")
    ctx.expect(default["controlPort"] == 8780 and default["bridgePorts"] == [8781, 8782], f"ports: {default}")
    ghost = ctx.rbx("dm", "status", "ghost", ok=False, timeout=60)
    ctx.expect_in("No daemon named 'ghost' was found", ghost.error, "unknown daemon")


def stale_discovery_cleanup(ctx: Context):
    """LIFE-029: dm stop and dm clean remove a discovery file whose process is gone and keep the default one."""
    dead = subprocess.run([sys.executable, "-c", "import os; print(os.getpid())"], capture_output=True, text=True, timeout=30)
    pid = int(dead.stdout.strip())
    ghost = LOCAL / "daemon-ghost.json"
    record = {"bridgePorts": [8797, 8798], "controlPort": 8796, "host": "127.0.0.1", "name": "ghost", "pid": pid, "schemaVersion": 2, "updatedUnixMs": int(time.time() * 1000)}
    try:
        ghost.write_text(json.dumps(record), encoding="utf-8")
        stopped = ctx.rbx("dm", "stop", "ghost", timeout=60)
        ctx.expect_in("Removed stale daemon discovery", str(stopped.get("detail")), "dm stop of a dead daemon")
        ctx.expect(not ghost.exists(), "dm stop removed the file")
        ghost.write_text(json.dumps(record), encoding="utf-8")
        cleaned = ctx.rbx("dm", "clean", timeout=60)
        removed = [str(Path(p).resolve()).lower() for p in cleaned.get("removed") or []]
        ctx.expect(str(ghost.resolve()).lower() in removed and not ghost.exists(), f"dm clean: {cleaned.json}")
        ctx.expect((LOCAL / "daemon.json").is_file(), "default discovery kept")
    finally:
        ghost.unlink(missing_ok=True)


def _named_daemon(ctx: Context, name: str, control: int, ports: str, extra: list[str] | None = None) -> subprocess.Popen:
    process = _spawn(ctx, ["bd", "--name", name, "--control-port", str(control), "--ports", ports] + (extra or []), ctx.root / f"daemon-{name}.log")
    deadline = time.monotonic() + 20
    while time.monotonic() < deadline:
        status = ctx.rbx("dm", "status", name, ok=None, timeout=60)
        if status.get("alive") and status.get("responsive"):
            return process
        time.sleep(0.5)
    _kill_tree(process)
    raise _failure(ctx, f"daemon {name} did not start: {(ctx.root / f'daemon-{name}.log').read_text(encoding='utf-8', errors='replace')[-400:]}")


def named_daemon_isolation(ctx: Context):
    """LIFE-030: a named daemon on its own ports serves --daemon NAME without seeing the default daemon's Studio,
    and dm stop NAME stops it."""
    ctx.open_studio()
    process = _named_daemon(ctx, "scratch30", 9790, "9791,9792")
    try:
        status = ctx.rbx("--daemon", "scratch30", "dm", "status", "scratch30", timeout=60)
        ctx.expect(status.get("controlPort") == 9790 and status.get("alive") is True, f"named daemon: {status.json}")
        isolated = ctx.rbx("--daemon", "scratch30", "status", "-w", "2", ok=None, timeout=60)
        ctx.expect(not _clients(isolated), f"the named daemon sees no default-daemon Studio: {isolated.json}")
        stopped = ctx.rbx("dm", "stop", "scratch30", timeout=60)
        ctx.expect(stopped.get("stopped") == ["scratch30"], f"dm stop scratch30: {stopped.json}")
        ctx.expect(len(_clients(ctx.rbx("status", timeout=60))) == 1, "default daemon still serves the Studio")
    finally:
        _kill_tree(process)
        ctx.rbx("dm", "stop", "scratch30", ok=None, timeout=60)


def named_daemon_auto_start(ctx: Context):
    """LIFE-031 (L2): --daemon NAME with no such daemon starts it on its own ports and leaves the default daemon
    alone. (`--daemon NAME dm status` without NAME reports the default daemon: the NAME argument defaults to
    "default"; this row reads `dm status scratch2`.)"""
    before = ctx.rbx("dm", "status", timeout=60)
    try:
        ctx.rbx("--daemon", "scratch2", "dm", "status", ok=None, timeout=90)
        started = ctx.rbx("--daemon", "scratch2", "status", "-w", "2", ok=None, timeout=90)
        after = ctx.rbx("dm", "status", timeout=60)
        ctx.expect(after.get("pid") == before.get("pid") and after.get("responsive") is True, f"default daemon untouched: {before.json} -> {after.json}")
        named = ctx.rbx("dm", "status", "scratch2", ok=None, timeout=60)
        ctx.expect(named.get("alive") is True and named.get("controlPort") not in (None, 8780), f"--daemon scratch2 did not start its own daemon (L2): {started.error[:300] or started.json}; dm status scratch2: {named.json or named.error}")
    finally:
        ctx.rbx("dm", "stop", "scratch2", ok=None, timeout=60)


def simultaneous_named_daemon_starts(ctx: Context):
    """LIFE-079: two --daemon NAME auto-starts issued at the same moment come up on different port blocks."""
    names = ("twin1", "twin2")
    try:
        with ThreadPoolExecutor(max_workers=2) as pool:
            starts = list(pool.map(lambda name: ctx.rbx("--daemon", name, "status", "-w", "2", ok=None, timeout=90), names))
        deadline = time.monotonic() + 30
        while True:
            records = [ctx.rbx("dm", "status", name, ok=None, timeout=60) for name in names]
            if all(record.get("alive") is True for record in records) or time.monotonic() > deadline:
                break
            time.sleep(2)
        ctx.expect(all(record.get("alive") is True for record in records), f"both named daemons alive: {[record.json or record.error for record in records]}; starts answered {[(start.code, (start.json or start.error)) for start in starts]}")
        ports = [record.get("controlPort") for record in records]
        ctx.expect(len(set(ports)) == 2 and all(isinstance(port, int) and port >= 8790 for port in ports), f"distinct blocks from 8790: {ports}")
    finally:
        for name in names:
            ctx.rbx("dm", "stop", name, ok=None, timeout=60)


def bd_beside_the_default(ctx: Context):
    """LIFE-033: bd while the default daemon runs exits at once and keeps it; a named bd on a taken control port is refused."""
    before = ctx.rbx("dm", "status", timeout=60).get("pid")
    started = time.monotonic()
    plain = ctx.rbx("bd", json_mode=False, ok=None, timeout=60)
    ctx.expect(plain.code == 0 and time.monotonic() - started < 15, f"bd beside the default: exit {plain.code} after {time.monotonic() - started:.1f}s")
    ctx.expect(ctx.rbx("dm", "status", timeout=60).get("pid") == before, "default daemon kept")
    taken = ctx.rbx("bd", "--name", "x", "--control-port", "8780", "--ports", "9793,9794", ok=False, timeout=60)
    ctx.expect_in("Failed to bind daemon control on 127.0.0.1:8780", taken.error or taken.stdout, "taken control port")
    ctx.rbx("dm", "stop", "x", ok=None, timeout=60)


def daemon_exits_with_its_parent(ctx: Context):
    """LIFE-034: a daemon started with --parent-pid exits within 6 s of that process."""
    parent = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"], creationflags=NO_WINDOW)
    process = None
    try:
        process = _named_daemon(ctx, "scratch34", 9800, "9801,9802", ["--parent-pid", str(parent.pid)])
        parent.kill()
        parent.wait(timeout=15)
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline and process.poll() is None:
            time.sleep(0.25)
        ctx.expect(process.poll() is not None, "daemon still running 6 s after its parent died")
        status = ctx.rbx("dm", "status", "scratch34", ok=None, timeout=60)
        ctx.expect(not status.ok or status.get("alive") is False, f"dm status after exit: {status.json}")
    finally:
        parent.kill()
        if process:
            _kill_tree(process)
        ctx.rbx("dm", "stop", "scratch34", ok=None, timeout=60)


def daemon_has_no_idle_exit(ctx: Context):
    """LIFE-035 (N1): a named daemon without requests is still the same responsive process after 10 minutes."""
    process = _named_daemon(ctx, "scratch35", 9810, "9811,9812")
    try:
        pid = ctx.rbx("dm", "status", "scratch35", timeout=60).get("pid")
        time.sleep(600)
        status = ctx.rbx("dm", "status", "scratch35", timeout=60)
        ctx.expect(status.get("pid") == pid and status.get("responsive") is True, f"after 10 min: {status.json}")
    finally:
        _kill_tree(process)
        ctx.rbx("dm", "stop", "scratch35", ok=None, timeout=60)


# ---------------------------------------------------------------- E. project creation and builds


def init_creates_and_keeps_a_project(ctx: Context):
    """LIFE-036: init --preview writes nothing; init creates the guides, manifest and src in an empty folder;
    a second run keeps everything."""
    fresh = ctx.root / "fresh"
    fresh.mkdir()
    preview = ctx.rbx("init", "--preview", cwd=fresh, timeout=60)
    ctx.expect(not any(fresh.iterdir()), f"preview wrote files: {list(fresh.iterdir())}")
    created = ctx.rbx("init", cwd=fresh, timeout=60)
    wanted = {"AGENTS.md", "RENIUM.md", "CLAUDE.md", "renium.project.jsonc", "RENIUM/sync.md"}
    ctx.expect(wanted <= set(created.get("create") or []), f"init: {created.json}")
    ctx.expect(created.get("directories") == ["src"] and preview.get("create") == created.get("create"), f"init directories / preview plan: {created.json}")
    ctx.expect((fresh / "RENIUM.md").exists() and (fresh / "src").is_dir(), "guides and src created")
    again = ctx.rbx("init", cwd=fresh, timeout=60)
    ctx.expect(again.get("create") == [] and again.get("update") == [] and "renium.project.jsonc" in (again.get("keep") or []), f"second init: {again.json}")


def init_keeps_a_user_claude_md(ctx: Context):
    """LIFE-038: init keeps a user CLAUDE.md and adds one pointer to RENIUM.md."""
    fresh = ctx.root / "fresh"
    fresh.mkdir()
    (fresh / "CLAUDE.md").write_text("own text\n", encoding="utf-8")
    created = ctx.rbx("init", cwd=fresh, timeout=60)
    ctx.expect("CLAUDE.md" in (created.get("update") or []), f"init update: {created.json}")
    text = (fresh / "CLAUDE.md").read_text(encoding="utf-8")
    ctx.expect("own text" in text and text.count("RENIUM.md") == 1, f"CLAUDE.md: {text!r}")
    ctx.rbx("init", cwd=fresh, timeout=60)
    ctx.expect((fresh / "CLAUDE.md").read_text(encoding="utf-8").count("RENIUM.md") == 1, "rerun keeps one pointer")


ROJO = '{"name":"Rojo","tree":{"$className":"DataModel","ServerScriptService":{"$path":"src/server"}}}'


def _rojo(folder: Path) -> Path:
    (folder / "src" / "server").mkdir(parents=True, exist_ok=True)
    (folder / "src" / "server" / "Main.server.luau").write_text("print(1)\n", encoding="utf-8")
    (folder / "default.project.json").write_text(ROJO, encoding="utf-8")
    return folder


def init_converts_a_rojo_project(ctx: Context):
    """LIFE-039: init in a Rojo folder converts default.project.json into renium.project.jsonc."""
    fresh = _rojo(ctx.root / "rojo")
    created = ctx.rbx("init", cwd=fresh, timeout=60)
    ctx.expect(created.get("converted") == "default.project.json", f"init: {created.json}")
    text = (fresh / "renium.project.jsonc").read_text(encoding="utf-8")
    ctx.expect('"ServerScriptService"' in text and "src/server" in text, f"converted tree: {text[:300]}")


def bare_place_file_flow(ctx: Context):
    """LIFE-040: the documented flow for a folder with only a place file: init, so, pl, ps src --verify."""
    ctx.close_studio()
    bare = ctx.root.parent / f"{ctx.root.name}-bare"
    bare.mkdir()
    shutil.copy(ctx.root / ctx.place, bare / ctx.place)
    file = bare / ctx.place
    env = _env(ctx, place=file)
    try:
        _run(ctx, "init", env=env, cwd=bare, timeout=60)
        opened = _run(ctx, "so", ctx.place, env=env, cwd=bare, timeout=200)
        ctx.expect(opened.get("connected") is True, f"so: {opened.json}")
        _run(ctx, "pl", env=env, cwd=bare, timeout=180)
        ctx.expect((bare / MAIN).is_file() and (bare / "instances" / "Workspace.renium").is_file(), "pull wrote src/ and instances/")
        verified = _run(ctx, "ps", "src", "--verify", env=env, cwd=bare, timeout=180)
        ctx.expect(verified.get("sourceVerifyFailed", 0) == 0, f"verify: {verified.json}")
    finally:
        _close_sibling(ctx, bare, env)


def commands_never_create_a_project(ctx: Context):
    """LIFE-041: status in an empty folder says there is no project; -r NEW names the missing folder and creates nothing."""
    empty = _outside(ctx, "empty")
    status = ctx.rbx("status", cwd=empty, ok=False, timeout=60)
    ctx.expect_in(f"No Renium project in {empty} or its parents", status.error, "no project")
    leading = ctx.rbx("-r", "NEW", "--output-mode", "json", "status", json_mode=False, cwd=empty, ok=False, timeout=60)
    ctx.expect_in("No Renium project in", leading.error, "-r NEW status")
    ctx.expect_in("does not exist", leading.error, "-r NEW names the missing folder")
    ctx.expect(not (empty / "NEW").exists(), "-r created no folder")
    ctx.expect(not list(empty.rglob("renium.project.json*")), "no project file written")
    shutil.rmtree(empty, ignore_errors=True)


def outdated_guide_refreshed(ctx: Context):
    """LIFE-042: a changed synced guide is restored by the next command, with a note on stderr."""
    guide = ctx.root / "RENIUM" / "sync.md"
    original = guide.read_text(encoding="utf-8")
    guide.write_text("edited\n", encoding="utf-8")
    status = ctx.rbx("status", ok=None, timeout=60)
    ctx.expect(guide.read_text(encoding="utf-8") == original, "guide restored")
    ctx.expect_in("[renium] updated RENIUM/sync.md; reread", status.stderr, "refresh note")


def rojo_conversion_preview_apply_force(ctx: Context):
    """LIFE-043: ir previews without writing, applies once, refuses a differing file without --force, and names
    folder ambiguity."""
    rojo = _rojo(ctx.root / "rojo")
    preview = ctx.rbx("ir", json_mode=False, cwd=rojo, timeout=60)
    ctx.expect(preview.code == 0 and '"ServerScriptService"' in preview.stdout and not (rojo / "renium.project.jsonc").exists(), "preview writes nothing")
    ctx.expect(ctx.rbx("ir", "--apply", cwd=rojo, timeout=60).get("changed") is True, "first apply")
    ctx.expect(ctx.rbx("ir", "--apply", cwd=rojo, timeout=60).get("changed") is False, "second apply")
    with (rojo / "renium.project.jsonc").open("a", encoding="utf-8") as handle:
        handle.write("// edited\n")
    refused = ctx.rbx("ir", "--apply", cwd=rojo, ok=False, timeout=60)
    ctx.expect_in("differs from the conversion; use --force", refused.error, "differing file")
    ctx.expect(ctx.rbx("ir", "--apply", "--force", cwd=rojo, timeout=60).get("changed") is True, "force")
    (rojo / "two").mkdir()
    (rojo / "two" / "a.project.json").write_text(ROJO, encoding="utf-8")
    (rojo / "two" / "b.project.json").write_text(ROJO, encoding="utf-8")
    (rojo / "none").mkdir()
    ctx.expect_in("Multiple Rojo projects exist in", ctx.rbx("ir", "two", cwd=rojo, ok=False, timeout=60).error, "two projects")
    ctx.expect_in("No *.project.json file exists in", ctx.rbx("ir", "none", cwd=rojo, ok=False, timeout=60).error, "no project")


def _adapter(ctx: Context, direction: str | None = None):
    """H10: a generated json adapter for data/config.json onto ReplicatedStorage.Config."""
    adapter = {"source": "data/config.json", "target": "ReplicatedStorage.Config", "format": "json", "generated": True}
    if direction:
        adapter["direction"] = direction
    ctx.write("renium.project.jsonc", json.dumps({"schemaVersion": 1, "adapters": [adapter]}, indent=2) + "\n")
    ctx.write("data/config.json", '{"k":1}\n')


def adapter_build_then_push(ctx: Context):
    """LIFE-045 (L10): ad build --check names stale output, ad build writes the module, which is valid Luau, and a
    push makes it requireable."""
    _adapter(ctx)
    stale = ctx.rbx("ad", "build", "--check", ok=False, timeout=60)
    ctx.expect_in("Generated adapter output is stale", stale.error, "stale check")
    built = ctx.rbx("ad", "build", timeout=60)
    ctx.expect(built.get("adapters") == 1, f"ad build: {built.json}")
    ctx.expect(ctx.rbx("ad", "build", "--check", timeout=60).get("checked") is True, "check passes after the build")
    syntax = ctx.rbx("ck", "src/ReplicatedStorage/Config.luau", ok=None, timeout=60)
    ctx.expect(syntax.ok, f"generated Config.luau is not valid Luau (L10): {ctx.read('src/ReplicatedStorage/Config.luau')!r} {str(syntax.json)[:200]}")
    ctx.open_studio()
    ctx.rbx("ps", "src/ReplicatedStorage/Config.luau", "--yes", timeout=120)
    ctx.expect(ctx.luau("return require(game.ReplicatedStorage.Config).k") == 1, "Config.k in Studio")


def two_way_adapter_follows_a_pull(ctx: Context):
    """LIFE-046 (L12): a two-way txt adapter (json is not reversible) pushes its StringValue and writes a Studio edit
    of it back to the data file on pull."""
    adapter = {"source": "data/note.txt", "target": "ReplicatedStorage.Note", "format": "txt", "direction": "two-way"}
    ctx.write("renium.project.jsonc", json.dumps({"schemaVersion": 1, "adapters": [adapter]}, indent=2) + "\n")
    ctx.write("data/note.txt", "one")
    ctx.rbx("ad", "build", timeout=60)
    ctx.open_studio()
    store = ctx.rbx("ps", "instances/ReplicatedStorage.renium", "--yes", ok=None, timeout=120)
    whole = ctx.rbx("ps", "--yes", ok=None, timeout=180)
    ctx.expect(store.ok or whole.ok, f"the adapter's StringValue cannot be pushed (L12): store push {store.error[:200]!r}; project push {whole.error[:200]!r}")
    ctx.expect(ctx.luau("return game.ReplicatedStorage.Note.Value") == "one", "Note in Studio")
    ctx.luau('game.ReplicatedStorage.Note.Value = "two" return true')
    ctx.rbx("pl", timeout=180)
    ctx.expect(ctx.read("data/note.txt") == "two", f"data/note.txt after the pull: {ctx.read('data/note.txt')!r}")
    preview = ctx.rbx("ad", "syncback", "--preview", timeout=60)
    ctx.expect(preview.get("operations") == [], f"syncback preview: {preview.json}")


def replaced_adapter_keeps_building(ctx: Context):
    """LIFE-044 (L11): after an adapter is built and then replaced in the manifest, ad build still works."""
    _adapter(ctx)
    ctx.rbx("ad", "build", timeout=60)
    adapter = {"source": "data/note.txt", "target": "ReplicatedStorage.Note", "format": "txt"}
    ctx.write("renium.project.jsonc", json.dumps({"schemaVersion": 1, "adapters": [adapter]}, indent=2) + "\n")
    ctx.write("data/note.txt", "one")
    rebuilt = ctx.rbx("ad", "build", ok=None, timeout=60)
    ctx.expect(rebuilt.ok, f"ad build after replacing the adapter (L11): {rebuilt.error[:300]}")
    ctx.expect(ctx.rbx("bg", "ReplicatedStorage", "Note", "Value", timeout=60).json == "one", "Note in the store")


def build_open_verify(ctx: Context):
    """LIFE-047: build writes build/place.rbxl, which opens in Studio and verifies against the sources."""
    ctx.close_studio()
    built = ctx.rbx("build", timeout=120)
    output = ctx.root / "build" / "place.rbxl"
    ctx.expect(output.is_file() and str(Path(built.get("output") or "").resolve()) == str(output.resolve()), f"build: {built.json}")
    env = _env(ctx, place=output)
    try:
        opened = _run(ctx, "so", str(output), env=env, timeout=200)
        ctx.expect(opened.get("connected") is True, f"so build/place.rbxl: {opened.json}")
        verified = _run(ctx, "ps", "src", "--verify", env=env, timeout=180)
        ctx.expect(verified.get("sourceVerifyFailed", 0) == 0, f"verify: {verified.json}")
    finally:
        _run(ctx, "sx", "--terminate", env=env, ok=None, timeout=90)


def build_needs_stores(ctx: Context):
    """LIFE-048: build refuses a plugin build to a place file and a non-model output, refuses a project without
    service stores, and writes a place from the fixture's stores."""
    plugin = ctx.rbx("build", "--plugin", "-o", "x.rbxl", ok=False, timeout=60)
    ctx.expect_in("--plugin requires an .rbxm or .rbxmx output", plugin.error, "plugin output")
    text = ctx.rbx("build", "-o", "x.txt", ok=False, timeout=60)
    ctx.expect_in("Build output must end in", text.error, "output extension")
    fresh = ctx.root / "empty"
    fresh.mkdir()
    ctx.rbx("init", cwd=fresh, timeout=60)
    refused = ctx.rbx("build", "-o", "out.rbxl", ok=False, cwd=fresh, timeout=60)
    ctx.expect_in("No service settings files", refused.error, "build without stores")
    built = ctx.rbx("build", "-o", "built.rbxl", timeout=120)
    ctx.expect(built.ok and (ctx.root / "built.rbxl").stat().st_size > 1000, f"build: {built.json}")


def _changed_within(path: Path, before: int, seconds: float) -> float | None:
    started = time.monotonic()
    while time.monotonic() - started < seconds:
        if path.exists() and path.stat().st_mtime_ns != before:
            return time.monotonic() - started
        time.sleep(0.05)
    return None


def watchers_rebuild(ctx: Context):
    """LIFE-049: build --watch rewrites its output and ad watch rewrites the adapter module within 2 s of an edit."""
    output = ctx.root / "watch.rbxl"
    builder = _spawn(ctx, ["build", "--watch", "-o", "watch.rbxl"], ctx.root / "build-watch.log")
    try:
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline and not output.exists():
            time.sleep(0.2)
        ctx.expect(output.exists(), "build --watch wrote its first output")
        time.sleep(1.0)
        before = output.stat().st_mtime_ns
        ctx.write(MAIN, 'print("watched")\n')
        latency = _changed_within(output, before, 2.0)
        ctx.expect(latency is not None, "build --watch did not rewrite the output within 2 s")
    finally:
        _kill_tree(builder)
    _adapter(ctx)
    ctx.rbx("ad", "build", timeout=60)
    module = ctx.root / "src" / "ReplicatedStorage" / "Config.luau"
    watcher = _spawn(ctx, ["ad", "watch"], ctx.root / "ad-watch.log")
    try:
        time.sleep(2.0)
        before = module.stat().st_mtime_ns
        ctx.write("data/config.json", '{"k":3}\n')
        latency = _changed_within(module, before, 2.0)
        ctx.expect(latency is not None and "3" in module.read_text(encoding="utf-8"), f"ad watch did not rewrite Config.luau within 2 s: {module.read_text(encoding='utf-8')!r}")
    finally:
        _kill_tree(watcher)


def sourcemap_after_a_pull(ctx: Context):
    """LIFE-050: after a pull the cached sourcemap lists a Studio-made module; --cached refuses --watch."""
    ctx.open_studio()
    ctx.luau('local m = Instance.new("ModuleScript") m.Name = "New" m.Source = "return 1" m.Parent = game.ReplicatedStorage.Shared return true')
    ctx.rbx("pl", timeout=180)
    cached = ctx.rbx("sm", "--cached", "--stdout", "--filter", "**/New*", json_mode=False, timeout=60)
    ctx.expect(cached.code == 0 and '"New"' in cached.stdout, f"cached sourcemap: {cached.stdout[:300]}")
    watch = ctx.rbx("sm", "--cached", "--stdout", "--watch", ok=False, timeout=60)
    ctx.expect_in("--cached cannot be combined with --watch", watch.error or watch.stdout, "cached + watch")


# ---------------------------------------------------------------- F. doctor, report, docs, setup, update, probe


def doctor_with_studio_and_bundle(ctx: Context):
    """LIFE-051 (L9): dr --json --bundle with Studio open passes and names its checks; the bundle is a zip holding
    the doctor JSON."""
    ctx.open_studio()
    report = ctx.rbx("dr", "--json", "--bundle", "out.zip", timeout=180)
    checks = {c.get("name"): c.get("status") for c in report.get("checks") or []}
    ctx.expect(report.get("version") and checks.get("project") == "ok" and checks.get("configuration") == "ok", f"dr: {checks}")
    ctx.expect(checks.get("daemon") == "ok" and "studioPlugin" in checks and any(n.startswith("tool:") for n in checks), f"dr checks: {checks}")
    if os.name == "nt":
        ctx.expect("rbxLauncher" in checks, f"rbxLauncher check: {checks}")
    ctx.expect(any(n.startswith("native:") for n in checks) and report.get("nativeOperations"), f"native checks: {sorted(checks)}")
    bundle = ctx.root / "out.zip"
    folder = ctx.root / "out"
    ctx.expect(bundle.is_file(), f"dr --bundle out.zip wrote no zip (L9); {folder} holds {sorted(p.name for p in folder.iterdir()) if folder.is_dir() else 'nothing'}")
    with zipfile.ZipFile(bundle) as archive:
        names = archive.namelist()
        ctx.expect(any(n.endswith("doctor.json") for n in names), f"bundle: {names}")


def doctor_outside_and_in_a_broken_project(ctx: Context):
    """LIFE-052: dr outside a project warns and exits 0; in a project with broken JSONC it reports errors."""
    empty = _outside(ctx, "empty")
    outside = ctx.rbx("dr", "--json", cwd=empty, timeout=120)
    shutil.rmtree(empty, ignore_errors=True)
    project = next(c for c in outside.get("checks") or [] if c.get("name") == "project")
    ctx.expect(outside.code == 0 and project.get("status") == "warn" and "rbx init" in str(project.get("action")), f"dr outside: {project}")
    broken = ctx.root / "broken"
    broken.mkdir()
    (broken / "renium.project.jsonc").write_text("{", encoding="utf-8")
    failed = ctx.rbx("dr", "--json", cwd=broken, ok=False, timeout=120)
    project = next(c for c in failed.get("checks") or [] if c.get("name") == "project")
    ctx.expect(project.get("status") == "error" and failed.get("ok") is False, f"dr broken: {project}")
    ctx.expect_in("Renium doctor found errors", failed.stderr, "doctor error line")


def invalid_config_blocks_commands_not_doctor(ctx: Context):
    """LIFE-053: an invalid config value blocks status and shows as a configuration error in dr."""
    ctx.write(".renium/config.json", '{"logLevel":5}\n')
    status = ctx.rbx("status", ok=False, timeout=60)
    ctx.expect_in("Configuration key 'logLevel' must be off, error,", status.error, "status refused")
    doctor = ctx.rbx("dr", "--json", ok=False, timeout=120)
    configuration = next(c for c in doctor.get("checks") or [] if c.get("name") == "configuration")
    ctx.expect(configuration.get("status") == "error", f"configuration check: {configuration}")


SECRET = "abcdef123456"


def report_bundle_is_masked(ctx: Context):
    """LIFE-055 (L6): report writes a masked bundle; the secret in the message appears nowhere, the issue URL included."""
    ctx.open_studio()
    report = ctx.rbx("report", "-m", f"token={SECRET} broke", timeout=180)
    rid = str(report.get("id"))
    ctx.expect(rid.startswith("RNM-") and len(rid.split("-")) == 3, f"id: {rid}")
    directory = Path(report.get("directory") or "")
    ctx.expect(directory.parent == ctx.root / ".renium" / "reports" and Path(report.get("zip") or "").is_file(), f"report paths: {report.json}")
    files = set(report.get("files") or [])
    ctx.expect({"report.md", "report.json", "doctor.json", "studio.json", "renium.project.jsonc"} <= files, f"files: {files}")
    ctx.expect(str(report.get("uploadCommand")) == f"rbx report --upload {rid}", f"uploadCommand: {report.get('uploadCommand')}")
    studio = (directory / "studio.json").read_text(encoding="utf-8")
    ctx.expect(ctx.place in studio, f"studio.json lists the client: {studio[:300]}")
    leaks = [p.name for p in directory.iterdir() if SECRET in p.read_text(encoding="utf-8", errors="replace")]
    ctx.expect(not leaks, f"secret in {leaks}")
    with zipfile.ZipFile(report.get("zip")) as bundle:
        ctx.expect(not [n for n in bundle.namelist() if SECRET.encode() in bundle.read(n)], "secret in the zip")
    ctx.expect(SECRET not in str(report.get("issueUrl")), f"issueUrl carries the secret unmasked (L6): {report.get('issueUrl')}")


def report_without_transcript_to_a_folder(ctx: Context):
    """LIFE-056: report --no-transcript -o DIR leaves out the transcript and repeats -o in the upload command."""
    out = ctx.root / "out"
    report = ctx.rbx("report", "--no-transcript", "-o", str(out), timeout=180)
    directory = Path(report.get("directory") or "")
    ctx.expect(directory.parent == out and "transcript.md" not in (report.get("files") or []), f"report: {report.json}")
    ctx.expect_in("(left out with --no-transcript)", (directory / "report.md").read_text(encoding="utf-8"), "report.md note")
    ctx.expect("-o" in str(report.get("uploadCommand")) and str(out) in str(report.get("uploadCommand")), f"uploadCommand: {report.get('uploadCommand')}")


def report_without_project_or_studio(ctx: Context):
    """LIFE-057: report in an empty folder writes under the app data reports folder; studio.json has no clients."""
    ctx.close_studio()
    empty = _outside(ctx, "empty")
    report = ctx.rbx("report", "-m", "no project", cwd=empty, timeout=180)
    directory = Path(report.get("directory") or "")
    try:
        ctx.expect(directory.parent == LOCAL / "reports", f"report directory: {directory}")
        studio = json.loads((directory / "studio.json").read_text(encoding="utf-8"))
        ctx.expect(studio.get("error") or not _clients(CommandResult([], 0, "", "", 0, studio)), f"studio.json: {studio}")
    finally:
        if directory.parent == LOCAL / "reports" and directory.name.startswith("RNM-"):
            shutil.rmtree(directory, ignore_errors=True)
            Path(str(directory) + ".zip").unlink(missing_ok=True)
        shutil.rmtree(empty, ignore_errors=True)


def docs_topics(ctx: Context):
    """LIFE-058: docs prints the README, a guide and a README section, and names the guides for an unknown topic."""
    readme = ctx.rbx("docs", timeout=60)
    ctx.expect(str(readme.get("text", "")).startswith("# Renium CLI") and "topic" in readme.json, f"docs: {str(readme.json)[:100]}")
    guide = ctx.rbx("docs", "sync", timeout=60)
    ctx.expect(str(guide.get("text", "")).startswith("# Sync") and guide.get("topic") == "sync", f"docs sync: {str(guide.json)[:100]}")
    section = ctx.rbx("docs", "Live Sync", timeout=60)
    ctx.expect(str(section.get("text", "")).startswith("## Live Sync"), f"docs 'Live Sync': {str(section.json)[:100]}")
    unknown = ctx.rbx("docs", "nosuch", ok=False, timeout=60)
    ctx.expect_in("No documentation topic matches 'nosuch'. Guides:", unknown.error, "unknown topic lists guides")


def docs_serve(ctx: Context):
    """LIFE-059: docs --serve --port 0 prints its URL and serves the README as HTML."""
    log = ctx.root / "docs-serve.log"
    server = _spawn(ctx, ["docs", "--serve", "--port", "0"], log)
    try:
        url = ""
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline and not url:
            for line in log.read_text(encoding="utf-8", errors="replace").splitlines():
                if line.startswith("Renium docs: http://127.0.0.1:"):
                    url = line.split(": ", 1)[1].strip()
            time.sleep(0.2)
        ctx.expect(url, f"no URL printed: {log.read_text(encoding='utf-8', errors='replace')[:300]}")
        with urllib.request.urlopen(url, timeout=10) as response:
            body = response.read().decode("utf-8", errors="replace")
            ctx.expect(response.status == 200 and "text/html" in response.headers.get("Content-Type", "") and "<pre>" in body, f"GET {url}: {response.status} {body[:200]}")
    finally:
        _kill_tree(server)


def _plugin_file(ctx: Context) -> Path:
    """A copy of the installed Studio plugin (the Renium.rbxm beside rbx can be an older build)."""
    installed = (Path(os.environ.get("LOCALAPPDATA", "")) / "Roblox" / "Plugins" / "Renium.rbxm") if os.name == "nt" else (Path.home() / "Documents" / "Roblox" / "Plugins" / "Renium.rbxm")
    ctx.expect(installed.is_file(), f"no installed plugin at {installed}")
    copy = ctx.root / "Renium-installed.rbxm"
    shutil.copy(installed, copy)
    return copy


def setup_into_a_scratch_folder(ctx: Context):
    """LIFE-060, LIFE-061 (L4): setup --status/--dry-run/install into a scratch Plugins folder with a scratch home;
    --repair reinstalls the plugin and the agent hints, and the help no longer promises PATH entries."""
    home = _scratch_home(ctx)
    env = _env(ctx, **home)
    plugins = ctx.root / "Plugins"
    plugin = _plugin_file(ctx)
    status = _run(ctx, "setup", "--status", "--dir", str(plugins), env=env, ok=None, timeout=60)
    ctx.expect(status.get("installed") is False, f"setup --status: {status.json}")
    dry = _run(ctx, "setup", "--dir", str(plugins), "--file", str(plugin), "--dry-run", env=env, timeout=60)
    ctx.expect(Path(dry.get("wouldInstallTo") or "") == plugins / "Renium.rbxm" and not (plugins / "Renium.rbxm").exists(), f"dry run: {dry.json}")
    installed = _run(ctx, "setup", "--dir", str(plugins), "--file", str(plugin), env=env, timeout=60)
    ctx.expect(Path(installed.get("installedTo") or "") == plugins / "Renium.rbxm" and (plugins / "Renium.rbxm").is_file(), f"install: {installed.json}")
    ctx.expect(all(str(Path(p)).startswith(home["USERPROFILE"]) for p in installed.get("agentHintsWritten") or []), f"hints outside the scratch home: {installed.get('agentHintsWritten')}")
    ctx.expect("RENIUM" in (Path(home["USERPROFILE"]) / ".claude" / "CLAUDE.md").read_text(encoding="utf-8").upper(), "hint written in the scratch home")
    after = _run(ctx, "setup", "--status", "--dir", str(plugins), env=env, timeout=60)
    ctx.expect(after.get("installed") is True and after.get("compatible") is True, f"status after install: {after.json}")
    repaired = _run(ctx, "setup", "--repair", "--dir", str(plugins), "--file", str(plugin), env=env, timeout=60)
    ctx.expect(repaired.get("action") == "repair", f"repair: {repaired.json}")
    ctx.expect(Path(repaired.get("installedTo") or "") == plugins / "Renium.rbxm", f"repair reinstalled the plugin: {repaired.json}")
    helped = _run(ctx, "setup", "--help", env=env, json_mode=False, ok=None, timeout=60)
    ctx.expect("PATH is left" in (helped.stdout + helped.stderr), "setup --help says PATH is left alone (L4)")


def setup_refusals(ctx: Context):
    """LIFE-062: setup refuses a non-Renium model, two actions at once, and removing a foreign Renium.rbxm."""
    home = _scratch_home(ctx)
    env = _env(ctx, **home)
    ctx.rbx("mep", "Workspace", "-i", "editor:1", "-o", "props.rbxm", timeout=60)
    plugins = ctx.root / "Plugins"
    foreign = _run(ctx, "setup", "--dir", str(plugins), "--file", str(ctx.root / "props.rbxm"), env=env, ok=False, timeout=60)
    ctx.expect_in("is not a Renium Studio plugin", foreign.error, "foreign --file")
    both = _run(ctx, "setup", "--status", "--repair", "--dir", str(plugins), env=env, ok=False, timeout=60)
    ctx.expect_in("Use only one of --status, --repair, or --uninstall", both.error, "two actions")
    plugins.mkdir(exist_ok=True)
    shutil.copy(ctx.root / "props.rbxm", plugins / "Renium.rbxm")
    refused = _run(ctx, "setup", "--uninstall", "--dir", str(plugins), env=env, ok=False, timeout=60)
    ctx.expect_in("Refusing to remove", refused.error, "foreign uninstall")
    ctx.expect((plugins / "Renium.rbxm").is_file(), "foreign file kept")


def update_manifest_refusals(ctx: Context):
    """LIFE-064: upd check refuses a plain-HTTP manifest and a local manifest whose payload was edited; the unedited
    cached manifest verifies offline (the LIFE-063 fields)."""
    cache = DATA / "update-check.json"
    ctx.expect(cache.is_file(), f"no cached update manifest at {cache}")
    manifest = json.loads(cache.read_text(encoding="utf-8"))["manifest"]
    ctx.write("real.json", json.dumps(manifest))
    manifest["payload"]["version"] = "9.9.9"
    ctx.write("edited.json", json.dumps(manifest))
    http = ctx.rbx("upd", "check", "--manifest", "http://x/m.json", ok=False, timeout=60)
    ctx.expect_in("Update manifests must use HTTPS or a local file", http.error, "http manifest")
    edited = ctx.rbx("upd", "check", "--manifest", "edited.json", ok=False, timeout=60)
    ctx.expect(edited.code != 0 and "signature verification failed" in edited.error, f"edited manifest: {edited.error[:200]}")
    real = ctx.rbx("upd", "check", "--manifest", "real.json", timeout=60)
    for key in ("currentVersion", "latestVersion", "updateAvailable", "downgrade", "platform"):
        ctx.expect(key in real.json, f"upd check lacks {key}: {real.json}")
    ctx.expect(real.get("signature") == "verified", f"signature: {real.get('signature')}")


def native_probe(ctx: Context):
    """LIFE-065: native-probe reports per-operation verdicts for the Studio; --require-native fails on a non-Studio pid."""
    opened = ctx.open_studio()
    probe = ctx.rbx("native-probe", "--pid", str(opened.get("pid")), ok=None, timeout=120)
    operations = probe.get("operations") or []
    ctx.expect(operations and all("verdict" in o for o in operations), f"probe: {str(probe.json)[:300]}")
    ctx.expect("build" in probe.json and "target" in probe.json, f"probe keys: {sorted(probe.json)}")
    failed = ctx.rbx("native-probe", "--pid", str(os.getpid()), "--require-native", ok=False, timeout=120)
    ctx.expect(failed.code != 0 and "Native compatibility probes failed" in failed.stderr, f"require-native: {failed.stderr[:200]}")


# ---------------------------------------------------------------- G. places


def _unpinned(ctx: Context) -> dict:
    """After pa the place file pin is refused ("Place ... is not configured"); the documented pin is the alias."""
    return _env(ctx, place="lobby")


def _pa(ctx: Context) -> CommandResult:
    ctx.close_studio()
    return _run(ctx, "pa", "222", "Lobby", "--game-id", "9", env=_env(ctx), timeout=60)


def place_add_converts_a_single_place(ctx: Context):
    """LIFE-066 (L7, L13): with Studio closed, pa turns the project into an experience that lists the current place
    and the new one; the place's project, sources and stores move under places/<alias> and the place still binds
    with its data."""
    added = _pa(ctx)
    ctx.expect(added.get("alias") == "lobby" and added.get("placeId") == 222 and added.get("root") == "places/lobby" and added.get("rebind") is True, f"pa: {added.json}")
    manifest = json.loads(ctx.read("renium.experience.json"))
    ctx.expect(manifest.get("gameId") == 9 and 222 in manifest.get("placeOrder", []), f"manifest: {manifest}")
    places = manifest.get("places") or {}
    alias = next((key for key, entry in places.items() if entry.get("placeId") != 222), "lobby")
    place = ctx.root / places[alias]["root"]
    env = _env(ctx, place=alias)
    ctx.expect((place / "renium.project.jsonc").is_file() and (place / MAIN).is_file(), f"project and src moved to {place}")
    status = _run(ctx, "status", env=env, ok=None, timeout=60)
    ctx.expect("No Renium project" not in status.error and "not configured" not in status.error, f"status binds: {status.error[:300]}")
    found = _run(ctx, "f", "Workspace", "-n", "Sign", env=env, ok=None, timeout=60)
    ctx.expect((place / "instances" / "Workspace.renium").is_file() and len(_matches(found)) == 1, f"pa left the stores in {ctx.root / 'instances'}; f Workspace -n Sign -> {found.stdout.strip()[:200]} (L7)")
    ctx.expect(len(places) == 2, f"pa with Studio closed did not add 222 beside the current place (L13): {manifest}")


def place_add_refusals(ctx: Context):
    """LIFE-067: pa refuses a configured place, a root outside the experience, a mismatched gameId and a project
    with sourceRoot '.' (project validation refuses that layout before pa's own check)."""
    plain = ctx.root.parent / f"{ctx.root.name}-dot"
    shutil.copytree(ctx.root, plain, ignore=IGNORE)
    try:
        _pa(ctx)
        env = _unpinned(ctx)
        ctx.expect_in("is already configured", _run(ctx, "pa", "222", "X", env=env, ok=False, timeout=60).error, "configured place")
        ctx.expect_in("Place root must be a relative path", _run(ctx, "pa", "333", "Y", "--root", "../out", env=env, ok=False, timeout=60).error, "root outside")
        ctx.expect_in("does not match project gameId 9", _run(ctx, "pa", "333", "Y", "--game-id", "8", env=env, ok=False, timeout=60).error, "gameId")
        (plain / "renium.project.jsonc").write_text('{"schemaVersion":1,"sourceRoot":"."}\n', encoding="utf-8")
        dot = _run(ctx, "pa", "222", "Lobby", env=_env(ctx, place=plain / ctx.place), cwd=plain, ok=False, timeout=60)
        ctx.expect_in("sourceRoot must be separate from the project's instances directory", dot.error, "sourceRoot '.'")
        ctx.expect(not (plain / "places").exists() and not (plain / "renium.experience.json").exists(), "nothing moved")
    finally:
        shutil.rmtree(plain, ignore_errors=True)


def place_rename(ctx: Context):
    """LIFE-068: pn renames the alias and its folder once, then reports no rebind; a taken alias is refused."""
    _pa(ctx)
    env = _unpinned(ctx)
    _run(ctx, "pa", "333", "Other", env=env, timeout=60)
    start = json.loads(ctx.read("renium.experience.json")).get("startPlace")
    renamed = _run(ctx, "pn", "222", "Arena", env=env, timeout=60)
    manifest = json.loads(ctx.read("renium.experience.json"))
    follows = manifest.get("startPlace") == ("arena" if start == "lobby" else start)
    ctx.expect(renamed.get("rebind") is True and (ctx.root / "places" / "arena").is_dir() and follows, f"pn: {renamed.json} {manifest}")
    env = _env(ctx, place="arena")
    ctx.expect(_run(ctx, "pn", "222", "Arena", env=env, timeout=60).get("rebind") is False, "repeat pn")
    ctx.expect_in("already exists", _run(ctx, "pn", "222", "other", env=env, ok=False, timeout=60).error, "taken alias")


def place_order(ctx: Context):
    """LIFE-069: po writes placeOrder and refuses an order that leaves a place out."""
    _pa(ctx)
    env = _unpinned(ctx)
    _run(ctx, "pa", "333", "Other", env=env, timeout=60)
    _run(ctx, "po", "333", "222", env=env, timeout=60)
    ctx.expect(json.loads(ctx.read("renium.experience.json")).get("placeOrder") == [333, 222], "placeOrder written")
    ctx.expect_in("p.order must contain every configured published place ID exactly once", _run(ctx, "po", "222", env=env, ok=False, timeout=60).error, "partial order")


# ---------------------------------------------------------------- H. configuration


def config_round_trip(ctx: Context):
    """LIFE-070: cfg set/get/unset round-trip a place-scoped key, path names the file, unknown keys are refused."""
    ctx.expect(ctx.rbx("cfg", "set", "liveSync.initialSyncPriority", "verify").get("value") == "verify", "cfg set")
    ctx.expect(ctx.rbx("cfg", "get", "liveSync.initialSyncPriority").json == "verify", "cfg get")
    path = ctx.rbx("cfg", "path")
    ctx.expect(Path(path.get("path") or "") == ctx.root / ".renium" / "config.json", f"cfg path: {path.json}")
    unset = ctx.rbx("cfg", "unset", "liveSync.initialSyncPriority")
    ctx.expect(unset.get("action") == "unset" and str(unset.get("path")).endswith("config.json"), f"cfg unset: {unset.json}")
    ctx.expect(ctx.rbx("cfg", "get", "liveSync.initialSyncPriority").json == "reconcile", "default restored")
    refused = ctx.rbx("cfg", "set", "nope.key", "1", ok=False)
    ctx.expect_in("Unknown Renium configuration key", refused.error, "unknown key")
    listing = ctx.rbx("cfg", "list")
    ctx.expect(any(item.get("name") == "bridgePorts" for item in listing.get("settings") or []), f"cfg list: {listing.json}")


def _setting(listing: CommandResult, name: str) -> dict:
    return next((s for s in listing.get("settings") or [] if s.get("name") == name), {})


def config_scope_precedence(ctx: Context):
    """LIFE-071: a place value beats a user value with origin place; after unsetting it the user value shows."""
    env = _env(ctx, **_scratch_home(ctx))
    key = "liveSync.initialSyncPriority"
    _run(ctx, "cfg", "set", key, "reconcile", "--scope", "user", env=env, timeout=60)
    _run(ctx, "cfg", "set", key, "verify", env=env, timeout=60)
    merged = _setting(_run(ctx, "cfg", "list", "--origins", env=env, timeout=60), key)
    place = ctx.root / ".renium" / "config.json"
    ctx.expect(merged.get("value") == "verify" and Path(merged.get("origin") or "") == place, f"merged: {merged}")
    _run(ctx, "cfg", "unset", key, "--scope", "place", env=env, timeout=60)
    after = _setting(_run(ctx, "cfg", "list", "--origins", env=env, timeout=60), key)
    user = Path(env["APPDATA"]) / "Renium" / "config.json" if os.name == "nt" else Path(env["HOME"]) / "Library" / "Application Support" / "Renium" / "config.json"
    ctx.expect(after.get("value") == "reconcile" and Path(after.get("origin") or "") == user, f"after unset: {after}")
    ctx.expect(_run(ctx, "cfg", "get", key, env=env, timeout=60).json == "reconcile", "get after unset")


def config_unset_reset_export(ctx: Context):
    """LIFE-072 (L5): unset of a missing key is refused, reset deletes the file, export writes the merged settings;
    a key cfg set accepts is listed by cfg list."""
    env = _env(ctx, **_scratch_home(ctx))
    ctx.expect_in("is not set in", _run(ctx, "cfg", "unset", "nosuch", env=env, ok=False, timeout=60).error, "unset missing")
    _run(ctx, "cfg", "set", "liveSync.initialSyncPriority", "verify", env=env, timeout=60)
    _run(ctx, "cfg", "export", "-o", "m.json", env=env, timeout=60)
    exported = json.loads(ctx.read("m.json"))
    ctx.expect(exported.get("liveSync", {}).get("initialSyncPriority") == "verify" and exported.get("schemaVersion") == 1, f"export: {exported}")
    reset = _run(ctx, "cfg", "reset", env=env, timeout=60)
    ctx.expect(reset.get("action") == "reset" and not (ctx.root / ".renium" / "config.json").exists(), f"reset: {reset.json}")
    accepted = _run(ctx, "cfg", "set", "liveSync.changesThreshold", "0", env=env, timeout=60)
    ctx.expect(accepted.get("value") == 0, f"changesThreshold accepted: {accepted.json}")
    names = [s.get("name") for s in _run(ctx, "cfg", "list", env=env, timeout=60).get("settings") or []]
    ctx.expect("liveSync.changesThreshold" in names, "cfg set accepts liveSync.changesThreshold but cfg list does not show it and nothing reads it (L5)")


def config_validation(ctx: Context):
    """LIFE-073: invalid values, unknown keys, the merged scope and a user-only key at place scope are refused
    without writing a file."""
    env = _env(ctx, **_scratch_home(ctx))
    cases = [
        (("cfg", "set", "logLevel", "loud"), "Configuration key 'logLevel' must be off, error,"),
        (("cfg", "set", "bogus", "1"), "Unknown Renium configuration key 'bogus'"),
        (("cfg", "set", "place", "x", "--scope", "merged"), "--scope merged is read-only"),
        (("cfg", "get", "bogus"), "Unknown setting 'bogus'"),
        (("cfg", "set", "studioAudioMode", "auto"), "studioAudioMode is global"),
    ]
    for args, message in cases:
        ctx.expect_in(message, _run(ctx, *args, env=env, ok=False, timeout=60).error, " ".join(args))
    ctx.expect(not (ctx.root / ".renium" / "config.json").exists(), "place config untouched")
    ctx.expect(not (Path(env["APPDATA"]) / "Renium" / "config.json").exists(), "user config untouched")


def config_edit(ctx: Context):
    """LIFE-074: cfg edit creates the file and runs EDITOR on it, reports a failing editor, and prints the path
    without one."""
    adds = ctx.write("edit-ok.py", "import json,sys\np=sys.argv[1]\nd=json.load(open(p))\nd['logLevel']='debug'\njson.dump(d,open(p,'w'))\n")
    ok_editor = ctx.write("edit-ok.cmd", f'@"{sys.executable}" -I "{adds}" %1\r\n')
    bad_editor = ctx.write("edit-bad.cmd", "@exit /b 3\r\n")
    config = ctx.root / ".renium" / "config.json"
    _run(ctx, "cfg", "edit", env=_env(ctx, unset=("VISUAL",), EDITOR=ok_editor), timeout=60)
    edited = json.loads(config.read_text(encoding="utf-8"))
    ctx.expect(edited.get("schemaVersion") == 1 and edited.get("logLevel") == "debug", f"edited: {edited}")
    failed = _run(ctx, "cfg", "edit", env=_env(ctx, unset=("VISUAL",), EDITOR=bad_editor), ok=False, timeout=60)
    ctx.expect_in("The configured editor exited with", failed.error, "failing editor")
    printed = _run(ctx, "cfg", "edit", env=_env(ctx, unset=("VISUAL", "EDITOR")), json_mode=False, timeout=60)
    ctx.expect(str(config) in printed.stdout, f"path printed: {printed.stdout[:200]}")


def config_defaults_for_global_flags(ctx: Context):
    """LIFE-075: place config outputMode/logLevel act as default global flags and an explicit flag wins. (`status`
    prints compact JSON in every mode, so `dm list` is the probe; the `yes` part is in DATA-043.)"""
    ctx.write(".renium/config.json", '{"schemaVersion":1,"outputMode":"json","logLevel":"off"}\n')
    plain = ctx.rbx("dm", "list", json_mode=False, timeout=60)
    ctx.expect(plain.stdout.strip().startswith("{") and "daemons" in json.loads(plain.stdout.strip().splitlines()[-1]), f"dm list without flags: {plain.stdout[:200]}")
    text = ctx.rbx("--output-mode", "text", "dm", "list", json_mode=False, timeout=60)
    ctx.expect(not text.stdout.strip().startswith("{") and "default" in text.stdout, f"explicit text: {text.stdout[:200]}")
    guide = ctx.root / "RENIUM" / "sync.md"
    guide.write_text("edited\n", encoding="utf-8")
    quiet = ctx.rbx("status", json_mode=False, ok=None, timeout=60)
    ctx.expect("[renium]" not in quiet.stderr and guide.read_text(encoding="utf-8") != "edited\n", f"config logLevel off: {quiet.stderr[:200]}")
    guide.write_text("edited\n", encoding="utf-8")
    loud = ctx.rbx("--log-level", "info", "status", json_mode=False, ok=None, timeout=60)
    ctx.expect_in("[renium] updated RENIUM/sync.md", loud.stderr, "explicit --log-level beats the config")


# ---------------------------------------------------------------- I. global output options


def output_modes_and_errors(ctx: Context):
    """LIFE-076: pretty output is indented JSON, an unknown mode is refused, and a captured failure is JSON on stderr."""
    ctx.open_studio()
    pretty = ctx.rbx("--output-mode", "pretty", "status", json_mode=False, timeout=60)
    ctx.expect(pretty.stdout.startswith("{\n  ") and json.loads(pretty.stdout).get("clients"), f"pretty: {pretty.stdout[:200]}")
    xml = ctx.rbx("--output-mode", "xml", "status", json_mode=False, ok=False, timeout=60)
    ctx.expect_in("Invalid --output-mode 'xml'", xml.error, "xml")
    failed = ctx.rbx("so", "Nope.rbxl", json_mode=False, ok=False, timeout=60)
    parsed = json.loads(failed.stderr.strip().splitlines()[-1])
    ctx.expect(parsed.get("ok") is False and "does not exist" in parsed.get("error", "") and not failed.stdout.strip(), f"captured failure: {failed.stdout!r} {failed.stderr!r}")


def fields_projection(ctx: Context):
    """LIFE-077: --fields keeps listed top-level keys whole and projects every other record array to the listed
    keys; f keeps its own --fields."""
    ctx.open_studio()
    projected = ctx.rbx("--fields", "pid", "status", timeout=60)
    records = all(isinstance(v, list) and all(set(r) == {"pid"} for r in v) for k, v in projected.json.items() if k != "error")
    ctx.expect(projected.get("clients") and records, f"--fields pid: {projected.json}")
    kept = ctx.rbx("--fields", "clients,pid", "status", timeout=60)
    others = all(isinstance(v, list) and all(set(r) == {"pid"} for r in v) for k, v in kept.json.items() if k not in ("clients", "error"))
    ctx.expect(others and "role" in ((kept.get("clients") or [{}])[0]), f"--fields clients,pid: {kept.json}")
    own = ctx.rbx("--fields", "x", "f", "Workspace", "-n", "Sign", timeout=60)
    ctx.expect(isinstance(own.json, list) and own.json and set(own.json[0]) == {"x"} and isinstance(own.json[0]["x"], int), f"f reads --fields as its own field list (x = store index): {own.json}")


def log_levels(ctx: Context):
    """LIFE-078: --log-level off silences [renium] lines, the default and -v show them, an unknown level is refused."""
    guide = ctx.root / "RENIUM" / "sync.md"
    original = guide.read_text(encoding="utf-8")
    guide.write_text("edited\n", encoding="utf-8")
    quiet = ctx.rbx("--log-level", "off", "cfg", "path", timeout=60)
    ctx.expect("[renium]" not in quiet.stderr + quiet.stdout and guide.read_text(encoding="utf-8") == original, f"off: {quiet.stderr[:200]}")
    guide.write_text("edited\n", encoding="utf-8")
    verbose = ctx.rbx("-v", "cfg", "path", timeout=60)
    ctx.expect_in("[renium] updated RENIUM/sync.md", verbose.stderr, "-v shows info lines")
    refused = ctx.rbx("--log-level", "loud", "status", ok=False, timeout=60)
    ctx.expect_in("Invalid --log-level 'loud'", refused.error, "unknown level")


SCENARIOS = [
    Scenario("LIFE-001", "second so reuses the open Studio", "small", slotted(second_so_reuses_the_studio)),
    Scenario("LIFE-002", "busy answer within five seconds", "small", slotted(busy_answer_within_five_seconds)),
    Scenario("LIFE-080", "another Studio's request is not held up", "small", slotted(another_studios_request_is_not_held_up)),
    Scenario("LIFE-003", "fresh launch reports the connection", "small", slotted(fresh_launch_reports_the_connection)),
    Scenario("LIFE-004", "so refuses missing and non-place files", "small", slotted(so_refuses_missing_and_non_place_files)),
    Scenario("LIFE-005", "so without a file", "small", slotted(so_without_a_file)),
    Scenario("LIFE-006", "so --check launches nothing", "small", slotted(so_check_launches_nothing)),
    Scenario("LIFE-007", "so --no-wait reports starting", "small", slotted(so_no_wait_reports_starting)),
    Scenario("LIFE-008", "Studio not installed", "small", studio_not_installed, needs_studio=False, platforms=("nt",)),
    Scenario("LIFE-009", "sx needs a choice", "small", slotted(sx_needs_a_choice)),
    Scenario("LIFE-010", "save, close and reopen", "small", slotted(save_close_and_reopen)),
    Scenario("LIFE-013", "ro while open and of a missing file", "small", slotted(ro_while_open_and_missing)),
    Scenario("LIFE-015", "two ro --no-wait launch once", "small", slotted(two_ro_no_wait_launch_once)),
    Scenario("LIFE-016", "status of a connected place", "small", slotted(status_of_a_connected_place)),
    Scenario("LIFE-017", "status --all sees other projects", "small", slotted(status_all_sees_other_projects)),
    Scenario("LIFE-018", "no matching Studio", "small", slotted(no_matching_studio)),
    Scenario("LIFE-019", "no Studio answers", "small", slotted(no_studio_answers), exclusive=True),
    Scenario("LIFE-020", "--place selects another Studio", "small", slotted(place_flag_selects_another_studio)),
    Scenario("LIFE-021", "two places need a pin", "small", slotted(two_places_need_a_pin)),
    Scenario("LIFE-023", "config place against RENIUM_PLACE", "small", slotted(config_place_against_renium_place), platforms=("nt",)),
    Scenario("LIFE-024", "PLACE variable is ignored", "small", slotted(place_env_variable_is_ignored)),
    Scenario("LIFE-025", "so binds the last opened place", "small", slotted(so_binds_the_last_opened_place)),
    Scenario("LIFE-027", "daemon listing and restart", "small", slotted(daemon_listing_and_restart), default_daemon=True),
    Scenario("LIFE-028", "dm list fields", "small", daemon_list_fields, needs_studio=False, default_daemon=True),
    Scenario("LIFE-029", "stale discovery cleanup", "small", stale_discovery_cleanup, needs_studio=False),
    Scenario("LIFE-030", "named daemon isolation", "small", slotted(named_daemon_isolation), default_daemon=True),
    Scenario("LIFE-031", "--daemon NAME auto-start", "small", named_daemon_auto_start, needs_studio=False, default_daemon=True),
    Scenario("LIFE-033", "bd beside the default daemon", "small", bd_beside_the_default, needs_studio=False, default_daemon=True),
    Scenario("LIFE-034", "daemon exits with its parent", "small", daemon_exits_with_its_parent, needs_studio=False, default_daemon=True),
    Scenario("LIFE-035", "daemon has no idle exit", "small", daemon_has_no_idle_exit, needs_studio=False, default_daemon=True, long=True),
    Scenario("LIFE-036", "init creates and keeps a project", "small", init_creates_and_keeps_a_project, needs_studio=False),
    Scenario("LIFE-038", "init keeps a user CLAUDE.md", "small", init_keeps_a_user_claude_md, needs_studio=False),
    Scenario("LIFE-039", "init converts a Rojo project", "small", init_converts_a_rojo_project, needs_studio=False),
    Scenario("LIFE-040", "bare place file flow", "small", slotted(bare_place_file_flow)),
    Scenario("LIFE-041", "commands never create a project", "small", commands_never_create_a_project, needs_studio=False),
    Scenario("LIFE-042", "outdated guide refreshed", "small", outdated_guide_refreshed, needs_studio=False),
    Scenario("LIFE-043", "ir preview, apply and force", "small", rojo_conversion_preview_apply_force, needs_studio=False),
    Scenario("LIFE-044", "a replaced adapter keeps building", "small", replaced_adapter_keeps_building, needs_studio=False),
    Scenario("LIFE-045", "adapter build then push", "small", slotted(adapter_build_then_push)),
    Scenario("LIFE-046", "two-way adapter follows a pull", "small", slotted(two_way_adapter_follows_a_pull)),
    Scenario("LIFE-047", "build, open and verify", "small", slotted(build_open_verify)),
    Scenario("LIFE-048", "build needs stores", "small", build_needs_stores, needs_studio=False),
    Scenario("LIFE-049", "watchers rebuild", "small", watchers_rebuild, needs_studio=False),
    Scenario("LIFE-050", "sourcemap after a pull", "small", slotted(sourcemap_after_a_pull)),
    Scenario("LIFE-051", "doctor with Studio and a bundle", "small", slotted(doctor_with_studio_and_bundle)),
    Scenario("LIFE-052", "doctor outside and in a broken project", "small", doctor_outside_and_in_a_broken_project, needs_studio=False),
    Scenario("LIFE-053", "invalid config blocks commands, not doctor", "small", invalid_config_blocks_commands_not_doctor, needs_studio=False),
    Scenario("LIFE-055", "report bundle is masked", "small", slotted(report_bundle_is_masked)),
    Scenario("LIFE-056", "report without transcript to a folder", "small", report_without_transcript_to_a_folder, needs_studio=False),
    Scenario("LIFE-057", "report without project or Studio", "small", report_without_project_or_studio, needs_studio=False),
    Scenario("LIFE-058", "docs topics", "small", docs_topics, needs_studio=False),
    Scenario("LIFE-059", "docs --serve", "small", docs_serve, needs_studio=False),
    Scenario("LIFE-060", "setup into a scratch folder", "small", setup_into_a_scratch_folder, needs_studio=False),
    Scenario("LIFE-062", "setup refusals", "small", setup_refusals, needs_studio=False),
    Scenario("LIFE-064", "update manifest refusals", "small", update_manifest_refusals, needs_studio=False),
    Scenario("LIFE-065", "native-probe", "small", slotted(native_probe)),
    Scenario("LIFE-066", "pa converts a single place", "small", place_add_converts_a_single_place, needs_studio=False),
    Scenario("LIFE-067", "pa refusals", "small", place_add_refusals, needs_studio=False),
    Scenario("LIFE-068", "pn renames alias and root", "small", place_rename, needs_studio=False),
    Scenario("LIFE-069", "po reorders places", "small", place_order, needs_studio=False),
    Scenario("LIFE-070", "config round trip", "small", config_round_trip, needs_studio=False),
    Scenario("LIFE-071", "config scope precedence", "small", config_scope_precedence, needs_studio=False),
    Scenario("LIFE-072", "config unset, reset and export", "small", config_unset_reset_export, needs_studio=False),
    Scenario("LIFE-073", "config validation", "small", config_validation, needs_studio=False),
    Scenario("LIFE-074", "cfg edit", "small", config_edit, needs_studio=False, platforms=("nt",)),
    Scenario("LIFE-075", "config defaults for global flags", "small", config_defaults_for_global_flags, needs_studio=False),
    Scenario("LIFE-076", "output modes and errors", "small", slotted(output_modes_and_errors)),
    Scenario("LIFE-077", "--fields projection", "small", slotted(fields_projection)),
    Scenario("LIFE-078", "log levels", "small", log_levels, needs_studio=False),
    Scenario("LIFE-079", "simultaneous named daemon starts", "small", simultaneous_named_daemon_starts, needs_studio=False, default_daemon=True),
]
