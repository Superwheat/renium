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

    shutil.rmtree(WORK, ignore_errors=True)
    print("scenario runner self-test passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
