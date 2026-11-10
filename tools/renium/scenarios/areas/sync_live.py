"""Live Sync worker, reconcile, sync history and daemon restart scenarios
(matrix sections F-K of scenarios/matrix/sync.md)."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from run import RBX, Context, Scenario

MAIN = "src/ServerScriptService/Main.server.luau"
UTIL = "src/ReplicatedStorage/Shared/Util.luau"
WORKSPACE = "instances/Workspace.renium"
LIGHTING = "instances/Lighting.renium"


def _live(ctx: Context):
    ctx.open_studio()
    ctx.rbx("lon", "--wait", "60", timeout=200)


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


def _wait_file(ctx: Context, relative: str, needle: str, seconds: int = 60):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline and needle not in ctx.read(relative):
        ctx.rbx("lst", "--wait", "10", ok=None, timeout=60)
    ctx.expect_in(needle, ctx.read(relative), f"{relative} after the Studio edit")


BAD_CLASS = "NoSuchStudioClass"
BAD = "Props.Bad"


def _blocked_push(ctx: Context):
    """H4: a stored instance of a class Studio does not have makes the push of that store fail for good.
    ba takes a class name its reflection database does not know, because the database can lag behind
    Studio (a non-creatable class such as Player is refused at ba since D11, an unknown enum item at bs),
    and the plugin refuses an upsert whose class Instance.new cannot create before it applies anything.
    The instance sits under Props: a new instance directly under the service root would travel as a
    binary import instead of an upsert."""
    ctx.rbx("ba", "Workspace", "-n", "Bad", "-c", BAD_CLASS, "-I", "Props")
    deadline = time.monotonic() + 40
    status = None
    while time.monotonic() < deadline:
        status = ctx.rbx("lst", ok=None, timeout=60)
        if _error_of(status):
            return status
        time.sleep(1)
    raise AssertionError(f"the push of the unknown class never failed: {status.json if status else None}")


def _error_of(status) -> str:
    return str(status.pointer("/daemon/error") or status.get("error") or "")


def _writer(ctx: Context, relative: str, template: str, interval: float, seconds: float) -> subprocess.Popen:
    """H12: a detached process rewriting one file on a timer; template holds {n}."""
    code = (
        "import sys,time\n"
        "path,template,interval,seconds=sys.argv[1],sys.argv[2],float(sys.argv[3]),float(sys.argv[4])\n"
        "end=time.monotonic()+seconds; n=0\n"
        "while time.monotonic()<end:\n"
        "    n+=1\n"
        "    open(path,'w',encoding='utf-8',newline='\\n').write(template.replace('{n}',str(n)))\n"
        "    time.sleep(interval)\n"
        "print(n)\n"
    )
    return subprocess.Popen([sys.executable, "-I", "-c", code, str(ctx.root / relative), template, str(interval), str(seconds)], stdout=subprocess.PIPE, text=True)


def _conflict_on_main(ctx: Context):
    """H: both sides edit Main while Live Sync is off; lon records the conflict."""
    _live(ctx)
    ctx.rbx("lof")
    ctx.write(MAIN, 'print("main")\nreturn 2\n')
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 3"')
    started = ctx.rbx("lon", ok=False, timeout=200)
    ctx.expect_in("changed on both sides", started.error, "lon reports the conflict")
    time.sleep(3)
    details = ctx.rbx("lst", "--details", ok=False, timeout=120)
    ctx.expect_in("Main.server.luau changed on both sides", details.error, "status names the conflicting file")
    ctx.expect_in("--prefer studio", details.error, "status lists the resolutions")
    ctx.expect(ctx.read(MAIN).strip().endswith("return 2"), "file untouched by the conflict")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main") == "return 3", "Studio untouched by the conflict")
    return details


# ---------------------------------------------------------------- F. pull


def pull_replaces_files_and_records_undo(ctx: Context):
    """SYNC-060: pl writes Studio state into the files and the sync history undoes it once."""
    ctx.open_studio()
    before_main = ctx.read(MAIN)
    before_anchored = ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json
    _studio_edit(ctx, 'workspace.Props.Crate.Anchored = not workspace.Props.Crate.Anchored; game.ServerScriptService.Main.Source = "return 9"')
    pulled = ctx.rbx("pl", timeout=120)
    history = pulled.get("historyId") or "latest"
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json == (not before_anchored), "store has the Studio value")
    ctx.expect(ctx.read(MAIN).strip() == "return 9", "file has the Studio source")
    restored = ctx.rbx("rev", "--sync", history, "--details", timeout=60)
    ctx.expect(restored.get("changedPathCount", 0) >= 2, f"undo restored: {restored.json}")
    ctx.expect(ctx.read(MAIN) == before_main, "Main restored")
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json == before_anchored, "store restored")
    again = ctx.rbx("rev", "--sync", str(restored.get("historyId") or history), "--details", ok=None, timeout=60)
    ctx.expect(not again.ok or again.get("changedPathCount", 0) == 0, f"the same pull undone twice changed files again: {again.json}")
    ctx.expect(ctx.read(MAIN) == before_main, "Main still restored after the second undo")


def pull_refuses_when_a_file_changes_during_export(ctx: Context):
    """SYNC-061: a file rewritten while the export runs makes the pull refuse instead of publishing partially."""
    ctx.open_studio()
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 61"')
    writer = _writer(ctx, MAIN, 'print("main")\nlocal N = {n}\n', 0.03, 8)
    try:
        time.sleep(0.5)
        result = ctx.rbx("pl", ok=None, timeout=120)
    finally:
        writer.wait(timeout=30)
    ctx.expect(not result.ok, f"pull succeeded while a file was being rewritten: {result.json}")
    ctx.expect_in("changed", result.error, "concurrent change error")
    ctx.expect_in("local N =", ctx.read(MAIN), "the writer's file survived")


def pull_undo_refused_after_a_newer_edit(ctx: Context):
    """SYNC-062: a file edited after the pull wrote it is not restored."""
    ctx.open_studio()
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 62"')
    ctx.rbx("pl", timeout=120)
    ctx.write(MAIN, "return 63\n")
    refused = ctx.rbx("rev", "--sync", "latest", ok=False, timeout=60)
    ctx.expect_in("newer", refused.error.lower(), "newer-edits refusal")
    ctx.expect(ctx.read(MAIN) == "return 63\n", "the newer edit survives")


def pull_with_live_sync_rebases_the_watcher(ctx: Context):
    """SYNC-063: a CLI pull while Live Sync runs leaves nothing pending and pushes nothing back."""
    _live(ctx)
    pushes, pulls = _counts(ctx)
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "pulled by cli"')
    ctx.rbx("pl", timeout=120)
    status = _settled(ctx)
    ctx.expect(status.pointer("/daemon/paused") is not True, f"still paused: {status.json}")
    after_pushes, _ = _counts(ctx)
    ctx.expect(after_pushes == pushes, f"the pulled files were pushed back: {pushes} -> {after_pushes}")
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "pulled by cli", "store has the pulled value")


def pull_keeps_a_blocked_store_pending(ctx: Context):
    """SYNC-064 (Oct 30 incident): a Live Sync pull must not overwrite a store whose push is blocked;
    once the blocker is removed both sides merge."""
    _live(ctx)
    _blocked_push(ctx)
    _studio_edit(ctx, "workspace.Props.Crate.Anchored = true")
    time.sleep(8)
    status = ctx.rbx("lst", ok=None, timeout=60)
    ctx.expect(ctx.rbx("bg", "Workspace", BAD, "ClassName", ok=None).json == BAD_CLASS, "the blocked store edit was overwritten by the pull")
    ctx.expect(_error_of(status), f"blocked push error vanished: {status.json}")
    ctx.rbx("br", "Workspace", BAD)
    _settled(ctx, 90)
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json is True, "Studio's anchor reached the store")
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True, "Studio keeps its anchor")


def pull_during_play_targets_edit(ctx: Context):
    """SYNC-065: a pull during Play exports the Edit runtime."""
    ctx.open_studio()
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "edit side"')
    ctx.rbx("play", "-s", timeout=180)
    try:
        ctx.rbx("pl", timeout=120)
    finally:
        ctx.rbx("play", "-x", ok=None, timeout=120)
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "edit side", "pull read the Edit runtime")


# ---------------------------------------------------------------- G. status


def lst_wait_names_the_unsettled_reason(ctx: Context):
    """SYNC-071: lst --wait on a blocked session fails and names why it is not settled."""
    _live(ctx)
    _blocked_push(ctx)
    result = ctx.rbx("lst", "--wait", "5", ok=False, timeout=60)
    ctx.expect(result.pointer("/daemon/settled") is not True, f"settled despite the blocked push: {result.json}")
    ctx.expect(result.pointer("/daemon/unsettled") or result.error, f"no reason given: {result.json}")


def lof_reports_the_previous_error(ctx: Context):
    """SYNC-073: lof stops the session and reports the error it was stopped with."""
    _live(ctx)
    _blocked_push(ctx)
    stopped = ctx.rbx("lof", timeout=60)
    ctx.expect(stopped.pointer("/daemon/running") is False, f"lof: {stopped.json}")
    ctx.expect(stopped.pointer("/daemon/previousError"), f"previousError missing: {stopped.json}")
    ctx.expect(not (ctx.root / ".renium" / "live-watch-state.enabled").exists(), "marker file removed by lof")


def status_commands_reject_lon_only_flags(ctx: Context):
    """SYNC-074 (0.5.7): lst, lof and the pending commands take only their own flags."""
    ctx.open_studio()
    for args in (["lst", "--prefer", "studio"], ["lof", "--prefer", "editor"], ["lst", "--clear-pending"]):
        result = ctx.rbx(*args, ok=None, timeout=30)
        ctx.expect(result.code != 0, f"{' '.join(args)} was accepted")


def second_lon_reattaches(ctx: Context):
    """SYNC-075: lon on a running session re-attaches without a second reconcile."""
    _live(ctx)
    pushes, pulls = _counts(ctx)
    again = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(again.pointer("/daemon/running") is True, f"second lon: {again.json}")
    ctx.expect(_counts(ctx) == (pushes, pulls), "second lon changed the push or pull counts")


def lon_refused_from_another_project_on_the_same_place(ctx: Context):
    """SYNC-076: a second project folder cannot start Live Sync on a Studio another project owns."""
    _live(ctx)
    other = ctx.root.parent / f"{ctx.root.name}-other"
    shutil.rmtree(other, ignore_errors=True)
    shutil.copytree(ctx.root, other, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
    try:
        refused = ctx.rbx("lon", ok=False, timeout=120, cwd=other)
        ctx.expect_in("owned", refused.error.lower(), "ownership refusal")
        mine = ctx.rbx("lst", timeout=60)
        ctx.expect(mine.pointer("/daemon/running") is True, "the owning project lost its session")
    finally:
        shutil.rmtree(other, ignore_errors=True)


def commands_report_a_pending_sync_review(ctx: Context):
    """SYNC-078: while a conflict needs a decision, every command result carries the sync review notice."""
    _conflict_on_main(ctx)
    result = ctx.rbx("l", "return 1", timeout=60)
    ctx.expect(result.get("syncReview"), f"no syncReview notice: {result.json}")


def status_shows_pushes_paused_in_verify_hold(ctx: Context):
    """SYNC-079: status reports the verify hold as pushesPaused."""
    _verify_hold(ctx)
    status = ctx.rbx("status", json_mode=True, timeout=60)
    ctx.expect(status.pointer("/liveSync/pushesPaused") is True, f"status: {status.json}")


# ---------------------------------------------------------------- H. reconcile


def same_script_on_both_sides_is_a_conflict(ctx: Context):
    """SYNC-084: a script edited on both sides is recorded as a conflict; neither side is overwritten."""
    _conflict_on_main(ctx)


def prefer_editor_resolves_toward_files(ctx: Context):
    """SYNC-085: lon --prefer editor applies the file side and clears the conflict."""
    _conflict_on_main(ctx)
    resolved = ctx.rbx("lon", "--prefer", "editor", "--wait", "60", timeout=200)
    ctx.expect("both sides" not in resolved.error, f"still unresolved: {resolved.json}")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main").strip().endswith("return 2"), "Studio has the file side")


def prefer_studio_writes_the_file_with_undo(ctx: Context):
    """SYNC-086 (Oct 30 incident): lon --prefer studio writes the Studio side into the file, records an
    undo entry, and rev --sync latest restores the file and pushes it back."""
    _conflict_on_main(ctx)
    resolved = ctx.rbx("lon", "--prefer", "studio", "--wait", "60", timeout=200)
    ctx.expect("both sides" not in resolved.error, f"still unresolved: {resolved.json}")
    ctx.expect(ctx.read(MAIN).strip() == "return 3", "file has the Studio side")
    restored = ctx.rbx("rev", "--sync", "latest", timeout=60)
    ctx.expect(restored.ok, f"undo: {restored.error[:200]}")
    ctx.expect(ctx.read(MAIN).strip().endswith("return 2"), "file restored")
    _settled(ctx)
    ctx.expect(_source(ctx, "game.ServerScriptService.Main").strip().endswith("return 2"), "restore pushed back to Studio")


def scoped_push_resolves_a_recorded_conflict(ctx: Context):
    """SYNC-087 (9912d13): a push of the conflicting path clears that conflict."""
    _conflict_on_main(ctx)
    pushed = ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect(pushed.ok, f"scoped push: {pushed.error[:200]}")
    details = ctx.rbx("lst", "--details", ok=None, timeout=60)
    ctx.expect("both sides" not in details.error and "unresolved" not in details.error.lower(), f"conflict still listed: {details.error[:400]}")


def verify_priority_without_differences_continues_in_reconcile(ctx: Context):
    """SYNC-088: verify priority with equal sides continues in reconcile mode."""
    ctx.open_studio()
    ctx.rbx("cfg", "set", "liveSync.initialSyncPriority", "verify")
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/mode") == "reconcile", f"mode: {started.json}")


def _verify_hold(ctx: Context):
    """A verified pair keeps reconcile on a plain restart; the hold applies when
    the Studio runtime is replaced (a reopen) with differences on both sides."""
    _live(ctx)
    ctx.rbx("lof")
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "studio 89"')
    ctx.close_studio(save=True)
    ctx.write(MAIN, 'print("main")\nreturn 89\n')
    ctx.rbx("cfg", "set", "liveSync.initialSyncPriority", "verify")
    ctx.reopen_studio()
    started = ctx.rbx("lon", ok=None, timeout=200)
    time.sleep(3)
    details = ctx.rbx("lst", "--details", ok=None, timeout=120)
    ctx.expect(details.pointer("/daemon/mode") == "verify" or "Sync needs review" in details.error, f"not in verify mode: {started.json} / {details.json} / {details.error[:200]}")
    return details


def verify_priority_with_differences_holds_both_directions(ctx: Context):
    """SYNC-089: verify priority with differences lists the paths each direction would move and pushes nothing."""
    details = _verify_hold(ctx)
    text = json.dumps(details.json) + details.error
    ctx.expect_in("push: src", text, "hold lists the file side")
    ctx.expect_in("pull: instances", text, "hold lists the Studio side")
    ctx.expect("both sides" not in text, "a hold is not a conflict")
    ctx.write(UTIL, "return { version = 89 }\n")
    time.sleep(6)
    ctx.expect("version = 89" not in _source(ctx, "game.ReplicatedStorage.Shared.Util"), "a file edit was pushed during the hold")
    ctx.expect("return 89" not in _source(ctx, "game.ServerScriptService.Main"), "the held push was applied")


def cli_push_during_verify_hold(ctx: Context):
    """SYNC-090: a CLI push is not held by verify mode."""
    _verify_hold(ctx)
    ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect_in("return 89", _source(ctx, "game.ServerScriptService.Main"), "CLI push applied during the hold")


def reopen_older_place_in_reconcile_priority(ctx: Context):
    """SYNC-092: a place file reopened older than the files with reconcile priority (live result on
    both platforms, 2026-11-01): the files are the newer side and Studio is brought up to them."""
    _live(ctx)
    ctx.write(UTIL, "return { version = 1 }\n")
    _settled(ctx)
    ctx.close_studio(save=True)
    shutil.copy2(ctx.root / ctx.place, ctx.root / "older.rbxl.bak")
    ctx.reopen_studio()
    _settled(ctx, 90)
    ctx.write(UTIL, "return { version = 2 }\n")
    _settled(ctx)
    ctx.close_studio(save=True)
    shutil.copy2(ctx.root / "older.rbxl.bak", ctx.root / ctx.place)
    ctx.reopen_studio()
    settled = ctx.rbx("lst", "--wait", "90", ok=None, timeout=200)
    ctx.expect_in("version = 2", ctx.read(UTIL), f"the newer file was overwritten by the older place: {settled.json}")
    ctx.expect("both sides" not in settled.error, f"reopen produced a conflict: {settled.error[:200]}")
    ctx.expect_in("version = 2", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Studio brought up to the files")


def configuration_change_keeps_the_baseline(ctx: Context):
    """SYNC-096: a project configuration change forces a full capture but creates no conflicts."""
    _live(ctx)
    manifest = ctx.read("renium.project.jsonc")
    ctx.write("renium.project.jsonc", manifest.rstrip() + "\n// touched by SYNC-096\n")
    status = _settled(ctx, 90)
    ctx.expect(status.pointer("/daemon/resolutionRequired") is not True and not status.pointer("/daemon/error"), f"after the manifest edit: {status.json}")


def missing_baseline_packs_rebuild_the_pairing(ctx: Context):
    """SYNC-097 (4af8ce1): deleted baseline packs make the next start pair fresh instead of failing."""
    _live(ctx)
    ctx.rbx("lof")
    reconcile = ctx.root / ".renium" / "reconcile"
    packs = [p for p in reconcile.iterdir() if p.is_dir()]
    ctx.expect(packs, f"no baseline pack directory under {reconcile}")
    for pack in packs:
        shutil.rmtree(pack)
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True and not started.pointer("/daemon/error"), f"fresh pairing: {started.json}")


# ---------------------------------------------------------------- I. worker pushes


def deleted_script_file_removes_the_instance(ctx: Context):
    """SYNC-102: deleting a script file removes its instance from Studio."""
    _live(ctx)
    (ctx.root / UTIL).unlink()
    _settled(ctx)
    ctx.expect(ctx.luau('return game.ReplicatedStorage.Shared:FindFirstChild("Util") == nil') is True, "Util removed in Studio")


def blocked_path_keeps_other_pushes_flowing(ctx: Context):
    """SYNC-103: a non-retryable failure blocks its path and reports it while other files still push."""
    _live(ctx)
    status = _blocked_push(ctx)
    ctx.expect_in(BAD_CLASS, _error_of(status), "error names the rejected class")
    ctx.write(MAIN, 'print("main")\nreturn 103\n')
    deadline = time.monotonic() + 40
    while time.monotonic() < deadline and "return 103" not in _source(ctx, "game.ServerScriptService.Main"):
        time.sleep(2)
    ctx.expect_in("return 103", _source(ctx, "game.ServerScriptService.Main"), "Main pushed despite the blocked store")
    after = ctx.rbx("lst", ok=None, timeout=60)
    ctx.expect(_error_of(after), f"the blocked store's error must persist while Main flows: {after.json}")


def retry_after_the_fix_clears_the_block(ctx: Context):
    """SYNC-104: rp after removing the bad instance clears the error and empties the queue."""
    _live(ctx)
    _blocked_push(ctx)
    ctx.rbx("br", "Workspace", BAD)
    ctx.rbx("rp", timeout=60)
    status = _settled(ctx, 90)
    ctx.expect(not status.pointer("/daemon/error"), f"error not cleared: {status.json}")


def discard_pending_keeps_studio_edits_pullable(ctx: Context):
    """SYNC-105 (sync.md: dp discards failed file edits): after dp the blocked file edit is gone and a
    Studio edit made just before dp is still pulled (D4 decides)."""
    _live(ctx)
    _blocked_push(ctx)
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "before dp"')
    ctx.rbx("dp", timeout=60)
    status = ctx.rbx("lst", ok=None, timeout=60)
    ctx.expect(status.pointer("/daemon/pendingCount", 0) == 0 and not status.pointer("/daemon/error"), f"after dp: {status.json}")
    ctx.expect(ctx.luau('return workspace.Props:FindFirstChild("Bad") == nil') is True, "the rejected instance never reached Studio")
    _settled(ctx, 30)
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "before dp", "the Studio edit made before dp was pulled (D4)")


def edits_during_a_push_settle_on_the_last_content(ctx: Context):
    """SYNC-106: a file rewritten every 100 ms for 5 s ends with Studio holding the last content and no error."""
    _live(ctx)
    writer = _writer(ctx, MAIN, 'print("main")\nlocal N = {n}\n', 0.1, 5)
    count = int(writer.communicate(timeout=60)[0].strip())
    status = _settled(ctx, 60)
    ctx.expect(not status.pointer("/daemon/error"), f"error after the write storm: {status.json}")
    ctx.expect_in(f"local N = {count}", _source(ctx, "game.ServerScriptService.Main"), "Studio has the last content")


def concurrent_edits_in_one_service_merge(ctx: Context):
    """SYNC-107: a Studio edit and a store edit of the same service within a second both survive."""
    _live(ctx)
    _studio_edit(ctx, "workspace.Props.Crate.Anchored = true")
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "both")
    _settled(ctx, 90)
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "both", "store edit reached Studio")
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True, "Studio edit kept")
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json is True, "Studio edit reached the store")
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "both", "store edit kept")


def two_agents_editing_files_under_live_sync(ctx: Context):
    """SYNC-108 (false-conflict incident): two file writers and a Studio editor for 15 s end settled
    with no conflict and Studio holding the final files."""
    _live(ctx)
    env = dict(os.environ, RENIUM_PLACE=str(ctx.root / ctx.place))
    a = _writer(ctx, MAIN, 'print("main")\nlocal A = {n}\n', 0.3, 15)
    b = _writer(ctx, UTIL, "return { b = {n} }\n", 0.3, 15)
    studio = subprocess.Popen([sys.executable, "-I", "-c", (
        "import subprocess,sys,time\n"
        "end=time.monotonic()+15; n=0\n"
        "while time.monotonic()<end:\n"
        "    n+=1\n"
        "    subprocess.run([sys.argv[1],'l','workspace.Props.Sign.Value = \"s%d\" return true' % n],cwd=sys.argv[2],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\n"
        "    time.sleep(1)\n"
    ), RBX, str(ctx.root)], env=env)
    na = int(a.communicate(timeout=60)[0].strip())
    nb = int(b.communicate(timeout=60)[0].strip())
    studio.wait(timeout=60)
    status = _settled(ctx, 90)
    ctx.expect(status.pointer("/daemon/resolutionRequired") is not True and not status.pointer("/daemon/error"), f"after the concurrent edits: {status.json}")
    ctx.expect_in(f"local A = {na}", _source(ctx, "game.ServerScriptService.Main"), "Studio Main")
    ctx.expect_in(f"b = {nb}", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Studio Util")


def two_pushes_at_once_through_one_daemon(ctx: Context):
    """SYNC-109: two CLI pushes started together both succeed."""
    ctx.open_studio()
    ctx.write(MAIN, 'print("main")\nreturn 109\n')
    ctx.write(UTIL, "return { version = 109 }\n")
    env = dict(os.environ, RENIUM_PLACE=str(ctx.root / ctx.place))
    first = subprocess.Popen([RBX, "--output-mode", "json", "ps", MAIN, "--yes"], cwd=ctx.root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    second = subprocess.Popen([RBX, "--output-mode", "json", "ps", UTIL, "--yes"], cwd=ctx.root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    outs = [p.communicate(timeout=180) for p in (first, second)]
    for (out, err), proc in zip(outs, (first, second)):
        ctx.expect(proc.returncode == 0, f"concurrent push failed: {out[-300:]} {err[-300:]}")
    ctx.expect_in("return 109", _source(ctx, "game.ServerScriptService.Main"), "Main")
    ctx.expect_in("version = 109", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util")


def same_named_siblings_push_and_pull(ctx: Context):
    """SYNC-111 (sibling-order incident): twelve same-named Models each holding its ordinal's count
    of same-named Parts push in one batch and round-trip through a pull."""
    ctx.open_studio()
    ops = [{"parent": "Workspace", "name": "Set", "class": "Folder", "ref": "set"}]
    for item in range(1, 13):
        ops.append({"parent": "#set", "name": "Item", "class": "Model", "ref": f"item{item}"})
        for _ in range(item):
            ops.append({"parent": f"#item{item}", "name": "Part", "class": "Part"})
    created = ctx.rbx("ba", "--batch", "-", stdin=json.dumps({"ops": ops}), timeout=120)
    ctx.expect(len(created.get("ops") or []) == len(ops), f"batch created {len(created.get('ops') or [])} of {len(ops)}")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=180)
    counts = ctx.luau("local t = {} for _, item in ipairs(workspace.Set:GetChildren()) do table.insert(t, #item:GetChildren()) end table.sort(t) return table.concat(t, ',')")
    ctx.expect(counts == ",".join(str(n) for n in range(1, 13)), f"Part counts per Item in Studio: {counts}")
    ctx.rbx("pl", timeout=120)
    found = ctx.rbx("f", "Workspace", "-n", "Item", "--all", "-l", "50")
    items = found.json if isinstance(found.json, list) else (found.get("matches") or [])
    ctx.expect(len(items) == 12, f"items after the pull: {len(items)}")


def manifest_edit_triggers_a_full_push(ctx: Context):
    """SYNC-113: editing the project manifest is a full-push path that settles without conflicts."""
    _live(ctx)
    manifest = ctx.read("renium.project.jsonc")
    ctx.write("renium.project.jsonc", manifest.rstrip() + "\n// full push\n")
    status = _settled(ctx, 90)
    ctx.expect(not status.pointer("/daemon/error") and status.pointer("/daemon/resolutionRequired") is not True, f"after the manifest edit: {status.json}")


def file_edit_during_play_reaches_edit_after_stop(ctx: Context):
    """SYNC-114 (sync.md: file edits during Play wait for Edit mode): an edit saved during Play is in
    the Edit runtime once Play stops, with no error."""
    _live(ctx)
    ctx.rbx("play", "-s", timeout=180)
    try:
        ctx.write(MAIN, 'print("main")\nreturn 114\n')
        time.sleep(5)
        during = ctx.rbx("lst", ok=None, timeout=60)
        ctx.expect(not during.pointer("/daemon/error"), f"error during Play: {during.json}")
    finally:
        ctx.rbx("play", "-x", ok=None, timeout=120)
    _settled(ctx, 90)
    ctx.expect_in("return 114", _source(ctx, "game.ServerScriptService.Main"), "Edit runtime after Play")


# ---------------------------------------------------------------- J. history


def revert_a_push_by_history_id(ctx: Context):
    """SYNC-122: rev --sync ID restores the files a push changed; a second revert is refused."""
    ctx.open_studio()
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "studio 122"')
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "file 122")
    pushed = ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    history_id = pushed.get("historyId")
    ctx.expect(history_id, f"no historyId: {pushed.json}")
    ctx.rbx("rev", "--sync", history_id, timeout=60)
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "studio 122", "store restored to the Studio side the push replaced")
    again = ctx.rbx("rev", "--sync", history_id, ok=False, timeout=60)
    ctx.expect_in("already", again.error.lower(), "second revert refused")


def revert_refuses_newer_edits(ctx: Context):
    """SYNC-123: rev --sync latest refuses when the restored file was edited after the sync."""
    _live(ctx)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 123"')
    _wait_file(ctx, MAIN, "return 123")
    # Live Sync off before the file edit: otherwise it may push the edit and
    # record a newer entry before the revert runs, and `latest` is that push.
    ctx.rbx("lof", timeout=60)
    ctx.write(MAIN, "return 124\n")
    refused = ctx.rbx("rev", "--sync", "latest", ok=False, timeout=60)
    ctx.expect_in("newer", refused.error.lower(), "newer-edits refusal")
    ctx.expect(ctx.read(MAIN) == "return 124\n", "the newer edit survives")


def revert_print_shows_the_saved_copy(ctx: Context):
    """SYNC-124: rev --path FILE --print writes the pre-sync bytes to stdout without restoring."""
    _live(ctx)
    before = ctx.read(MAIN)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 125"')
    _wait_file(ctx, MAIN, "return 125")
    printed = ctx.rbx("rev", "--path", MAIN, "--print", json_mode=False, timeout=60)
    ctx.expect(printed.stdout.replace("\r\n", "\n").strip() == before.strip(), f"printed: {printed.stdout[:200]!r}")
    ctx.expect("return 125" in ctx.read(MAIN), "--print must not restore")


def revert_apply_studio_without_live_sync(ctx: Context):
    """SYNC-125 (sync.md says to use --apply-studio when Live Sync is off): the restored file is
    also pushed to Studio (N3 decides)."""
    ctx.open_studio()
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "studio 125"')
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "file 125")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    restored = ctx.rbx("rev", "--sync", "latest", "--apply-studio", ok=None, timeout=120)
    ctx.expect(restored.ok, f"--apply-studio refused (N3): {restored.error[:300]}")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "studio 125", "Studio got the restored store value")


def rapid_studio_edits_settle_once(ctx: Context):
    """SYNC-126: ten quick Studio edits are pulled and the session settles with the last value stored."""
    _live(ctx)
    for n in range(10):
        _studio_edit(ctx, f'workspace.Props.Sign.Value = "rapid {n}"')
    status = _settled(ctx, 90)
    ctx.expect(not status.pointer("/daemon/error"), f"after rapid edits: {status.json}")
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "rapid 9", "last value stored")


def pull_failure_on_a_corrupt_store_resumes(ctx: Context):
    """SYNC-127 (a38aa4a): a pull that fails on an undecodable store reports the failure and resumes
    once the file is repaired and Studio changes again."""
    _live(ctx)
    good = (ctx.root / LIGHTING).read_bytes()
    (ctx.root / LIGHTING).write_bytes(b"not a store")
    _studio_edit(ctx, "game.Lighting.Brightness = 2")
    deadline = time.monotonic() + 30
    failed = None
    while time.monotonic() < deadline:
        failed = ctx.rbx("lst", ok=None, timeout=60)
        if _error_of(failed):
            break
        time.sleep(2)
    ctx.expect(failed is not None and _error_of(failed), f"no pull failure reported: {failed.json if failed else None}")
    (ctx.root / LIGHTING).write_bytes(good)
    _studio_edit(ctx, "game.Lighting.Brightness = 3")
    deadline = time.monotonic() + 90
    status = None
    while time.monotonic() < deadline:
        status = ctx.rbx("lst", ok=None, timeout=60)
        if (
            not _error_of(status)
            and status.get("busy") is not True
            and status.pointer("/daemon/syncing") is not True
            and status.pointer("/daemon/pendingCount", 0) == 0
            and status.get("pendingChanges", 0) == 0
        ):
            break
        time.sleep(3)
    ctx.expect(status is not None and not _error_of(status), f"error after the repair (the reconcile retries with backoff): {status.json if status else None}")
    ctx.expect(status.get("pendingChanges", 0) == 0, f"the Studio change was not pulled after the repair: {status.json}")
    ctx.expect(ctx.rbx("bg", "Lighting", "-p", "Brightness").json == 3, "pull resumed after the repair")


# ---------------------------------------------------------------- K. restart


def _stop_daemon(ctx: Context):
    ctx.rbx("dm", "stop", ok=None, timeout=60)
    time.sleep(1.5)


def daemon_restart_restores_live_sync(ctx: Context):
    """SYNC-130: after the daemon stops while idle, the next status restores Live Sync without conflicts."""
    _live(ctx)
    _stop_daemon(ctx)
    status = ctx.rbx("lst", "--wait", "60", timeout=120)
    ctx.expect(status.pointer("/daemon/running") is True, f"not restored: {status.json}")
    ctx.expect(status.pointer("/daemon/resolutionRequired") is not True and not status.pointer("/daemon/error"), f"restore outcome: {status.json}")
    ctx.expect((ctx.root / ".renium" / "live-watch-state.enabled").exists(), "marker kept across the restart")


def files_edited_during_a_daemon_outage_are_pushed(ctx: Context):
    """SYNC-131 (Oct 30 incident): file edits made while the daemon was down are pushed after the
    restore, not overwritten by Studio."""
    _live(ctx)
    _stop_daemon(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 131\n')
    ctx.write(UTIL, "return { version = 131 }\n")
    status = ctx.rbx("lst", "--wait", "90", timeout=200)
    ctx.expect(not status.pointer("/daemon/error"), f"after the restore: {status.json}")
    ctx.expect_in("return 131", ctx.read(MAIN), "Main file kept")
    ctx.expect_in("version = 131", ctx.read(UTIL), "Util file kept")
    ctx.expect_in("return 131", _source(ctx, "game.ServerScriptService.Main"), "Main pushed")
    ctx.expect_in("version = 131", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util pushed")


def any_command_restores_live_sync(ctx: Context):
    """SYNC-133: a store edit command after a daemon stop restores Live Sync first and its edit is pushed."""
    _live(ctx)
    _stop_daemon(ctx)
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "restored")
    status = ctx.rbx("lst", "--wait", "90", timeout=200)
    ctx.expect(status.pointer("/daemon/running") is True, f"not restored: {status.json}")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "restored", "edit pushed after the restore")


def studio_closed_while_live_sync_runs(ctx: Context):
    """SYNC-135: closing Studio under Live Sync makes status report the missing Studio; reopening restores."""
    _live(ctx)
    ctx.close_studio(save=True)
    missing = ctx.rbx("lst", ok=False, timeout=60)
    ctx.expect_in("studio", missing.error.lower(), "lst without Studio")
    ctx.expect((ctx.root / ".renium" / "live-watch-state.enabled").exists(), "marker kept while Studio is closed")
    ctx.reopen_studio()
    status = ctx.rbx("lst", "--wait", "90", timeout=200)
    ctx.expect(status.pointer("/daemon/running") is True and not status.pointer("/daemon/error"), f"after reopen: {status.json}")


def clean_restart_needs_no_capture(ctx: Context):
    """SYNC-137: a daemon restart after a settled session restores without conflicts or errors."""
    _live(ctx)
    _settled(ctx)
    _stop_daemon(ctx)
    status = ctx.rbx("lst", "--wait", "60", timeout=120)
    ctx.expect(status.pointer("/daemon/running") is True and not status.pointer("/daemon/error") and status.pointer("/daemon/resolutionRequired") is not True, f"restore: {status.json}")


def offline_file_edits_push_after_reopen(ctx: Context):
    """SYNC-138: files edited while Studio was closed are pushed when it reopens."""
    _live(ctx)
    ctx.close_studio(save=True)
    ctx.write(MAIN, 'print("main")\nreturn 138\n')
    ctx.reopen_studio()
    _settled(ctx, 90)
    ctx.expect_in("return 138", _source(ctx, "game.ServerScriptService.Main"), "offline edit pushed")


def _pair_reset(ctx: Context):
    """H11: forget the pairing so the next lon is a first pairing."""
    import shutil
    ctx.rbx("lof", ok=None, timeout=60)
    shutil.rmtree(ctx.root / ".renium" / "reconcile", ignore_errors=True)
    marker = ctx.root / ".renium" / "live-watch-state.enabled"
    if marker.exists():
        marker.unlink()


def first_pairing_same_file_difference_is_a_conflict(ctx: Context):
    """SYNC-081: with no baseline, a script that differs on both sides is a conflict and nothing is written."""
    ctx.open_studio()
    ctx.write(MAIN, 'print("main")\nreturn 81\n')
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 82"')
    started = ctx.rbx("lon", ok=False, timeout=200)
    ctx.expect_in("both sides", started.error, "first pairing reports the conflict")
    ctx.expect("return 81" in ctx.read(MAIN) and _source(ctx, "game.ServerScriptService.Main") == "return 82", "neither side written")


def first_pairing_preference_resolves(ctx: Context):
    """SYNC-082: liveSync.initialConflictPreference editor/studio resolves a first-pairing difference
    without a conflict; the studio preference writes the file with a sync history entry."""
    ctx.open_studio()
    ctx.rbx("cfg", "set", "liveSync.initialConflictPreference", "editor", timeout=60)
    ctx.write(MAIN, 'print("main")\nreturn 81\n')
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 82"')
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect("both sides" not in started.error, f"editor preference left a conflict: {started.error[:200]}")
    ctx.expect_in("return 81", _source(ctx, "game.ServerScriptService.Main"), "Studio took the file side")
    _pair_reset(ctx)
    ctx.rbx("cfg", "set", "liveSync.initialConflictPreference", "studio", timeout=60)
    ctx.write(MAIN, 'print("main")\nreturn 83\n')
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "return 84"')
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect("both sides" not in started.error, f"studio preference left a conflict: {started.error[:200]}")
    ctx.expect_in("return 84", ctx.read(MAIN), "file took the Studio side")
    printed = ctx.rbx("rev", "--path", MAIN, "--print", ok=None, timeout=60)
    ctx.expect(printed.code == 0 and "return 83" in printed.stdout, "the losing file side is in sync history")


def large_batch_under_live_sync(ctx: Context):
    """SYNC-112: a 6,000-instance batch written under Live Sync settles in one push with the right counts."""
    _live(ctx)
    ops = [{"parent": "Workspace", "name": "Big", "class": "Folder", "ref": "big"}]
    for item in range(1, 61):
        ops.append({"parent": "#big", "name": "Item", "class": "Model", "ref": f"i{item}"})
        for _ in range(100):
            ops.append({"parent": f"#i{item}", "name": "Part", "class": "Part"})
    created = ctx.rbx("ba", "--batch", "-", stdin=json.dumps({"ops": ops}), timeout=300)
    ctx.expect(len(created.get("ops") or []) == len(ops), f"batch created {len(created.get('ops') or [])}")
    status = ctx.rbx("lst", "--wait", "120", timeout=300)
    ctx.expect(status.pointer("/daemon/pendingCount", 0) == 0 and not _error_of(status), f"after the batch: {status.json}")
    ctx.expect(ctx.luau("local n = 0 for _, m in ipairs(workspace.Big:GetChildren()) do n += #m:GetChildren() end return n") == 6000, "6,000 parts in Studio")


def full_push_rewrites_script_and_recreates_instance(ctx: Context):
    """SYNC-139 (D12): one full push both recreates a destroyed instance and rewrites a script edited in Studio."""
    ctx.open_studio()
    _studio_edit(ctx, 'workspace.Props.Sign:Destroy() game.ServerScriptService.Main.Source = "print(2)"')
    pushed = ctx.rbx("ps", "--yes", timeout=120)
    ctx.expect(pushed.pointer("/scripts/updated") == 1 and pushed.pointer("/instances/created") == 1, f"full push: {pushed.json}")
    ctx.expect_in('print("main")', _source(ctx, "game.ServerScriptService.Main"), "script rewritten")
    ctx.expect(ctx.luau("return workspace.Props:FindFirstChild('Sign') ~= nil") is True, "Sign recreated")


def staged_push_reviews_against_the_project_threshold(ctx: Context):
    """SYNC-258: a full push without --yes stages the project in a private copy; the review threshold comes from
    the project's place configuration, not from that copy, so threshold 0 holds a one-change push (it applies
    nothing and answers heldForReview with its change list) while the plugin keeps its default threshold of 5."""
    ctx.open_studio()
    ctx.rbx("cfg", "set", "liveSync.changesThreshold", "0", timeout=60)
    try:
        ctx.write(MAIN, ctx.read(MAIN).rstrip() + "\n-- staged review\n")
        held = ctx.rbx("ps", ok=None, timeout=120)
        ctx.expect(held.get("heldForReview") is True and held.get("effectiveChanges") == 1, f"a one-change staged push with the project threshold at 0 was not held: {held.json} stderr={held.stderr[-300:]!r}")
        ctx.expect("Main" in json.dumps(held.get("review")), f"the change list names Main: {held.json}")
        ctx.expect("-- staged review" not in _source(ctx, "game.ServerScriptService.Main"), "Studio Main unchanged while the push is held")
    finally:
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "5", ok=None, timeout=60)


def chunked_review_honors_the_project_threshold(ctx: Context):
    """SYNC-259: a review payload over 1 MiB reaches the plugin in chunks and still applies the project's
    liveSync.changesThreshold: with the project at 100000 the push applies without a hold;
    with the project at 0 the CLI returns a held review and leaves Studio unchanged. A subsequent
    rename and pull keeps every imported instance's store identity."""
    from areas.data_live import _review_threshold_zero

    count = 6000
    # Studio truncates Name to 100 bytes. Keep these unique ASCII names within
    # that limit so the identity receipt can verify the actual imported tree.
    names = [f"Part{index:05d}".ljust(100, "x") for index in range(count)]
    ops = [{"parent": "Workspace", "name": "Bulk", "class": "Folder", "ref": "bulk"}]
    ops += [{"parent": "#bulk", "name": name, "class": "Part"} for name in names]
    with _review_threshold_zero(ctx):
        created = ctx.rbx("ba", "--batch", "-", stdin=json.dumps({"ops": ops}), timeout=600)
        ctx.expect(len(created.get("ops") or []) == len(ops), f"batch created {len(created.get('ops') or [])} of {len(ops)}")
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "100000", timeout=60)
        started = time.monotonic()
        pushed = ctx.rbx("ps", WORKSPACE, "-u", ok=None, timeout=600)
        ctx.expect(pushed.ok and pushed.get("heldForReview") is not True and pushed.get("skippedByReview") is not True and "held for review" not in pushed.stderr and time.monotonic() - started < 85, f"a chunked review with the project threshold above the change count was held or skipped: {pushed.json} stderr={pushed.stderr[-300:]!r}")
        # The daemon may still be verifying the 6000-instance push; a request
        # that finds it busy is answered "5s limit", so the count is polled.
        deadline = time.monotonic() + 90
        while True:
            counted = ctx.rbx("l", "return #workspace.Bulk:GetChildren()", "-t", "60", ok=None, timeout=120)
            if counted.ok or time.monotonic() > deadline:
                break
            time.sleep(3)
        ctx.expect(counted.ok and (counted.get("results") or [None])[0] == count, f"every Part reached Studio: {counted.json or counted.error[:200]}")
        ctx.rbx("l", "workspace.Bulk:Destroy() return true", "-t", "60", timeout=120)
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "0", timeout=60)
        held = ctx.rbx("ps", WORKSPACE, "-u", timeout=600)
        ctx.expect(held.get("heldForReview") is True and held.get("reviewInstances", 0) >= count,
                   f"a chunked review with the project threshold at 0 was not held: {held.json}")
        ctx.expect(ctx.luau("return workspace:FindFirstChild('Bulk') == nil") is True, "Studio unchanged by the held review")
        # Check the import identity round trip after the review checks: a pull
        # also captures engine defaults, which would change their payload.
        ctx.rbx("cfg", "set", "liveSync.changesThreshold", "100000", timeout=60)
        ctx.rbx("ps", WORKSPACE, "-u", timeout=600)
        renamed = count // 2
        ctx.luau(f'workspace.Bulk[{json.dumps(names[renamed])}].Name = "RenamedPart" return true')
        ctx.rbx("pl", "-s", "Workspace", timeout=600)
        expected_ids = {op["id"]: op["path"].rsplit(".", 1)[-1] for op in created["ops"]}
        expected_ids[created["ops"][renamed + 1]["id"]] = "RenamedPart"
        pulled = ctx.rbx("f", "Workspace", "--path", "Bulk", "--all", timeout=60).json
        ctx.expect(len(pulled) == len(expected_ids) and {row["id"]: row["n"] for row in pulled} == expected_ids,
                   "every imported instance keeps its store id after a Studio rename and pull")


def _names_viewport_camera(ctx: Context) -> bool:
    """Whether the Workspace store's root holds a CurrentCamera reference to an instance; a store without
    the field answers the class default, an empty reference."""
    value = ctx.rbx("bg", "Workspace", "-p", "CurrentCamera", ok=None).json
    return isinstance(value, dict) and value.get("ok") is not False and bool(set(value) - {"_type"})


def legacy_store_keeps_the_viewport_camera_on_a_pull(ctx: Context):
    """SYNC-260: a store that names no Workspace.CurrentCamera still keeps its viewport camera row on a
    Live Sync pull: a Studio edit of the camera's FieldOfView is not pulled while an unrelated Studio edit
    is, and the pulled store then names the camera. bs refuses every edit of CurrentCamera, so the store
    comes from an import of the saved place, which leaves the engine-managed reference out and keeps the
    identities of the store it replaces."""
    ctx.open_studio()
    if _names_viewport_camera(ctx):
        ctx.rbx("pi", ctx.place, "-s", "Workspace", timeout=120)
        ctx.expect(not _names_viewport_camera(ctx), f"precondition: the store names no CurrentCamera (the place import kept it: {ctx.rbx('bg', 'Workspace', '-p', 'CurrentCamera', ok=None).json})")
    before = ctx.rbx("bg", "Workspace", "Camera", "FieldOfView").json
    ctx.rbx("lon", "--wait", "60", timeout=200)
    _studio_edit(ctx, "workspace.CurrentCamera.FieldOfView = 60 workspace.Props.Crate.Anchored = false")
    _settled(ctx)
    ctx.expect(ctx.rbx("bg", "Workspace", "Camera", "FieldOfView").json == before, "the viewport camera keeps the store's FieldOfView")
    ctx.expect(ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored").json is False, "the unrelated Studio edit is pulled")
    ctx.expect(_names_viewport_camera(ctx), "the pulled store names the viewport camera")


SCENARIOS = [
    Scenario("SYNC-060", "pull replaces files and records undo", "small", pull_replaces_files_and_records_undo),
    Scenario("SYNC-061", "pull refuses when a file changes during export", "small", pull_refuses_when_a_file_changes_during_export),
    Scenario("SYNC-062", "pull undo refused after a newer edit", "small", pull_undo_refused_after_a_newer_edit),
    Scenario("SYNC-063", "pull with Live Sync rebases the watcher", "small", pull_with_live_sync_rebases_the_watcher),
    Scenario("SYNC-064", "pull keeps a blocked store pending", "small", pull_keeps_a_blocked_store_pending),
    Scenario("SYNC-065", "pull during Play targets Edit", "small", pull_during_play_targets_edit),
    Scenario("SYNC-071", "lst --wait names the unsettled reason", "small", lst_wait_names_the_unsettled_reason),
    Scenario("SYNC-073", "lof reports the previous error", "small", lof_reports_the_previous_error),
    Scenario("SYNC-074", "status commands reject lon-only flags", "small", status_commands_reject_lon_only_flags),
    Scenario("SYNC-075", "second lon re-attaches", "small", second_lon_reattaches),
    Scenario("SYNC-076", "lon refused from another project on the same place", "small", lon_refused_from_another_project_on_the_same_place),
    Scenario("SYNC-078", "commands report a pending sync review", "small", commands_report_a_pending_sync_review),
    Scenario("SYNC-079", "status shows pushesPaused in a verify hold", "small", status_shows_pushes_paused_in_verify_hold),
    Scenario("SYNC-084", "same script on both sides is a conflict", "small", same_script_on_both_sides_is_a_conflict),
    Scenario("SYNC-085", "lon --prefer editor resolves toward files", "small", prefer_editor_resolves_toward_files),
    Scenario("SYNC-086", "lon --prefer studio writes the file with undo", "small", prefer_studio_writes_the_file_with_undo),
    Scenario("SYNC-087", "scoped push resolves a recorded conflict", "small", scoped_push_resolves_a_recorded_conflict),
    Scenario("SYNC-088", "verify priority without differences continues in reconcile", "small", verify_priority_without_differences_continues_in_reconcile),
    Scenario("SYNC-089", "verify priority with differences holds both directions", "small", verify_priority_with_differences_holds_both_directions),
    Scenario("SYNC-090", "CLI push during a verify hold", "small", cli_push_during_verify_hold),
    Scenario("SYNC-092", "reopen an older place in reconcile priority", "small", reopen_older_place_in_reconcile_priority),
    Scenario("SYNC-096", "configuration change keeps the baseline", "small", configuration_change_keeps_the_baseline),
    Scenario("SYNC-097", "missing baseline packs rebuild the pairing", "small", missing_baseline_packs_rebuild_the_pairing),
    Scenario("SYNC-102", "deleted script file removes the instance", "small", deleted_script_file_removes_the_instance),
    Scenario("SYNC-103", "blocked path keeps other pushes flowing", "small", blocked_path_keeps_other_pushes_flowing),
    Scenario("SYNC-104", "rp after the fix clears the block", "small", retry_after_the_fix_clears_the_block),
    Scenario("SYNC-105", "dp discards file edits and keeps Studio edits pullable", "small", discard_pending_keeps_studio_edits_pullable),
    Scenario("SYNC-106", "edits during a push settle on the last content", "small", edits_during_a_push_settle_on_the_last_content),
    Scenario("SYNC-107", "concurrent edits in one service merge", "small", concurrent_edits_in_one_service_merge),
    Scenario("SYNC-108", "two agents editing files under Live Sync", "small", two_agents_editing_files_under_live_sync),
    Scenario("SYNC-109", "two pushes at once through one daemon", "small", two_pushes_at_once_through_one_daemon, default_daemon=True),
    Scenario("SYNC-111", "same-named siblings push and pull", "small", same_named_siblings_push_and_pull),
    Scenario("SYNC-113", "manifest edit triggers a full push", "small", manifest_edit_triggers_a_full_push),
    Scenario("SYNC-114", "file edit during Play reaches Edit after stop", "small", file_edit_during_play_reaches_edit_after_stop),
    Scenario("SYNC-122", "revert a push by history id", "small", revert_a_push_by_history_id),
    Scenario("SYNC-123", "revert refuses newer edits", "small", revert_refuses_newer_edits),
    Scenario("SYNC-124", "rev --print shows the saved copy", "small", revert_print_shows_the_saved_copy),
    Scenario("SYNC-125", "rev --apply-studio without Live Sync", "small", revert_apply_studio_without_live_sync),
    Scenario("SYNC-126", "rapid Studio edits settle once", "small", rapid_studio_edits_settle_once),
    Scenario("SYNC-127", "pull failure on a corrupt store resumes", "small", pull_failure_on_a_corrupt_store_resumes),
    Scenario("SYNC-130", "daemon restart restores Live Sync", "small", daemon_restart_restores_live_sync, default_daemon=True),
    Scenario("SYNC-131", "files edited during a daemon outage are pushed", "small", files_edited_during_a_daemon_outage_are_pushed, default_daemon=True),
    Scenario("SYNC-133", "any command restores Live Sync", "small", any_command_restores_live_sync, default_daemon=True),
    Scenario("SYNC-135", "Studio closed while Live Sync runs", "small", studio_closed_while_live_sync_runs),
    Scenario("SYNC-137", "clean restart needs no capture", "small", clean_restart_needs_no_capture, default_daemon=True),
    Scenario("SYNC-138", "offline file edits push after reopen", "small", offline_file_edits_push_after_reopen),
    Scenario("SYNC-081", "first pairing same-file difference is a conflict", "small", first_pairing_same_file_difference_is_a_conflict),
    Scenario("SYNC-082", "first pairing preference resolves", "small", first_pairing_preference_resolves),
    Scenario("SYNC-112", "large batch under Live Sync", "small", large_batch_under_live_sync),
    Scenario("SYNC-139", "full push rewrites a script and recreates an instance", "small", full_push_rewrites_script_and_recreates_instance),
    Scenario("SYNC-258", "staged push reads the project's threshold", "small", staged_push_reviews_against_the_project_threshold),
    Scenario("SYNC-259", "chunked review honors the project's threshold", "small", chunked_review_honors_the_project_threshold, exclusive=True),
    Scenario("SYNC-260", "legacy store keeps the viewport camera on a pull", "small", legacy_store_keeps_the_viewport_camera_on_a_pull),
]
