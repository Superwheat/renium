"""Synchronization scenarios: push, pull, Live Sync, reconcile, sync history.

Row ids follow scenarios/matrix/sync.md. Every row constructs a situation that
has occurred (an incident, or a code path with the steps that reach it) and
asserts the behavior the feature promises. Fixture: Workspace.Props (Folder,
editor:1) holding Crate (Part, editor:2), Crate[2] (editor:3) and Sign
(StringValue, editor:4); ServerScriptService.Main; ReplicatedStorage.Shared.Util;
StarterPlayerScripts.Client.
"""

from __future__ import annotations

import json
import os
import sys
import subprocess
import time

from run import RBX, Context, Scenario, ScenarioFailure

MAIN = "src/ServerScriptService/Main.server.luau"
UTIL = "src/ReplicatedStorage/Shared/Util.luau"
CLIENT = "src/StarterPlayer/StarterPlayerScripts/Client.client.luau"
WORKSPACE = "instances/Workspace.renium"


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


# ---------------------------------------------------------------- A. ps selection


def ps_one_script_path(ctx: Context):
    """SYNC-001: ps FILE pushes that script alone; other scripts stay as Studio has them."""
    ctx.open_studio()
    _studio_edit(ctx, 'game.ReplicatedStorage.Shared.Util.Source = "return { studio = true }"')
    ctx.write(MAIN, 'print("main")\nreturn 2\n')
    pushed = ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect(pushed.pointer("/scripts/updated") == 1, f"push result: {pushed.json}")
    ctx.expect_in("return 2", _source(ctx, "game.ServerScriptService.Main"), "Main in Studio")
    ctx.expect_in("studio = true", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util untouched by a scoped push")


def ps_directory(ctx: Context):
    """SYNC-002: ps DIR pushes every changed script below it."""
    ctx.open_studio()
    ctx.write(MAIN, 'print("main")\nreturn 3\n')
    ctx.write(UTIL, "return { version = 3 }\n")
    ctx.rbx("ps", "src", "--yes", timeout=120)
    ctx.expect_in("return 3", _source(ctx, "game.ServerScriptService.Main"), "Main")
    ctx.expect_in("version = 3", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util")


def ps_repeated_p_and_list_file(ctx: Context):
    """SYNC-003: -p repeated and -f LIST select the same paths as positional arguments."""
    ctx.open_studio()
    ctx.write(MAIN, 'print("main")\nreturn 4\n')
    ctx.write(UTIL, "return { version = 4 }\n")
    ctx.write("list.txt", f"{MAIN}\n{UTIL}\n")
    ctx.rbx("ps", "-f", "list.txt", "--yes", timeout=120)
    ctx.expect_in("return 4", _source(ctx, "game.ServerScriptService.Main"), "Main via -f")
    ctx.expect_in("version = 4", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util via -f")
    ctx.write(MAIN, 'print("main")\nreturn 5\n')
    ctx.write(UTIL, "return { version = 5 }\n")
    ctx.rbx("ps", "-p", MAIN, "-p", UTIL, "--yes", timeout=120)
    ctx.expect_in("return 5", _source(ctx, "game.ServerScriptService.Main"), "Main via -p")
    ctx.expect_in("version = 5", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Util via -p")


def ps_empty_list_file(ctx: Context):
    """SYNC-004: an empty -f list is refused instead of becoming a full push."""
    ctx.open_studio()
    _studio_edit(ctx, 'Instance.new("Part", workspace).Name = "Only"')
    ctx.write("list.txt", "")
    refused = ctx.rbx("ps", "-f", "list.txt", "--yes", ok=False, timeout=120)
    ctx.expect_in("contain no paths", refused.error, "empty list error")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Only") ~= nil') is True, "Studio untouched by the refused push")


def ps_target_id_widens_model_subtree(ctx: Context):
    """SYNC-005: -i ID on a Model pushes its subtree and nothing else from the store."""
    ctx.open_studio()
    rig = ctx.rbx("ba", "Workspace", "-n", "Rig", "-c", "Model", "--no-parent")
    rig_id = rig.get("settingsId") or rig.pointer("/created/0/settingsId")
    ctx.expect(rig_id, f"ba result lacks settingsId: {rig.json}")
    ctx.rbx("ba", "Workspace", "-n", "Arm", "-c", "Part", "-I", rig_id)
    ctx.rbx("ba", "Workspace", "-n", "Other", "-c", "Part", "--no-parent")
    ctx.rbx("ps", WORKSPACE, "-i", rig_id, "--yes", timeout=120)
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Rig") ~= nil and workspace.Rig:FindFirstChild("Arm") ~= nil') is True, "Rig and Arm created")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Other") == nil') is True, "sibling outside -i not created")


def ps_target_id_on_script_does_not_widen(ctx: Context):
    """SYNC-006 (live result on both platforms, 2026-11-01): -i ID on a newly stored script creates the
    script with the children stored under it, like a model; the filter widening test only describes
    which ids the filter adds."""
    ctx.open_studio()
    script = ctx.rbx("ba", "ServerScriptService", "-n", "Runner", "-c", "Script", "--no-parent", "-p", 'Source="return 1"')
    script_id = script.get("settingsId") or script.pointer("/created/0/settingsId")
    ctx.expect(script_id, f"ba result lacks settingsId: {script.json}")
    ctx.rbx("ba", "ServerScriptService", "-n", "Child", "-c", "Folder", "-I", script_id)
    ctx.rbx("ps", "instances/ServerScriptService.renium", "-i", script_id, "--yes", timeout=120)
    ctx.expect(ctx.luau('return game.ServerScriptService:FindFirstChild("Runner") ~= nil') is True, "Runner created")
    ctx.expect(ctx.luau('return game.ServerScriptService.Runner:FindFirstChild("Child") ~= nil') is True, "stored child created with the new script")


def ps_upsert_keeps_studio_only_instances(ctx: Context):
    """SYNC-007: -u applies store changes without deleting what only Studio has."""
    ctx.open_studio()
    _studio_edit(ctx, 'Instance.new("Part", workspace).Name = "Only"')
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Anchored", "--bool", "true")
    ctx.rbx("ps", WORKSPACE, "-u", "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True, "Crate anchored")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Only") ~= nil') is True, "Studio-only Part kept by -u")


def ps_service_store_removes_studio_only_instances(ctx: Context):
    """SYNC-008: a service store push without -u reconciles the service, removing Studio-only content."""
    ctx.open_studio()
    _studio_edit(ctx, 'Instance.new("Part", workspace).Name = "Only"')
    ctx.rbx("bs", "Workspace", "-i", "editor:2", "-p", "Anchored", "--bool", "true")
    ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") is True, "Crate anchored")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Only") == nil') is True, "Studio-only Part removed by the store push")


def ps_unfiltered_is_whole_place_and_cached(ctx: Context):
    """SYNC-009: an unfiltered ps replaces the whole place; a repeat reports unchanged."""
    ctx.open_studio()
    _studio_edit(ctx, 'Instance.new("Part", workspace).Name = "Only"')
    first = ctx.rbx("ps", "--yes", timeout=180)
    ctx.expect(first.get("historyId") or first.get("unchanged") is not True, f"first full push: {first.json}")
    ctx.expect(ctx.luau('return workspace:FindFirstChild("Only") == nil') is True, "Studio-only Part removed by the full push")
    second = ctx.rbx("ps", "--yes", timeout=180)
    ctx.expect(second.get("unchanged") is True, f"second full push must be unchanged: {second.json}")


def ps_unfiltered_cache_invalidated_by_studio_edit(ctx: Context):
    """SYNC-010: a Studio edit after a full push invalidates the verified-proof cache."""
    ctx.open_studio()
    ctx.rbx("ps", "--yes", timeout=180)
    second = ctx.rbx("ps", "--yes", timeout=180)
    ctx.expect(second.get("unchanged") is True, f"second full push must be unchanged: {second.json}")
    _studio_edit(ctx, "workspace.Props.Crate.Anchored = not workspace.Props.Crate.Anchored")
    third = ctx.rbx("ps", "--yes", timeout=180)
    ctx.expect(third.get("unchanged") is not True, f"push after a Studio edit must not be cached: {third.json}")
    stored = ctx.rbx("bg", "Workspace", "-i", "editor:2", "Anchored")
    ctx.expect(ctx.luau("return workspace.Props.Crate.Anchored") == bool(stored.json), f"Crate back to the stored value {stored.json}")


def ps_store_path_touches_only_its_service(ctx: Context):
    """SYNC-011: a store path push compares and pushes only that service."""
    ctx.open_studio()
    _studio_edit(ctx, 'Instance.new("Folder", game.ServerStorage).Name = "Only"')
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "pushed")
    pushed = ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(pushed.get("historyId"), f"push result lacks historyId: {pushed.json}")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "pushed", "Sign pushed")
    ctx.expect(ctx.luau('return game.ServerStorage:FindFirstChild("Only") ~= nil') is True, "other service untouched")


def ps_with_live_sync_advances_the_baseline(ctx: Context):
    """SYNC-013: a CLI push while Live Sync runs leaves nothing pending and no second push."""
    _live(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 13\n')
    _settled(ctx)
    pushes, _ = _counts(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 13 -- cli\n')
    ctx.rbx("ps", MAIN, "--yes", timeout=120)
    time.sleep(5)
    status = _settled(ctx)
    ctx.expect(status.pointer("/daemon/paused") is not True, f"Live Sync still paused: {status.json}")
    after, _ = _counts(ctx)
    ctx.expect(after - pushes <= 1, f"the worker pushed more than once around the CLI push: {pushes} -> {after}")
    ctx.expect_in("return 13 -- cli", _source(ctx, "game.ServerScriptService.Main"), "Main in Studio")


def full_ps_with_live_sync_records_a_full_baseline(ctx: Context):
    """SYNC-014: an unfiltered push while Live Sync runs settles and a later lon reports no conflicts."""
    _live(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 14\n')
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "fourteen")
    _settled(ctx)
    pushes, _ = _counts(ctx)
    ctx.write(MAIN, 'print("main")\nreturn 14 -- cli\n')
    ctx.rbx("ps", "--yes", timeout=180)
    status = _settled(ctx)
    after, _ = _counts(ctx)
    ctx.expect(after - pushes <= 1, f"the worker pushed more than once around the full push: {pushes} -> {after}")
    ctx.rbx("lof")
    again = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(again.pointer("/daemon/resolutionRequired") is not True and not again.get("conflicts"), f"lon after a full push: {again.json}")


# ---------------------------------------------------------------- B. verify


def verify_matches(ctx: Context):
    """SYNC-020: --verify compares and pushes nothing when the sources match."""
    ctx.open_studio()
    result = ctx.rbx("ps", MAIN, "--verify", timeout=120)
    ctx.expect(result.get("verifyOnly") is True and result.get("pushed") is not True, f"verify result: {result.json}")
    ctx.expect(result.get("sourceVerified") == 1 and not result.get("sourceVerifyFailed"), f"verify counts: {result.json}")


def verify_reports_a_mismatch(ctx: Context):
    """SYNC-021: --verify fails with the mismatch and pushes nothing."""
    ctx.open_studio()
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "x"')
    result = ctx.rbx("ps", MAIN, "--verify", ok=False, timeout=120)
    ctx.expect_in("mismatch", (result.error + json.dumps(result.json or {})), "mismatch report")
    ctx.expect(_source(ctx, "game.ServerScriptService.Main") == "x", "nothing pushed by --verify")


def verify_without_scripts_is_refused(ctx: Context):
    """SYNC-022: --verify on a store path verifies zero sources and pushes nothing; a selection that
    matches nothing in the project is refused."""
    ctx.open_studio()
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "untouched"')
    ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "stored")
    result = ctx.rbx("ps", WORKSPACE, "--verify", timeout=120)
    ctx.expect(result.get("verifyOnly") is True and result.get("sourceVerified") == 0, f"store verify: {result.json}")
    ctx.expect(ctx.luau("return workspace.Props.Sign.Value") == "untouched", "--verify pushed a store change")
    refused = ctx.rbx("ps", "src/Nowhere.luau", "--verify", ok=False, timeout=120)
    ctx.expect_in("matched", refused.error, "unmatched selection error")


def verify_accepts_crlf(ctx: Context):
    """SYNC-023: --verify treats a CRLF file as equal to Studio's LF source."""
    ctx.open_studio()
    content = ctx.read(MAIN)
    (ctx.root / MAIN).write_bytes(content.replace("\n", "\r\n").encode("utf-8"))
    result = ctx.rbx("ps", MAIN, "--verify", timeout=120)
    ctx.expect(result.get("sourceVerified") == 1, f"CRLF verify: {result.json}")


def verify_sources_after_push(ctx: Context):
    """SYNC-024: --verify-sources pushes and then checks the sources it wrote."""
    ctx.open_studio()
    ctx.write(MAIN, 'print("main")\nreturn 24\n')
    result = ctx.rbx("ps", MAIN, "--verify-sources", "--yes", timeout=120)
    ctx.expect(result.get("sourceVerified") == 1 and not result.get("sourceVerifyFailed"), f"verify-sources: {result.json}")
    ctx.expect_in("return 24", _source(ctx, "game.ServerScriptService.Main"), "Main in Studio")


def verify_sources_counts_unchanged_scripts(ctx: Context):
    """SYNC-025: --verify-sources over a directory counts unchanged scripts separately."""
    ctx.open_studio()
    ctx.write(UTIL, "return { version = 25 }\n")
    result = ctx.rbx("ps", "src", "--verify-sources", "--yes", timeout=120)
    ctx.expect(result.get("sourceVerified") == 3 and result.pointer("/scripts/updated") == 1, f"verify-sources over src: {result.json}")
    ctx.expect(not result.get("sourceVerifyFailed"), f"verify failures: {result.json}")


def push_with_an_open_editor_tab(ctx: Context):
    """SYNC-026: a script open in the Studio editor receives a pushed source and verifies."""
    ctx.open_studio()
    ctx.luau('game:GetService("ScriptEditorService"):OpenScriptDocumentAsync(game.ServerScriptService.Main) return true', timeout=60)
    ctx.write(MAIN, 'print("main")\nreturn 26\n')
    result = ctx.rbx("ps", MAIN, "--verify-sources", "--yes", timeout=120)
    ctx.expect(not result.get("sourceVerifyFailed"), f"verify with an open tab: {result.json}")
    ctx.expect_in("return 26", _source(ctx, "game.ServerScriptService.Main"), "Main in Studio")
    editor_text = ctx.luau('local d = game:GetService("ScriptEditorService"):FindScriptDocument(game.ServerScriptService.Main) return d and d:GetText() or ""')
    ctx.expect_in("return 26", editor_text or "", "open tab shows the pushed text")


def push_right_after_reopen_with_a_restored_tab(ctx: Context):
    """SYNC-027 (Drift Battles, Nov 1): a tab Studio restores on reopen lags the Source; a push right
    after the reopen must still verify against the Source, and a later --verify must pass."""
    ctx.open_studio()
    ctx.luau('game:GetService("ScriptEditorService"):OpenScriptDocumentAsync(game.ServerScriptService.Main) return true', timeout=60)
    ctx.close_studio(save=True)
    ctx.reopen_studio()
    ctx.write(MAIN, 'print("main")\nreturn 27\n')
    result = ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect(result.ok and "did not retain" not in result.error, f"push after reopen: {result.json}")
    time.sleep(3)
    verify = ctx.rbx("ps", MAIN, "--verify", timeout=120)
    ctx.expect(not verify.get("sourceVerifyFailed"), f"verify after reopen push: {verify.json}")


# ---------------------------------------------------------------- C. review


def _edit_six_store_values(ctx: Context):
    for index, value in enumerate(["a", "b", "c", "d", "e", "f"]):
        ctx.rbx("ba", "Workspace", "-n", f"Mark{index}", "-c", "StringValue", "-I", "editor:1", "-p", f'Value="{value}"')


def push_above_threshold_opens_the_review(ctx: Context):
    """SYNC-030 (documented): a push of more than 5 changes without --yes is held for the Studio review;
    rv skip leaves Studio unchanged and the result says skippedByReview."""
    ctx.open_studio()
    _edit_six_store_values(ctx)
    env = dict(os.environ, RENIUM_PLACE=str(ctx.root / ctx.place))
    held = subprocess.Popen([RBX, "--output-mode", "json", "ps", WORKSPACE], cwd=ctx.root, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, encoding="utf-8")
    decided = False
    deadline = time.monotonic() + 25
    while time.monotonic() < deadline and held.poll() is None:
        time.sleep(1)
        skip = ctx.rbx("rv", "skip", ok=None, timeout=30)
        if skip.ok:
            decided = True
            break
    out, err = held.communicate(timeout=120)
    result = {}
    for stream in (out, err):
        for line in reversed(stream.strip().splitlines()):
            try:
                result = json.loads(line.strip())
                break
            except Exception:
                continue
        if result:
            break
    ctx.expect(decided, f"the push was never held for review (D1): out={out[:300]!r} err={err[:300]!r}")
    ctx.expect(result.get("skippedByReview") is True or "skipped at the Studio review" in str(result.get("error")), f"skipped push result: {result} out={out[-300:]!r} err={err[-300:]!r}")
    ctx.expect(ctx.luau('return workspace.Props:FindFirstChild("Mark0") == nil') is True, "Studio unchanged after rv skip")


def yes_skips_the_review(ctx: Context):
    """SYNC-031: --yes applies a push above the threshold at once."""
    ctx.open_studio()
    _edit_six_store_values(ctx)
    started = time.monotonic()
    result = ctx.rbx("ps", WORKSPACE, "--yes", timeout=120)
    ctx.expect(time.monotonic() - started < 60 and result.get("skippedByReview") is not True, f"--yes push: {result.json}")
    ctx.expect(ctx.luau('return workspace.Props.Mark5.Value') == "f", "all six values applied")


# ---------------------------------------------------------------- F/G/H/I/J rows carried from the first runner


def lon_wait_settles(ctx: Context):
    """SYNC-070 (0.5.7): lon --wait returns the settled daemon status instead of returning early."""
    ctx.open_studio()
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/running") is True, f"lon: {started.json}")
    ctx.expect(started.pointer("/daemon/pendingCount", 0) == 0, f"not settled: {started.json}")
    ctx.expect(started.get("tracking") is True, f"tracking: {started.json}")


def independent_edits_merge_on_start(ctx: Context):
    """SYNC-083: with Live Sync off, a file edit and a Studio edit of different things both survive lon."""
    _live(ctx)
    ctx.rbx("lof")
    ctx.write(MAIN, 'print("main")\nlocal PENDING = true\n')
    _studio_edit(ctx, 'workspace.Props.Sign.Value = "studio"')
    started = ctx.rbx("lon", "--wait", "60", timeout=200)
    ctx.expect(started.pointer("/daemon/resolutionRequired") is not True, f"lon: {started.json}")
    ctx.expect_in("PENDING", ctx.read(MAIN), "the file keeps its edit through the start")
    ctx.expect_in("PENDING", _source(ctx, "game.ServerScriptService.Main"), "Studio received the file edit")
    ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value").json == "studio", "the Studio edit reached the store")


def reopen_older_in_verify_then_scoped_push(ctx: Context):
    """SYNC-091 (Drift Battles, Nov 1): a place reopened older than the files in verify priority lists the
    difference, a scoped push resolves it, status does not report unresolved changes, and lon settles."""
    _live(ctx)
    ctx.write(UTIL, "return { version = 2 }\n")
    _settled(ctx)
    ctx.close_studio(save=False)
    ctx.rbx("cfg", "set", "liveSync.initialSyncPriority", "verify")
    ctx.reopen_studio()
    time.sleep(3)
    details = ctx.rbx("lst", "--details", ok=None, timeout=120)
    ctx.expect_in("pull: src", details.error, "verify-mode difference listing")
    ctx.rbx("lof", ok=None)
    pushed = ctx.rbx("ps", "--yes", "--verify-sources", UTIL, timeout=120)
    ctx.expect(pushed.get("verified") is True or pushed.get("unchanged") is True or pushed.get("sourceVerified"), f"scoped push result: {pushed.json}")
    status = ctx.rbx("lst", "--details", timeout=60)
    ctx.expect("unresolved" not in status.stdout + status.stderr, "status must not report unresolved changes after the push")
    ctx.expect_in("version = 2", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Studio has the file's version")
    ctx.rbx("cfg", "set", "liveSync.initialSyncPriority", "reconcile")
    settled = ctx.rbx("lon", "--wait", "90", timeout=200)
    ctx.expect(settled.pointer("/daemon/pendingCount", 0) == 0, f"lon after the push: {settled.json}")


def unsaved_close_bootstraps_studio_from_files(ctx: Context):
    """SYNC-093: Studio closed without saving and reopened is brought up to the files (the file digest
    is unchanged since the last sync, so the files bootstrap the replacement runtime)."""
    _live(ctx)
    ctx.write(UTIL, "return { version = 3 }\n")
    _settled(ctx)
    ctx.close_studio(save=False)
    ctx.reopen_studio()
    settled = ctx.rbx("lon", "--wait", "90", timeout=200)
    ctx.expect(settled.pointer("/daemon/pendingCount", 0) == 0, f"lon: {settled.json}")
    ctx.expect_in("version = 3", _source(ctx, "game.ReplicatedStorage.Shared.Util"), "Studio after reconcile")


def file_edit_reaches_studio(ctx: Context):
    """SYNC-100: with Live Sync settled, a saved file edit reaches Studio and nothing stays pending."""
    _live(ctx)
    ctx.write(MAIN, 'print("main")\nlocal EDIT = 1\n')
    _settled(ctx)
    ctx.expect_in("local EDIT = 1", _source(ctx, "game.ServerScriptService.Main"), "Studio source")


def batch_creation_is_delivered(ctx: Context):
    """SYNC-101 (0.5.7): instances created with ba --batch while Live Sync runs reach Studio."""
    _live(ctx)
    batch = {"ops": [
        {"parent": "ServerStorage", "name": "Batch", "class": "Folder", "ref": "root"},
        {"parent": "#root", "name": "Config", "class": "ModuleScript", "source": "return { ok = true }"},
        {"parent": "#root", "name": "Marker", "class": "StringValue", "props": {"Value": "batch"}},
    ]}
    created = ctx.rbx("ba", "--batch", "-", stdin=json.dumps(batch), timeout=60)
    ctx.expect(len(created.get("ops") or []) == 3, f"created: {created.json}")
    _settled(ctx)
    ctx.expect(ctx.luau("return game.ServerStorage.Batch.Marker.Value") == "batch", "Studio has the batch")


def studio_edit_reaches_files(ctx: Context):
    """SYNC-120: a Studio-side source edit is pulled into the file, and the pull leaves a sync history entry."""
    _live(ctx)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "print(\\"from studio\\")"')
    _wait_file(ctx, MAIN, "from studio")
    printed = ctx.rbx("rev", "--path", MAIN, "--print", ok=None, timeout=60)
    ctx.expect(printed.code == 0, f"rev --print after a pull: {printed.error[:200]}")


def sync_history_restores_a_pull(ctx: Context):
    """SYNC-121 (0.5.5; runner defect D6 on 0.5.7): rev --sync latest undoes the file content a
    Studio-to-file pull wrote, and Live Sync pushes the restore back to Studio."""
    _live(ctx)
    before = ctx.read(MAIN)
    _studio_edit(ctx, 'game.ServerScriptService.Main.Source = "print(\\"pulled\\")"')
    _wait_file(ctx, MAIN, "pulled")
    restored = ctx.rbx("rev", "--sync", "latest", timeout=120)
    ctx.expect(restored.ok, f"rev --sync latest: {restored.error[:200]}")
    ctx.expect(ctx.read(MAIN) == before, "file content restored")
    _settled(ctx)
    ctx.expect_in('print("main")', _source(ctx, "game.ServerScriptService.Main"), "Studio got the restore")


def heavy_import_is_one_native_push(ctx: Context):
    """SYNC-053 (0.5.5/0.5.6, heavy): a 10k-instance model imported into a store while Live Sync runs is
    pushed as one native replacement and settles. The heavy fixture's files carry an edit of
    BatchTest/Config.luau that its place lacks, so that file is pushed before Live Sync pairs."""
    ctx.open_studio()
    ctx.rbx("ps", "src/ServerStorage/BatchTest/Config.luau", "--yes", timeout=180)
    _live(ctx)
    model = str(ctx.root / "mv.rbxm")
    imported = ctx.rbx("mip", "ServerStorage", "-I", "debug:0_212083", "-m", model, timeout=120)
    ctx.expect(imported.ok, f"mip: {imported.error[:200]}")
    settled = ctx.rbx("lst", "--wait", "120", ok=None, timeout=200)
    if sys.platform == "darwin" and not settled.ok:
        ctx.expect("protected write" in settled.error, f"macOS cannot apply the import's protected writes through the plugin; the refusal must say so: {settled.error[:300]}")
        return
    ctx.expect(settled.pointer("/daemon/pendingCount", 0) == 0, f"not settled after the import: {settled.json}")
    count = ctx.luau('local m = game.ServerStorage:FindFirstChild("Medieval Village", true) return m and #m:GetDescendants() or 0')
    ctx.expect(isinstance(count, (int, float)) and count > 9000, f"imported descendants in Studio: {count}")


SCENARIOS = [
    Scenario("SYNC-001", "ps one script path", "small", ps_one_script_path),
    Scenario("SYNC-002", "ps directory", "small", ps_directory),
    Scenario("SYNC-003", "ps -p repeated and -f list file", "small", ps_repeated_p_and_list_file),
    Scenario("SYNC-004", "ps empty -f list is refused", "small", ps_empty_list_file),
    Scenario("SYNC-005", "ps -i on a Model widens to its subtree", "small", ps_target_id_widens_model_subtree),
    Scenario("SYNC-006", "ps -i on a new script carries its stored children", "small", ps_target_id_on_script_does_not_widen),
    Scenario("SYNC-007", "ps -u keeps Studio-only instances", "small", ps_upsert_keeps_studio_only_instances),
    Scenario("SYNC-008", "ps service store removes Studio-only instances", "small", ps_service_store_removes_studio_only_instances),
    Scenario("SYNC-009", "ps unfiltered replaces the place and caches", "small", ps_unfiltered_is_whole_place_and_cached),
    Scenario("SYNC-010", "ps unfiltered cache invalidated by a Studio edit", "small", ps_unfiltered_cache_invalidated_by_studio_edit),
    Scenario("SYNC-011", "ps store path touches only its service", "small", ps_store_path_touches_only_its_service),
    Scenario("SYNC-013", "ps with Live Sync running advances the baseline", "small", ps_with_live_sync_advances_the_baseline),
    Scenario("SYNC-014", "unfiltered ps with Live Sync records a full baseline", "small", full_ps_with_live_sync_records_a_full_baseline),
    Scenario("SYNC-020", "--verify matches", "small", verify_matches),
    Scenario("SYNC-021", "--verify reports a mismatch", "small", verify_reports_a_mismatch),
    Scenario("SYNC-022", "--verify without scripts is refused", "small", verify_without_scripts_is_refused),
    Scenario("SYNC-023", "--verify accepts CRLF", "small", verify_accepts_crlf),
    Scenario("SYNC-024", "--verify-sources after push", "small", verify_sources_after_push),
    Scenario("SYNC-025", "--verify-sources counts unchanged scripts", "small", verify_sources_counts_unchanged_scripts),
    Scenario("SYNC-026", "push with an open editor tab", "small", push_with_an_open_editor_tab),
    Scenario("SYNC-027", "push right after reopen with a restored tab", "small", push_right_after_reopen_with_a_restored_tab),
    Scenario("SYNC-030", "push above the threshold opens the review", "small", push_above_threshold_opens_the_review),
    Scenario("SYNC-031", "--yes skips the review", "small", yes_skips_the_review),
    Scenario("SYNC-053", "heavy import is one native push", "heavy", heavy_import_is_one_native_push),
    Scenario("SYNC-070", "lon --wait returns settled", "small", lon_wait_settles),
    Scenario("SYNC-083", "independent edits merge on start", "small", independent_edits_merge_on_start),
    Scenario("SYNC-091", "reopen older save in verify, scoped push resolves", "small", reopen_older_in_verify_then_scoped_push),
    Scenario("SYNC-093", "unsaved close bootstraps Studio from the files", "small", unsaved_close_bootstraps_studio_from_files),
    Scenario("SYNC-100", "file edit reaches Studio with nothing pending", "small", file_edit_reaches_studio),
    Scenario("SYNC-101", "ba --batch under Live Sync reaches Studio", "small", batch_creation_is_delivered),
    Scenario("SYNC-120", "Studio edit reaches the file and leaves sync history", "small", studio_edit_reaches_files),
    Scenario("SYNC-121", "rev --sync latest undoes a pull and pushes back", "small", sync_history_restores_a_pull),
]
