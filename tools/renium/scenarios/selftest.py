"""Checks how run.py turns shard reports into one result, without a Studio.

    python -I tools/renium/scenarios/selftest.py

The shards and serial phases are replaced by a stand-in child that writes the
reports a case calls for, so every case exercises the real splitting,
merging, second run and exit code of the parallel driver.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import shutil
import sys
import tempfile
import types
from pathlib import Path

HERE = Path(__file__).resolve().parent
WORK = Path(tempfile.mkdtemp(prefix="renium-scenario-selftest-"))
# run.py reads its folders from the environment when it is imported.
os.environ["LOCALAPPDATA"] = str(WORK / "local")
os.environ["RENIUM_SCENARIO_SCRATCH"] = str(WORK / "runs")
os.environ["RENIUM_SCENARIO_RBX"] = sys.executable
sys.path.insert(0, str(HERE))
import run  # noqa: E402

SECRET = "PRIVATE-PATH-MARKER"
CHILD = '''
import argparse, json, os, sys
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("--only", default="")
parser.add_argument("--seed")
parser.add_argument("--shard", default="")
parser.add_argument("--child", action="store_true")
parser.add_argument("--report-prefix", default="")
args = parser.parse_args()
assert args.child, "the driver marks every process it starts"
case = json.loads(os.environ["SELFTEST_CASE"])
reports = Path(os.environ["LOCALAPPDATA"]) / "Renium" / "scenario-reports"
wanted = [name for name in args.only.split(",") if name]
phase = "shard" if args.shard else "serial"
print("child", phase, wanted, "__SECRET__")


def write(rows, suffix=".json"):
    (reports / (args.report_prefix + suffix)).write_text(json.dumps(rows), encoding="utf-8")


rows = []
for name in wanted:
    status = case.get(phase, {}).get(name, "pass")
    if status == "die":
        write(rows, ".partial.json")
        sys.exit(3)
    reason = "another Studio stayed connected" if status == "skip" else "__SECRET__"
    rows.append({"id": name, "title": "title of " + name, "size": "small", "status": status, "reason": reason, "seconds": 1.5, "log": []})
write(rows)
'''.replace("__SECRET__", SECRET)


def drive(case: dict, *, rerun_failed: bool = False, brief: bool = False, shards: int = 2) -> tuple[int, str, list[dict]]:
    """Runs four rows through the parallel driver; returns its exit code, what
    it printed and the merged report."""
    reports = run.REPORTS
    shutil.rmtree(reports, ignore_errors=True)
    os.environ["SELFTEST_CASE"] = json.dumps(case)
    rows = [run.Scenario(id=name, title=f"title of {name}", size="small", run=lambda ctx: None) for name in "ABCD"]
    printed = io.StringIO()
    with contextlib.redirect_stdout(printed):
        code = run.run_parallel([], shards, 7, rows, rerun_failed, brief)
    merged = [path for path in reports.glob("*.json") if path.stem.replace("-", "").isdigit()]
    report = json.loads(merged[0].read_text(encoding="utf-8")) if len(merged) == 1 else []
    return code, printed.getvalue(), report


def status_of(report: list[dict]) -> dict:
    return {row["id"]: row["status"] for row in report}


HELD = "Studio (pid 7) asked to open its place and has not begun, while macOS is asking on this Mac's screen: RobloxStudio would like to access files in your Documents folder."


def session_rows(reset_errors=(), fail_first=None) -> tuple[list[str], int]:
    """Runs the real row loop over two rows with a stand-in session. `reset_errors`
    are raised in turn by a row's reset and by the reopen that follows a failed
    reset; `fail_first` fails the first row itself. Returns the two outcomes and
    how often Studio was reopened."""
    errors = list(reset_errors)
    reopened = []

    class StubContext:
        def __init__(self):
            self.log, self.studio_open, self.row_started = [], True, 0.0

    class StubSession:
        resets = reopens = 0
        root = WORK / "stub-session"
        slot = types.SimpleNamespace(acquire=lambda: None)

        def __init__(self, size, label):
            pass

        def context(self, scenario):
            return StubContext()

        def reset(self, ctx):
            if errors:
                raise RuntimeError(errors.pop(0))
            return "push"

        def close_studio(self, ctx):
            pass

        def open_studio(self, ctx):
            reopened.append(1)
            if errors:
                raise RuntimeError(errors.pop(0))

        def finish(self, ctx, keep):
            pass

    class StubLock:
        def __init__(self, *args):
            pass

        def __getattr__(self, name):
            return lambda *args: None

    def first(ctx):
        if fail_first:
            raise run.ScenarioFailure(fail_first)

    rows = [run.Scenario(id="A", title="", size="small", run=first), run.Scenario(id="B", title="", size="small", run=lambda ctx: None)]
    real = run.Session, run.RowLock, run.RowGate
    run.Session, run.RowLock, run.RowGate = StubSession, StubLock, StubLock
    run.ABORT_FILE.unlink(missing_ok=True)
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            outcomes = run.run_session(rows, "small", "stub", False, 1)
    finally:
        run.Session, run.RowLock, run.RowGate = real
        run.ABORT_FILE.unlink(missing_ok=True)
    return [row["status"] for row in outcomes], len(reopened)


def reset_after_a_failed_push(error: str) -> str:
    """The real Session.reset when its push fails with `error`: how it recovered,
    or what it raised."""
    session = object.__new__(run.Session)
    session.environment, session.studio_open, session.counts, session.resets, session.reopens = dict(os.environ), True, {}, 0, 0
    session.restore_files = lambda: None
    session.close_studio = session.open_studio = lambda ctx: None
    ctx = types.SimpleNamespace(log=[], studio_open=True, guard_binding=lambda: None, luau=lambda source: {},
                                rbx=lambda *args, **options: types.SimpleNamespace(ok=args[0] != "ps", error=error))
    real = run.terminate_pids, run.orphan_play_server_pids
    run.terminate_pids, run.orphan_play_server_pids = (lambda pids: None), (lambda: [])
    cwd = os.getcwd()
    try:
        return session.reset(ctx)
    except run.ScenarioFailure as failure:
        return f"raised: {failure}"
    finally:
        run.terminate_pids, run.orphan_play_server_pids = real
        os.chdir(cwd)


def isolated_rows(first_reason: str) -> list[str]:
    """The real isolated loop over two rows whose first fails with `first_reason`."""
    rows = [run.Scenario(id=name, title="", size="small", run=lambda ctx: None) for name in "AB"]
    real = run.run_scenario
    run.run_scenario = lambda scenario, keep: {"id": scenario.id, "status": "fail" if scenario.id == "A" else "pass", "reason": first_reason if scenario.id == "A" else "", "seconds": 0, "log": []}
    run.ABORT_FILE.unlink(missing_ok=True)
    try:
        with contextlib.redirect_stdout(io.StringIO()):
            return [row["status"] for row in run.run_isolated(rows, False)]
    finally:
        run.run_scenario = real
        run.ABORT_FILE.unlink(missing_ok=True)


def main() -> int:
    child_dir = WORK / "child"
    child_dir.mkdir(parents=True)
    (child_dir / "run.py").write_text(CHILD, encoding="utf-8")
    run.HERE = child_dir
    run.SHARD_START_GAP_SECONDS = 0

    code, printed, report = drive({})
    assert code == 0 and status_of(report) == dict.fromkeys("ABCD", "pass"), (code, report)
    assert "4/4 passed" in printed, printed

    # Every row runs once: the shards take the rows the driver gives them.
    assert sorted(row["id"] for row in report) == list("ABCD"), report

    # A shard that dies keeps the rows it finished and fails the rest. The
    # split depends on recorded durations, so it is read with none recorded,
    # the state every case starts from.
    shutil.rmtree(run.REPORTS, ignore_errors=True)
    second_of_shard_one = run.assign_shards([run.Scenario(id=name, title="", size="small", run=lambda ctx: None) for name in "ABCD"], 2, 7)[1][1].id
    code, printed, report = drive({"shard": {second_of_shard_one: "die"}})
    assert code == 1, printed
    assert status_of(report)[second_of_shard_one] == "error", report
    assert sum(1 for row in report if row["status"] == "pass") == 3, report

    # A second run that passes turns the row green and keeps the first outcome.
    code, printed, report = drive({"shard": {"B": "fail"}}, rerun_failed=True)
    assert code == 0 and "passed on the second run only: B" in printed, printed
    assert next(row for row in report if row["id"] == "B")["firstRun"]["status"] == "fail", report

    # A second run that fails or cannot run leaves the row failed.
    for second in ("fail", "skip"):
        code, printed, report = drive({"shard": {"B": "fail"}, "serial": {"B": second}}, rerun_failed=True)
        assert code == 1 and status_of(report)["B"] == "fail", (second, printed)
        assert next(row for row in report if row["id"] == "B")["secondRun"]["status"] == second, report

    # Without the option a failed row is not run again.
    code, printed, report = drive({"shard": {"B": "fail"}})
    assert code == 1 and "second run" not in printed, printed

    # A row that never got a Studio of its own did not pass.
    code, printed, report = drive({"shard": {"C": "skip"}, "serial": {"C": "skip"}})
    assert code == 1 and status_of(report)["C"] == "fail", (printed, report)
    code, printed, report = drive({"shard": {"C": "skip"}})
    assert code == 0 and status_of(report)["C"] == "pass", (printed, report)

    # A brief run prints ids and outcomes, never a reason or a log.
    code, printed, report = drive({"shard": {"B": "fail"}, "serial": {"B": "fail"}}, rerun_failed=True, brief=True)
    assert code == 1 and "[B] FAIL" in printed and "title of B" in printed, printed
    assert SECRET not in printed and str(run.REPORTS) not in printed, printed
    code, printed, report = drive({"shard": {"B": "fail"}})
    assert SECRET in printed, "a full run prints the shard logs"

    # More shards than rows: the empty ones are not started.
    code, printed, report = drive({}, shards=6)
    assert code == 0 and len(report) == 4, (printed, report)

    # A failure only the user can clear stops the run after one row, and a
    # reset that fails with it is not answered by reopening Studio.
    assert "macOS" in (run.abort_at_once_reason(HELD, "LIFE-003") or "") and "LIFE-003" in run.abort_at_once_reason(HELD, "LIFE-003")
    assert "signed out" in (run.abort_at_once_reason("Studio (pid 7) is signed out and shows the Roblox sign-in dialog instead of its place", "A") or "")
    assert run.abort_at_once_reason("Studio (pid 7) has not connected within 120s.", "A") is None
    assert not run.infrastructure_failure({"status": "fail", "reason": HELD})
    assert session_rows(fail_first=HELD) == (["fail", "abort"], 0)
    assert session_rows(reset_errors=[HELD]) == (["error", "abort"], 0)
    assert session_rows(reset_errors=["push failed", HELD]) == (["error", "abort"], 1)
    assert session_rows(reset_errors=["push failed"]) == (["pass", "pass"], 1)
    assert session_rows(fail_first="Sign is missing") == (["fail", "pass"], 0)
    assert reset_after_a_failed_push(HELD) == f"raised: {HELD}"
    assert reset_after_a_failed_push("push failed") == "reopen"
    assert isolated_rows(HELD) == ["fail", "abort"]
    assert isolated_rows("Sign is missing") == ["fail", "pass"]

    shutil.rmtree(WORK, ignore_errors=True)
    print("scenario runner self-test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
