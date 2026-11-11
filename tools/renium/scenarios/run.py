"""Scenario runner: drives the installed `rbx` against a scratch copy of a fixture
project in a real Studio, one scenario at a time, and reports pass/fail.

    python -I tools/renium/scenarios/run.py --area sync            # one area
    python -I tools/renium/scenarios/run.py --only SYNC-003,SYNC-010
    python -I tools/renium/scenarios/run.py --parallel 4 --rerun-failed   # every row, four Studios
    python -I tools/renium/scenarios/run.py --list

Each scenario module under scenarios/<area>.py exposes SCENARIOS: a list of
Scenario(id, title, size, run) where run(ctx) raises on failure. The runner
copies the fixture, opens its place, runs the scenario, and recovers Studio and
the project between scenarios. Reports land in %LOCALAPPDATA%/Renium/scenario-reports.
"""

from __future__ import annotations

import argparse
import importlib
import json
import os
import random
import shutil
import subprocess
import sys
import time
import traceback
from dataclasses import dataclass, field
from pathlib import Path
from typing import Callable

HERE = Path(__file__).resolve().parent
FIXTURES = {
    "small": Path(os.environ.get("RENIUM_SCENARIO_FIXTURE", r"E:\Documents\rblx\renium-scenario-fixture")),
    "heavy": Path(os.environ.get("RENIUM_SCENARIO_HEAVY_FIXTURE", r"E:\Documents\rblx\renium-push-repro")),
}
SCRATCH_ROOT = Path(os.environ.get("RENIUM_SCENARIO_SCRATCH", r"E:\Documents\rblx\renium-scenario-runs"))
SESSION_ROOT = Path(os.environ.get("RENIUM_SCENARIO_SESSIONS", str(SCRATCH_ROOT) + "-sessions"))
REPORTS = (Path(os.environ.get("LOCALAPPDATA", str(Path.home() / "Library" / "Application Support" if os.name != "nt" else "."))) / "Renium" / "scenario-reports").resolve()
LOCAL = Path(os.environ.get("LOCALAPPDATA", "")) / "Renium" if os.name == "nt" else Path.home() / ".renium"
DATA = LOCAL if os.name == "nt" else Path.home() / "Library" / "Application Support" / "Renium"
RBX = os.environ.get("RENIUM_SCENARIO_RBX", "rbx")


class ScenarioSkip(Exception):
    """Raised by a row that cannot run in this session (another Studio is
    connected and the row changes Studio-wide state); the runner records it
    as skipped with the reason."""


class ScenarioFailure(AssertionError):
    pass


@dataclass
class Scenario:
    id: str
    title: str
    size: str
    run: Callable[["Context"], None]
    needs_studio: bool = True
    platforms: tuple = ("nt", "posix")
    exclusive: bool | str = False
    long: bool = False
    default_daemon: bool = False


@dataclass
class CommandResult:
    args: list[str]
    code: int
    stdout: str
    stderr: str
    seconds: float
    json: dict | list | None = None

    @property
    def ok(self) -> bool:
        if isinstance(self.json, dict) and "ok" in self.json:
            return bool(self.json["ok"])
        return self.code == 0

    def __getitem__(self, key):
        if not isinstance(self.json, dict):
            raise ScenarioFailure(f"{self.args}: no JSON object in output: {self.stdout[:300]!r}")
        return self.json[key]

    def get(self, key, default=None):
        if isinstance(self.json, dict):
            return self.json.get(key, default)
        return default

    def pointer(self, path: str, default=None):
        value = self.json
        for part in path.strip("/").split("/"):
            if isinstance(value, dict) and part in value:
                value = value[part]
            elif isinstance(value, list) and part.isdigit() and int(part) < len(value):
                value = value[int(part)]
            else:
                return default
        return value

    @property
    def error(self) -> str:
        return str(self.get("error") or self.stderr.strip())


@dataclass
class Context:
    scenario: Scenario
    root: Path
    place: str
    log: list[str] = field(default_factory=list)
    studio_open: bool = False
    session: "Session | None" = None
    row_started: float = 0.0

    def rbx(self, *args: str, stdin: str | None = None, timeout: float = 300, json_mode: bool = True, ok: bool | None = True, cwd: Path | None = None) -> CommandResult:
        """Runs rbx in the scratch project (or cwd) with JSON output; by default asserts ok:true."""
        argv = [RBX]
        if json_mode:
            argv += ["--output-mode", "json"]
        argv += list(args)
        env = dict(os.environ, RENIUM_PLACE=str(self.root / self.place))
        started = time.monotonic()
        try:
            completed = run_captured(argv, cwd=cwd or self.root, stdin=stdin, timeout=timeout, env=env)
        except subprocess.TimeoutExpired as error:
            raise ScenarioFailure(f"{' '.join(argv)} did not finish within {timeout}s") from error
        seconds = time.monotonic() - started
        parsed = None
        for stream in (completed.stdout, completed.stderr):
            for line in reversed(stream.strip().splitlines()):
                line = line.strip()
                if not line or line.startswith("[renium]"):
                    continue
                try:
                    parsed = json.loads(line)
                    break
                except json.JSONDecodeError:
                    continue
            if parsed is not None:
                break
        if parsed is None:
            for stream in (completed.stdout, completed.stderr):
                body = chr(10).join(l for l in stream.splitlines() if not l.startswith("[renium]")).strip()
                if body.startswith(("{", "[")):
                    try:
                        parsed = json.loads(body)
                        break
                    except json.JSONDecodeError:
                        continue
        result = CommandResult(argv, completed.returncode, completed.stdout, completed.stderr, seconds, parsed)
        self.log.append(f"$ {' '.join(args)}  [+{time.monotonic() - self.row_started:.0f}s {seconds:.1f}s exit {completed.returncode}]\n{completed.stdout.strip()[:1200]}\n{completed.stderr.strip()[:600]}")
        if ok is True and not result.ok:
            raise ScenarioFailure(f"{' '.join(args)} failed: {result.error[:500]}")
        if ok is False and result.ok:
            raise ScenarioFailure(f"{' '.join(args)} succeeded but was expected to fail: {result.stdout[:300]}")
        return result

    def luau(self, code: str, *, server: bool = False, timeout: float = 120):
        """Runs Luau in Studio and returns the first result value."""
        args = ["l", code] + (["--server"] if server else [])
        result = self.rbx(*args, timeout=timeout)
        results = result.get("results") or []
        return results[0] if results else None

    def write(self, relative: str, content: str) -> Path:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("w", encoding="utf-8", newline="\n") as handle:
            handle.write(content)
        return path

    def read(self, relative: str) -> str:
        return (self.root / relative).read_text(encoding="utf-8")

    def open_studio(self, wait: int = 120):
        space_studio_launch()
        result = self.rbx("so", self.place, json_mode=True, timeout=wait + 30)
        self.studio_open = True
        self.guard_binding()
        return result

    def reopen_studio(self, wait: int = 120):
        space_studio_launch()
        result = self.rbx("ro", json_mode=True, timeout=wait + 30)
        self.studio_open = True
        self.guard_binding()
        return result

    def guard_binding(self):
        """Refuses to go on unless the bound Studio is the scratch place: a
        scratch folder must never act on a Studio it did not open. Commands pin
        the absolute place path, so the bound client must carry that file."""
        expected = str((self.root / self.place).resolve())
        files: list = []
        bound: list = []
        for attempt in range(3):
            status = self.rbx("status", json_mode=True, ok=None, timeout=60)
            bound = [c for c in (status.get("clients") or []) if c.get("role") == "edit"]
            listing = self.rbx("cs", json_mode=True, ok=None, timeout=60)
            local_files = {c.get("pid"): c.get("localFile") for c in (listing.get("clients") or [])}
            files = []
            for client in bound:
                file = local_files.get(client.get("pid")) or client.get("localFile")
                files.append(str(Path(file).resolve()) if file else "")
            if bound and all(file == expected for file in files):
                return
            time.sleep(2)
        raise ScenarioFailure(f"the bound Studio is not the scratch copy {expected}: {files or 'no edit client'}; refusing to act on it")

    def close_studio(self, save: bool = False):
        if not self.studio_open:
            return
        self.guard_binding()
        self.rbx("sx", "--save" if save else "--terminate", ok=None, timeout=120)
        self.studio_open = False
        time.sleep(1.5)

    def expect(self, condition: bool, message: str):
        if not condition:
            raise ScenarioFailure(message)

    def expect_in(self, needle: str, haystack: str, message: str = ""):
        if needle not in haystack:
            raise ScenarioFailure(f"{message or 'expected text'}: {needle!r} not in {haystack[:400]!r}")


def run_captured(argv: list[str], cwd=None, stdin: str | None = None, timeout: float = 300, env=None) -> subprocess.CompletedProcess:
    """Runs a command with its output in temporary files instead of pipes: a
    helper the command leaves behind (a Studio launcher, a worker) may keep a
    pipe open and would block a pipe read forever; files never do. On a
    timeout the whole process tree is killed."""
    import tempfile
    with tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as out, tempfile.TemporaryFile(mode="w+", encoding="utf-8", errors="replace") as err:
        process = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE if stdin is not None else subprocess.DEVNULL, stdout=out, stderr=err, text=True, encoding="utf-8")
        if stdin is not None:
            try:
                process.stdin.write(stdin)
            except OSError:
                pass
            process.stdin.close()
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            if os.name == "nt":
                subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True, timeout=30)
            else:
                process.kill()
            raise
        out.seek(0)
        err.seek(0)
        return subprocess.CompletedProcess(argv, code, out.read(), err.read())


def fresh_scratch(size: str, scenario_id: str) -> tuple[Path, str]:
    source = FIXTURES[size]
    if not source.is_dir():
        raise RuntimeError(f"fixture missing: {source}")
    target = SCRATCH_ROOT / f"{scenario_id}-{int(time.time())}-{os.getpid()}"
    SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
    shutil.copytree(source, target, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
    places = sorted(p.name for p in target.glob("*.rbxl"))
    if not places:
        raise RuntimeError(f"fixture {source} has no .rbxl place")
    return target, places[0]


def path_prefixes(roots: tuple) -> tuple:
    return tuple(str(root.resolve()).lower() + os.sep for root in roots)


def scratch_studio_pids(roots: tuple = (SCRATCH_ROOT,)) -> list[int]:
    """Every Studio process whose window title names a place under the scratch
    root: the runner opened it, so the runner may close it."""
    try:
        completed = run_captured([RBX, "--output-mode", "json", "status", "--all"], timeout=60)
        line = [l for l in completed.stdout.strip().splitlines() if l.startswith("{")][-1]
        status = json.loads(line)
    except Exception:
        return []
    prefixes = path_prefixes(roots)
    pids = []
    for process in (status.get("diagnosis") or {}).get("studioProcesses") or []:
        title = str(process.get("title") or "").lower()
        if title.startswith(prefixes) and process.get("pid"):
            pids.append(int(process["pid"]))
    for client in (status.get("studios") or []) + (status.get("clients") or []):
        file = str(client.get("localFile") or "").lower()
        if file.startswith(prefixes) and client.get("pid"):
            pids.append(int(client["pid"]))
    if os.name != "nt":
        try:
            listing = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True, timeout=30).stdout
        except Exception:
            listing = ""
        for line in listing.splitlines():
            parts = line.strip().split(None, 1)
            if len(parts) == 2 and "RobloxStudio" in parts[1] and any(prefix in parts[1].lower() for prefix in prefixes) and "CrashHandler" not in parts[1]:
                pids.append(int(parts[0]))
    return sorted(set(pids))


RUNNER_STARTED = time.time()


def orphan_play_server_pids() -> list[int]:
    """Play servers and clients a row left behind: the daemon lists them as
    orphans (never connected), they have no window, and they started after
    this runner did, so nothing the user opened is among them."""
    try:
        completed = run_captured([RBX, "--output-mode", "json", "cs"], timeout=60)
        line = [l for l in completed.stdout.strip().splitlines() if l.startswith("{")][-1]
        orphans = json.loads(line).get("orphans") or []
    except Exception:
        return []
    pids = []
    for orphan in orphans:
        if orphan.get("connected") is not False or not orphan.get("pid"):
            continue
        pid = int(orphan["pid"])
        try:
            if os.name == "nt":
                # Windows PowerShell's `Get-Date -UFormat %s` counts from the epoch in
                # local time, which is off by the UTC offset next to time.time().
                query = subprocess.run(["powershell", "-NoProfile", "-Command", f"$p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($p) {{ '{{0}}|{{1}}' -f $p.MainWindowTitle, [DateTimeOffset]::new($p.StartTime).ToUnixTimeSeconds() }}"], capture_output=True, text=True, timeout=30).stdout.strip()
            else:
                query = subprocess.run(["ps", "-o", "lstart=", "-p", str(pid)], capture_output=True, text=True, timeout=30).stdout.strip()
                query = "|" + str(int(time.mktime(time.strptime(query)))) if query else ""
        except Exception:
            continue
        if not query or "|" not in query:
            continue
        title, started = query.rsplit("|", 1)
        if title.strip() == "" and started.isdigit() and int(started) >= int(RUNNER_STARTED) - 5:
            pids.append(pid)
    return pids


def close_scratch_studios_gracefully(roots: tuple = (SCRATCH_ROOT,)):
    """Asks every connected scratch Studio to close without saving through the
    plugin first: a force-killed Studio leaves recovery files and its next
    launch may wait on a dialog instead of connecting."""
    try:
        completed = run_captured([RBX, "--output-mode", "json", "cs"], timeout=60)
        line = [l for l in completed.stdout.strip().splitlines() if l.startswith("{")][-1]
        clients = json.loads(line).get("clients") or []
    except Exception:
        return
    prefixes = path_prefixes(roots)
    for client in clients:
        file = str(client.get("localFile") or "")
        if file.lower().startswith(prefixes):
            try:
                run_captured([RBX, "--place", file, "sx", "--terminate"], timeout=60)
            except subprocess.TimeoutExpired:
                pass
    time.sleep(1.5)


def terminate_pids(pids: list[int]):
    if not pids:
        return
    if os.name == "nt":
        subprocess.run(["powershell", "-NoProfile", "-Command", "Stop-Process -Id " + ",".join(str(p) for p in pids) + " -Force -ErrorAction SilentlyContinue"], capture_output=True, timeout=60)
    else:
        for pid in pids:
            try:
                os.kill(pid, 9)
            except OSError:
                pass
    time.sleep(2)


def studio_pids_by_title(prefix: str) -> list[int]:
    """Studio processes whose window title starts with a path, read from the
    OS so a broken rbx cannot leave a session's Studio behind."""
    wanted = prefix.lower()
    pids = []
    try:
        if os.name == "nt":
            listing = subprocess.run(["powershell", "-NoProfile", "-Command", "Get-Process RobloxStudioBeta -ErrorAction SilentlyContinue | ForEach-Object { '{0}|{1}' -f $_.Id, $_.MainWindowTitle }"], capture_output=True, text=True, timeout=30).stdout
            for line in listing.splitlines():
                pid, _, title = line.strip().partition("|")
                if pid.isdigit() and title.lower().startswith(wanted):
                    pids.append(int(pid))
        else:
            listing = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True, timeout=30).stdout
            for line in listing.splitlines():
                parts = line.strip().split(None, 1)
                if len(parts) == 2 and "RobloxStudio" in parts[1] and wanted in parts[1].lower() and "CrashHandler" not in parts[1]:
                    pids.append(int(parts[0]))
    except Exception:
        pass
    return pids


def place_studio_pids(place: Path) -> list[int]:
    """The Studio processes bound to one place file (a session's own copy)."""
    wanted = str(place.resolve()).lower()
    pids = studio_pids_by_title(wanted)
    try:
        completed = run_captured([RBX, "--output-mode", "json", "status", "--all"], timeout=60)
        line = [l for l in completed.stdout.strip().splitlines() if l.startswith("{")][-1]
        status = json.loads(line)
    except Exception:
        return sorted(set(pids))
    for process in (status.get("diagnosis") or {}).get("studioProcesses") or []:
        if str(process.get("title") or "").lower().startswith(wanted) and process.get("pid"):
            pids.append(int(process["pid"]))
    for client in (status.get("studios") or []) + (status.get("clients") or []):
        if str(client.get("localFile") or "").lower() == wanted and client.get("pid"):
            pids.append(int(client["pid"]))
    if os.name != "nt":
        try:
            listing = subprocess.run(["ps", "-axo", "pid=,command="], capture_output=True, text=True, timeout=30).stdout
        except Exception:
            listing = ""
        for line in listing.splitlines():
            parts = line.strip().split(None, 1)
            if len(parts) == 2 and "RobloxStudio" in parts[1] and wanted in parts[1].lower() and "CrashHandler" not in parts[1]:
                pids.append(int(parts[0]))
    return sorted(set(pids))


def kill_scratch_studios(ctx: Context):
    """Closes the scenario's Studio even when the scenario left it in a bad
    state, but only Studios whose place is a scratch copy."""
    if ctx.session is not None:
        ctx.session.close_studio(ctx)
        return
    try:
        ctx.guard_binding()
        ctx.rbx("sx", "--terminate", ok=None, timeout=60)
        time.sleep(1.5)
    except Exception:
        pass
    close_scratch_studios_gracefully()
    terminate_pids(scratch_studio_pids())
    terminate_pids(orphan_play_server_pids())
    ctx.studio_open = False


INSTALL_FLAG = SCRATCH_ROOT / ".install-pending"


def wait_for_install():
    """An installer that wants to swap the rbx binary raises a flag; runners
    finish their current row and wait between rows until it is cleared, so
    the swap never kills a command in flight."""
    waited = 0
    while INSTALL_FLAG.exists():
        if waited == 0:
            print("  (waiting for an rbx install to finish)", flush=True)
        waited += 1
        time.sleep(2)


def lock_file(handle, blocking: bool) -> bool:
    if os.name == "nt":
        import msvcrt
        while True:
            try:
                handle.seek(0)
                msvcrt.locking(handle.fileno(), msvcrt.LK_NBLCK, 1)
                return True
            except OSError:
                if not blocking:
                    return False
                time.sleep(2)
    import fcntl
    try:
        fcntl.flock(handle.fileno(), fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        return True
    except OSError:
        return False


def unlock_file(handle):
    try:
        if os.name == "nt":
            import msvcrt
            handle.seek(0)
            msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
        else:
            import fcntl
            fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
    except OSError:
        pass


STUDIO_SLOTS = int(os.environ.get("RENIUM_SCENARIO_STUDIOS", "4"))
# Shards of the current parallel run (0 outside one): when they hold every
# slot, a row that needs a second Studio skips at once for the final phase.
SHARD_COUNT = 0


class StudioSlot:
    """At most STUDIO_SLOTS sessions hold a Studio at once, across every
    runner process on the machine; the others wait for a slot."""

    def __init__(self):
        SESSION_ROOT.mkdir(parents=True, exist_ok=True)
        self.handle = None
        self.index = None

    def acquire(self):
        if self.handle is not None:
            return
        waited = 0
        while True:
            for index in range(STUDIO_SLOTS):
                handle = open(SESSION_ROOT / f".studio-slot-{index}.lock", "a+")
                if lock_file(handle, False):
                    self.handle = handle
                    self.index = index
                    return
                handle.close()
            if waited == 0:
                print(f"  (waiting for one of {STUDIO_SLOTS} Studio slots)", flush=True)
            waited += 1
            time.sleep(3)

    def release(self):
        if self.handle is None:
            return
        unlock_file(self.handle)
        self.handle.close()
        self.handle = None


ABORT_FILE = SESSION_ROOT / "abort.txt"
DURATIONS = REPORTS / "durations.json"
LONG_ENABLED = False
# Shards run shared rows only; globally exclusive rows run after every shard
# finished, one at a time, so no shard ever idles behind another's gate.
DEFER_EXCLUSIVE = False
FINAL_PASS_REASONS = ("another Studio stayed connected", "Studio slot", "deferred to the final serial phase")


def load_durations() -> dict:
    try:
        return {k: float(v) for k, v in json.loads(DURATIONS.read_text(encoding="utf-8")).items()}
    except (OSError, ValueError):
        return {}


def save_durations(outcomes: list) -> None:
    durations = load_durations()
    for o in outcomes:
        if o.get("status") == "pass":
            durations[o["id"]] = round(max(0.0, float(o.get("seconds", 0)) - float(o.get("gateSeconds", 0))), 1)
    REPORTS.mkdir(parents=True, exist_ok=True)
    DURATIONS.write_text(json.dumps(durations, indent=0, sort_keys=True), encoding="utf-8")


def assign_shards(scenarios: list, count: int, seed: int) -> list:
    """Balances rows across shards by their measured durations (longest rows
    placed first on the least loaded shard); the seed breaks ties so the
    spread still varies between runs."""
    durations = load_durations()
    rng = random.Random(seed)
    expected = {
        s.id: (0.1 if (s.long and not LONG_ENABLED) or os.name not in s.platforms or s.exclusive is True else durations.get(s.id, 10.0)) + rng.random() * 0.01
        for s in scenarios
    }
    loads = [0.0] * count
    shards = [[] for _ in range(count)]
    for scenario in sorted(scenarios, key=lambda s: -expected[s.id]):
        # Rows about the default daemon itself stay on shard 0, the one shard
        # that runs against it; every other shard drives its own named daemon.
        index = 0 if scenario.default_daemon else min(range(count), key=lambda i: loads[i])
        shards[index].append(scenario)
        loads[index] += expected[scenario.id]
    return shards


def order_within_shard(rows: list, seed: int) -> list:
    """Rows expected to run for minutes start first so their wait overlaps
    other work; globally exclusive rows run last, when few readers remain for
    their gate to wait on; everything else keeps a random order."""
    durations = load_durations()
    rows = list(rows)
    random.Random(seed).shuffle(rows)
    long = [s for s in rows if durations.get(s.id, 0) >= 120 and s.exclusive is not True]
    rest = [s for s in rows if s not in long]
    return long + [s for s in rest if s.exclusive is not True] + [s for s in rest if s.exclusive is True]
ABORT_STREAK = 3
ABORT_MARKERS = ("has not connected within", "No Studio runtime connected to this project")
# One row is enough for these: no later row can pass until the user acts.
ABORT_AT_ONCE = (
    ("is signed out and shows the Roblox sign-in dialog", "Studio is signed out (seen in {row}); sign in to Studio, then start the run again"),
    ("macOS is asking on this Mac's screen", "macOS is asking for a permission on this Mac's screen (seen in {row}); answer the prompt, then start the run again"),
)


def abort_at_once_reason(reason: str, row: str) -> str | None:
    """Why a run stops after this row's failure, or None when later rows can still pass."""
    return next((text.format(row=row) for marker, text in ABORT_AT_ONCE if marker in reason), None)


def infrastructure_failure(outcome: dict) -> bool:
    """A row that never reached a connected Studio says nothing about the
    feature it tests; a streak of them means the machine itself is broken
    (logged-out Studio, missing plugin, dead daemon) and the run must stop."""
    return outcome.get("status") in ("fail", "error") and any(marker in outcome.get("reason", "") for marker in ABORT_MARKERS)


def abort_requested() -> str | None:
    try:
        return ABORT_FILE.read_text(encoding="utf-8").strip() or "aborted"
    except OSError:
        return None


def request_abort(reason: str) -> None:
    SESSION_ROOT.mkdir(parents=True, exist_ok=True)
    ABORT_FILE.write_text(reason, encoding="utf-8")


STUDIO_LAUNCH_GAP_SECONDS = 8.0
# Shards start this far apart so their first daemon calls do not pile up.
SHARD_START_GAP_SECONDS = 5


def space_studio_launch():
    """Studio windows start at least STUDIO_LAUNCH_GAP_SECONDS apart across every
    session on this machine: on macOS two windows starting together sign the
    account out of Studio (the second refreshes the session with a replaced
    cookie and is answered 401)."""
    SESSION_ROOT.mkdir(parents=True, exist_ok=True)
    lock = SESSION_ROOT / ".studio-launch.lock"
    stamp = SESSION_ROOT / ".studio-launch.at"
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        try:
            handle = os.open(str(lock), os.O_CREAT | os.O_EXCL | os.O_WRONLY)
        except (FileExistsError, PermissionError):
            # Windows answers "permission denied" while another runner's
            # unlink of the lock is still pending; it is the same wait.
            try:
                if time.time() - lock.stat().st_mtime > 60:
                    lock.unlink(missing_ok=True)
            except OSError:
                pass
            time.sleep(0.2)
            continue
        try:
            try:
                last = float(stamp.read_text(encoding="utf-8").strip() or 0)
            except (OSError, ValueError):
                last = 0.0
            wait = STUDIO_LAUNCH_GAP_SECONDS - (time.time() - last)
            if wait > 0:
                time.sleep(wait)
            stamp.write_text(str(time.time()), encoding="utf-8")
            return
        finally:
            os.close(handle)
            lock.unlink(missing_ok=True)


class RowGate:
    """Readers-writer gate over the rows of every runner process on the
    machine. An ordinary row holds a reader file while it runs; an exclusive
    row (OS-level input, Studio-wide settings) takes the writer lock, which
    stops new rows from starting, and waits for the running ones to finish.
    Sessions keep their Studio open across the gate, so an exclusive row waits
    for rows, never for whole sessions, and two exclusive rows serialize on
    the writer lock instead of each holding half of the slots."""

    def __init__(self):
        SESSION_ROOT.mkdir(parents=True, exist_ok=True)
        self.writer = None
        self.reader = None
        self.reader_path = SESSION_ROOT / f".row-gate-reader-{os.getpid()}.lock"

    def _writer_handle(self):
        return open(SESSION_ROOT / ".row-gate-writer.lock", "a+")

    def enter_shared(self):
        waited = 0
        while True:
            handle = self._writer_handle()
            if lock_file(handle, False):
                self.reader = open(self.reader_path, "a+")
                lock_file(self.reader, True)
                unlock_file(handle)
                handle.close()
                return
            handle.close()
            if waited == 0:
                print("  (waiting for an exclusive row on another session)", flush=True)
            waited += 1
            time.sleep(2)

    def exit_shared(self):
        if self.reader is None:
            return
        unlock_file(self.reader)
        self.reader.close()
        self.reader = None
        try:
            self.reader_path.unlink()
        except OSError:
            pass

    def enter_exclusive(self):
        self.writer = self._writer_handle()
        if not lock_file(self.writer, False):
            print("  (waiting for another exclusive row)", flush=True)
            lock_file(self.writer, True)
        waited = 0
        while True:
            busy = []
            for path in SESSION_ROOT.glob(".row-gate-reader-*.lock"):
                handle = open(path, "a+")
                if lock_file(handle, False):
                    unlock_file(handle)
                    handle.close()
                    try:
                        path.unlink()
                    except OSError:
                        pass
                else:
                    handle.close()
                    busy.append(path.name)
            if not busy:
                return
            if waited == 0:
                print(f"  (waiting for {len(busy)} running row(s): exclusive row)", flush=True)
            waited += 1
            time.sleep(2)

    def enter_category(self, category: str):
        """A category gate (`exclusive="input"`): rows of the category run one
        at a time, beside ordinary rows, and count as readers for the
        globally exclusive ones."""
        self.category = open(SESSION_ROOT / f".row-gate-cat-{category}.lock", "a+")
        if not lock_file(self.category, False):
            print(f"  (waiting for another {category} row)", flush=True)
            lock_file(self.category, True)
        self.enter_shared()

    def exit_category(self):
        self.exit_shared()
        if getattr(self, "category", None) is not None:
            unlock_file(self.category)
            self.category.close()
            self.category = None

    def exit_exclusive(self):
        if self.writer is None:
            return
        unlock_file(self.writer)
        self.writer.close()
        self.writer = None


class RowLock:
    """Held by a session while one of its rows runs; an installer takes every
    row lock before swapping the binary."""

    def __init__(self, label: str):
        SESSION_ROOT.mkdir(parents=True, exist_ok=True)
        self.path = SESSION_ROOT / f".row-{label}-{os.getpid()}.lock"
        self.handle = open(self.path, "a+")

    def acquire(self):
        wait_for_install()
        lock_file(self.handle, True)

    def release(self):
        unlock_file(self.handle)

    def close(self):
        self.release()
        self.handle.close()
        try:
            self.path.unlink()
        except OSError:
            pass


class StudioLock:
    """One runner at a time owns the scratch Studios and the daemon's Studio
    channels; a second runner (another agent, another terminal) waits here
    instead of closing the first one's Studio under it."""

    def __init__(self):
        SCRATCH_ROOT.mkdir(parents=True, exist_ok=True)
        self.path = SCRATCH_ROOT / ".studio.lock"
        self.handle = None

    def __enter__(self):
        wait_for_install()
        self.handle = open(self.path, "a+")
        if os.name == "nt":
            import msvcrt
            waited = 0
            while True:
                try:
                    self.handle.seek(0)
                    msvcrt.locking(self.handle.fileno(), msvcrt.LK_NBLCK, 1)
                    break
                except OSError:
                    if waited == 0:
                        print("  (waiting for another runner's Studio to finish)", flush=True)
                    waited += 1
                    time.sleep(2)
        else:
            import fcntl
            fcntl.flock(self.handle.fileno(), fcntl.LOCK_EX)
        return self

    def __exit__(self, *_):
        try:
            if os.name == "nt":
                import msvcrt
                self.handle.seek(0)
                msvcrt.locking(self.handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl
                fcntl.flock(self.handle.fileno(), fcntl.LOCK_UN)
        finally:
            self.handle.close()


def run_scenario(scenario: Scenario, keep: bool) -> dict:
    with StudioLock():
        return run_scenario_locked(scenario, keep)


SERVICE_COUNTS = (
    "local t = {} for _, s in ipairs({'Workspace','ReplicatedStorage','ServerScriptService','ServerStorage','StarterPlayer',"
    "'StarterGui','StarterPack','ReplicatedFirst','Lighting','SoundService','Teams','TextChatService'}) do "
    "local ok, service = pcall(game.GetService, game, s) if ok and service then t[s] = #service:GetDescendants() end end return t"
)


class Session:
    """One Studio for many rows, the way a user works: the place is copied
    once, Studio opens once, and between rows the project files are restored
    from the fixture and pushed back into the open Studio. Studio is reopened
    only when a row closed it or left it unable to take the push."""

    def __init__(self, size: str, label: str):
        source = FIXTURES[size]
        if not source.is_dir():
            raise RuntimeError(f"fixture missing: {source}")
        self.size = size
        self.source = source
        self.root = SESSION_ROOT / f"{label}-{int(time.time())}-{os.getpid()}"
        SESSION_ROOT.mkdir(parents=True, exist_ok=True)
        shutil.copytree(source, self.root, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
        places = sorted(p.name for p in self.root.glob("*.rbxl"))
        if not places:
            raise RuntimeError(f"fixture {source} has no .rbxl place")
        self.place = places[0]
        self.studio_open = False
        self.counts: dict | None = None
        self.reopens = 0
        self.resets = 0
        self.environment = dict(os.environ)
        self.slot = StudioSlot()

    def context(self, scenario: Scenario) -> Context:
        ctx = Context(scenario=scenario, root=self.root, place=self.place)
        ctx.session = self
        ctx.studio_open = self.studio_open
        return ctx

    def restore_files(self):
        import stat

        def writable_then(function, path, _info):
            try:
                os.chmod(path, stat.S_IWRITE | stat.S_IREAD)
                function(path)
            except OSError:
                pass

        for entry in self.root.iterdir():
            if entry.name == self.place:
                continue
            if entry.is_dir() and not entry.is_symlink():
                shutil.rmtree(entry, onexc=writable_then) if hasattr(shutil, "rmtree") and "onexc" in shutil.rmtree.__code__.co_varnames else shutil.rmtree(entry, onerror=writable_then)
            else:
                try:
                    entry.unlink()
                except OSError:
                    try:
                        os.chmod(entry, stat.S_IWRITE | stat.S_IREAD)
                        entry.unlink()
                    except OSError:
                        pass
        for entry in self.source.iterdir():
            if entry.name in (".renium", self.place) or entry.name.endswith(".lock"):
                continue
            target = self.root / entry.name
            if entry.is_dir():
                shutil.copytree(entry, target, ignore=shutil.ignore_patterns(".renium", "*.lock"))
            else:
                shutil.copy2(entry, target)

    def open_studio(self, ctx: Context):
        self.slot.acquire()
        self.restore_place()
        space_studio_launch()
        ctx.rbx("so", self.place, timeout=150)
        ctx.studio_open = True
        self.studio_open = True
        ctx.guard_binding()
        self.counts = ctx.luau(SERVICE_COUNTS)

    def restore_place(self):
        shutil.copy2(self.source / self.place, self.root / self.place)

    def close_studio(self, ctx: Context):
        try:
            if self.studio_open:
                ctx.guard_binding()
                ctx.rbx("sx", "--terminate", ok=None, timeout=60)
                time.sleep(1.5)
        except Exception:
            pass
        terminate_pids(place_studio_pids(self.root / self.place))
        terminate_pids(orphan_play_server_pids())
        terminate_pids(studio_pids_by_title(str(self.root.resolve())))
        self.studio_open = False
        ctx.studio_open = False

    def reset(self, ctx: Context) -> str:
        """Brings the files and the open Studio back to the fixture. Returns
        how: "push" (Studio kept), "open" (no Studio was open) or "reopen"."""
        os.environ.clear()
        os.environ.update(self.environment)
        os.chdir(HERE)
        if self.studio_open:
            ctx.studio_open = True
            try:
                ctx.guard_binding()
            except ScenarioFailure:
                self.close_studio(ctx)
        if self.studio_open:
            ctx.rbx("play", "-x", ok=None, timeout=60)
            ctx.rbx("lof", ok=None, timeout=60)
            ctx.rbx("dp", ok=None, timeout=60)
        terminate_pids(orphan_play_server_pids())
        self.restore_files()
        if not self.studio_open:
            self.open_studio(ctx)
            return "open"
        pushed = ctx.rbx("ps", "--yes", ok=None, timeout=300)
        counts = None
        if pushed.ok:
            try:
                counts = ctx.luau(SERVICE_COUNTS)
            except ScenarioFailure:
                counts = None
        if pushed.ok and counts == self.counts:
            self.resets += 1
            return "push"
        ctx.log.append(f"# reset push did not restore the fixture ({pushed.error[:200] if not pushed.ok else counts}); reopening")
        self.close_studio(ctx)
        self.open_studio(ctx)
        self.reopens += 1
        return "reopen"

    def finish(self, ctx: Context, keep: bool):
        self.close_studio(ctx)
        self.slot.release()
        if not keep:
            shutil.rmtree(self.root, ignore_errors=True)


def run_session(scenarios: list[Scenario], size: str, label: str, keep: bool, order_seed: int, progress: Path | None = None, earlier: list[dict] | None = None) -> list[dict]:
    outcomes = []
    session = Session(size, label)
    ctx = session.context(scenarios[0])
    failed_any = False
    row_lock = RowLock(label)
    gate = RowGate()
    session.slot.acquire()
    streak = 0
    try:
        for scenario in scenarios:
            aborted = abort_requested()
            if aborted:
                outcomes.append({"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "abort", "reason": aborted, "seconds": 0, "log": []})
                print(f"[{scenario.id}] ABORT: {aborted}", flush=True)
                continue
            print(f"[{scenario.id}] {scenario.title} ...", flush=True)
            ctx.row_started = time.monotonic()
            if os.name not in scenario.platforms:
                outcomes.append({"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "skip", "reason": f"not run on {os.name}", "seconds": 0, "log": []})
                print(f"[{scenario.id}] SKIP (not run on this platform)", flush=True)
                continue
            if scenario.exclusive is True and DEFER_EXCLUSIVE:
                outcomes.append({"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "skip", "reason": "exclusive row deferred to the final serial phase", "seconds": 0, "log": []})
                print(f"[{scenario.id}] DEFERRED (exclusive; final phase)", flush=True)
                continue
            if scenario.long and not LONG_ENABLED:
                outcomes.append({"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "skip", "reason": "long row (pass --long to include it)", "seconds": 0, "log": []})
                print(f"[{scenario.id}] SKIP (long row; pass --long)", flush=True)
                continue
            row_lock.release()
            row_lock.acquire()
            ctx = session.context(scenario)
            started = time.monotonic()
            outcome = {"id": scenario.id, "title": scenario.title, "size": scenario.size, "root": str(session.root), "seed": order_seed}
            try:
                try:
                    outcome["reset"] = session.reset(ctx)
                except Exception as error:
                    outcome["resetError"] = f"{type(error).__name__}: {str(error)[:400]}"
                    if abort_at_once_reason(str(error), scenario.id):
                        raise
                    session.close_studio(ctx)
                    session.open_studio(ctx)
                    outcome["reset"] = "reopen after a failed reset"
                gate_started = time.monotonic()
                if scenario.exclusive is True:
                    gate.enter_exclusive()
                    outcome["gateSeconds"] = round(time.monotonic() - gate_started, 1)
                    try:
                        scenario.run(ctx)
                    finally:
                        gate.exit_exclusive()
                elif scenario.exclusive:
                    gate.enter_category(str(scenario.exclusive))
                    outcome["gateSeconds"] = round(time.monotonic() - gate_started, 1)
                    try:
                        scenario.run(ctx)
                    finally:
                        gate.exit_category()
                else:
                    gate.enter_shared()
                    outcome["gateSeconds"] = round(time.monotonic() - gate_started, 1)
                    try:
                        scenario.run(ctx)
                    finally:
                        gate.exit_shared()
                outcome["status"] = "pass"
            except ScenarioSkip as error:
                outcome["status"] = "skip"
                outcome["reason"] = str(error)
            except ScenarioFailure as error:
                outcome["status"] = "fail"
                outcome["reason"] = str(error)
            except Exception as error:
                outcome["status"] = "error"
                outcome["reason"] = f"{type(error).__name__}: {error}\n{traceback.format_exc()[-1500:]}"
            session.studio_open = ctx.studio_open
            outcome["seconds"] = round(time.monotonic() - started, 1)
            outcome["log"] = ctx.log
            outcomes.append(outcome)
            failed_any |= outcome["status"] != "pass"
            status = outcome["status"].upper()
            print(f"[{scenario.id}] {status} in {outcome['seconds']}s" + (f": {outcome.get('reason','')[:300]}" if status != "PASS" else ""), flush=True)
            streak = streak + 1 if infrastructure_failure(outcome) else 0
            needs_user = abort_at_once_reason(outcome.get("reason", ""), scenario.id) if outcome["status"] != "pass" else None
            if needs_user and not abort_requested():
                request_abort(needs_user)
            if streak >= ABORT_STREAK and not abort_requested():
                request_abort(f"{ABORT_STREAK} rows in a row never reached a connected Studio (last: {scenario.id}); check that Studio is logged in and the plugin is installed")
            if progress is not None:
                progress.parent.mkdir(parents=True, exist_ok=True)
                progress.write_text(json.dumps([*(earlier or []), *outcomes], indent=1), encoding="utf-8")
    finally:
        session.finish(ctx, keep or failed_any)
        row_lock.close()
        print(f"session {label}: {session.resets} resets by push, {session.reopens} reopens", flush=True)
    return outcomes


def reported_rows(prefix: str) -> list[dict]:
    """The rows a run wrote down: its report, or what it had finished when it
    ended without one."""
    for name in (f"{prefix}.json", f"{prefix}.partial.json"):
        try:
            return json.loads((REPORTS / name).read_text(encoding="utf-8"))
        except (OSError, ValueError):
            continue
    return []


def wait_for_shards(processes: list, stamp: str, total: int, brief: bool) -> None:
    """Waits for every shard; a brief run prints how far each shard is every
    two minutes, since its logs stay in the reports folder."""
    printed = time.monotonic()
    while any(process.poll() is None for _, process, _ in processes):
        time.sleep(2)
        if brief and time.monotonic() - printed >= 120:
            printed = time.monotonic()
            done = [len(reported_rows(f"{stamp}-shard{shard}")) for shard, _, _ in processes]
            print(f"  {' + '.join(str(count) for count in done)} of {total} rows done", flush=True)
    for _, _, handle in processes:
        handle.close()


def run_serial_phase(argv: list[str], ids: list[str], seed: int, prefix: str, brief: bool) -> dict:
    """Runs the named rows alone, one Studio, after every shard finished;
    returns their outcomes by id."""
    log = REPORTS / f"{prefix}.log"
    with open(log, "w", encoding="utf-8") as handle:
        subprocess.run([sys.executable, "-I", str(HERE / "run.py"), *argv, "--only", ",".join(ids), "--seed", str(seed), "--child", "--report-prefix", prefix], cwd=HERE, stdout=handle, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)
    if not brief:
        print(log.read_text(encoding="utf-8", errors="replace"))
    return {o["id"]: o for o in reported_rows(prefix)}


def run_parallel(argv: list[str], parallel: int, seed: int, selected: list[Scenario], rerun_failed: bool, brief: bool) -> int:
    """Runs the selection as N shards, each its own process, session and
    Studio; the shards' reports are merged into one."""
    processes = []
    REPORTS.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    # The rows are split here, once: shards start seconds apart, and a split
    # each shard worked out for itself could differ when the recorded
    # durations changed in between, running one row twice and another never.
    assigned = assign_shards(selected, parallel, seed)
    for shard in range(parallel):
        if not assigned[shard]:
            continue
        log = REPORTS / f"{stamp}-shard{shard}.log"
        handle = open(log, "w", encoding="utf-8")
        command = [sys.executable, "-I", str(HERE / "run.py"), *argv, "--seed", str(seed), "--shard", f"{shard}/{parallel}", "--only", ",".join(s.id for s in assigned[shard]), "--child", "--report-prefix", f"{stamp}-shard{shard}"]
        env = dict(os.environ)
        if shard > 0:
            # Each shard beyond the first drives a daemon of its own: every rbx
            # call of the shard inherits the name, and the default daemon
            # forwards the Studios that daemon opens to its ports (launch
            # tickets). Rows that need the default daemon sit on shard 0.
            env["RENIUM_DAEMON_NAME"] = f"shard{shard}"
        processes.append((shard, subprocess.Popen(command, cwd=HERE, env=env, stdout=handle, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL), handle))
        time.sleep(SHARD_START_GAP_SECONDS)
    wait_for_shards(processes, stamp, len(selected), brief)
    for shard in range(1, parallel):
        subprocess.run([RBX, "dm", "stop", f"shard{shard}"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    outcomes = []
    for shard, _, _ in processes:
        outcomes.extend(reported_rows(f"{stamp}-shard{shard}"))
        if not brief:
            print((REPORTS / f"{stamp}-shard{shard}.log").read_text(encoding="utf-8", errors="replace"))
    # A shard that died took its remaining rows with it; they count as not
    # passed instead of dropping out of the total.
    reported = {o["id"] for o in outcomes}
    for scenario in selected:
        if scenario.id not in reported:
            outcomes.append({"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "error", "reason": "the shard that held this row ended without reporting it", "seconds": 0, "log": []})
    retry = [o["id"] for o in outcomes if o["status"] == "skip" and any(reason in o.get("reason", "") for reason in FINAL_PASS_REASONS)]
    if retry and not abort_requested():
        # Rows that need a lone Studio or a second slot ran into the other
        # shards; they run again alone now that those shards are done.
        print("\nfinal serial phase for " + str(len(retry)) + " row(s) (exclusive rows and rows that need a lone Studio)", flush=True)
        replaced = run_serial_phase(argv, retry, seed, f"{stamp}-final", brief)
        outcomes = [replaced.get(o["id"], o) for o in outcomes]
    failed = [o["id"] for o in outcomes if o["status"] in ("fail", "error")]
    if rerun_failed and failed and not abort_requested():
        # One more attempt, alone, tells a row that fails from one that lost
        # a race with the other shards; the report keeps the first outcome.
        print(f"\nsecond run for {len(failed)} row(s) that did not pass", flush=True)
        second = run_serial_phase(argv, failed, seed, f"{stamp}-rerun", brief)
        for index, first in enumerate(outcomes):
            again = second.get(first["id"])
            if again is None:
                continue
            if again["status"] == "pass":
                again["firstRun"] = {"status": first["status"], "reason": first.get("reason", "")}
                outcomes[index] = again
            else:
                first["secondRun"] = {"status": again["status"], "reason": again.get("reason", "")}
    for outcome in outcomes:
        # A row still waiting for a Studio of its own after the serial phase
        # never ran: another Studio stayed connected the whole time.
        if outcome["status"] == "skip" and any(reason in outcome.get("reason", "") for reason in FINAL_PASS_REASONS):
            outcome["status"] = "fail"
            outcome["reason"] = "could not run, even alone after the shards: " + outcome.get("reason", "")
    report = REPORTS / f"{stamp}.json"
    report.write_text(json.dumps(outcomes, indent=1), encoding="utf-8")
    save_durations(outcomes)
    passed = sum(1 for o in outcomes if o["status"] == "pass")
    skipped = sum(1 for o in outcomes if o["status"] == "skip")
    if brief:
        # The reasons and the paths in them stay in the report on this machine.
        for outcome in outcomes:
            if outcome["status"] not in ("pass", "skip"):
                print(f"[{outcome['id']}] {outcome['status'].upper()} in {outcome['seconds']}s: {outcome['title']}")
    second_run_passes = [o["id"] for o in outcomes if o["status"] == "pass" and "firstRun" in o]
    if second_run_passes:
        print(f"\npassed on the second run only: {', '.join(second_run_passes)}")
    aborted = abort_requested()
    if aborted:
        print(f"\nRUN ABORTED: {aborted}")
    print(f"\n{passed}/{len(outcomes) - skipped} passed; {skipped} skipped; seed {seed}; report {report.name if brief else report}")
    return 0 if passed + skipped == len(outcomes) else 1


def clean_up() -> int:
    """Closes the scratch Studios and stops the shard daemons that a run cut
    short by a timeout or a cancel left behind."""
    owned = (SCRATCH_ROOT, SESSION_ROOT)
    close_scratch_studios_gracefully(owned)
    terminate_pids(scratch_studio_pids(owned))
    listing = subprocess.run([RBX, "dm", "list"], stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=60).stdout
    for line in listing.splitlines():
        name = line.split(" ", 1)[0]
        if name.startswith("shard") and name[5:].isdigit():
            subprocess.run([RBX, "dm", "stop", name], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=60)
    return 0


def run_scenario_locked(scenario: Scenario, keep: bool) -> dict:
    close_scratch_studios_gracefully()
    terminate_pids(scratch_studio_pids())
    root, place = fresh_scratch(scenario.size, scenario.id)
    ctx = Context(scenario=scenario, root=root, place=place)
    started = time.monotonic()
    outcome = {"id": scenario.id, "title": scenario.title, "size": scenario.size, "root": str(root)}
    try:
        scenario.run(ctx)
        outcome["status"] = "pass"
    except ScenarioFailure as error:
        outcome["status"] = "fail"
        outcome["reason"] = str(error)
    except Exception as error:
        outcome["status"] = "error"
        outcome["reason"] = f"{type(error).__name__}: {error}\n{traceback.format_exc()[-1500:]}"
    finally:
        kill_scratch_studios(ctx)
        outcome["seconds"] = round(time.monotonic() - started, 1)
        outcome["log"] = ctx.log
        if outcome.get("status") == "pass" and not keep:
            shutil.rmtree(root, ignore_errors=True)
    return outcome


def load_scenarios(areas: list[str]) -> list[Scenario]:
    sys.path.insert(0, str(HERE))
    sys.modules.setdefault("run", sys.modules[__name__])
    scenarios: list[Scenario] = []
    for area in areas:
        module = importlib.import_module(f"areas.{area}")
        scenarios.extend(module.SCENARIOS)
    return scenarios


def ensure_daemon():
    """Starts the daemon before any captured command: a daemon spawned by a
    captured rbx inherits the pipes and the runner would wait on them forever."""
    subprocess.run([RBX, "dm", "status"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=120)


def main() -> int:
    for stream in (sys.stdout, sys.stderr):
        try:
            stream.reconfigure(encoding="utf-8", errors="replace")
        except (AttributeError, ValueError):
            pass
    parser = argparse.ArgumentParser()
    parser.add_argument("--area", action="append", default=[], help="area module(s) under scenarios/areas; default: all")
    parser.add_argument("--only", default="", help="comma-separated scenario ids")
    parser.add_argument("--size", choices=["small", "heavy"], help="run only rows of this fixture size")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--keep", action="store_true", help="keep scratch copies of passing scenarios")
    parser.add_argument("--isolated", action="store_true", help="a fresh copy and a fresh Studio per row (only for rows that test opening and closing)")
    parser.add_argument("--seed", type=int, help="row order seed for a session run (printed; default random)")
    parser.add_argument("--shard", default="", help="i/n: run the i-th of n interleaved slices of the shuffled rows")
    parser.add_argument("--parallel", type=int, default=1, help="run n sessions at once, each with its own Studio")
    parser.add_argument("--report-prefix", default="", help=argparse.SUPPRESS)
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--long", action="store_true", help="include rows that wait for many minutes on purpose (PLAY-036)")
    parser.add_argument("--rerun-failed", action="store_true", help="run the rows that did not pass once more, alone, after the shards")
    parser.add_argument("--brief", action="store_true", help="print row ids and outcomes only; logs and failure reasons stay in the reports folder")
    parser.add_argument("--cleanup", action="store_true", help="close the scratch Studios and shard daemons of a run that was cut short, then exit")
    args = parser.parse_args()
    if args.cleanup:
        return clean_up()
    if (args.rerun_failed or args.brief) and (args.shard or args.isolated):
        parser.error("--rerun-failed and --brief belong to a whole run; they do not combine with --shard or --isolated")
    if not args.child:
        # An abort recorded by an earlier run would stop this one at its first row.
        ABORT_FILE.unlink(missing_ok=True)
    global LONG_ENABLED
    LONG_ENABLED = args.long
    areas = args.area or sorted(p.stem for p in (HERE / "areas").glob("*.py") if not p.stem.startswith("_"))
    scenarios = load_scenarios(areas)
    if args.only:
        wanted = {item.strip() for item in args.only.split(",") if item.strip()}
        scenarios = [s for s in scenarios if s.id in wanted]
    if args.size:
        scenarios = [s for s in scenarios if s.size == args.size]
    if args.list:
        for scenario in scenarios:
            print(f"{scenario.id}\t{scenario.size}\t{scenario.title}")
        return 0
    REPORTS.mkdir(parents=True, exist_ok=True)
    seed = args.seed if args.seed is not None else random.randrange(1, 10**9)
    if (args.parallel > 1 or args.rerun_failed or args.brief) and not args.isolated and not args.child:
        passthrough = []
        skip = False
        for a in sys.argv[1:]:
            if skip:
                skip = False
                continue
            if a == "--parallel":
                skip = True
                continue
            if a.startswith("--parallel") or a in ("--rerun-failed", "--brief"):
                continue
            passthrough.append(a)
        return run_parallel(passthrough, max(args.parallel, 1), seed, scenarios, args.rerun_failed, args.brief)
    ensure_daemon()
    stamp = args.report_prefix or time.strftime("%Y%m%d-%H%M%S")
    outcomes = []
    if not args.isolated:
        if args.shard:
            index, count = (int(v) for v in args.shard.split("/"))
            global SHARD_COUNT
            SHARD_COUNT = count
            ordered = order_within_shard(scenarios if args.child else assign_shards(scenarios, count, seed)[index], seed + index)
            global DEFER_EXCLUSIVE
            DEFER_EXCLUSIVE = True
        else:
            ordered = order_within_shard(scenarios, seed)
        print(f"session order seed {seed}; {len(ordered)} rows", flush=True)
        for size in ("small", "heavy"):
            rows = [s for s in ordered if s.size == size]
            if rows:
                outcomes.extend(run_session(rows, size, f"{size}-{seed}" + (f"-{args.shard.replace('/', 'of')}" if args.shard else ""), args.keep, seed, REPORTS / f"{stamp}.partial.json", list(outcomes)))
        report = REPORTS / f"{stamp}.json"
        report.write_text(json.dumps(outcomes, indent=1), encoding="utf-8")
        passed = sum(1 for o in outcomes if o["status"] == "pass")
        skipped = sum(1 for o in outcomes if o["status"] == "skip")
        print(f"\n{passed}/{len(outcomes) - skipped} passed; {skipped} skipped; seed {seed}; report {report}")
        return 0 if passed + skipped == len(outcomes) else 1
    for scenario in scenarios:
        print(f"[{scenario.id}] {scenario.title} ...", flush=True)
        if os.name not in scenario.platforms:
            outcome = {"id": scenario.id, "title": scenario.title, "size": scenario.size, "status": "skip", "reason": f"not run on {os.name}", "seconds": 0, "log": []}
            outcomes.append(outcome)
            print(f"[{scenario.id}] SKIP (not run on this platform)", flush=True)
            continue
        outcome = run_scenario(scenario, args.keep)
        outcomes.append(outcome)
        status = outcome["status"].upper()
        print(f"[{scenario.id}] {status} in {outcome['seconds']}s" + (f": {outcome.get('reason','')[:300]}" if status != "PASS" else ""), flush=True)
    report = REPORTS / f"{stamp}.json"
    report.write_text(json.dumps(outcomes, indent=1), encoding="utf-8")
    passed = sum(1 for o in outcomes if o["status"] == "pass")
    skipped = sum(1 for o in outcomes if o["status"] == "skip")
    print(f"\n{passed}/{len(outcomes) - skipped} passed; {skipped} skipped; report {report}")
    return 0 if passed + skipped == len(outcomes) else 1


if __name__ == "__main__":
    sys.exit(main())
