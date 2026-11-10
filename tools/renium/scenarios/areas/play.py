"""Play, live Luau, console, input, capture and device scenarios (matrix/play.md).

Each row constructs a situation seen live on the fixture and asserts the result shape and the
Studio state; scenario ids are the matrix row ids (a function covering a group of rows carries
the first one). Multi-client rows launch a local server and client windows and close them
before the row ends. Rows that change Studio-wide state (gfx, dev, net, audio, pf, access)
restore it in a finally block.
"""

from __future__ import annotations

import json
import math
import os
import sys
import re
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from run import ScenarioSkip, RBX, Context, Scenario

MAIN = "src/ServerScriptService/Main.server.luau"
SAMPLER = (
    # Measured by the clock, not by counting waits: a client below 60 fps
    # stretches task.wait(0.05) to whole frames and would undercount.
    "local U = game:GetService('UserInputService') state.down = 0 local last = os.clock() "
    "while true do local now = os.clock() if U:IsKeyDown(Enum.KeyCode.W) then state.down += now - last end last = now task.wait(0.05) end"
)
LOGGER = (
    "local m = game.ReplicatedStorage:FindFirstChild('Log') or Instance.new('ModuleScript') "
    "m.Name = 'Log' m.Source = 'return {p = print, w = warn}' m.Parent = game.ReplicatedStorage return true"
)
CLEAN = (
    "local shop = game.StarterGui:FindFirstChild('Shop') if shop then shop:Destroy() end "
    "for _, x in workspace.Props:GetChildren() do if x.Name == 'Target' then x.Name = 'Crate' end for _, d in x:GetChildren() do if d:IsA('ClickDetector') then d:Destroy() end end end "
    "for _, n in {'Log', 'Bad'} do local m = game.ReplicatedStorage:FindFirstChild(n) while m do m:Destroy() m = game.ReplicatedStorage:FindFirstChild(n) end end "
    "local mesh = workspace:FindFirstChild('Mesh') while mesh do mesh:Destroy() mesh = workspace:FindFirstChild('Mesh') end "
    "for _, a in {'Go', 'Late', 'Clicked'} do workspace:SetAttribute(a, nil) end return true"
)
GUI = """
local StarterGui = game:GetService("StarterGui")
local old = StarterGui:FindFirstChild("Shop")
if old then old:Destroy() end
local shop = Instance.new("ScreenGui")
shop.Name = "Shop"
shop.ResetOnSpawn = false
local function button(name, x, y)
	local b = Instance.new("TextButton")
	b.Name = name
	b.Text = name
	b.Size = UDim2.fromOffset(120, 40)
	b.Position = UDim2.fromOffset(x, y)
	b.Parent = shop
	return b
end
local buy = button("BuyButton", 200, 200)
local counter = Instance.new("LocalScript")
counter.Source = 'local b = script.Parent b:SetAttribute("n", 0) b.Activated:Connect(function() b:SetAttribute("n", b:GetAttribute("n") + 1) end)'
counter.Parent = buy
local box = Instance.new("TextBox")
box.Name = "Box"
box.Text = ""
box.Size = UDim2.fromOffset(160, 40)
box.Position = UDim2.fromOffset(200, 260)
box.Parent = shop
local watcher = Instance.new("LocalScript")
watcher.Source = 'local t = script.Parent t.FocusLost:Connect(function(enter) t:SetAttribute("enter", enter) end)'
watcher.Parent = box
button("Dup", 400, 200)
button("Dup", 400, 260).Visible = false
button("Hidden", 600, 200).Visible = false
button("Covered", 600, 320)
local cover = Instance.new("Frame")
cover.Name = "Cover"
cover.Active = true
cover.ZIndex = 5
cover.Size = UDim2.fromOffset(160, 80)
cover.Position = UDim2.fromOffset(580, 300)
cover.Parent = shop
button("Offscreen", -600, -600)
shop.Parent = StarterGui
local crate
for _, child in workspace.Props:GetChildren() do
	if child.Name == "Crate" then crate = child break end
end
crate.Name = "Target"
crate.Position = crate.Position + Vector3.new(6, 0, 0)
local detector = Instance.new("ClickDetector")
detector.MaxActivationDistance = 1000
detector.Parent = crate
local clicks = Instance.new("Script")
clicks.Source = 'workspace:SetAttribute("Clicked", 0) script.Parent.MouseClick:Connect(function() workspace:SetAttribute("Clicked", workspace:GetAttribute("Clicked") + 1) end)'
clicks.Parent = detector
return true
"""


def _play(ctx: Context, *args: str):
    if not ctx.studio_open:
        ctx.open_studio()
    started = ctx.rbx("play", "-s", *args, timeout=300)
    ctx.expect(started.get("action") == "start", f"play -s: {started.json}")
    return started


def _stop(ctx: Context):
    ctx.rbx("play", "-x", ok=None, timeout=120)


def _bg(ctx: Context, *args: str, **kwargs):
    """Runs one rbx command on a worker thread; the future's result is its CommandResult."""
    pool = ThreadPoolExecutor(max_workers=1)
    future = pool.submit(ctx.rbx, *args, **kwargs)
    pool.shutdown(wait=False)
    return future


def _roles(result) -> list:
    return sorted(c.get("role") for c in result.get("clients") or [])


def _gui(ctx: Context):
    """H4: the Shop ScreenGui and a ClickDetector on one crate, built in Edit before Play. That crate is
    renamed Target and moved six studs aside: a client replicates same-named siblings in its own order, so
    Crate[1] on a client is not Crate[1] in Edit, the fixture's crates overlap, and the rows address the
    detector's crate by its unique name at its own spot."""
    _clean(ctx)
    ctx.write("gui.luau", GUI)
    ctx.rbx("l", "--edit", "-f", str(ctx.root / "gui.luau"), timeout=60)


def _own_test_windows(ctx: Context, listing) -> list:
    """The play windows that belong to this row's Studio: other sessions' Studios share the daemon, so a
    `cs` listing also names their test windows."""
    place = str((ctx.root / ctx.place).resolve())
    clients = listing.get("clients") or []
    own = [c for c in clients if c.get("role") == "edit" and c.get("localFile") and str(Path(c["localFile"]).resolve()) == place]
    pids = {c.get("pid") for c in own}
    runtimes = {c.get("runtimeId") for c in own}
    return [c for c in clients if c.get("role") != "edit" and (c.get("pid") in pids or c.get("launchEditRuntimeId") in runtimes or (c.get("localFile") and str(Path(c["localFile"]).resolve()) == place))]


def _mac_without_locator(result) -> bool:
    """Studio 0.745 on macOS has no verified native locator: the access commands refuse and name the plugin path."""
    return sys.platform == "darwin" and "no verified structural locator" in (result.error or "")


def _clean(ctx: Context):
    """Removes what the rows build in Edit (H4, H13, Bad, Mesh, workspace attributes): a session keeps
    one Studio for many rows and its reset push restores the stores, not extra instances."""
    ctx.rbx("l", "--edit", CLEAN, ok=None, timeout=60)


def _poll(ctx: Context, check, seconds: float = 15, interval: float = 0.5):
    deadline = time.monotonic() + seconds
    value = check()
    while not value and time.monotonic() < deadline:
        time.sleep(interval)
        value = check()
    return value


def _stream(ctx: Context, seconds: float, *args: str) -> str:
    """Runs a following rbx command (text output) for a fixed time, then ends its process tree."""
    env = dict(os.environ, RENIUM_PLACE=str(ctx.root / ctx.place))
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as out:
        process = subprocess.Popen([RBX, *args], cwd=ctx.root, env=env, stdin=subprocess.DEVNULL, stdout=out, stderr=subprocess.STDOUT, text=True, encoding="utf-8")
        time.sleep(seconds)
        if os.name == "nt":
            subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True, timeout=30)
        else:
            process.kill()
        process.wait(timeout=30)
        out.seek(0)
        text = out.read()
    ctx.log.append(f"$ {' '.join(args)}  [streamed {seconds}s]\n{text.strip()[:1200]}")
    return text


def _only_scratch_studio(ctx: Context, seconds: float = 600):
    """pf and --global modes reach every connected Studio: wait until the scratch one is alone, and
    refuse when another Studio stays connected."""
    expected = str((ctx.root / ctx.place).resolve()).lower()
    files: list = []

    def alone():
        nonlocal files
        listing = ctx.rbx("cs", timeout=60)
        files = [str(c.get("localFile") or "") for c in listing.get("clients") or [] if c.get("role") == "edit"]
        return bool(files) and all(f and str(Path(f).resolve()).lower() == expected for f in files)

    if not _poll(ctx, alone, min(seconds, 30), 5):
        raise ScenarioSkip(f"another Studio stayed connected ({files}); this Studio-wide change needs a lone Studio (rerun alone)")


# ---------------------------------------------------------------- play sessions


def solo_play_starts_and_stops(ctx: Context):
    """PLAY-001: play -s runs ordinary Play in the Edit window with a server and a client; l reaches the
    server; play -x stops it and a second -x answers with the stopped status; bare play starts."""
    started = _play(ctx)
    ctx.expect(started.get("mode") == "play" and started.get("serverReady") is True, f"solo start: {started.json}")
    ctx.expect(started.get("launchNonce") or any(c.get("launchNonce") for c in started.get("clients") or []), f"launchNonce: {started.json}")
    ctx.expect(isinstance(started.get("editPid"), int), f"editPid: {started.json}")
    ctx.expect(_roles(started) == ["play-client", "play-server"], f"solo play clients: {_roles(started)}")
    status = ctx.rbx("status", timeout=60)
    ctx.expect(set(status.get("availableDataModels") or []) >= {"Edit", "Server", "Client"}, f"data models during play: {status.json}")
    server = ctx.rbx("l", "return game:GetService('RunService'):IsRunning()", timeout=60)
    ctx.expect(server.get("context") == "server" and server.get("results") == [True], f"l during play: {server.json}")
    stopped = ctx.rbx("play", "-x", timeout=120)
    ctx.expect(stopped.get("action") == "stop" and stopped.pointer("/status/engineRunning") is False, f"play -x: {stopped.json}")
    again = ctx.rbx("play", "-x", timeout=120)
    ctx.expect(again.get("action") == "status" and again.get("running") is False, f"idle stop: {again.json}")
    after = ctx.rbx("status", timeout=60)
    ctx.expect(after.get("availableDataModels") == ["Edit"], f"data models after stop: {after.json}")
    try:
        bare = ctx.rbx("play", timeout=240)
        ctx.expect(bare.get("action") == "start" and bare.get("mode") == "play", f"bare play: {bare.json}")
    finally:
        _stop(ctx)


def second_start_reports_the_running_session(ctx: Context):
    """PLAY-002: play -s while a session runs answers with the session's status, not a second launch;
    play -s -p 2 is refused."""
    started = _play(ctx)
    try:
        again = ctx.rbx("play", "-s", timeout=120)
        ctx.expect(again.get("action") == "status" and again.get("running") is True, f"second start: {again.json}")
        ctx.expect(again.get("launchNonce") == started.get("launchNonce"), f"same launchNonce: {again.get('launchNonce')} vs {started.get('launchNonce')}")
        multi = ctx.rbx("play", "-s", "-p", "2", ok=False, timeout=120)
        ctx.expect_in("A Studio play session is already active", multi.error, "-p 2 during Play")
    finally:
        _stop(ctx)


def run_and_server_modes(ctx: Context):
    """PLAY-003: --mode run and --mode server both start a run session with a play server and no player."""
    ctx.open_studio()
    for mode in ("run", "server"):
        started = ctx.rbx("play", "-s", "--mode", mode, timeout=240)
        try:
            ctx.expect(started.get("mode") == "run", f"--mode {mode}: {started.json}")
            ctx.expect(_roles(started) == ["play-server"], f"--mode {mode} clients: {_roles(started)}")
            players = ctx.rbx("l", "return #game.Players:GetPlayers()", timeout=60)
            ctx.expect(players.get("results") == [0], f"players in --mode {mode}: {players.json}")
        finally:
            _stop(ctx)


def play_refusals_before_studio(ctx: Context):
    """PLAY-004: conflicting play flags are refused before any Studio is reached; a player count outside
    1-8 is refused by the start once Studio answers, without launching anything."""
    cases = [
        (("-s", "-x"), "Use either --start or --stop"),
        (("-x", "-p", "2"), "cannot be combined with --stop"),
        (("-s", "--mode", "foo"), "Invalid play mode"),
        (("-s", "--mode", "run", "-p", "2"), "only be used with --mode play"),
        (("-s", "-p", "1"), "--players 1 launches"),
    ]
    for args, needle in cases:
        refused = ctx.rbx("play", *args, "-w", "2", ok=False, timeout=60)
        ctx.expect_in(needle, refused.error, f"play {' '.join(args)}")
    ctx.open_studio()
    for count in ("9", "0"):
        refused = ctx.rbx("play", "-s", "-p", count, ok=False, timeout=120)
        ctx.expect_in("between 1 and 8", refused.error, f"play -s -p {count}")
    listing = ctx.rbx("cs", timeout=60)
    ctx.expect(not _own_test_windows(ctx, listing), f"test windows after the refusals: {listing.json}")


def restart_keeps_or_overrides_the_shape(ctx: Context):
    """PLAY-005: play -r restarts ordinary Play with a new launch and the pushed script; -r -p 2 switches
    to a two-client session and says so."""
    started = _play(ctx)
    try:
        ctx.write(MAIN, 'print("main2")\n')
        ctx.rbx("ps", MAIN, "--yes", timeout=120)
        restarted = ctx.rbx("play", "-r", timeout=240)
        ctx.expect(restarted.get("restarted") is True and restarted.get("action") == "start" and restarted.get("mode") == "play", f"play -r: {restarted.json}")
        ctx.expect(restarted.get("launchNonce") != started.get("launchNonce"), f"new launchNonce: {restarted.get('launchNonce')}")
        ctx.expect("restartNote" not in (restarted.json or {}), f"restartNote on a known launch: {restarted.json}")
        main2 = _poll(ctx, lambda: ctx.rbx("co", "--server", "--grep", "^main2$", timeout=60).get("matched") == 1)
        ctx.expect(main2, "the restarted session runs the pushed script (co --server --grep ^main2$)")
        multi = ctx.rbx("play", "-r", "-p", "2", timeout=300)
        ctx.expect(multi.get("mode") == "multi" and "restarted the 2-client session" in str(multi.get("restartNote")), f"play -r -p 2: {multi.json}")
    finally:
        _stop(ctx)


def restart_without_a_known_launch(ctx: Context):
    """PLAY-006: play -r on a fresh Edit window with nothing running starts ordinary Play and says that
    no launch was known. The Edit window is reopened first so the daemon knows no launch for it."""
    ctx.open_studio()
    ctx.close_studio()
    ctx.reopen_studio()
    try:
        restarted = ctx.rbx("play", "-r", timeout=240)
        ctx.expect(restarted.get("restarted") is False and restarted.get("mode") == "play", f"play -r with nothing known: {restarted.json}")
        ctx.expect_in("No session was running and none started by this Renium daemon", str(restarted.get("restartNote")), "restartNote")
    finally:
        _stop(ctx)


def start_right_after_a_stop(ctx: Context):
    """PLAY-007: play -s at once after play -x starts; a retried start names its attempts and reason."""
    _play(ctx)
    ctx.rbx("play", "-x", timeout=120)
    try:
        again = ctx.rbx("play", "-s", timeout=240)
        ctx.expect(again.get("action") == "start", f"start after stop: {again.json}")
        if "retriedAfterMs" in (again.json or {}):
            ctx.expect((again.get("startAttempts") or 0) >= 2 and again.get("retryReason"), f"retried start: {again.json}")
    finally:
        _stop(ctx)


# ---------------------------------------------------------------- multi-client sessions


def multi_client_session_lifecycle(ctx: Context):
    """PLAY-020: -p 2 launches a server and two clients in their own processes; add-players and leave
    change the roster; play -r restarts with the same shape; play -x closes every window; -p 1 --mode
    play runs one client against a local server."""
    started = _play(ctx, "-p", "2")
    try:
        ctx.expect(started.get("mode") == "multi" and started.get("players") == 2 and started.get("serverReady") is True, f"multi start: {started.json}")
        ctx.expect_in("local server plus 2 client windows", str(started.get("note")), "multi note")
        ctx.expect(_roles(started) == ["play-client", "play-client", "play-server"], f"roles: {_roles(started)}")
        listing = ctx.rbx("cs", timeout=60)
        play_pids = {c.get("pid") for c in listing.get("clients") or [] if c.get("role") != "edit"}
        ctx.expect(play_pids and started.get("editPid") not in play_pids, f"play pids {play_pids} vs editPid {started.get('editPid')}")
        ctx.expect(ctx.rbx("lc", "return game.Players.LocalPlayer.Name", "2", timeout=60).get("results") == ["Player2"], "lc on client 2")
        ctx.expect(ctx.rbx("l", "--server", "return #game.Players:GetPlayers()", timeout=60).get("results") == [2], "server sees two players")
        added = ctx.rbx("play", "--add-players", "1", timeout=240)
        ctx.expect(added.get("added") == ["Player3"] and added.get("clients") == 3, f"add-players: {added.json}")
        left = ctx.rbx("play", "--leave", "-p", "3", timeout=120)
        ctx.expect(left.get("kicked") is True and left.get("windowClosed") is True and left.get("serverListsPlayer") is False, f"leave: {left.json}")
        ctx.expect(ctx.rbx("l", "--server", "return #game.Players:GetPlayers()", timeout=60).get("results") == [2], "server back to two players")
        restarted = ctx.rbx("play", "-r", timeout=300)
        ctx.expect(restarted.get("restarted") is True and restarted.get("players") == 2, f"multi restart: {restarted.json}")
    finally:
        _stop(ctx)
    listing = ctx.rbx("cs", timeout=60)
    ctx.expect(not _own_test_windows(ctx, listing), f"test windows after stop: {listing.json}")
    one = ctx.rbx("play", "-s", "-p", "1", "--mode", "play", timeout=300)
    try:
        ctx.expect(one.get("mode") == "multi" and one.get("players") == 1, f"-p 1 --mode play: {one.json}")
    finally:
        _stop(ctx)


def client_selectors(ctx: Context):
    """PLAY-021: a client is chosen by index or player name; an unknown selector names the connected
    clients and how to add one."""
    _play(ctx, "-p", "2")
    try:
        ctx.expect(ctx.rbx("lc", "return game.Players.LocalPlayer.Name", "2", timeout=60).get("results") == ["Player2"], "lc by index")
        ctx.expect(ctx.rbx("lc", "return game.Players.LocalPlayer.Name", "Player1", timeout=60).get("results") == ["Player1"], "lc by name")
        missing = ctx.rbx("lc", "return 1", "5", ok=False, timeout=60)
        ctx.expect_in("No connected play client matches player selector", missing.error, "lc on client 5")
        ctx.expect_in("rbx play --add-players 1", missing.error, "lc on client 5 names add-players")
        nobody = ctx.rbx("inp", "-p", "Nobody", "key", "E", ok=False, timeout=60)
        ctx.expect_in("No connected play client matches player selector", nobody.error, "inp -p Nobody")
    finally:
        _stop(ctx)


def add_players_limits(ctx: Context):
    """PLAY-022: --add-players joins clients to a multi-client test, refuses more than Studio's eight, and
    fails without a test or in ordinary Play instead of answering an empty list."""
    _play(ctx, "-p", "2")
    try:
        added = ctx.rbx("play", "--add-players", "1", timeout=240)
        ctx.expect(added.get("added") == ["Player3"] and added.get("clients") == 3, f"add-players 1: {added.json}")
        too_many = ctx.rbx("play", "--add-players", "7", ok=False, timeout=120)
        ctx.expect_in("add at most 5", too_many.error, "add-players 7")
    finally:
        _stop(ctx)
    none = ctx.rbx("play", "--add-players", "1", "-w", "2", ok=False, timeout=120)
    ctx.expect_in("No play test is running", none.error, "add-players without Play")
    _play(ctx)
    try:
        solo = ctx.rbx("play", "--add-players", "1", ok=None, timeout=240)
        ctx.expect(not solo.ok and solo.get("added") != [], f"add-players in ordinary Play: {solo.json} {solo.error[:300]}")
    finally:
        _stop(ctx)


def leave_needs_a_client_index(ctx: Context):
    """PLAY-023: --leave -p N kicks that client's player and closes its window; a missing, zero or unknown
    index is refused."""
    _play(ctx, "-p", "2")
    try:
        ctx.rbx("play", "--add-players", "1", timeout=240)
        left = ctx.rbx("play", "--leave", "-p", "3", timeout=120)
        ctx.expect(left.get("player") == "Player3" and left.get("kicked") is True and left.get("serverListsPlayer") is False and left.get("windowClosed") is True and left.get("clients") == 2, f"leave -p 3: {left.json}")
        ctx.expect_in("Name the client to remove", ctx.rbx("play", "--leave", ok=False, timeout=60).error, "--leave alone")
        ctx.expect_in("start at 1", ctx.rbx("play", "--leave", "-p", "0", ok=False, timeout=60).error, "--leave -p 0")
        ctx.expect_in("No play client 7 is connected", ctx.rbx("play", "--leave", "-p", "7", "-w", "2", ok=False, timeout=60).error, "--leave -p 7")
    finally:
        _stop(ctx)


def _orphaned_test(ctx: Context) -> list:
    """H8: a -p 1 --mode play test whose Edit window is closed under it; returns the test pids."""
    started = _play(ctx, "-p", "1", "--mode", "play")
    pids = sorted(c.get("pid") for c in started.get("clients") or [])
    ctx.close_studio()
    return pids


def _orphan_pids(ctx: Context) -> list:
    listing = ctx.rbx("cs", ok=None, timeout=60)
    return sorted(o.get("pid") for o in listing.get("orphans") or [])


def orphans_of_a_closed_edit_window(ctx: Context):
    """PLAY-024: test windows whose Edit window exited are listed as orphans; play --kill-orphans closes
    them; a multi-client start after a reopen closes its place's orphans first."""
    try:
        pids = _orphaned_test(ctx)
        listed = _poll(ctx, lambda: _orphan_pids(ctx) == pids, 20, 1)
        ctx.expect(listed, f"orphans {_orphan_pids(ctx)} vs test pids {pids}")
        killed = ctx.rbx("play", "--kill-orphans", timeout=120)
        ctx.expect(killed.get("action") == "killOrphans" and sorted(killed.get("closed") or []) == pids, f"kill-orphans with RENIUM_PLACE={ctx.root / ctx.place}: {killed.json}")
        ctx.expect(_poll(ctx, lambda: _orphan_pids(ctx) == [], 20, 1), "orphans left after kill-orphans")
        ctx.reopen_studio()
        pids = _orphaned_test(ctx)
        ctx.expect(_poll(ctx, lambda: _orphan_pids(ctx) == pids, 20, 1), f"second orphan set {_orphan_pids(ctx)} vs {pids}")
        reopened = ctx.reopen_studio()
        started = ctx.rbx("play", "-s", "-p", "2", timeout=300)
        try:
            if sys.platform == "darwin":
                # Studio on macOS opens a new window beside a running test server as
                # a client of that test, so the reopen closes the orphans first.
                ctx.expect(sorted(reopened.get("closedOrphans") or []) == pids, f"ro closedOrphans: {reopened.json}")
                ctx.expect(not started.get("closedOrphans"), f"no orphans left for the start: {started.json}")
            else:
                ctx.expect(sorted(started.get("closedOrphans") or []) == pids, f"closedOrphans: {started.json}")
        finally:
            _stop(ctx)
    finally:
        ctx.rbx("--place", ctx.place, "play", "--kill-orphans", ok=None, timeout=120)


# ---------------------------------------------------------------- --until, --result, wait


def until_result_and_timeout(ctx: Context):
    """PLAY-030: --until waits on the server and --result evaluates once there; an --until condition that
    never holds times out and says so while the session stays up."""
    ctx.open_studio()
    started = ctx.rbx("play", "-s", "--until", "workspace:FindFirstChild('Props') ~= nil", "--result", "return #workspace.Props:GetChildren()", timeout=240)
    try:
        ctx.expect(started.get("action") == "start", f"play --until: {started.json}")
        ctx.expect(started.pointer("/until/ok") is True and started.pointer("/until/value") == "true" and started.get("result") == 3, f"until and result: {started.json}")
    finally:
        _stop(ctx)
    timed = ctx.rbx("play", "-s", "--until", "false", "--until-timeout", "3", ok=None, timeout=240)
    try:
        ctx.expect(timed.code == 1 and timed.get("action") == "start" and timed.pointer("/until/timedOut") is True, f"until timeout: {timed.code} {timed.json}")
        status = ctx.rbx("status", timeout=60)
        ctx.expect("Server" in (status.get("availableDataModels") or []), f"session should still run: {status.json}")
    finally:
        _stop(ctx)


def bad_until_and_failing_result(ctx: Context):
    """PLAY-031: an --until that does not parse is refused before Play starts; a --result that errors
    after the condition held fails with the condition marked as met."""
    ctx.open_studio()
    bad = ctx.rbx("play", "-s", "--until", "workspace.(", ok=False, timeout=240)
    ctx.expect_in("neither a Luau expression nor a chunk", bad.error, "unparsable --until")
    status = ctx.rbx("status", timeout=60)
    ctx.expect(status.get("availableDataModels") == ["Edit"], f"no Play after a bad --until: {status.json}")
    failed = ctx.rbx("play", "-s", "--until", "true", "--result", "error('x')", ok=None, timeout=240)
    try:
        ctx.expect(failed.code == 1, f"failing --result exit: {failed.code} {failed.json}")
        ctx.expect(failed.pointer("/until/conditionMet") is True, f"conditionMet: {failed.json}")
        ctx.expect_in("the result expression failed", str(failed.pointer("/until/error")).lower(), "until.error")
    finally:
        _stop(ctx)


def wait_forms_and_timeout(ctx: Context):
    """PLAY-032: wait returns an expression's value (with --result), runs a chunk with its own return,
    evaluates on a play client with -c, and reports a timeout with the last value; --server without
    Play fails."""
    ctx.open_studio()
    held = ctx.rbx("wait", "workspace:FindFirstChild('Props') ~= nil", "-t", "10", "--result", "return #workspace.Props:GetChildren()", timeout=60)
    ctx.expect(held.get("result") == 3 and held.get("value") == "true" and isinstance(held.get("elapsedSeconds"), (int, float)), f"wait ok: {held.json}")
    chunk = ctx.rbx("wait", "local p = workspace:FindFirstChild('Props') return p ~= nil", "-t", "10", timeout=60)
    ctx.expect(chunk.ok and chunk.get("value") == "true", f"wait chunk: {chunk.json}")
    expired = ctx.rbx("wait", "workspace:GetAttribute('Never')", "-t", "2", ok=False, timeout=60)
    ctx.expect(expired.code == 1 and expired.get("timedOut") is True and expired.get("value") == "nil", f"wait timeout: {expired.code} {expired.json}")
    ctx.expect_in("Timed out after 2s", expired.error, "wait timeout text")
    no_server = ctx.rbx("wait", "true", "--server", "-t", "2", ok=False, timeout=60)
    ctx.expect(no_server.error, "wait --server without Play must fail")
    _play(ctx)
    try:
        client = ctx.rbx("wait", "game:GetService('RunService'):IsClient()", "-c", "-t", "10", timeout=60)
        ctx.expect(client.ok and client.get("value") == "true", f"wait -c: {client.json}")
    finally:
        _stop(ctx)


def wait_across_runner_slices(ctx: Context):
    """PLAY-033: a server wait longer than one Luau runner's limit keeps polling across slices and returns
    once a detached runner makes the condition true after 110 s."""
    _play(ctx)
    try:
        ctx.rbx("l", "--server", "--detach", "late", "task.wait(110) workspace:SetAttribute('Late', true)", timeout=60)
        held = ctx.rbx("wait", "workspace:GetAttribute('Late') == true", "--server", "-t", "150", timeout=240)
        ctx.expect(held.ok and (held.get("elapsedSeconds") or 0) >= 105, f"wait across slices: {held.json}")
    finally:
        _stop(ctx)


def wait_holds_its_runtime(ctx: Context):
    """PLAY-034: a server wait holds the server's request gate, so a later server request runs after it
    without a busy answer, while an Edit request answers at once."""
    _play(ctx)
    try:
        waiting = _bg(ctx, "wait", "false", "--server", "-t", "8", ok=False, timeout=60)
        time.sleep(1)
        server = _bg(ctx, "l", "--server", "return 1", timeout=60)
        time.sleep(0.5)
        edit = ctx.rbx("l", "--edit", "return 2", timeout=60)
        ctx.expect(edit.get("results") == [2] and edit.seconds < 2, f"--edit during a server wait: {edit.seconds:.1f}s {edit.json}")
        queued = server.result(timeout=90)
        ctx.expect(queued.get("results") == [1] and queued.seconds >= 5.5, f"server request after the wait: {queued.seconds:.1f}s {queued.json}")
        ctx.expect(waiting.result(timeout=90).get("timedOut") is True, "the wait itself timed out")
    finally:
        _stop(ctx)


def edit_wait_can_be_satisfied(ctx: Context):
    """PLAY-035 (D9): an Edit wait for an attribute that a later rbx l --edit sets. Expected: the setter
    runs and the wait returns ok; while D9 stands the setter answers busy and the wait times out."""
    ctx.open_studio()
    _clean(ctx)
    waiting = _bg(ctx, "wait", "workspace:GetAttribute('Go') == true", "-t", "20", ok=None, timeout=90)
    time.sleep(1)
    setter = ctx.rbx("l", "--edit", "workspace:SetAttribute('Go', true) return true", ok=None, timeout=60)
    held = waiting.result(timeout=120)
    ctx.expect(setter.ok, f"setter during an Edit wait ({setter.seconds:.1f}s): {setter.error[:300]}")
    ctx.expect(held.ok and held.get("value") == "true", f"Edit wait after the setter ({held.seconds:.1f}s): {held.json}")


def wait_past_thirty_minutes(ctx: Context):
    """PLAY-036 (D6): wait -t 2400 times out at 2400 s with timedOut:true, as runtime.md promises up to an
    hour; while D6 stands the CLI gives up at the 1800 s transport timeout."""
    _play(ctx)
    try:
        expired = ctx.rbx("wait", "false", "--server", "-t", "2400", ok=None, timeout=2700)
        ctx.expect(expired.get("timedOut") is True and expired.seconds >= 2390, f"wait -t 2400 after {expired.seconds:.0f}s: {expired.json} {expired.error[:300]}")
    finally:
        _stop(ctx)


def stop_during_a_wait(ctx: Context):
    """PLAY-037: play -x during a server wait stops in normal time and the wait fails soon after instead
    of running to its timeout."""
    _play(ctx)
    waiting = _bg(ctx, "wait", "false", "--server", "-t", "60", ok=None, timeout=120)
    time.sleep(2)
    stopped = ctx.rbx("play", "-x", timeout=120)
    ctx.expect(stopped.get("action") == "stop" and stopped.seconds < 15, f"stop during a wait: {stopped.seconds:.1f}s {stopped.json}")
    held = waiting.result(timeout=150)
    ctx.expect(not held.ok and held.seconds < 2 + stopped.seconds + 10, f"wait after the stop: {held.seconds:.1f}s {held.json} {held.error[:200]}")


# ---------------------------------------------------------------- l and lc: targets and results


def luau_contexts(ctx: Context):
    """PLAY-040, PLAY-041: l runs in Edit when nothing plays (unsaved persistence) and --server fails
    without Play; during Play l runs on the server, --edit reaches the Edit window and --server the
    server."""
    ctx.open_studio()
    edit = ctx.rbx("l", "return workspace.Name", timeout=60)
    ctx.expect(edit.get("context") == "edit" and edit.get("results") == ["Workspace"] and edit.get("persistence") == "unsaved", f"edit context: {edit.json}")
    no_server = ctx.rbx("l", "--server", "return 1", ok=False, timeout=60)
    ctx.expect_in("No play server is connected: Play is stopped", no_server.error, "--server without Play")
    _play(ctx)
    try:
        server = ctx.rbx("l", "return game:GetService('RunService'):IsServer()", timeout=60)
        ctx.expect(server.get("context") == "server" and server.get("results") == [True] and "persistence" not in server.json, f"server context: {server.json}")
        edit_during = ctx.rbx("l", "--edit", "return game:GetService('RunService'):IsRunning()", timeout=60)
        ctx.expect(edit_during.get("results") == [False] and "context" not in edit_during.json, f"--edit during play: {edit_during.json}")
        pinned = ctx.rbx("l", "--server", "return 1", timeout=60)
        ctx.expect(pinned.get("results") == [1], f"--server during play: {pinned.json}")
    finally:
        _stop(ctx)


def clients_via_l_and_lc(ctx: Context):
    """PLAY-042: l -c and l -p N reach play clients; lc without a player picks one client without an
    ambiguity error."""
    _play(ctx, "-p", "2")
    try:
        client = ctx.rbx("l", "-c", "return game.Players.LocalPlayer ~= nil", timeout=60)
        ctx.expect(client.get("results") == [True], f"l -c: {client.json}")
        second = ctx.rbx("l", "-p", "2", "return game.Players.LocalPlayer.Name", timeout=60)
        ctx.expect(second.get("results") == ["Player2"], f"l -p 2: {second.json}")
        any_client = ctx.rbx("lc", "return game.Players.LocalPlayer.Name", timeout=60)
        ctx.expect(any_client.get("results") in (["Player1"], ["Player2"]), f"lc without a player: {any_client.json}")
    finally:
        _stop(ctx)


def lc_argument_mistakes(ctx: Context):
    """PLAY-043: lc argument order, extra arguments, --collect with code and l --edit -c are refused."""
    _play(ctx, "-p", "2")
    try:
        ctx.expect_in("lc takes the Luau code before the player", ctx.rbx("lc", "1", "return 1", ok=False, timeout=60).error, "lc PLAYER CODE")
        ctx.expect_in("unexpected extra argument", ctx.rbx("lc", "return 1", "1", "x", ok=False, timeout=60).error, "lc extra argument")
        ctx.expect_in("--collect takes no code", ctx.rbx("lc", "--collect", "k", "return 1", "1", ok=False, timeout=60).error, "lc --collect with code")
        conflict = ctx.rbx("l", "--edit", "-c", "return 1", ok=False, timeout=60)
        ctx.expect(conflict.code == 2 and "cannot be used with" in conflict.error, f"l --edit -c: {conflict.code} {conflict.error[:300]}")
    finally:
        _stop(ctx)


def value_shapes(ctx: Context):
    """PLAY-044: l results keep arrays, maps, nil, Instances and Vector3 typed; a large map is truncated
    at 128 keys with the total."""
    ctx.open_studio()
    shaped = ctx.rbx("l", "return {1,2}, {a=1}, nil, workspace.Props, Vector3.new(1,2,3)", timeout=60)
    results = shaped.get("results") or []
    ctx.expect(len(results) == 5 and results[0] == [1, 2] and results[1] == {"a": 1}, f"array and map: {shaped.json}")
    ctx.expect(results[2] == {"_type": "Nil"}, f"nil: {results[2:3]}")
    ctx.expect(isinstance(results[3], dict) and results[3].get("_type") == "Instance" and "Props" in str(results[3].get("path")), f"Instance: {results[3:4]}")
    ctx.expect(isinstance(results[4], dict) and results[4].get("_type") == "Vector3", f"Vector3: {results[4:5]}")
    big = ctx.rbx("l", "local t = {} for i = 1, 200 do t['k'..i] = i end return t", timeout=60)
    table = (big.get("results") or [{}])[0]
    keys = [k for k in table if k != "_truncated"]
    ctx.expect(len(keys) == 128 and table.get("_truncated") == {"kept": 128, "total": 200}, f"truncated map: {len(keys)} keys, {table.get('_truncated')}")


def captured_output(ctx: Context):
    """PLAY-045: a runner's print and warn come back in output and stay out of the console; a flood is cut
    at 257 entries with a closing note; Play runner prints stay out of the server console too."""
    ctx.open_studio()
    printed = ctx.rbx("l", "print('cap1') warn('cap2') return 1", timeout=60)
    messages = [(o.get("type"), o.get("message")) for o in printed.get("output") or []]
    ctx.expect([m for _, m in messages] == ["cap1", "cap2"], f"output: {printed.json}")
    ctx.expect(ctx.rbx("co", "--grep", "cap", timeout=60).get("matched") == 0, "runner prints reached the Edit console")
    flood = ctx.rbx("l", "for i = 1, 300 do print(i) end", timeout=60)
    entries = flood.get("output") or []
    ctx.expect(len(entries) == 257 and "Renium stopped capturing command output" in str(entries[-1].get("message")), f"flood: {len(entries)} entries, last {entries[-1:]}")
    _play(ctx)
    try:
        ctx.rbx("l", "print('cap3')", timeout=60)
        ctx.expect(ctx.rbx("co", "--server", "--grep", "cap3", timeout=60).get("matched") == 0, "runner print reached the server console")
    finally:
        _stop(ctx)


def edit_and_play_lifetimes(ctx: Context):
    """PLAY-046: each Edit run requires modules afresh while a Play server keeps them; threads an Edit
    run leaves behind die with it."""
    ctx.open_studio()
    _clean(ctx)
    ctx.rbx("l", "require(game.ReplicatedStorage.Shared.Util).x = 1", timeout=60)
    fresh = ctx.rbx("l", "return require(game.ReplicatedStorage.Shared.Util).x", timeout=60)
    ctx.expect(fresh.get("results") == [{"_type": "Nil"}], f"Edit module cache: {fresh.json}")
    ctx.rbx("l", "task.delay(1, function() workspace:SetAttribute('Late', true) end)", timeout=60)
    time.sleep(2.5)
    late = ctx.rbx("l", "return workspace:GetAttribute('Late')", timeout=60)
    ctx.expect(late.get("results") == [{"_type": "Nil"}], f"an Edit run's delayed thread ran: {late.json}")
    _play(ctx)
    try:
        ctx.rbx("l", "require(game.ReplicatedStorage.Shared.Util).x = 1", timeout=60)
        kept = ctx.rbx("l", "return require(game.ReplicatedStorage.Shared.Util).x", timeout=60)
        ctx.expect(kept.get("results") == [1], f"Play module cache: {kept.json}")
    finally:
        _stop(ctx)


# ---------------------------------------------------------------- l input, errors, timeouts, gates


def luau_from_file_and_stdin(ctx: Context):
    """PLAY-050: -f FILE and - (stdin, also with a BOM) run the same chunk; missing code and a missing
    file are named."""
    ctx.open_studio()
    ctx.write("s.luau", "return 6 * 7\n")
    from_file = ctx.rbx("l", "-f", str(ctx.root / "s.luau"), timeout=60)
    ctx.expect(from_file.get("results") == [42], f"-f: {from_file.json}")
    from_stdin = ctx.rbx("l", "-", stdin="return 'piped'", timeout=60)
    ctx.expect(from_stdin.get("results") == ["piped"], f"stdin: {from_stdin.json}")
    with_bom = ctx.rbx("l", "-", stdin="﻿return 'piped'", timeout=60)
    ctx.expect(with_bom.get("results") == ["piped"], f"stdin with a BOM: {with_bom.json}")
    ctx.expect_in("Missing Luau code. Use -e <code> or -f <file>.", ctx.rbx("l", ok=False, timeout=60).error, "l without code")
    ctx.expect_in("Failed to read", ctx.rbx("l", "-f", "nope.luau", ok=False, timeout=60).error, "l -f missing file")


def luau_errors(ctx: Context):
    """PLAY-051: a syntax error is refused without a console entry; runtime errors name the runtime they
    ran in (Edit window, play server, client 1)."""
    ctx.open_studio()
    before = ctx.rbx("co", timeout=60)
    syntax = ctx.rbx("l", "return (", ok=False, timeout=60)
    ctx.expect_in("Invalid Luau syntax", syntax.error, "syntax error")
    after = ctx.rbx("co", "--since-seq", str(before.get("nextSeq") or 0), timeout=60)
    ctx.expect(after.get("entries") == [], f"a syntax error reached the console: {after.json}")
    ctx.expect_in("[Edit window]", ctx.rbx("l", "error('boom')", ok=False, timeout=60).error, "Edit error")
    client = ctx.rbx("lc", "return 1", "1", ok=False, timeout=60)
    ctx.expect_in("rbx play -s", client.error, "client without Play names the start command")
    _play(ctx, "-p", "2")
    try:
        ctx.expect_in("[play server]", ctx.rbx("l", "--server", "error('boom')", ok=False, timeout=60).error, "server error")
        ctx.expect_in("[client 1]", ctx.rbx("lc", "error('boom')", "1", ok=False, timeout=60).error, "client error")
    finally:
        _stop(ctx)


def luau_timeouts(ctx: Context):
    """PLAY-052: l times out at 10 s by default and at -t; a busy loop is stopped at its timeout and the
    next run answers at once; a Play run past 120 s hits the runner limit."""
    ctx.open_studio()
    default = ctx.rbx("l", "task.wait(12)", ok=False, timeout=60)
    ctx.expect("timed out after 10.0s" in default.error and 9 <= default.seconds <= 14, f"default timeout ({default.seconds:.1f}s): {default.error[:300]}")
    short = ctx.rbx("l", "-t", "2", "task.wait(5)", ok=False, timeout=60)
    ctx.expect("2.0s" in short.error and short.seconds < 5, f"-t 2 ({short.seconds:.1f}s): {short.error[:300]}")
    loop = ctx.rbx("l", "-t", "2", "while true do end", ok=False, timeout=60)
    ctx.expect(loop.seconds < 6, f"busy loop stopped after {loop.seconds:.1f}s: {loop.error[:300]}")
    after = ctx.rbx("l", "return 1", timeout=60)
    ctx.expect(after.get("results") == [1] and after.seconds < 3, f"run after the loop ({after.seconds:.1f}s): {after.json}")
    _play(ctx)
    try:
        long = ctx.rbx("l", "--server", "-t", "300", "task.wait(125)", ok=False, timeout=400)
        ctx.expect("Luau runner timed out after 120.0s" in long.error and 115 <= long.seconds <= 135, f"Play runner limit ({long.seconds:.1f}s): {long.error[:300]}")
    finally:
        _stop(ctx)


def module_load_failure(ctx: Context):
    """PLAY-053 (D10): a module that errors while loading fails the run with the error and carries
    consoleErrors with the lines Studio logged, as runtime.md promises."""
    ctx.open_studio()
    _clean(ctx)
    ctx.rbx("l", "--edit", "local m = Instance.new('ModuleScript') m.Name = 'Bad' m.Source = 'error(\"bad module\")' m.Parent = game.ReplicatedStorage return true", timeout=60)
    _play(ctx)
    try:
        failed = ctx.rbx("l", "--server", "return require(game.ReplicatedStorage.Bad)", ok=False, timeout=60)
        # macOS Studio reports only "Requested module experienced an error while
        # loading" in the error; the module's own message is in the console lines.
        ctx.expect("bad module" in failed.error or "bad module" in json.dumps(failed.get("consoleErrors")), f"module error: {failed.error[:200]} {failed.json}")
        ctx.expect(isinstance(failed.get("consoleErrors"), list) and "bad module" in json.dumps(failed.get("consoleErrors")), f"consoleErrors (D10): {failed.json}")
    finally:
        _stop(ctx)
        _clean(ctx)


def what_waits_behind_an_edit_holder(ctx: Context):
    """PLAY-055: with a 12 s Edit run holding the Edit gate, perf answers at once while a push and a
    Play start wait for the holder and then succeed."""
    ctx.open_studio()
    holder = _bg(ctx, "l", "--edit", "task.wait(12) return 1", "-t", "20", timeout=60)
    time.sleep(1)
    perf = ctx.rbx("perf", timeout=60)
    ctx.expect(perf.seconds < 2, f"perf behind the holder took {perf.seconds:.1f}s")
    pushed = ctx.rbx("ps", MAIN, "--yes", timeout=120)
    ctx.expect(pushed.ok, f"push behind the holder: {pushed.json}")
    try:
        started = ctx.rbx("play", "-s", timeout=240)
        ctx.expect(started.get("action") == "start", f"play behind the holder: {started.json}")
    finally:
        _stop(ctx)
    ctx.expect(holder.result(timeout=60).get("results") == [1], "holder result")


def play_runtimes_gate_separately(ctx: Context):
    """PLAY-056: a 6 s run on client 1 holds only client 1: client 2 and Edit answer at once and the next
    client 1 run waits for it without a busy answer."""
    _play(ctx, "-p", "2")
    try:
        holder = _bg(ctx, "lc", "task.wait(6) return 1", "1", timeout=60)
        time.sleep(1)
        other = ctx.rbx("lc", "return 2", "2", timeout=60)
        ctx.expect(other.get("results") == [2] and other.seconds < 2, f"client 2 ({other.seconds:.1f}s): {other.json}")
        edit = ctx.rbx("l", "--edit", "return 3", timeout=60)
        ctx.expect(edit.get("results") == [3] and edit.seconds < 2, f"Edit ({edit.seconds:.1f}s): {edit.json}")
        same = ctx.rbx("lc", "return 4", "1", timeout=60)
        ctx.expect(same.get("results") == [4] and same.seconds >= 3, f"client 1 after its holder ({same.seconds:.1f}s): {same.json}")
        ctx.expect(holder.result(timeout=60).get("results") == [1], "client 1 holder result")
    finally:
        _stop(ctx)


def file_relative_to_the_cwd(ctx: Context):
    """PLAY-057 (D7): l -f with a relative path run from a subfolder reads the file in that folder, as
    every other path argument does; while D7 stands it reads from the project root."""
    ctx.open_studio()
    ctx.write("src/s.luau", "return 6 * 7\n")
    result = ctx.rbx("l", "-f", "s.luau", cwd=ctx.root / "src", ok=None, timeout=60)
    ctx.expect(result.get("results") == [42], f"l -f s.luau from src (D7): {result.json} {result.error[:300]}")


def play_gates_follow_the_requests_client(ctx: Context):
    """PLAY-132: a client request submitted before its client connects runs behind that client's gate once
    it is there, so the next request to the same client waits for it; a bare client request shares the gate
    of the client it runs on with the explicit request to that client, and the Edit window answers at once."""
    _play(ctx, "-p", "2")
    try:
        added = _bg(ctx, "play", "--add-players", "1", timeout=300)
        time.sleep(0.5)
        early = _bg(ctx, "lc", "task.wait(6) return 1", "3", timeout=120)
        ctx.expect(added.result(timeout=300).get("added") == ["Player3"], "a third client joined")
        later = ctx.rbx("lc", "return 2", "3", timeout=120)
        ctx.expect(later.get("results") == [2] and later.seconds >= 3, f"client 3 after the request that waited for it ({later.seconds:.1f}s): {later.json}")
        ctx.expect(early.result(timeout=120).get("results") == [1], "the early request ran on client 3")
    finally:
        _stop(ctx)
    _play(ctx)
    try:
        bare = _bg(ctx, "lc", "task.wait(5) return 1", timeout=120)
        time.sleep(1)
        edit = ctx.rbx("l", "--edit", "return 3", timeout=60)
        ctx.expect(edit.get("results") == [3] and edit.seconds < 2, f"Edit beside a bare client holder ({edit.seconds:.1f}s): {edit.json}")
        explicit = ctx.rbx("lc", "return 2", "1", timeout=120)
        ctx.expect(explicit.get("results") == [2] and explicit.seconds >= 2.5, f"client 1 after the bare holder ({explicit.seconds:.1f}s): {explicit.json}")
        ctx.expect(bare.result(timeout=120).get("results") == [1], "bare holder result")
    finally:
        _stop(ctx)


# ---------------------------------------------------------------- detached runners


def detached_runner_collects_state(ctx: Context):
    """PLAY-060: --detach returns at the first yield, --collect reads its state, --stop ends it; a client
    runner lives on its own client only."""
    _play(ctx)
    try:
        detached = ctx.rbx("l", "--detach", "probe", "state.n = 0 while true do state.n += 1 task.wait(0.2) end", timeout=60)
        ctx.expect(detached.get("detached") == "probe" and detached.get("running") is True, f"detach: {detached.json}")
        time.sleep(1.2)
        collected = ctx.rbx("l", "--collect", "probe", timeout=60)
        ctx.expect((collected.pointer("/state/n") or 0) >= 3 and collected.get("done") is False, f"collect: {collected.json}")
        stopped = ctx.rbx("l", "--collect", "probe", "--stop", timeout=60)
        ctx.expect(stopped.get("stopped") is True, f"stop: {stopped.json}")
        gone = ctx.rbx("l", "--collect", "probe", ok=False, timeout=60)
        ctx.expect_in('No detached runner named "probe" is running here', gone.error, "collect after stop")
    finally:
        _stop(ctx)
    _play(ctx, "-p", "2")
    try:
        ctx.rbx("lc", "--detach", "k", "state.t = 1 task.wait(60)", "1", timeout=60)
        other = ctx.rbx("lc", "--collect", "k", "2", ok=False, timeout=60)
        ctx.expect_in('No detached runner named "k"', other.error, "client 2 collect of client 1's runner")
    finally:
        _stop(ctx)


def finished_expired_and_replaced_runners(ctx: Context):
    """PLAY-061: a runner that returns is collected as done with its results; one past its lifetime is
    gone; a second detach under the same name replaces the first; an unknown name lists the running."""
    _play(ctx)
    try:
        quick = ctx.rbx("l", "--detach", "q", "return 5", timeout=60)
        ctx.expect(quick.get("running") is False and quick.get("results") == [5], f"detach q: {quick.json}")
        collected = ctx.rbx("l", "--collect", "q", ok=None, timeout=60)
        ctx.expect(collected.get("done") is True, f"collect q: {collected.json} {collected.error[:200]}")
        ctx.rbx("l", "--detach", "b2", "--lifetime", "2", "while true do task.wait(1) end", timeout=60)
        time.sleep(3)
        ctx.expect_in('No detached runner named "b2"', ctx.rbx("l", "--collect", "b2", ok=False, timeout=60).error, "expired runner")
        ctx.rbx("l", "--detach", "a", "state.v = 1 task.wait(60)", timeout=60)
        ctx.rbx("l", "--detach", "a", "state.v = 2 task.wait(60)", timeout=60)
        ctx.rbx("l", "--detach", "b", "task.wait(60)", timeout=60)
        replaced = ctx.rbx("l", "--collect", "a", timeout=60)
        ctx.expect(replaced.pointer("/state/v") == 2, f"replaced runner: {replaced.json}")
        ctx.expect_in("running: a, b", ctx.rbx("l", "--collect", "zz", ok=False, timeout=60).error, "unknown runner lists the running ones")
    finally:
        _stop(ctx)


def detach_refusals(ctx: Context):
    """PLAY-062: --detach needs Play and a short name; --stop and --lifetime need --collect / --detach."""
    ctx.open_studio()
    ctx.expect_in("Detached runners need Play", ctx.rbx("l", "--detach", "x", "return 1", ok=False, timeout=60).error, "detach in Edit")
    ctx.expect_in("Runner names use up to 64", ctx.rbx("l", "--detach", "a b", "return 1", ok=False, timeout=60).error, "runner name with a space")
    stop = ctx.rbx("l", "--stop", "return 1", ok=False, timeout=60)
    ctx.expect(stop.code == 2 and "--collect" in stop.error, f"--stop without --collect: {stop.code} {stop.error[:300]}")
    lifetime = ctx.rbx("l", "--lifetime", "5", "return 1", ok=False, timeout=60)
    ctx.expect(lifetime.code == 2 and "--detach" in lifetime.error, f"--lifetime without --detach: {lifetime.code} {lifetime.error[:300]}")


# ---------------------------------------------------------------- co console


def console_reads_filters_and_clears(ctx: Context):
    """PLAY-070, PLAY-072, PLAY-073, PLAY-074: co --server shows the fixture's print with seq, time and
    type; the client console has its own; --grep and --level filter (output and print match a print);
    -F is plain text and a bad regex names -F; --since-seq continues after nextSeq; a restart starts a
    new epoch; -c clears the buffer."""
    _play(ctx)
    try:
        entries = _poll(ctx, lambda: (lambda r: r if r.get("matched", 0) >= 1 else None)(ctx.rbx("co", "--server", "--grep", "^main$", "-n", "50", timeout=60)))
        ctx.expect(entries, "server console lacks the script's print")
        first = (entries.get("entries") or [{}])[0]
        ctx.expect(all(k in first for k in ("seq", "time", "type")), f"entry fields: {first}")
        client = ctx.rbx("co", "--client", "--grep", "^client$", timeout=60)
        ctx.expect(client.get("matched") == 1, f"client console: {client.json}")
        grep = ctx.rbx("co", "--server", "--grep", "^main$", timeout=60)
        ctx.expect(grep.get("matched") == 1, f"grep: {grep.json}")
        errors = ctx.rbx("co", "--server", "--level", "error", timeout=60)
        ctx.expect(all(e.get("type") == "MessageError" for e in errors.get("entries") or []), f"level error returned other levels: {errors.json}")
        for level in ("output", "print"):
            matched = ctx.rbx("co", "--server", "--level", level, "--grep", "^main$", timeout=60)
            ctx.expect(matched.get("matched") == 1, f"--level {level} (D2): {matched.json}")
        ctx.expect(ctx.rbx("co", "--server", "--grep", "ma.n", "-F", timeout=60).get("matched") == 0, "-F must be plain text (D1)")
        paren = ctx.rbx("co", "--server", "--grep", "(", "-F", timeout=60)
        ctx.expect(paren.ok and all("(" in str(e.get("message")) for e in paren.get("entries") or []), f"-F with a regex metacharacter: {paren.json}")
        ctx.expect_in("--grep takes a regex; pass -F", ctx.rbx("co", "--server", "--grep", "(", ok=False, timeout=60).error, "bad regex")
        ctx.expect(ctx.rbx("co", "--server", "--grep", "ma.n", timeout=60).get("matched") == 1, "regex grep")
        later = ctx.rbx("co", "--server", "--since-seq", str(entries.get("nextSeq")), timeout=60)
        ctx.expect(later.get("entries") == [], f"since nextSeq: {later.json}")
        printed = ctx.rbx("l", "print('again') return 1", timeout=60)
        ctx.expect(any(o.get("message") == "again" for o in printed.get("output") or []), f"l returns its own prints: {printed.json}")
        ctx.rbx("play", "-r", timeout=240)
        fresh = _poll(ctx, lambda: (lambda r: r if r.get("matched", 0) >= 1 else None)(ctx.rbx("co", "--server", "--grep", "^main$", "-n", "50", timeout=60)))
        ctx.expect(fresh and fresh.get("epoch") != entries.get("epoch"), f"a restart starts a new console epoch: {fresh and fresh.json}")
        ctx.rbx("co", "--server", "-c", timeout=60)
        cleared = ctx.rbx("co", "--server", timeout=60)
        ctx.expect(cleared.get("entries") == [], f"after clear: {cleared.json}")
    finally:
        _stop(ctx)


def console_filters_fold_and_follow(ctx: Context):
    """PLAY-071, PLAY-074: filters scan the whole buffer before -n and report scanned/matched; repeated
    lines fold; --since-seq returns only newer game lines; co -f streams new matching lines."""
    ctx.open_studio()
    _clean(ctx)
    ctx.luau(LOGGER)
    _play(ctx)
    try:
        ctx.rbx("l", "--server", "local L = require(game.ReplicatedStorage.Log) for i = 1, 30 do L.p('tick' .. i) end L.w('warned once') for i = 1, 5 do L.p('same') end", timeout=60)
        ticks = _poll(ctx, lambda: (lambda r: r if r.get("matched") == 30 else None)(ctx.rbx("co", "--server", "--grep", "tick", "-n", "3", timeout=60)))
        ctx.expect(ticks, "co --grep tick -n 3 never matched 30")
        ctx.expect([e.get("message") for e in ticks.get("entries") or []] == ["tick28", "tick29", "tick30"] and (ticks.get("scanned") or 0) >= 36, f"last three ticks: {ticks.json}")
        warned = ctx.rbx("co", "--server", "--level", "warn", "--grep", "warned once", timeout=60)
        ctx.expect(warned.get("matched") == 1, f"--level warn: {warned.json}")
        same = ctx.rbx("co", "--server", "--grep", "^same$", timeout=60)
        folded = same.get("entries") or []
        ctx.expect(len(folded) == 1 and folded[0].get("repeat") == 5 and same.get("folded") == 4, f"folding: {same.json}")
        mark = ctx.rbx("co", "--server", timeout=60).get("nextSeq")
        ctx.rbx("l", "--server", "require(game.ReplicatedStorage.Log).p('again')", timeout=60)
        newer = _poll(ctx, lambda: (lambda r: r if r.get("entries") else None)(ctx.rbx("co", "--server", "--since-seq", str(mark), timeout=60)))
        ctx.expect(newer and [e.get("message") for e in newer.get("entries")] == ["again"], f"since-seq: {newer and newer.json}")
        ctx.rbx("l", "--server", "--detach", "ticker", "local L = require(game.ReplicatedStorage.Log) task.wait(1.5) for i = 1, 3 do L.p('follow tick') task.wait(0.3) end", timeout=60)
        streamed = _stream(ctx, 5, "co", "--server", "-f", "--grep", "follow tick")
        lines = [line for line in streamed.splitlines() if line.strip()]
        ctx.expect(len(lines) == 3 and all(line.rstrip().endswith("tick") for line in lines), f"co -f: {streamed[:600]!r}")
    finally:
        _stop(ctx)
        _clean(ctx)


def client_console_per_player(ctx: Context):
    """PLAY-075: co -p N reads one client's console in a multi-client session."""
    ctx.open_studio()
    _clean(ctx)
    ctx.luau(LOGGER)
    _play(ctx, "-p", "2")
    try:
        for player in ("1", "2"):
            entries = _poll(ctx, lambda: ctx.rbx("co", "--client", "-p", player, "--grep", "^client$", timeout=60).get("matched", 0) >= 1)
            ctx.expect(entries, f"client {player} console lacks the client script's print")
        ctx.rbx("lc", "require(game.ReplicatedStorage.Log).p('from two')", "2", timeout=60)
        two = _poll(ctx, lambda: ctx.rbx("co", "-p", "2", "--grep", "from two", timeout=60).get("matched") == 1)
        ctx.expect(two, "co -p 2 lacks the line client 2 logged")
        one = ctx.rbx("co", "-p", "1", "--grep", "from two", timeout=60)
        ctx.expect(one.get("matched") == 0, f"client 1 shows client 2's line: {one.json}")
    finally:
        _stop(ctx)
        _clean(ctx)


# ---------------------------------------------------------------- input


def ui_lists_elements(ctx: Context):
    """PLAY-080: ui lists the visible GUI elements with paths; --include-offscreen adds the off-screen one."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        listing = _poll(ctx, lambda: (lambda r: r if any(i.get("p") == "Shop.BuyButton" for i in r.get("items") or []) else None)(ctx.rbx("ui", "-p", "1", timeout=60)))
        ctx.expect(listing, "ui -p 1 never listed Shop.BuyButton")
        ctx.expect(listing.get("count") == len(listing.get("items")), f"count: {listing.json}")
        ctx.expect(not any("Offscreen" in str(i.get("p")) for i in listing.get("items")), "off-screen button listed without --include-offscreen")
        everything = ctx.rbx("ui", "-p", "1", "--include-offscreen", timeout=60)
        ctx.expect(any("Offscreen" in str(i.get("p")) for i in everything.get("items") or []), f"--include-offscreen: {everything.json}")
    finally:
        _stop(ctx)
        _clean(ctx)


def verified_presses(ctx: Context):
    """PLAY-081: pr presses a GUI button through the game's input and reports what it matched; inp clicks
    are verified; the button's own script counts three activations."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        _poll(ctx, lambda: ctx.rbx("ui", "-p", "1", timeout=60).get("count", 0) > 0)
        pressed = ctx.rbx("pr", "Shop.BuyButton", "-p", "1", timeout=60)
        ctx.expect(pressed.get("action") == "press" and pressed.get("matchedCount") == 1 and pressed.get("inputMethod") == "virtual", f"pr: {pressed.json}")
        ctx.expect(all(k in pressed.json for k in ("target", "ordinalPath", "id")), f"pr fields: {pressed.json}")
        clicks = ctx.rbx("inp", "-p", "1", "click", "Shop.BuyButton", "wait", "100", "click", "Shop.BuyButton", timeout=60)
        ctx.expect(clicks.get("verifiedClicks") == 2, f"inp clicks: {clicks.json}")
        count = _poll(ctx, lambda: ctx.rbx("lc", "return game.Players.LocalPlayer.PlayerGui.Shop.BuyButton:GetAttribute('n')", "1", timeout=60).get("results") == [3], 5)
        ctx.expect(count, "BuyButton's Activated count is not 3")
    finally:
        _stop(ctx)
        _clean(ctx)


def press_ambiguity_and_refusals(ctx: Context):
    """PLAY-082: a name with one visible match presses it; two visible matches need an ordinal or id; a
    missing, hidden or covered element is refused with its reason."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        _poll(ctx, lambda: ctx.rbx("ui", "-p", "1", timeout=60).get("count", 0) > 0)
        one = ctx.rbx("pr", "Shop.Dup", "-p", "1", timeout=60)
        ctx.expect(one.get("action") == "press" and one.get("matchedCount") == 2, f"pr Shop.Dup with one visible: {one.json}")
        ctx.rbx("lc", "for _, b in game.Players.LocalPlayer.PlayerGui.Shop:GetChildren() do if b.Name == 'Dup' then b.Visible = true end end return true", "1", timeout=60)
        ctx.expect_in("Disambiguate with [n] ordinals or press by id", ctx.rbx("pr", "Shop.Dup", "-p", "1", ok=False, timeout=60).error, "two visible Dup")
        second = ctx.rbx("pr", "Shop.Dup[2]", "-p", "1", timeout=60)
        ctx.expect(second.get("action") == "press", f"pr Shop.Dup[2]: {second.json}")
        listing = ctx.rbx("ui", "-p", "1", timeout=60)
        dup_id = next((i.get("id") for i in listing.get("items") or [] if str(i.get("p")).startswith("Shop.Dup")), None)
        ctx.expect(dup_id, f"no Dup id in ui: {listing.json}")
        by_id = ctx.rbx("pr", "--id", str(dup_id), "-p", "1", timeout=60)
        ctx.expect(by_id.get("action") == "press", f"pr --id: {by_id.json}")
        ctx.expect_in("matched nothing", ctx.rbx("pr", "NoSuchGui", "-p", "1", ok=False, timeout=60).error, "pr NoSuchGui")
        ctx.expect_in("is not visible", ctx.rbx("pr", "Shop.Hidden", "-p", "1", ok=False, timeout=60).error, "pr Shop.Hidden")
        ctx.expect_in("is covered by", ctx.rbx("pr", "Shop.Covered", "-p", "1", ok=False, timeout=60).error, "pr Shop.Covered")
    finally:
        _stop(ctx)
        _clean(ctx)


def press_a_world_part(ctx: Context):
    """PLAY-083: pr --world clicks a part's ClickDetector once it is on screen and refuses one behind the
    camera."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        ctx.rbx("go", "Props.Target", "-p", "1", timeout=120)
        pressed = ctx.rbx("pr", "Props.Target", "--world", "-p", "1", timeout=60)
        ctx.expect(pressed.ok, f"pr --world: {pressed.json}")
        clicked = _poll(ctx, lambda: ctx.rbx("l", "--server", "return workspace:GetAttribute('Clicked')", timeout=60).get("results") == [1], 5)
        ctx.expect(clicked, "the ClickDetector did not count one click")
        ctx.rbx("lc", "local c = workspace.CurrentCamera local crate = workspace.Props:FindFirstChild('Target') c.CameraType = Enum.CameraType.Scriptable local eye = crate.Position + Vector3.new(0, 5, 15) c.CFrame = CFrame.lookAt(eye, eye + Vector3.new(0, 0, 20)) return true", "1", timeout=60)
        behind = ctx.rbx("pr", "Props.Target", "--world", "-p", "1", ok=False, timeout=60)
        ctx.expect_in("is not on screen (behind the camera", behind.error, "part behind the camera")
    finally:
        _stop(ctx)
        _clean(ctx)


def viewport_clicks(ctx: Context):
    """PLAY-084: inp click clicks viewport points through the game's input, and right-down, wait,
    right-up hold the right button; the Roblox menu corner is refused. The throughSystemUi case is
    not scripted: no probed point of the fixture's viewport lies under Roblox UI that the engine
    reports (see the matrix row)."""
    _play(ctx)
    try:
        clicked = ctx.rbx("inp", "-p", "1", "click", "450,323", timeout=60)
        ctx.expect(clicked.get("actions") == 1 and clicked.get("inputMethod") == "virtual", f"inp click: {clicked.json}")
        right = ctx.rbx("inp", "-p", "1", "right-down", "450,323", "wait", "200", "right-up", "450,323", timeout=60)
        ctx.expect(right.get("actions") == 3, f"inp right-down wait 200 right-up: {right.json}")
        ctx.expect_in("The engine reserves this point (the Roblox menu button)", ctx.rbx("inp", "-p", "1", "click", "30,30", ok=False, timeout=60).error, "menu corner")
    finally:
        _stop(ctx)


def key_presses(ctx: Context):
    """PLAY-085: inp key presses a key; Roblox refuses Escape and the CLI refuses non-keyboard key
    codes."""
    _play(ctx)
    try:
        key = ctx.rbx("inp", "-p", "1", "key", "Space", timeout=60)
        ctx.expect(key.get("actions") == 1 and key.get("inputMethod") == "virtual", f"inp key Space: {key.json}")
        ctx.expect_in("Roblox keeps this key", ctx.rbx("inp", "-p", "1", "key", "Escape", ok=False, timeout=60).error, "inp key Escape")
        ctx.expect_in("Unsupported keyboard key", ctx.rbx("inp", "-p", "1", "key", "Gamepad1", ok=False, timeout=60).error, "inp key Gamepad1")
    finally:
        _stop(ctx)


def long_key_holds(ctx: Context):
    """PLAY-086 (D3): inp hold W 45000 holds W for 45 s as the guide promises (up to 60 s), and 70000
    ends after 60 s; while D3 stands the call fails at the 30 s bridge timeout."""
    _play(ctx)
    try:
        ctx.rbx("lc", "--detach", "keys", SAMPLER, "1", timeout=60)
        held = ctx.rbx("inp", "-p", "1", "hold", "W", "45000", ok=None, timeout=180)
        sampled = ctx.rbx("lc", "--collect", "keys", "1", timeout=60).pointer("/state/down") or 0
        ctx.expect(held.ok and held.seconds >= 44, f"inp hold W 45000 ({held.seconds:.1f}s): {held.json} {held.error[:300]}")
        ctx.expect(40 <= sampled <= 50, f"sampler saw W down for {sampled}s")
        clamped = ctx.rbx("inp", "-p", "1", "hold", "W", "70000", ok=None, timeout=180)
        ctx.expect(clamped.ok and 59 <= clamped.seconds < 69, f"inp hold W 70000 ({clamped.seconds:.1f}s): {clamped.json} {clamped.error[:300]}")
    finally:
        _stop(ctx)


def type_into_a_box(ctx: Context):
    """PLAY-087: ty focuses a TextBox, types and presses Enter; the box's own FocusLost sees the Enter; a
    path that is not a TextBox is refused."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        _poll(ctx, lambda: ctx.rbx("ui", "-p", "1", timeout=60).get("count", 0) > 0)
        typed = ctx.rbx("ty", "hello", "--path", "Shop.Box", "--enter", "-p", "1", timeout=60)
        ctx.expect(typed.get("chars") == 5 and "Box" in str(typed.get("focused")) and typed.get("enter") is True, f"ty: {typed.json}")
        box = _poll(ctx, lambda: (lambda r: r if r.get("results") == [["hello", True]] else None)(ctx.rbx("lc", "local b = game.Players.LocalPlayer.PlayerGui.Shop.Box return {b.Text, b:GetAttribute('enter')}", "1", timeout=60)), 5)
        ctx.expect(box, "Box text and Enter not seen by the game")
        ctx.expect_in("is not a TextBox", ctx.rbx("ty", "x", "--path", "Shop.BuyButton", "-p", "1", ok=False, timeout=60).error, "ty into a button")
    finally:
        _stop(ctx)
        _clean(ctx)


def walk_and_teleport(ctx: Context):
    """PLAY-088: go walks to a part and teleports to a position; ambiguous, non-part, missing and badly
    formed targets are refused; an unreachable point times out."""
    _play(ctx)
    try:
        walked = ctx.rbx("go", "Props.Crate[1]", "-p", "1", timeout=120)
        ctx.expect(walked.get("action") == "goto" and (walked.get("finalDistance") or 99) < 8, f"go Crate[1]: {walked.json}")
        teleported = ctx.rbx("go", "--pos", "0,5,0", "--tp", "-p", "1", timeout=60)
        ctx.expect(teleported.get("action") == "teleport", f"go --tp: {teleported.json}")
        ctx.expect_in("add [n] ordinals", ctx.rbx("go", "Props.Crate", "-p", "1", ok=False, timeout=60).error, "ambiguous go")
        ctx.expect_in("is not a BasePart or Model", ctx.rbx("go", "Props.Sign", "-p", "1", ok=False, timeout=60).error, "go to a StringValue")
        ctx.expect_in("not found", ctx.rbx("go", "Props.Nope", "-p", "1", ok=False, timeout=60).error, "go to a missing part")
        ctx.expect_in("--pos must be X,Y,Z", ctx.rbx("go", "--pos", "1,2", "-p", "1", ok=False, timeout=60).error, "go --pos 1,2")
        far = ctx.rbx("go", "--pos", "900,5,900", "-t", "3", "-p", "1", ok=False, timeout=60)
        ctx.expect_in("Character did not reach", far.error, "unreachable in 3 s")
    finally:
        _stop(ctx)


def long_walk(ctx: Context):
    """PLAY-089 (D5): go -t 200 on a 125 s walk arrives (or reports "within 200s"); while D5 stands the
    plugin's 120 s runner limit ends it first."""
    _play(ctx)
    try:
        walked = ctx.rbx("go", "--pos", "200,5,0", "--speed-multiplier", "0.1", "-t", "200", "-p", "1", ok=None, timeout=400)
        text = walked.error if not walked.ok else ""
        ctx.expect(walked.ok or "within 200s" in text, f"go -t 200 ({walked.seconds:.0f}s, D5): {walked.json} {text[:300]}")
    finally:
        _stop(ctx)


def inp_keys(ctx: Context):
    """PLAY-090: inp runs key sequences; kd holds a key across calls until ku, and the game sees it."""
    _play(ctx)
    try:
        sequence = ctx.rbx("inp", "-p", "1", "key", "W", "wait", "100", "key", "S", timeout=60)
        ctx.expect(sequence.get("actions") == 3 and sequence.get("inputMethod") == "virtual", f"inp sequence: {sequence.json}")
        down = ctx.rbx("inp", "-p", "1", "kd", "W", timeout=60)
        ctx.expect("W" in (down.get("heldKeys") or []) and down.pointer("/keysObserved/W") is True, f"inp kd W: {down.json}")
        ctx.expect(ctx.rbx("lc", "return game:GetService('UserInputService'):IsKeyDown(Enum.KeyCode.W)", "1", timeout=60).get("results") == [True], "W down in the game")
        up = ctx.rbx("inp", "-p", "1", "ku", "W", timeout=60)
        ctx.expect("W" not in (up.get("heldKeys") or []), f"inp ku W: {up.json}")
        ctx.expect(ctx.rbx("lc", "return game:GetService('UserInputService'):IsKeyDown(Enum.KeyCode.W)", "1", timeout=60).get("results") == [False], "W up in the game")
    finally:
        _stop(ctx)


def inp_long_hold(ctx: Context):
    """PLAY-091 (D4): inp hold W 15000 holds W for 15 s; while D4 stands each inp action stops at 10 s."""
    _play(ctx)
    try:
        ctx.rbx("lc", "--detach", "keys", SAMPLER, "1", timeout=60)
        held = ctx.rbx("inp", "-p", "1", "hold", "W", "15000", ok=None, timeout=120)
        sampled = ctx.rbx("lc", "--collect", "keys", "1", timeout=60).pointer("/state/down") or 0
        ctx.expect(held.ok and held.seconds >= 14.5 and 13.5 <= sampled <= 16.5, f"inp hold W 15000 ({held.seconds:.1f}s, sampler {sampled}s, D4): {held.json}")
    finally:
        _stop(ctx)


def inp_world_clicks(ctx: Context):
    """PLAY-092 (D8): inp move over a part sets Mouse.Target; inp click with a world part path clicks its
    ClickDetector whether or not another action precedes it; while D8 stands the lone click takes the
    GUI path and fails."""
    ctx.open_studio()
    _gui(ctx)
    _play(ctx)
    try:
        ctx.rbx("go", "Props.Target", "-p", "1", timeout=120)
        point = ctx.rbx("lc", "local crate = workspace.Props:FindFirstChild('Target') local p = workspace.CurrentCamera:WorldToViewportPoint(crate.Position) return {math.floor(p.X), math.floor(p.Y)}", "1", timeout=60).get("results")[0]
        ctx.rbx("inp", "-p", "1", "move", f"{point[0]},{point[1]}", timeout=60)
        target = ctx.rbx("lc", "local t = game.Players.LocalPlayer:GetMouse().Target return t and t.Name", "1", timeout=60)
        ctx.expect(target.get("results") == ["Target"], f"Mouse.Target after inp move: {target.json}")
        clicks = lambda: ctx.rbx("l", "--server", "return workspace:GetAttribute('Clicked')", timeout=60).get("results")[0]
        sequenced = ctx.rbx("inp", "-p", "1", "move", "1,1", "click", "Workspace.Props.Target", ok=None, timeout=60)
        ctx.expect(sequenced.ok and _poll(ctx, lambda: clicks() == 1, 5), f"inp move then click a world part: {sequenced.json} {sequenced.error[:200]}")
        alone = ctx.rbx("inp", "-p", "1", "click", "Workspace.Props.Target", ok=None, timeout=60)
        ctx.expect(alone.ok and _poll(ctx, lambda: clicks() == 2, 5), f"inp click a world part alone (D8): {alone.json} {alone.error[:300]}")
    finally:
        _stop(ctx)
        _clean(ctx)


def inp_parse_errors(ctx: Context):
    """PLAY-093: inp refuses unknown actions, bad durations and missing values before reaching Studio;
    more than 256 actions is refused by the daemon before any input is sent."""
    cases = [
        (("jump",), "Unknown input action"),
        (("wait", "abc"), "wait takes a duration"),
        (("hold", "W"), "hold needs a key and a duration"),
        (("key",), "needs a value"),
    ]
    for actions, needle in cases:
        refused = ctx.rbx("inp", "-p", "1", "-w", "2", *actions, ok=False, timeout=60)
        ctx.expect_in(needle, refused.error, f"inp {' '.join(actions)}")
    _play(ctx)
    try:
        refused = ctx.rbx("inp", "-p", "1", *(("key", "W") * 257), ok=False, timeout=60)
        ctx.expect_in("1 through 256 actions", refused.error, "inp with 257 actions")
    finally:
        _stop(ctx)


# ---------------------------------------------------------------- capture


def screenshots_studio_and_client(ctx: Context):
    """PLAY-100: sc --studio writes a PNG with its size; sc in ordinary Play and sc -p N capture a play
    client window."""
    ctx.open_studio()
    shot = ctx.rbx("sc", "--studio", "-o", str(ctx.root / "s.png"), timeout=120)
    ctx.expect(shot.get("target") == "studio" and shot.get("width", 0) > 100 and shot.get("height", 0) > 100 and shot.get("path"), f"sc --studio: {shot.json}")
    ctx.expect((ctx.root / "s.png").stat().st_size > 10_000, "s.png written")
    _play(ctx)
    try:
        solo = ctx.rbx("sc", "-o", str(ctx.root / "a.png"), timeout=120)
        ctx.expect(solo.get("target") == "play-client", f"sc in ordinary Play: {solo.json}")
    finally:
        _stop(ctx)
    _play(ctx, "-p", "2")
    try:
        client = ctx.rbx("sc", "-p", "1", "-o", str(ctx.root / "c.png"), timeout=120)
        ctx.expect(client.get("target") == "play-client" and client.get("window"), f"sc -p 1: {client.json}")
        ctx.expect((ctx.root / "c.png").stat().st_size > 10_000, "c.png written")
    finally:
        _stop(ctx)


def camera_for_a_screenshot(ctx: Context):
    """PLAY-101: sc --camera-position with --look-at moves the camera for the shot and restores it after;
    --camera-position alone is a usage error; a look-at equal to the position is not held."""
    ctx.open_studio()
    read_camera = "local c = workspace.CurrentCamera.CFrame return {c.X, c.Y, c.Z}"
    before = ctx.luau(read_camera)
    shot = ctx.rbx("sc", "--studio", "-o", str(ctx.root / "c.png"), "--camera-position", "-10,5,0", "--look-at", "-20,0,0", timeout=120)
    ctx.expect(shot.pointer("/camera/moved") is True, f"camera moved: {shot.json}")
    actual = shot.pointer("/camera/actual")
    position = actual.get("position") if isinstance(actual, dict) else actual
    ctx.expect(isinstance(position, list) and math.dist(position[:3], [-10, 5, 0]) <= 1, f"camera.actual: {actual}")
    after = ctx.luau(read_camera)
    ctx.expect(math.dist(before, after) <= 0.01, f"camera restored: {before} -> {after}")
    alone = ctx.rbx("sc", "--studio", "-o", str(ctx.root / "d.png"), "--camera-position", "1,2,3", ok=False, timeout=60)
    ctx.expect(alone.code == 2 and "--look-at" in alone.error, f"--camera-position alone: {alone.code} {alone.error[:300]}")
    same = ctx.rbx("sc", "--studio", "-o", str(ctx.root / "e.png"), "--camera-position", "1,2,3", "--look-at", "1,2,3", ok=None, timeout=120)
    ctx.expect(same.pointer("/camera/moved") is False and same.pointer("/camera/note"), f"position equal to look-at: {same.json}")


def recording_round_trip(ctx: Context):
    """PLAY-102: rs records the Studio window, re stops it with frame counts, the MP4 and its review, rf
    renders the overview; a client recording with --no-review has no review."""
    ctx.open_studio()
    started = ctx.rbx("rs", "--studio", "-o", str(ctx.root / "r.mp4"), "--max-seconds", "3", "--fps", "10", timeout=120)
    ctx.expect(started.get("recordingId") and started.get("target") == "studio", f"rs: {started.json}")
    time.sleep(5)
    ended = ctx.rbx("re", timeout=120)
    frames = ended.get("frames") or 0
    ctx.expect(10 <= frames <= 31 and 2500 <= (ended.get("durationMs") or 0) <= 3500 and "achievedFps" in ended.json, f"re: {ended.json}")
    ctx.expect((ctx.root / "r.mp4").stat().st_size > 10_000, "r.mp4 written")
    ctx.expect(Path(str(ended.pointer("/review/path"))).is_file() and ended.pointer("/review/totalFrames") == frames, f"review: {ended.get('review')}")
    overview = ctx.rbx("rf", str(ctx.root / "r.mp4"), timeout=120)
    shown = [f.get("frame") for f in overview.get("frames") or []]
    ctx.expect(overview.get("mode") == "overview" and 0 < len(shown) <= 12 and shown[0] == 1 and shown[-1] == frames, f"rf overview: {shown} of {frames}")
    _play(ctx, "-p", "2")
    try:
        client = ctx.rbx("rs", "-p", "1", "-o", str(ctx.root / "c.mp4"), "--max-seconds", "5", timeout=120)
        ctx.expect(client.get("target") == "play-client", f"rs -p 1: {client.json}")
        ctx.rbx("inp", "-p", "1", "key", "W", timeout=60)
        done = ctx.rbx("re", "--no-review", timeout=120)
        ctx.expect("review" not in done.json and (ctx.root / "c.mp4").is_file(), f"re --no-review: {done.json}")
    finally:
        _stop(ctx)


def review_pages_and_frames(ctx: Context):
    """PLAY-103: rf pages hold 12 consecutive frames with nextPage/totalPages; --frame renders one frame;
    out-of-range frames, --page with --frame and a non-PNG output (the recording itself included) are
    refused."""
    ctx.open_studio()
    video = ctx.root / "r.mp4"
    ctx.rbx("rs", "--studio", "-o", str(video), "--max-seconds", "3", "--fps", "10", timeout=120)
    time.sleep(4)
    frames = ctx.rbx("re", timeout=120).get("frames") or 0
    pages = math.ceil(frames / 12)
    first = ctx.rbx("rf", str(video), "--page", "1", timeout=120)
    numbers = [f.get("frame") for f in first.get("frames") or []]
    ctx.expect(numbers == list(range(1, min(12, frames) + 1)) and first.get("totalPages") == pages, f"page 1: {numbers}, {first.get('totalPages')} pages of {frames} frames")
    ctx.expect(first.get("nextPage") == (2 if pages > 1 else None), f"page 1 nextPage: {first.get('nextPage')}")
    last = ctx.rbx("rf", str(video), "--page", str(pages), timeout=120)
    ctx.expect(last.get("nextPage") is None, f"last page nextPage: {last.get('nextPage')}")
    one = ctx.rbx("rf", str(video), "--frame", "5", timeout=120)
    ctx.expect(one.get("mode") == "frame" and str(one.get("path")).replace("\\", "/").endswith("r.mp4.review/frame-000005.png") and Path(str(one.get("path"))).is_file(), f"--frame 5: {one.json}")
    ctx.expect_in("out of range", ctx.rbx("rf", str(video), "--frame", "9999", ok=False, timeout=60).error, "--frame 9999")
    both = ctx.rbx("rf", str(video), "--page", "1", "--frame", "2", ok=False, timeout=60)
    ctx.expect(both.code == 2 and "cannot be used with" in both.error, f"--page with --frame: {both.code} {both.error[:300]}")
    ctx.expect_in("must be a .png file", ctx.rbx("rf", str(video), "-o", str(ctx.root / "x.jpg"), ok=False, timeout=60).error, "-o x.jpg")
    ctx.expect_in("must be a .png file", ctx.rbx("rf", str(video), "-o", str(video), ok=False, timeout=60).error, "-o the recording")


def capture_paths_relative_to_the_cwd(ctx: Context):
    """PLAY-104 (D7): sc -o and rs -o with relative paths run from a subfolder land in that folder; while
    D7 stands rs -o lands in the project root. rs without -o names a recording_*.mp4 path."""
    ctx.open_studio()
    src = ctx.root / "src"
    shot = ctx.rbx("sc", "--studio", "-o", "rel.png", cwd=src, timeout=120)
    ctx.expect((src / "rel.png").is_file(), f"sc -o rel.png from src: {shot.get('path')}")
    started = ctx.rbx("rs", "--studio", "-o", "rel.mp4", "--max-seconds", "2", cwd=src, timeout=120)
    time.sleep(3)
    ended = ctx.rbx("re", str(started.get("recordingId")), cwd=src, timeout=120)
    default = ctx.rbx("rs", "--studio", "--max-seconds", "2", cwd=src, timeout=120)
    time.sleep(3)
    ctx.rbx("re", str(default.get("recordingId")), cwd=src, timeout=120)
    ctx.expect(Path(str(default.get("path"))).name.startswith("recording_"), f"rs without -o: {default.json}")
    ctx.expect((src / "rel.mp4").is_file(), f"rs -o rel.mp4 from src (D7) wrote {ended.get('path')}")


def recording_conflicts_and_validation(ctx: Context):
    """PLAY-105: one recording at a time; re with a wrong id keeps it; a second re has nothing to stop;
    bad fps, duration and output extension are refused."""
    ctx.open_studio()
    started = ctx.rbx("rs", "--studio", "-o", str(ctx.root / "a.mp4"), timeout=120)
    try:
        ctx.expect_in("is still active", ctx.rbx("rs", "--studio", "-o", str(ctx.root / "b.mp4"), ok=False, timeout=60).error, "second rs")
        ctx.expect_in("is active, not wrongid", ctx.rbx("re", "wrongid", ok=False, timeout=60).error, "re wrongid")
        ctx.rbx("re", str(started.get("recordingId")), timeout=120)
    except Exception:
        ctx.rbx("re", ok=None, timeout=120)
        raise
    ctx.expect_in("no recording is active", ctx.rbx("re", ok=False, timeout=60).error, "re without a recording")
    for args, needle in ((("--fps", "0"), "Invalid record-start fps"), (("--max-seconds", "301"), "maxSeconds"), (("-o", str(ctx.root / "x.avi")), "expected a .mp4 path")):
        refused = ctx.rbx("rs", "--studio", *args, ok=False, timeout=60)
        ctx.expect_in(needle, refused.error, f"rs {' '.join(args)}")


def stop_recording_behind_an_edit_holder(ctx: Context):
    """PLAY-106 (D11): re answers within 2 s while a 12 s Edit run holds the Edit gate, so the recording
    stops when asked; while D11 stands re waits for the holder and the recording overruns."""
    ctx.open_studio()
    ctx.rbx("rs", "--studio", "-o", str(ctx.root / "g.mp4"), "--max-seconds", "30", timeout=120)
    holder = _bg(ctx, "l", "--edit", "task.wait(12) return 1", "-t", "20", timeout=60)
    time.sleep(1)
    ended = ctx.rbx("re", timeout=120)
    holder.result(timeout=60)
    ctx.expect(ended.seconds < 2 and (ended.get("durationMs") or 0) < 5000, f"re behind the holder ({ended.seconds:.1f}s, durationMs {ended.get('durationMs')}, D11)")


# ---------------------------------------------------------------- device, network, render quality


def device_simulator_edit_only(ctx: Context):
    """PLAY-110: dev list, set with orientation and scaling, status; the simulation is reapplied after a
    save and reopen; dev stop twice reports alreadyStopped the second time; dev is refused during Play."""
    ctx.open_studio()
    try:
        listing = ctx.rbx("dev", "list", timeout=120)
        ctx.expect((listing.get("count") or 0) > 0 and listing.get("devices"), f"dev list: {str(listing.json)[:300]}")
        chosen = ctx.rbx("dev", "set", "iPhone 16 Pro", "--orientation", "portrait", "--scaling", "fit", timeout=120)
        ctx.expect(chosen.pointer("/device/name") == "iPhone 16 Pro" and "device" in (chosen.get("changed") or []), f"dev set: {chosen.json}")
        ctx.expect(chosen.get("orientation") is not None or chosen.pointer("/device/orientation") is not None, f"orientation in dev set: {chosen.json}")
        status = ctx.rbx("dev", "status", timeout=60)
        ctx.expect(status.get("simulating") is True and status.pointer("/device/id") == "iphone_16_pro", f"dev status: {status.json}")
        ctx.close_studio()
        ctx.reopen_studio()
        reopened = _poll(ctx, lambda: (lambda r: r if r.pointer("/device/id") == "iphone_16_pro" else None)(ctx.rbx("dev", "status", ok=None, timeout=60)), 20, 1)
        ctx.expect(reopened and reopened.get("simulating") is True, f"dev status after reopen: {reopened and reopened.json}")
        stopped = ctx.rbx("dev", "stop", timeout=120)
        ctx.expect(stopped.get("stopped") is True and stopped.get("alreadyStopped") is False, f"dev stop: {stopped.json}")
        again = ctx.rbx("dev", "stop", timeout=120)
        ctx.expect(again.get("alreadyStopped") is True, f"second dev stop: {again.json}")
        _play(ctx)
        try:
            ctx.expect_in("Stop Play", ctx.rbx("dev", "status", ok=False, timeout=60).error, "dev during Play")
        finally:
            _stop(ctx)
    finally:
        ctx.rbx("dev", "stop", ok=None, timeout=120)


def device_errors(ctx: Context):
    """PLAY-111: unknown and ambiguous devices, bad resolutions and densities are refused; during Play
    dev status is refused while dev list still answers."""
    ctx.open_studio()
    try:
        cases = [
            (("set", "NoSuchPhone"), "Unknown device"),
            (("set", "iPhone 1"), "is ambiguous"),
            (("set", "iPhone 16 Pro", "--resolution", "0x5"), "positive 32-bit"),
            (("set", "iPhone 16 Pro", "--resolution", "abc"), "Invalid resolution"),
            (("set", "iPhone 16 Pro", "--pixel-density=-1"), "Pixel density"),
        ]
        for args, needle in cases:
            ctx.expect_in(needle, ctx.rbx("dev", *args, ok=False, timeout=60).error, f"dev {' '.join(args)}")
        _play(ctx)
        try:
            ctx.expect_in("Stop Play before reading or changing device simulation", ctx.rbx("dev", "status", ok=False, timeout=60).error, "dev status during Play")
            ctx.expect(ctx.rbx("dev", "list", timeout=60).ok, "dev list during Play")
        finally:
            _stop(ctx)
    finally:
        ctx.rbx("dev", "stop", ok=None, timeout=120)


def network_simulation_settings(ctx: Context):
    """PLAY-112: net presets lists templates offline; show reports inactive defaults; set with a delay
    activates it; a preset with an override keeps the override; reset restores."""
    ctx.open_studio()
    try:
        presets = ctx.rbx("net", "presets", timeout=60)
        ctx.expect(len(presets.get("presets") or []) == 4 and presets.get("units"), f"net presets: {presets.json}")
        shown = ctx.rbx("net", "show", timeout=60)
        ctx.expect(shown.get("active") is False and shown.get("preset") is None and shown.pointer("/settings/inDelay") == 0, f"net show: {shown.json}")
        changed = ctx.rbx("net", "set", "--in-delay", "120", timeout=60)
        ctx.expect(changed.get("pid") and changed.get("changed"), f"net set: {changed.json}")
        after = ctx.rbx("net", "show", timeout=60)
        ctx.expect(after.pointer("/settings/inDelay") == 120 and after.get("active") is True, f"net after set: {after.json}")
        mid = ctx.rbx("net", "set", "--preset", "mid", "--out-loss", "0.2", timeout=60)
        shown_mid = ctx.rbx("net", "show", timeout=60)
        ctx.expect(abs((shown_mid.pointer("/settings/outLoss") or 0) - 0.2) < 1e-6 and shown_mid.pointer("/settings/inDelay") == 50 and mid.get("preset") == "mid", f"mid with outLoss 0.2: {mid.json} / {shown_mid.json}")
    finally:
        ctx.rbx("net", "reset", ok=None, timeout=60)
    back = ctx.rbx("net", "show", timeout=60)
    ctx.expect(back.get("active") is False and back.pointer("/settings/inDelay") == 0, f"net reset: {back.json}")


def network_validation(ctx: Context):
    """PLAY-113: net refuses empty sets, --preset outside set, out-of-range values, restore without a
    player, a zero player and presets with settings."""
    ctx.open_studio()
    cases = [
        (("set",), "set needs at least one value"),
        (("show", "--preset", "mid"), "Only net set accepts --preset"),
        (("set", "--out-loss", "0.6"), "between 0 and 0.5"),
        (("set", "--in-delay", "1001"), "between 0 and 1000"),
        (("restore",), "Restore requires --player"),
        (("set", "--player", "0", "--in-delay", "5"), "positive index"),
        (("presets", "--player", "1"), "offline"),
    ]
    try:
        for args, needle in cases:
            ctx.expect_in(needle, ctx.rbx("net", *args, ok=False, timeout=60).error, f"net {' '.join(args)}")
    finally:
        ctx.rbx("net", "reset", ok=None, timeout=60)


def network_per_client(ctx: Context):
    """PLAY-114: during a multi-client test net changes one named client and leaves the others; a change
    without --player is refused; restore undoes it; in ordinary Play the client shares the Edit
    process, and after the stop the Edit window is inactive again."""
    _play(ctx, "-p", "2")
    try:
        mid = ctx.rbx("net", "set", "--player", "1", "--preset", "mid", timeout=60)
        ctx.expect(mid.get("preset") == "mid" or ctx.rbx("net", "show", "--player", "1", timeout=60).get("preset") == "mid", f"p1 mid: {mid.json}")
        other = ctx.rbx("net", "show", "--player", "2", timeout=60)
        ctx.expect(other.get("active") is False, f"p2 untouched: {other.json}")
        ctx.expect_in("Use --player to change network simulation during Play", ctx.rbx("net", "set", "--preset", "poor", ok=False, timeout=60).error, "net set without --player")
        restored = ctx.rbx("net", "restore", "--player", "1", timeout=60)
        ctx.expect(restored.ok and ctx.rbx("net", "show", "--player", "1", timeout=60).get("active") is False, f"p1 restored: {restored.json}")
    finally:
        ctx.rbx("net", "restore", "--player", "1", ok=None, timeout=60)
        _stop(ctx)
    started = _play(ctx)
    try:
        solo = ctx.rbx("net", "set", "--player", "1", "--preset", "high", timeout=60)
        ctx.expect(solo.get("pid") == started.get("editPid"), f"solo client pid {solo.get('pid')} vs editPid {started.get('editPid')}")
        ctx.rbx("net", "show", timeout=60)
    finally:
        _stop(ctx)
    try:
        after = ctx.rbx("net", "show", timeout=60)
        ctx.expect(after.get("active") is False, f"Edit after the solo stop: {after.json}")
    finally:
        ctx.rbx("net", "reset", ok=None, timeout=60)


def graphics_quality_show_set_restore(ctx: Context):
    """PLAY-115: gfx shows both levels; set, max and auto change them; restore returns the first values;
    an empty set, an out-of-range level and max with a level are refused."""
    ctx.open_studio()
    shown = ctx.rbx("gfx", "show", timeout=60)
    ctx.expect(isinstance(shown.pointer("/settings/edit"), int) and isinstance(shown.pointer("/settings/play"), int) and shown.get("maximum") == 21, f"gfx show: {shown.json}")
    before = (shown.pointer("/settings/edit"), shown.pointer("/settings/play"))
    try:
        changed = ctx.rbx("gfx", "set", "--edit", "5" if before[0] != 5 else "6", timeout=60)
        ctx.expect(changed.get("changed"), f"gfx set: {changed.json}")
        maxed = ctx.rbx("gfx", "max", timeout=60)
        ctx.expect((maxed.pointer("/settings/edit"), maxed.pointer("/settings/play")) == (21, 21), f"gfx max: {maxed.json}")
        auto = ctx.rbx("gfx", "auto", timeout=60)
        ctx.expect((auto.pointer("/settings/edit"), auto.pointer("/settings/play")) == (0, 0), f"gfx auto: {auto.json}")
        ctx.expect_in("needs at least one", ctx.rbx("gfx", "set", ok=False, timeout=60).error, "gfx set alone")
        high = ctx.rbx("gfx", "set", "--edit", "22", ok=False, timeout=60)
        ctx.expect(high.code == 2 and "1 to 21" in high.error, f"gfx set --edit 22: {high.code} {high.error[:300]}")
        ctx.expect_in("sets both levels itself", ctx.rbx("gfx", "max", "--edit", "3", ok=False, timeout=60).error, "gfx max --edit 3")
    finally:
        restored = ctx.rbx("gfx", "restore", ok=None, timeout=60)
    ctx.expect(restored.get("restored") is True, f"gfx restore: {restored.json}")
    final = ctx.rbx("gfx", "show", timeout=60)
    ctx.expect((final.pointer("/settings/edit"), final.pointer("/settings/play")) == before, f"quality restored: {final.json} vs {before}")


def graphics_quality_per_client(ctx: Context):
    """PLAY-116: gfx max --player 1 changes that client's process only; in ordinary Play the change is
    undone when the session ends."""
    ctx.open_studio()
    before = ctx.rbx("gfx", timeout=60)
    edit_before = (before.pointer("/settings/edit"), before.pointer("/settings/play"))
    _play(ctx, "-p", "2")
    try:
        two_before = ctx.rbx("gfx", "--player", "2", timeout=60)
        maxed = ctx.rbx("gfx", "max", "--player", "1", timeout=60)
        ctx.expect((maxed.pointer("/settings/edit"), maxed.pointer("/settings/play")) == (21, 21), f"p1 max: {maxed.json}")
        two_after = ctx.rbx("gfx", "--player", "2", timeout=60)
        ctx.expect(two_after.get("settings") == two_before.get("settings"), f"p2 changed: {two_before.json} -> {two_after.json}")
    finally:
        ctx.rbx("gfx", "restore", "--player", "1", ok=None, timeout=60)
        _stop(ctx)
    _play(ctx)
    try:
        ctx.rbx("gfx", "max", "--player", "1", timeout=60)
    finally:
        _stop(ctx)
    try:
        after = ctx.rbx("gfx", timeout=60)
        ctx.expect((after.pointer("/settings/edit"), after.pointer("/settings/play")) == edit_before, f"Edit values after the solo stop: {after.json} vs {edit_before}")
    finally:
        ctx.rbx("gfx", "restore", ok=None, timeout=60)


# ---------------------------------------------------------------- perf, pf, audio, access


def perf_counters_and_capture(ctx: Context):
    """PLAY-120: perf reads Edit counters; a server capture completes with frame statistics and exports
    a JSON file whose frames match frameCount."""
    ctx.open_studio()
    counters = ctx.rbx("perf", timeout=120)
    ctx.expect(isinstance(counters.pointer("/counters/InstanceCount"), (int, float)), f"perf: {counters.json}")
    _play(ctx)
    try:
        started = ctx.rbx("perf", "start", "--server", "--seconds", "2", timeout=60)
        capture = started.get("captureId")
        ctx.expect(capture, f"perf start: {started.json}")
        time.sleep(3)
        read = ctx.rbx("perf", "read", "--server", "--capture", str(capture), timeout=60)
        ctx.expect(read.get("state") == "complete" and (read.get("frameCount") or 0) > 0, f"perf read: {str(read.json)[:600]}")
        out = ctx.root / "p.json"
        ctx.rbx("perf", "export", "--server", "--capture", str(capture), "--out", str(out), timeout=60)
        exported = json.loads(out.read_text(encoding="utf-8"))
        ctx.expect(len(exported.get("frames") or []) == read.get("frameCount"), f"exported frames {len(exported.get('frames') or [])} vs frameCount {read.get('frameCount')}")
    finally:
        _stop(ctx)


def perf_refusals(ctx: Context):
    """PLAY-121: a running capture cannot be exported; read and export need a capture id and --out;
    --frames belongs to micro-start; heap needs Play."""
    ctx.open_studio()
    ctx.expect_in("running Play session", ctx.rbx("perf", "heap", "--out", str(ctx.root / "h.json"), ok=False, timeout=60).error, "perf heap in Edit")
    ctx.expect_in("require a capture ID", ctx.rbx("perf", "read", ok=False, timeout=60).error, "perf read without --capture")
    ctx.expect_in("Only micro-start accepts --frames", ctx.rbx("perf", "start", "--frames", "300", ok=False, timeout=60).error, "perf start --frames")
    ctx.expect_in("require --out", ctx.rbx("perf", "export", "--capture", "x", ok=False, timeout=60).error, "perf export without --out")
    _play(ctx)
    try:
        started = ctx.rbx("perf", "start", "--server", "--seconds", "60", timeout=60)
        out = ctx.root / "x.json"
        early = ctx.rbx("perf", "export", "--server", "--capture", str(started.get("captureId")), "--out", str(out), ok=False, timeout=60)
        ctx.expect_in("Stop the capture before exporting", early.error, "export of a running capture")
        ctx.expect(not out.exists(), "x.json written for a running capture")
        ctx.rbx("perf", "stop", "--server", "--capture", str(started.get("captureId")), ok=None, timeout=60)
    finally:
        _stop(ctx)


def heap_finds_a_leak(ctx: Context):
    """PLAY-122: perf heap on the server reports an unparented Part kept alive by a script reference."""
    _play(ctx)
    try:
        ctx.rbx("l", "--server", "local p = Instance.new('Part') p.Name = 'Leaked' p.Parent = workspace shared.keep = p p.Parent = nil return true", timeout=60)
        ctx.rbx("l", "--server", "--detach", "holder", "local p = Instance.new('Part') p.Name = 'LeakedRunner' p.Parent = workspace p:Destroy() state.keep = p task.wait(120)", timeout=60)
        out = ctx.root / "h.json"
        heap = ctx.rbx("perf", "heap", "--server", "--out", str(out), timeout=180)
        summary = heap.get("summary") or {}
        ctx.expect((heap.get("bytes") or 0) > 0, f"perf heap: {sorted(heap.json)}")
        listed = json.dumps(summary.get("unparentedInstanceReferences"))
        raw = out.read_text(encoding="utf-8")
        saved = json.loads(raw)
        unparented = (saved.get("Refs") or {}).get("UnparentedReferences")
        roots = json.dumps(unparented if unparented else (saved.get("Refs") or {}).get("Roots"))
        ctx.expect(re.search(r"Leaked(?!Runner)", roots), f"Part held by shared.keep not among the file's roots (runner-held part listed: {'LeakedRunner' in roots}; Refs keys {sorted(saved.get('Refs') or {})}): {roots[:300]}")
        ctx.expect(listed and all(isinstance(e.get("instances"), int) for e in summary.get("unparentedInstanceReferences") or []), f"summary lists the largest root references: {listed[:300]}")
        ctx.expect("Graph" in (saved.get("Report") or {}) and "Roots" in (saved.get("Refs") or {}), f"heap file keys: {list(saved)[:10]}")
        ctx.expect(heap.get("target") == "server", f"perf heap target: {heap.get('target')!r} (keys {sorted(heap.json)})")
    finally:
        _stop(ctx)


def microprofiler_capture(ctx: Context):
    """PLAY-123: micro-start, micro and micro-stop on a client write .gprx dumps with metadata; analyze
    reads at most --top frames."""
    _play(ctx)
    try:
        started = ctx.rbx("perf", "micro-start", "--player", "1", "--frames", "64", timeout=60)
        ctx.expect(started.ok, f"micro-start: {started.json}")
        time.sleep(2)
        dump = ctx.root / "m.gprx"
        snap = ctx.rbx("perf", "micro", "--player", "1", "--out", str(dump), timeout=120)
        ctx.expect((snap.get("frames") or 0) >= 0 and dump.is_file() and Path(str(dump) + ".json").is_file(), f"perf micro: {snap.json}")
        final = ctx.root / "s.gprx"
        ctx.rbx("perf", "micro-stop", "--player", "1", "--out", str(final), timeout=120)
        ctx.expect(final.is_file(), "s.gprx written")
    finally:
        ctx.rbx("perf", "micro-stop", "--player", "1", ok=None, timeout=60)
        _stop(ctx)
    analysis = ctx.rbx("perf", "analyze", str(final), "--top", "5", timeout=300)
    frames = analysis.get("frames") or []
    ctx.expect(analysis.ok and len(frames) <= 5 and all("scopes" in f for f in frames), f"perf analyze: {str(analysis.json)[:600]}")


def performance_profile(ctx: Context):
    """PLAY-124: pf ls lists tiers; pf use applies a profile to the Studio pid and again after a save and
    reopen; pf off clears it."""
    ctx.open_studio()
    _only_scratch_studio(ctx)
    try:
        tiers = ctx.rbx("pf", "ls", timeout=60)
        ctx.expect("iphone-11" in json.dumps(tiers.json), f"pf ls: {str(tiers.json)[:400]}")
        ctx.rbx("pf", "use", "iphone-11", timeout=120)
        shown = ctx.rbx("pf", "show", timeout=60)
        pid = ctx.rbx("cs", timeout=60).pointer("/clients/0/pid")
        ctx.expect(str(pid) in json.dumps(shown.json) and "iphone-11" in json.dumps(shown.json), f"pf show on pid {pid}: {shown.json}")
        ctx.close_studio()
        ctx.reopen_studio()
        new_pid = ctx.rbx("cs", timeout=60).pointer("/clients/0/pid")
        again = _poll(ctx, lambda: (lambda r: r if str(new_pid) in json.dumps(r.json) else None)(ctx.rbx("pf", "show", timeout=60)), 20, 1)
        ctx.expect(again, f"pf show after reopen lacks pid {new_pid}")
    finally:
        off = ctx.rbx("pf", "off", ok=None, timeout=120)
    ctx.expect(off.get("phase") == "off" or ctx.rbx("pf", "status", ok=None, timeout=60).get("phase") == "off", f"pf off: {off.json}")


def advanced_performance_profile(ctx: Context):
    """PLAY-125: pf adv applies and saves exact constraints and pf use reuses them; unsafe, invalid,
    duplicated and built-in-shadowing values and unknown profiles are refused."""
    ctx.open_studio()
    _only_scratch_studio(ctx)
    try:
        applied = ctx.rbx("pf", "adv", "cpu=25", "cores=2", "headroom=1g", "prio=low", "save=slow-test", timeout=120)
        ctx.expect(applied.ok, f"pf adv: {applied.json}")
        ctx.expect(ctx.rbx("pf", "use", "slow-test", timeout=120).ok, "pf use slow-test")
        cases = [
            (("mem=1g",), "risk=crash"),
            (("cpu=0",), "from 0.01"),
            (("headroom=1g", "mem=1g", "risk=crash"), "headroom or mem"),
            (("foo=1",), "Unknown advanced setting"),
            (("cpu=1", "cpu=2"), "twice"),
            (("cpu=10", "save=iphone-11"), "built-in"),
        ]
        for values, needle in cases:
            ctx.expect_in(needle, ctx.rbx("pf", "adv", *values, ok=False, timeout=60).error, f"pf adv {' '.join(values)}")
        ctx.expect_in("Unknown performance profile", ctx.rbx("pf", "use", "nope", ok=False, timeout=60).error, "pf use nope")
    finally:
        ctx.rbx("pf", "off", ok=None, timeout=120)


def audio_mute_and_unmute(ctx: Context):
    """PLAY-126: audio status reports the mode and sessions; mute mutes every session, unmute and off clear
    it, auto mutes the unfocused scratch Studio; a client's mute leaves the other client alone. The runner
    keeps every scratch Studio in global auto mode, so the row clears that first and restores it after."""
    ctx.open_studio()
    ctx.rbx("audio", "off", "--global", ok=None, timeout=60)
    try:
        status = ctx.rbx("audio", timeout=60)
        ctx.expect(all(k in status.json for k in ("mode", "focused", "sessions", "mutedSessions")), f"audio status: {status.json}")
        muted = ctx.rbx("audio", "mute", timeout=60)
        ctx.expect(muted.get("mode") == "mute" and (muted.get("mutedSessions") or 0) >= 1 and muted.get("mutedSessions") == muted.get("sessions"), f"audio mute: {muted.json}")
        unmuted = ctx.rbx("audio", "unmute", timeout=60)
        ctx.expect(unmuted.get("mode") == "off" and unmuted.get("mutedSessions") == 0, f"audio unmute: {unmuted.json}")
        auto = ctx.rbx("audio", "auto", timeout=60)
        ctx.expect(auto.get("mode") == "auto" and (auto.get("focused") is True or (auto.get("mutedSessions") or 0) >= 1), f"audio auto unfocused: {auto.json}")
        ctx.rbx("audio", "off", timeout=60)
        _play(ctx, "-p", "2")
        try:
            one = ctx.rbx("audio", "mute", "--player", "1", timeout=60)
            ctx.expect((one.get("mutedSessions") or 0) >= 1, f"audio mute --player 1: {one.json}")
            two = ctx.rbx("audio", "status", "--player", "2", timeout=60)
            ctx.expect(two.get("mutedSessions") == 0, f"audio status --player 2: {two.json}")
        finally:
            ctx.rbx("audio", "off", "--player", "1", ok=None, timeout=60)
            _stop(ctx)
    finally:
        ctx.rbx("audio", "auto", "--global", ok=None, timeout=60)
        ctx.rbx("audio", "off", ok=None, timeout=60)


def audio_global_mode(ctx: Context):
    """PLAY-127: audio auto --global covers a Studio window opened after it; off --global releases it."""
    ctx.open_studio()
    _only_scratch_studio(ctx)
    try:
        ctx.rbx("audio", "auto", "--global", timeout=60)
        ctx.close_studio()
        ctx.reopen_studio()
        _only_scratch_studio(ctx)
        status = _poll(ctx, lambda: (lambda r: r if r.get("mode") == "auto" else None)(ctx.rbx("audio", "status", timeout=60)), 15, 1)
        ctx.expect(status, "the reopened Studio is not in global auto mode")
    finally:
        released = ctx.rbx("audio", "off", "--global", ok=None, timeout=60)
    ctx.expect(released.ok, f"audio off --global: {released.json}")
    after = ctx.rbx("audio", "status", timeout=60)
    ctx.expect(after.get("mode") == "off", f"audio after off --global: {after.json}")
    ctx.rbx("audio", "auto", "--global", ok=None, timeout=60)


def access_approvals(ctx: Context):
    """PLAY-128: ask mode answers a protected read with one approval request per exact request; approve
    runs it once; a multi-target read is one approval for every row."""
    ctx.open_studio()
    try:
        mode = ctx.rbx("access", "mode", timeout=60)
        ctx.expect(mode.get("mode") == "ask", f"access mode: {mode.json}")
        first = ctx.rbx("access", "read", "Workspace", "StreamingEnabled", ok=None, timeout=60)
        if _mac_without_locator(first):
            return
        second = ctx.rbx("access", "read", "Workspace", "StreamingEnabled", ok=None, timeout=60)
        ctx.expect(first.get("status") == "approval-required" and first.get("requestId") and first.get("requestId") == second.get("requestId"), f"approval requests: {first.json} / {second.json}")
        approved = ctx.rbx("access", "approve", str(first.get("requestId")), timeout=60)
        ctx.expect("value" in json.dumps(approved.json), f"approve: {approved.json}")
        ctx.expect_in("already used", ctx.rbx("access", "approve", str(first.get("requestId")), ok=False, timeout=60).error, "second approve")
        multi = ctx.rbx("access", "read", "Workspace.Props", "Name", "Workspace.Props.Sign", ok=None, timeout=60)
        if multi.get("status") == "approval-required":
            multi = ctx.rbx("access", "approve", str(multi.get("requestId")), timeout=60)
        ctx.expect(len(multi.get("results") or []) == 2, f"multi-target read: {multi.json}")
    finally:
        ctx.rbx("access", "mode", "ask", ok=None, timeout=60)


def access_modes_and_refusals(ctx: Context):
    """PLAY-129: read-only rejects writes; read-write needs --accept-risk and --accept-risk only applies to
    it; a rejected request cannot be approved; play clients and more than 64 targets are refused."""
    ctx.open_studio()
    try:
        ctx.rbx("access", "mode", "read-only", timeout=60)
        refused = ctx.rbx("access", "write", "Workspace", "StreamingEnabled", "true", ok=False, timeout=60)
        if _mac_without_locator(refused):
            return
        ctx.expect_in("Read-only mode rejects", refused.error, "write in read-only")
        ctx.expect_in("requires --accept-risk", ctx.rbx("access", "mode", "read-write", ok=False, timeout=60).error, "read-write without --accept-risk")
        ctx.expect_in("applies only to read-write", ctx.rbx("access", "mode", "ask", "--accept-risk", ok=False, timeout=60).error, "ask --accept-risk")
        ctx.rbx("access", "mode", "ask", timeout=60)
        pending = ctx.rbx("access", "read", "Workspace", "StreamingEnabled", ok=None, timeout=60)
        rejected = ctx.rbx("access", "reject", str(pending.get("requestId")), timeout=60)
        ctx.expect(rejected.get("status") == "rejected", f"reject: {rejected.json}")
        ctx.expect_in("unknown", ctx.rbx("access", "approve", str(pending.get("requestId")), ok=False, timeout=60).error.lower(), "approve after reject")
        targets = ["Workspace"] * 65
        ctx.expect_in("Read up to 64", ctx.rbx("access", "read", targets[0], "Name", *targets[1:], ok=False, timeout=60).error, "65 targets")
        _play(ctx)
        try:
            ctx.expect_in("require Edit mode", ctx.rbx("access", "read", "Workspace", "StreamingEnabled", "-p", "1", ok=False, timeout=60).error, "access on a play client")
        finally:
            _stop(ctx)
    finally:
        ctx.rbx("access", "mode", "ask", ok=None, timeout=60)


def collision_fidelity_allowlist(ctx: Context):
    """PLAY-130: CollisionFidelity on a MeshPart is written and verified without approval in ask mode; an
    invalid value names the allowed ones. A MeshPart without a mesh keeps Box (Hull falls back to Box,
    and the write then reports that it was not verified), so the row writes Box."""
    ctx.open_studio()
    _clean(ctx)
    ctx.rbx("l", "--edit", "local m = Instance.new('MeshPart') m.Name = 'Mesh' m.Parent = workspace return true", timeout=60)
    written = ctx.rbx("access", "write", "Workspace.Mesh", "CollisionFidelity", "Box", ok=None, timeout=60)
    if _mac_without_locator(written):
        return
    after = ctx.luau("return workspace.Mesh.CollisionFidelity.Name")
    ctx.expect(written.ok and written.get("status") != "approval-required", f"allowlisted write (value afterwards {after!r}): {written.json}")
    ctx.expect_in("must be Default, Hull, Box, or PreciseConvexDecomposition", ctx.rbx("access", "write", "Workspace.Mesh", "CollisionFidelity", "Bogus", ok=False, timeout=60).error, "bogus fidelity")
    try:
        ctx.expect(after == "Box", f"CollisionFidelity after the write: {after!r}")
    finally:
        _clean(ctx)


def approval_expiry_and_revocation(ctx: Context):
    """PLAY-131: an approval request expires after 300 s; a mode change revokes pending requests; a
    protected call needs approval before it runs."""
    ctx.open_studio()
    try:
        pending = ctx.rbx("access", "read", "Workspace", "StreamingEnabled", ok=None, timeout=60)
        revoked = ctx.rbx("access", "read", "Workspace", "Name", ok=None, timeout=60)
        ctx.rbx("access", "mode", "read-only", timeout=60)
        ctx.rbx("access", "mode", "ask", timeout=60)
        ctx.expect_in("unknown", ctx.rbx("access", "approve", str(revoked.get("requestId") or pending.get("requestId")), ok=False, timeout=60).error.lower(), "approve after a mode change")
        call = ctx.rbx("access", "call", "Workspace", "Destroy", "[]", ok=None, timeout=60)
        ctx.expect(not call.ok or call.get("status") == "approval-required", f"access call Workspace Destroy: {call.json}")
        ctx.expect(ctx.luau("return workspace ~= nil and workspace.Parent == game") is True, "Workspace still in place")
        fresh = ctx.rbx("access", "read", "Workspace", "StreamingEnabled", ok=None, timeout=60)
        time.sleep(305)
        expired = ctx.rbx("access", "approve", str(fresh.get("requestId")), ok=False, timeout=60)
        ctx.expect("expired" in expired.error.lower() or "unknown" in expired.error.lower(), f"approve after 305 s: {expired.error[:300]}")
    finally:
        ctx.rbx("access", "mode", "ask", ok=None, timeout=60)


SCENARIOS = [
    Scenario("PLAY-001", "solo play starts and stops", "small", solo_play_starts_and_stops),
    Scenario("PLAY-002", "second start reports the running session", "small", second_start_reports_the_running_session),
    Scenario("PLAY-003", "--mode run and server", "small", run_and_server_modes),
    Scenario("PLAY-004", "play refusals before Studio", "small", play_refusals_before_studio),
    Scenario("PLAY-005", "-r keeps or overrides the shape", "small", restart_keeps_or_overrides_the_shape),
    Scenario("PLAY-006", "-r without a known launch", "small", restart_without_a_known_launch),
    Scenario("PLAY-007", "start right after a stop", "small", start_right_after_a_stop),
    Scenario("PLAY-020", "multi-client session lifecycle", "small", multi_client_session_lifecycle),
    Scenario("PLAY-021", "client selectors", "small", client_selectors),
    Scenario("PLAY-022", "--add-players limits", "small", add_players_limits),
    Scenario("PLAY-023", "--leave needs a client index", "small", leave_needs_a_client_index),
    Scenario("PLAY-024", "orphans of a closed Edit window", "small", orphans_of_a_closed_edit_window, exclusive=True),
    Scenario("PLAY-030", "--until, --result and timeout", "small", until_result_and_timeout),
    Scenario("PLAY-031", "bad --until and failing --result", "small", bad_until_and_failing_result),
    Scenario("PLAY-032", "wait forms and timeout", "small", wait_forms_and_timeout),
    Scenario("PLAY-033", "wait across runner slices", "small", wait_across_runner_slices),
    Scenario("PLAY-034", "wait holds its runtime", "small", wait_holds_its_runtime),
    Scenario("PLAY-035", "Edit wait can be satisfied (D9)", "small", edit_wait_can_be_satisfied),
    Scenario("PLAY-036", "wait past 30 minutes (D6)", "small", wait_past_thirty_minutes, long=True),
    Scenario("PLAY-037", "stop during a wait", "small", stop_during_a_wait),
    Scenario("PLAY-040", "luau contexts", "small", luau_contexts),
    Scenario("PLAY-042", "clients via l and lc", "small", clients_via_l_and_lc),
    Scenario("PLAY-043", "lc argument mistakes", "small", lc_argument_mistakes),
    Scenario("PLAY-044", "value shapes", "small", value_shapes),
    Scenario("PLAY-045", "captured output", "small", captured_output),
    Scenario("PLAY-046", "Edit and Play lifetimes", "small", edit_and_play_lifetimes),
    Scenario("PLAY-050", "luau from file and stdin", "small", luau_from_file_and_stdin),
    Scenario("PLAY-051", "luau errors name the runtime", "small", luau_errors),
    Scenario("PLAY-052", "luau timeouts", "small", luau_timeouts),
    Scenario("PLAY-053", "module load failure (D10)", "small", module_load_failure),
    Scenario("PLAY-055", "what waits behind an Edit holder", "small", what_waits_behind_an_edit_holder),
    Scenario("PLAY-056", "play runtimes gate separately", "small", play_runtimes_gate_separately),
    Scenario("PLAY-057", "l -f relative to the cwd (D7)", "small", file_relative_to_the_cwd),
    Scenario("PLAY-060", "detached runner collects state", "small", detached_runner_collects_state),
    Scenario("PLAY-061", "finished, expired and replaced runners", "small", finished_expired_and_replaced_runners),
    Scenario("PLAY-062", "detach refusals", "small", detach_refusals),
    Scenario("PLAY-070", "console reads, filters and clears", "small", console_reads_filters_and_clears),
    Scenario("PLAY-071", "console filters, folding and follow", "small", console_filters_fold_and_follow),
    Scenario("PLAY-075", "client console per player", "small", client_console_per_player),
    Scenario("PLAY-080", "ui lists elements", "small", ui_lists_elements),
    Scenario("PLAY-081", "verified presses", "small", verified_presses, exclusive=True),
    Scenario("PLAY-082", "press ambiguity and refusals", "small", press_ambiguity_and_refusals, exclusive=True),
    Scenario("PLAY-083", "press a world part", "small", press_a_world_part, exclusive=True),
    Scenario("PLAY-084", "viewport clicks", "small", viewport_clicks, exclusive=True),
    Scenario("PLAY-085", "key presses", "small", key_presses, exclusive=True),
    Scenario("PLAY-086", "long key holds (D3)", "small", long_key_holds, exclusive=True, long=True),
    Scenario("PLAY-087", "type into a box", "small", type_into_a_box, exclusive=True),
    Scenario("PLAY-088", "walk and teleport", "small", walk_and_teleport),
    Scenario("PLAY-089", "long walk (D5)", "small", long_walk, long=True),
    Scenario("PLAY-090", "inp keys", "small", inp_keys, exclusive=True),
    Scenario("PLAY-091", "inp long hold (D4)", "small", inp_long_hold, exclusive=True),
    Scenario("PLAY-092", "inp world clicks (D8)", "small", inp_world_clicks, exclusive=True),
    Scenario("PLAY-093", "inp parse errors", "small", inp_parse_errors),
    Scenario("PLAY-100", "screenshots of Studio and a client", "small", screenshots_studio_and_client, platforms=("nt",)),
    Scenario("PLAY-101", "camera for a screenshot", "small", camera_for_a_screenshot, platforms=("nt",)),
    Scenario("PLAY-102", "recording round trip", "small", recording_round_trip, platforms=("nt",)),
    Scenario("PLAY-103", "review pages and frames", "small", review_pages_and_frames, platforms=("nt",)),
    Scenario("PLAY-104", "capture paths relative to the cwd (D7)", "small", capture_paths_relative_to_the_cwd, platforms=("nt",)),
    Scenario("PLAY-105", "recording conflicts and validation", "small", recording_conflicts_and_validation, platforms=("nt",)),
    Scenario("PLAY-106", "stop a recording behind an Edit holder (D11)", "small", stop_recording_behind_an_edit_holder, platforms=("nt",)),
    Scenario("PLAY-110", "device simulator", "small", device_simulator_edit_only, exclusive=True),
    Scenario("PLAY-111", "device errors", "small", device_errors, exclusive=True),
    Scenario("PLAY-112", "network simulation settings", "small", network_simulation_settings, exclusive=True),
    Scenario("PLAY-113", "network validation", "small", network_validation, exclusive=True),
    Scenario("PLAY-114", "network per client", "small", network_per_client, exclusive=True),
    Scenario("PLAY-115", "graphics quality show, set, restore", "small", graphics_quality_show_set_restore, exclusive=True),
    Scenario("PLAY-116", "graphics quality per client", "small", graphics_quality_per_client, exclusive=True),
    Scenario("PLAY-120", "perf counters and capture", "small", perf_counters_and_capture),
    Scenario("PLAY-121", "perf refusals", "small", perf_refusals),
    Scenario("PLAY-122", "heap finds a leak", "small", heap_finds_a_leak),
    Scenario("PLAY-123", "MicroProfiler capture", "small", microprofiler_capture),
    Scenario("PLAY-124", "performance profile", "small", performance_profile, platforms=("nt",), exclusive=True),
    Scenario("PLAY-125", "advanced performance profile", "small", advanced_performance_profile, platforms=("nt",), exclusive=True),
    Scenario("PLAY-126", "audio mute and unmute", "small", audio_mute_and_unmute, exclusive=True),
    Scenario("PLAY-127", "audio global mode", "small", audio_global_mode, exclusive=True),
    Scenario("PLAY-128", "access approvals", "small", access_approvals),
    Scenario("PLAY-129", "access modes and refusals", "small", access_modes_and_refusals),
    Scenario("PLAY-130", "CollisionFidelity allowlist", "small", collision_fidelity_allowlist),
    Scenario("PLAY-131", "approval expiry and revocation", "small", approval_expiry_and_revocation, long=True),
    Scenario("PLAY-132", "play gates follow the request's client", "small", play_gates_follow_the_requests_client),
]
