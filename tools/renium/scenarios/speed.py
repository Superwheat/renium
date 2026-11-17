"""Speed run: times every sync path of Renium on one big place, three times each,
and compares the averages with the runs before it.

    python -I tools/renium/scenarios/speed.py                  # the offline and the Studio phase
    python -I tools/renium/scenarios/speed.py --only offline   # offline, studio or push
    python -I tools/renium/scenarios/speed.py --record         # add a passing run to the history
    python -I tools/renium/scenarios/speed.py --cleanup        # close what a cut-short run left open

The push phase fills an empty place from the whole project, verifies that a
second push is unchanged, and checks that pulling changes no project file.

The place is a private file on the machine that runs this. Its path comes from
RENIUM_SPEED_PLACE or from the one-line file ~/.renium/speed-place.txt. The
output names scenarios and seconds only: every path, every command and every
error text stays in the run's log under ~/.renium/speed-reports.

A run fails when a step fails, when a result is wrong (a pushed edit that did
not arrive, a rebuilt place that differs), or when a scenario is both 1.5 times
and one second slower than the median of the last five recorded runs.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import statistics
import subprocess
import sys
import time
from pathlib import Path

HOME = Path.home() / ".renium"
REPORTS = Path(os.environ.get("RENIUM_SPEED_REPORTS") or HOME / "speed-reports")
SCRATCH = Path(os.environ.get("RENIUM_SPEED_SCRATCH") or HOME / "speed-runs")
PLACE_FILE = HOME / "speed-place.txt"
RBX = os.environ.get("RENIUM_SPEED_RBX") or os.environ.get("RENIUM_SCENARIO_RBX") or "rbx"
DAEMON = os.environ.get("RENIUM_SPEED_DAEMON") or "speed"
NO_WINDOW = 0x08000000 if os.name == "nt" else 0
PARTS = 500
SCRIPTS = 200
SLOWER_RATIO = 1.5
SLOWER_SECONDS = 1.0
BASELINE_RUNS = 5


class SpeedFailure(Exception):
    pass


class Run:
    def __init__(self, rounds: int, log_path: Path):
        self.rounds = rounds
        self.samples: dict[str, list[float]] = {}
        self.supplemental_import_writes: list[int] = []
        self.log_path = log_path
        self.log_file = log_path.open("w", encoding="utf-8")
        self.opened: list[Path] = []

    def log(self, text: str) -> None:
        self.log_file.write(text.rstrip() + "\n")
        self.log_file.flush()

    def sample(self, name: str, seconds: float) -> None:
        self.samples.setdefault(name, []).append(seconds)
        self.log(f"SAMPLE {name}: {seconds:.3f}")

    def rbx(self, args, cwd: Path, place: Path | None = None, timeout: float = 900, ok: bool = True, json_mode: bool = True):
        argv = [RBX] + (["--output-mode", "json"] if json_mode else []) + [str(arg) for arg in args]
        env = dict(os.environ, RENIUM_DAEMON_NAME=DAEMON,
                   RENIUM_PLACE=str(place or (cwd / "place.rbxl")))
        started = time.perf_counter()
        try:
            done = subprocess.run(argv, cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout, creationflags=NO_WINDOW)
            code, out, err = done.returncode, done.stdout, done.stderr
        except subprocess.TimeoutExpired:
            code, out, err = 124, "", f"no answer within {timeout:.0f}s"
        seconds = time.perf_counter() - started
        data = None
        for stream in (out, err):
            for line in reversed(stream.strip().splitlines()):
                line = line.strip()
                if line.startswith(("{", "[")):
                    try:
                        data = json.loads(line)
                        break
                    except ValueError:
                        pass
            if data is not None:
                break
        shown = " ".join(str(arg) if len(str(arg)) < 120 else str(arg)[:117] + "..." for arg in args)
        self.log(f"$ rbx {shown}  [{code}, {seconds:.2f}s] cwd={cwd}\n  out: {out.strip()[-1500:]}\n  err: {err.strip()[-1500:]}")
        failed = code != 0 or (isinstance(data, dict) and data.get("ok") is False)
        if ok and failed:
            raise SpeedFailure(f"rbx {args[0]} failed")
        return seconds, data, out

    def timed(self, name: str, args, cwd: Path, **options):
        seconds, data, out = self.rbx(args, cwd, **options)
        self.sample(name, seconds)
        return data, out


def expect(condition: bool, what: str) -> None:
    if not condition:
        raise SpeedFailure(what)


def source_place() -> Path:
    text = os.environ.get("RENIUM_SPEED_PLACE") or (PLACE_FILE.read_text(encoding="utf-8").strip() if PLACE_FILE.is_file() else "")
    if not text:
        raise SpeedFailure("no place is set on this machine (RENIUM_SPEED_PLACE or ~/.renium/speed-place.txt)")
    place = Path(text)
    if not place.is_file():
        raise SpeedFailure("the place set on this machine is not a file")
    return place


def fresh(folder: Path) -> Path:
    if folder.exists():
        shutil.rmtree(folder)
    folder.mkdir(parents=True)
    return folder


def differences(data, studio_saved: bool = False) -> int:
    """How many instances differ. A place Studio saved may differ in the
    collision data of meshes alone: Studio writes a placeholder for a mesh it
    had not cooked yet at that moment, so two saves of one place differ there."""
    found = (data or {}).get("differences")
    if not isinstance(found, list):
        return int(found) if isinstance(found, (int, float)) else -1

    def cook_state_only(entry) -> bool:
        names = {item.get("name") for item in entry.get("properties") or []}
        return entry.get("kind") == "changed" and not entry.get("attributes") and names == {"PhysicalConfigData"}

    return sum(1 for entry in found if not (studio_saved and cook_state_only(entry)))


def offline(run: Run, source: Path, root: Path) -> None:
    for index in range(1, run.rounds + 1):
        folder = fresh(root / f"offline-{index}")
        place = folder / "place.rbxl"
        shutil.copy2(source, place)
        run.rbx(["init", "."], folder)
        run.timed("import a place, empty project", ["pi", "place.rbxl"], folder, timeout=1800)
        run.timed("import a place, nothing changed", ["pi", "place.rbxl"], folder, timeout=1800)
        run.timed("build a place", ["bep", "--base", "place.rbxl", "-o", "rebuilt.rbxl"], folder, timeout=1800)
        data, _ = run.timed("compare two places", ["cmp", "place.rbxl", "rebuilt.rbxl", "--full", "--all"], folder, timeout=1800)
        expect(differences(data) == 0, "the place built from the import differs from the place")
        run.timed("check every script", ["ck", "src"], folder, ok=False, timeout=600)
        run.timed("find text in scripts", ["sg", "function", "-c"], folder, ok=False)
        run.timed("find instances", ["f", "Workspace", "-c", "Part", "--limit", "20"], folder, ok=False)
        stores = sorted((folder / "instances").glob("*.renium"), key=lambda path: path.stat().st_size)
        expect(bool(stores), "the import wrote no store")
        run.timed("render the biggest store as text", ["vct", f"instances/{stores[-1].name}"], folder, json_mode=False, timeout=600)
        run.timed("write the sourcemap", ["sm", "-o", "sourcemap.json"], folder, timeout=600)
        shutil.rmtree(folder, ignore_errors=True)


def luau(run: Run, folder: Path, place: Path, code: str, timeout: int = 120):
    _, data, out = run.rbx(["l", "--edit", "-t", str(timeout), code], folder, place=place, timeout=timeout + 30)
    if isinstance(data, dict):
        results = data.get("results")
        if isinstance(results, list) and results:
            return results[0]
        if "result" in data:
            return data["result"]
    return out.strip()


def wait_closed(run: Run, folder: Path, place: Path) -> None:
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline:
        _, data, out = run.rbx(["cs"], folder, place=place, ok=False, timeout=60)
        if '"edit"' not in out:
            break
        time.sleep(1)
    time.sleep(3)


def open_studio(run: Run, folder: Path, place: Path, name: str | None) -> None:
    seconds, _, _ = run.rbx(["so", place.name], folder, place=place, timeout=900)
    if place not in run.opened:
        run.opened.append(place)
    if name:
        run.sample(name, seconds)


SEED = """
local function folder(parent)
    local old = parent:FindFirstChild("SpeedRun")
    if old then old:Destroy() end
    local made = Instance.new("Folder")
    made.Name = "SpeedRun"
    made.Parent = parent
    return made
end
local parts = folder(workspace)
for index = 1, %d do
    local part = Instance.new("Part")
    part.Name = string.format("P%%04d", index)
    part.Anchored = true
    part.Position = Vector3.new(index %% 50 * 5, 500, index // 50 * 5)
    part.Parent = parts
end
local scripts = folder(game:GetService("ServerScriptService"))
for index = 1, %d do
    local module = Instance.new("ModuleScript")
    module.Name = string.format("M%%03d", index)
    module.Source = "return " .. index .. "\\n"
    module.Parent = scripts
end
return #parts:GetChildren() + #scripts:GetChildren()
""" % (PARTS, SCRIPTS)


def script_file(folder: Path, index: int) -> Path:
    return folder / "src" / "ServerScriptService" / "SpeedRun" / f"M{index:03d}.luau"


def studio(run: Run, source: Path, root: Path) -> None:
    folder = fresh(root / "studio")
    place = folder / "place.rbxl"
    shutil.copy2(source, place)
    run.rbx(["init", "."], folder)
    open_studio(run, folder, place, "open the place in Studio")
    made = luau(run, folder, place, SEED)
    expect(str(made) == str(PARTS + SCRIPTS), "Studio did not make the test instances")

    for index in range(1, run.rounds + 1):
        for name in ("src", "instances", ".renium", "sourcemap.json"):
            target = folder / name
            if target.is_dir():
                shutil.rmtree(target)
            elif target.exists():
                target.unlink()
        run.rbx(["init", "."], folder)
        run.timed("pull, empty project", ["pl"], folder, place=place, timeout=1800)
        expect(script_file(folder, 1).is_file(), "the pull into an empty project wrote no script")

    for index in range(1, run.rounds + 1):
        run.timed("pull, nothing changed", ["pl"], folder, place=place, timeout=1800)

    for index in range(1, run.rounds + 1):
        marker = f"return 'pulled {index} {time.time_ns()}'"
        luau(run, folder, place, f"game.ServerScriptService.SpeedRun.M001.Source = {json.dumps(marker)} return 1")
        run.timed("pull, one script changed", ["pl"], folder, place=place, timeout=1800)
        expect(marker in script_file(folder, 1).read_text(encoding="utf-8"), "a script changed in Studio did not reach its file")

    for index in range(1, run.rounds + 1):
        value = round(0.1 * index, 2)
        luau(run, folder, place, f"workspace.SpeedRun.P0001.Transparency = {value} return 1")
        run.timed("pull, one property changed", ["pl"], folder, place=place, timeout=1800)
        _, data, out = run.rbx(["bg", "Workspace", "SpeedRun.P0001", "-p", "Transparency"], folder)
        expect(str(value) in out, "a property changed in Studio did not reach the store")

    for index in range(1, run.rounds + 1):
        value = round(0.05 * index, 2)
        luau(run, folder, place, f"for _, part in workspace.SpeedRun:GetChildren() do part.Reflectance = {value} end return 1")
        run.timed(f"pull, {PARTS:,} instances changed", ["pl"], folder, place=place, timeout=1800)
        _, data, out = run.rbx(["bg", "Workspace", f"SpeedRun.P{PARTS:04d}", "-p", "Reflectance"], folder)
        expect(str(value) in out, "properties changed in Studio did not reach the store")

    for index in range(1, run.rounds + 1):
        marker = f"-- pulled many {index} {time.time_ns()}"
        luau(run, folder, place, f"for _, module in game.ServerScriptService.SpeedRun:GetChildren() do module.Source = 'return 0 ' .. {json.dumps(marker)} end return 1")
        run.timed(f"pull, {SCRIPTS} scripts changed", ["pl"], folder, place=place, timeout=1800)
        expect(marker in script_file(folder, SCRIPTS).read_text(encoding="utf-8"), "scripts changed in Studio did not reach their files")

    for index in range(1, run.rounds + 1):
        run.timed("push, nothing changed", ["ps", "--yes"], folder, place=place, timeout=1800)

    for index in range(1, run.rounds + 1):
        marker = f"pushed {index} {time.time_ns()}"
        script_file(folder, 2).write_text(f"return '{marker}'\n", encoding="utf-8")
        run.timed("push, one script changed", ["ps", "src/ServerScriptService/SpeedRun/M002.luau"], folder, place=place, timeout=1800)
        expect(marker in str(luau(run, folder, place, "return game.ServerScriptService.SpeedRun.M002.Source")), "a pushed script did not reach Studio")

    for index in range(1, run.rounds + 1):
        marker = f"pushed many {index} {time.time_ns()}"
        for number in range(1, SCRIPTS + 1):
            script_file(folder, number).write_text(f"return '{marker}'\n", encoding="utf-8")
        run.timed(f"push, {SCRIPTS} scripts changed", ["ps", "--yes"], folder, place=place, timeout=1800)
        expect(marker in str(luau(run, folder, place, f"return game.ServerScriptService.SpeedRun.M{SCRIPTS:03d}.Source")), "pushed scripts did not reach Studio")

    for index in range(1, run.rounds + 1):
        value = round(0.3 + 0.1 * index, 2)
        run.timed("store edit, one property", ["bs", "Workspace", "SpeedRun.P0002", "-p", "Transparency", "--num", str(value)], folder)
        run.timed("push, one property changed", ["ps", "--yes"], folder, place=place, timeout=1800)
        expect(str(value) in str(luau(run, folder, place, "return tostring(math.round(workspace.SpeedRun.P0002.Transparency * 100) / 100)")), "a pushed property did not reach Studio")

    for index in range(1, run.rounds + 1):
        value = round(0.2 + 0.1 * index, 2)
        run.timed(f"store edit, {PARTS:,} instances", ["bs", "Workspace", "SpeedRun", "--where", "is:Part", "-p", f"Reflectance={value}"], folder, timeout=600)
        run.timed(f"push, {PARTS:,} instances changed", ["ps", "--yes"], folder, place=place, timeout=1800)
        expect(str(value) in str(luau(run, folder, place, f"return tostring(math.round(workspace.SpeedRun.P{PARTS:04d}.Reflectance * 100) / 100)")), "pushed properties did not reach Studio")

    for index in range(1, run.rounds + 1):
        run.timed("Live Sync, start", ["lon", "--wait", "120"], folder, place=place, timeout=900)
        marker = f"live file {index} {time.time_ns()}"
        started = time.perf_counter()
        script_file(folder, 3).write_text(f"return '{marker}'\n", encoding="utf-8")
        run.rbx(["lst", "--wait", "60"], folder, place=place, timeout=120)
        run.sample("Live Sync, file edit to Studio", time.perf_counter() - started)
        expect(marker in str(luau(run, folder, place, "return game.ServerScriptService.SpeedRun.M003.Source")), "a file edit did not reach Studio through Live Sync")
        marker = f"live studio {index} {time.time_ns()}"
        target = script_file(folder, 4)
        luau(run, folder, place, f"game.ServerScriptService.SpeedRun.M004.Source = {json.dumps('return ' + repr(marker))} return 1")
        started = time.perf_counter()
        while marker not in target.read_text(encoding="utf-8", errors="replace"):
            expect(time.perf_counter() - started < 180, "a Studio edit did not reach its file through Live Sync")
            time.sleep(0.05)
        run.sample("Live Sync, Studio edit to file", time.perf_counter() - started)
        run.timed("Live Sync, stop", ["lof"], folder, place=place, timeout=300)

    for index in range(1, run.rounds + 1):
        run.timed("playtest, start", ["play", "-s"], folder, place=place, timeout=900)
        run.timed("playtest, stop", ["play", "-x"], folder, place=place, timeout=600)

    for index in range(1, run.rounds + 1):
        run.timed("save and close Studio", ["sx", "--save"], folder, place=place, timeout=900)
        wait_closed(run, folder, place)
        if index < run.rounds:
            open_studio(run, folder, place, "open the place in Studio")
    data, _ = run.timed("compare the saved place with the project", ["cmp", "place.rbxl", "--full", "--all"], folder, timeout=1800)
    expect(differences(data, studio_saved=True) == 0, "the saved place differs from the project after the run")


def project_files(folder: Path) -> dict:
    files = (path for path in folder.rglob("*")
             if path.is_file() and ".renium" not in path.relative_to(folder).parts)
    return {str(path.relative_to(folder)): (path.stat().st_mtime_ns, hashlib.sha256(path.read_bytes()).digest())
            for path in files}


def push(run: Run, source: Path, root: Path) -> None:
    empty_project = fresh(root / "empty")
    run.rbx(["init", "."], empty_project)
    # A place needs one store to be built from; the push removes the marker.
    run.rbx(["ba", "Workspace", "-n", "SpeedRunMarker", "-c", "Folder", "--no-parent"], empty_project)
    run.rbx(["bep", "-o", "empty.rbxl"], empty_project)
    empty = empty_project / "empty.rbxl"
    expect(empty.is_file(), "no empty place could be built")
    for index in range(1, run.rounds + 1):
        folder = fresh(root / f"push-{index}")
        imported = folder / "imported.rbxl"
        shutil.copy2(source, imported)
        run.rbx(["init", "."], folder)
        _, data, _ = run.rbx(["pi", "imported.rbxl"], folder, timeout=1800)
        imported.unlink()
        place = folder / "place.rbxl"
        shutil.copy2(empty, place)
        open_studio(run, folder, place, "open an empty place in Studio")
        data, _ = run.timed("push, whole project into an empty place", ["ps", "--yes"], folder, place=place, timeout=3600, ok=False)
        expect((data or {}).get("ok") is True and data.get("verified") is True, "the whole import was not verified")
        writes = data.get("supplementalImportWrites")
        expect(type(writes) is int and writes >= 0, "the push did not report its supplemental write count")
        offline = data.get("supplementalImportOfflineWrites", 0)
        expect(type(offline) is int and offline == 0, "the whole import used supplemental offline place-file writes")
        run.supplemental_import_writes.append(writes)
        run.log(f"Whole import needed {writes} individual supplemental field writes")
        expect(writes == 0, f"the whole import fell back to {writes} individual supplemental field writes")
        # This read queues behind the actual continuation, including when the
        # push caller has already disconnected. No delay estimates completion.
        run.timed("settle after the push into an empty place", ["l", "return true"], folder, place=place, timeout=1800)
        data, _ = run.timed("push again, nothing changed", ["ps", "--yes"], folder, place=place, timeout=1800)
        expect((data or {}).get("unchanged") is True, "a second push into the filled place still changed it")
        before_pull = project_files(folder)
        run.timed("pull after the push into an empty place", ["pl"], folder, place=place, timeout=1800)
        expect(project_files(folder) == before_pull, "pulling the filled place changed project files")
        run.rbx(["sx", "--terminate"], folder, place=place, ok=False, timeout=300)
        wait_closed(run, folder, place)
        shutil.rmtree(folder, ignore_errors=True)


PHASES = {"offline": offline, "studio": studio, "push": push}
DEFAULT_PHASES = ["offline", "studio", "push"]


def history() -> list[dict]:
    path = REPORTS / "history.jsonl"
    if not path.is_file():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def baseline(records: list[dict], name: str) -> float | None:
    values = [record["averages"][name] for record in records if name in record.get("averages", {})][-BASELINE_RUNS:]
    return statistics.median(values) if values else None


def report(run: Run, records: list[dict]) -> list[str]:
    slower = []
    print(f"{'scenario':<46}{'average':>9}{'fastest':>9}{'slowest':>9}{'before':>9}{'change':>8}")
    for name, values in run.samples.items():
        average = statistics.fmean(values)
        before = baseline(records, name)
        change = ""
        if before:
            change = f"{(average / before - 1) * 100:+.0f}%"
            if average > before * SLOWER_RATIO and average - before > SLOWER_SECONDS:
                slower.append(name)
                change += " !"
        shown = f"{before:9.2f}" if before else f"{'':>9}"
        print(f"{name:<46}{average:9.2f}{min(values):9.2f}{max(values):9.2f}{shown}{change:>8}")
    return slower


def version(run: Run) -> dict:
    try:
        _, data, _ = run.rbx(["dr", "--json", "--root", str(SCRATCH)], SCRATCH, ok=False, json_mode=False, timeout=120)
    except Exception:
        data = None
    return {key: (data or {}).get(key) for key in ("version", "gitHash")}


def close_everything(run: Run) -> None:
    for place in run.opened:
        if place.parent.is_dir():
            run.rbx(["sx", "--terminate"], place.parent, place=place, ok=False, timeout=120)
    run.rbx(["dm", "stop", DAEMON], SCRATCH, ok=False, json_mode=False, timeout=120)


def clean_up() -> int:
    """Closes the Studios a run that was cut short left on its scratch places
    and stops its daemon. Only Studios the run's own daemon lists, on files
    under the scratch folder, are closed."""
    SCRATCH.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, RENIUM_DAEMON_NAME=DAEMON)
    env.pop("RENIUM_PLACE", None)

    def call(*args):
        try:
            return subprocess.run([RBX, *args], cwd=SCRATCH, env=env, capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=120, creationflags=NO_WINDOW).stdout
        except (OSError, subprocess.TimeoutExpired):
            return ""

    clients = []
    for line in reversed(call("--output-mode", "json", "cs").strip().splitlines()):
        if line.startswith("{"):
            try:
                clients = json.loads(line).get("clients") or []
            except ValueError:
                pass
            break
    scratch = str(SCRATCH).lower()
    closed = 0
    for client in clients:
        file = str(client.get("localFile") or "")
        if file.lower().startswith(scratch):
            call("--place", file, "sx", "--terminate")
            closed += 1
    call("dm", "stop", DAEMON)
    print(f"closed {closed} Studio(s)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--only", default="", help="comma-separated phases: offline, studio, push")
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--record", action="store_true", help="add this run to the history when it passes")
    parser.add_argument("--accept", action="store_true", help="add this run to the history even when it is slower")
    parser.add_argument("--keep", action="store_true", help="keep the scratch copies")
    parser.add_argument("--cleanup", action="store_true", help="close the Studios and the daemon of a run that was cut short, then exit")
    args = parser.parse_args()
    if args.cleanup:
        return clean_up()
    phases = [name.strip() for name in args.only.split(",") if name.strip()] or DEFAULT_PHASES
    for name in phases:
        if name not in PHASES:
            parser.error(f"unknown phase {name}")
    REPORTS.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    root = SCRATCH / stamp
    root.mkdir(parents=True)
    run = Run(args.rounds, REPORTS / f"{stamp}.log")
    failure = None
    try:
        source = source_place()
        for name in phases:
            print(f"phase {name}", flush=True)
            PHASES[name](run, source, root)
    except SpeedFailure as error:
        failure = str(error)
    except Exception as error:
        run.log(f"ERROR {error!r}")
        failure = "the run stopped on an error of its own"
    finally:
        close_everything(run)
    records = history()
    slower = report(run, records)
    record = {"stamp": stamp, **version(run), "rounds": args.rounds, "phases": phases, "averages": {name: round(statistics.fmean(values), 3) for name, values in run.samples.items()}, "samples": {name: [round(value, 3) for value in values] for name, values in run.samples.items()}, "failure": failure, "slower": slower}
    if run.supplemental_import_writes:
        record["supplementalImportWrites"] = run.supplemental_import_writes
        print("Individual supplemental import writes per round:", *run.supplemental_import_writes)
    (REPORTS / f"{stamp}.json").write_text(json.dumps(record, indent=1), encoding="utf-8")
    if failure:
        print(f"FAILED: {failure}. The log of this run in the speed-reports folder has the commands.")
    elif slower:
        print(f"SLOWER than the runs before: {', '.join(slower)}")
    if not failure and (args.accept or (args.record and not slower)):
        with (REPORTS / "history.jsonl").open("a", encoding="utf-8") as file:
            file.write(json.dumps({key: record[key] for key in ("stamp", "version", "gitHash", "rounds", "averages")}) + "\n")
        print("recorded")
    run.log_file.close()
    if not args.keep and not failure:
        shutil.rmtree(root, ignore_errors=True)
    return 1 if failure or (slower and not args.accept) else 0


if __name__ == "__main__":
    sys.stdout.reconfigure(encoding="utf-8")
    sys.exit(main())
