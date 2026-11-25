"""Live verification for `rbx fps`: Studio focus-loss render throttling removal.

Runs against an isolated scratch Studio (never focused, never raised, never
minimized, isolated named daemon) and verifies that `rbx fps unlock` removes
focus-loss and background-covering render throttling while preserving Studio's
own frame rate cap (never raising FPS above what a focused window gets):

  1. Edit viewport:
     held before (~60 FPS) -> unlocked (~240 / focused cap) -> covered by an
     opaque non-activating window (still ~240) -> late deactivation after
     unlock (still ~240) -> `rbx fps default` restores held rate (~60 FPS).
  2. Play solo client: same sequence.
  3. Player window of a local server test (`play -s -p 1 --mode play`): same
     sequence, plus `--server` unlock/default check.
  4. Global mode: `rbx fps unlock --global` set BEFORE starting a local-server
     Play test automatically adopts Edit, Server, and Player windows so the
     deactivated + covered Player window runs at the focused rate without any
     per-window command.

Never modifies Studio settings files on disk, never touches any Studio we did
not open, and cleans up all windows/settings in `finally`.
"""

from __future__ import annotations

import argparse
import ctypes as c
import ctypes.wintypes as w
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

WM_ACTIVATE = 0x0006
WM_SETFOCUS = 0x0007
WM_KILLFOCUS = 0x0008
WM_ACTIVATEAPP = 0x001C
WM_NCACTIVATE = 0x0086
WM_CLOSE = 0x0010
WM_DESTROY = 0x0002
WA_INACTIVE = 0
WA_ACTIVE = 1
SMTO_ABORTIFHUNG = 0x0002
GW_HWNDPREV = 3
WS_POPUP = 0x80000000
WS_VISIBLE = 0x10000000
WS_EX_NOACTIVATE = 0x08000000
WS_EX_TOOLWINDOW = 0x00000080
SWP_NOACTIVATE = 0x0010
SWP_SHOWWINDOW = 0x0040
SW_SHOWNOACTIVATE = 4

MEASURE_LUAU = """
local RunService = game:GetService("RunService")
local Stats = game:GetService("Stats")
local t0 = os.clock()
while os.clock() - t0 < 0.35 do
    RunService.RenderStepped:Wait()
end
local start = os.clock()
local frames = 0
local ftSum = 0
local dtSum = 0
local dtMin = math.huge
local dtMax = 0
while os.clock() - start < 1.5 do
    local dt = RunService.RenderStepped:Wait()
    frames += 1
    dtSum += dt
    if dt < dtMin then dtMin = dt end
    if dt > dtMax then dtMax = dt end
    ftSum += Stats.FrameTime
end
local elapsed = os.clock() - start
return {
    fps = frames / elapsed,
    frames = frames,
    elapsed = elapsed,
    meanDtMs = (dtSum / frames) * 1000,
    minDtMs = dtMin * 1000,
    maxDtMs = dtMax * 1000,
    statsFrameTimeMs = (ftSum / frames) * 1000,
}
""".strip()


def read_studio_framerate_cap() -> int:
    xml_path = Path(os.environ.get("LOCALAPPDATA", "")) / "Roblox" / "GlobalBasicSettings_13_Studio.xml"
    if xml_path.is_file():
        text = xml_path.read_text(encoding="utf-8", errors="ignore")
        match = re.search(r'<int name="FramerateCap">(\d+)</int>', text)
        if match:
            cap = int(match.group(1))
            if cap > 0:
                return cap
    return 240


def foreground_pid() -> int:
    user = c.WinDLL("user32")
    user.GetForegroundWindow.restype = w.HWND
    user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    owner = w.DWORD()
    hwnd = user.GetForegroundWindow()
    if hwnd:
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
    return int(owner.value)


def top_level_windows(pid: int) -> list[int]:
    user = c.WinDLL("user32")
    user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    user.IsWindowVisible.argtypes = [w.HWND]
    user.GetWindowTextLengthW.argtypes = [w.HWND]
    user.GetClassNameW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
    found: list[int] = []

    @c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
    def visit(hwnd, _):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        if owner.value == pid and user.IsWindowVisible(hwnd):
            cls = c.create_unicode_buffer(256)
            user.GetClassNameW(hwnd, cls, len(cls))
            if cls.value.startswith("Qt") or user.GetWindowTextLengthW(hwnd) > 0:
                found.append(int(hwnd))
        return True

    user.EnumWindows(visit, 0)
    return found


def deliver_deactivation(pid: int) -> int:
    """Deliver Windows focus-in then focus-out notifications (WM_SETFOCUS ->
    WM_KILLFOCUS + WM_ACTIVATE(WA_INACTIVE) + WM_ACTIVATEAPP(0)) to our scratch
    Studio's top-level Qt windows so Qt5's qwindows.dll transitions
    QGuiApplication::applicationState ApplicationActive -> ApplicationInactive
    and pushes Studio's focus-loss render throttle without ever giving the
    window real Windows focus or raising it."""
    user = c.WinDLL("user32")
    user.SendMessageTimeoutW.argtypes = [
        w.HWND,
        w.UINT,
        w.WPARAM,
        w.LPARAM,
        w.UINT,
        w.UINT,
        c.POINTER(c.c_size_t),
    ]
    user.SendMessageTimeoutW.restype = w.LPARAM
    windows = top_level_windows(pid)
    result = c.c_size_t()
    for hwnd in windows:
        user.SendMessageTimeoutW(
            hwnd, WM_SETFOCUS, 0, 0, SMTO_ABORTIFHUNG, 2000, c.byref(result)
        )
    time.sleep(0.15)
    for hwnd in windows:
        user.SendMessageTimeoutW(
            hwnd, WM_KILLFOCUS, 0, 0, SMTO_ABORTIFHUNG, 2000, c.byref(result)
        )
        user.SendMessageTimeoutW(
            hwnd, WM_ACTIVATE, WA_INACTIVE, 0, SMTO_ABORTIFHUNG, 2000, c.byref(result)
        )
        user.SendMessageTimeoutW(
            hwnd, WM_NCACTIVATE, 0, 0, SMTO_ABORTIFHUNG, 2000, c.byref(result)
        )
        user.SendMessageTimeoutW(
            hwnd, WM_ACTIVATEAPP, 0, 0, SMTO_ABORTIFHUNG, 2000, c.byref(result)
        )
    time.sleep(0.35)
    return len(windows)


class CoverWindow:
    """Non-activating opaque Win32 window positioned immediately above our
    scratch Studio HWND (staying behind all user foreground windows) to verify
    covered-window frame rate without ever stealing focus."""

    def __init__(self, target_hwnd: int):
        self.target_hwnd = target_hwnd
        self.hwnd: int = 0
        self.ready = threading.Event()
        self.thread: threading.Thread | None = None

    def __enter__(self) -> "CoverWindow":
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        self.ready.wait(timeout=5.0)
        time.sleep(0.25)
        return self

    def __exit__(self, *_exc) -> None:
        if self.hwnd:
            user = c.WinDLL("user32")
            user.PostMessageW.argtypes = [w.HWND, w.UINT, w.WPARAM, w.LPARAM]
            user.PostMessageW(self.hwnd, WM_CLOSE, 0, 0)
        if self.thread is not None:
            self.thread.join(timeout=3.0)

    def _run(self) -> None:
        user = c.WinDLL("user32")
        kernel = c.WinDLL("kernel32")
        user.GetWindowRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
        user.GetWindow.argtypes = [w.HWND, w.UINT]
        user.GetWindow.restype = w.HWND
        user.CreateWindowExW.argtypes = [
            w.DWORD,
            w.LPCWSTR,
            w.LPCWSTR,
            w.DWORD,
            c.c_int,
            c.c_int,
            c.c_int,
            c.c_int,
            w.HWND,
            w.HMENU,
            w.HINSTANCE,
            w.LPVOID,
        ]
        user.CreateWindowExW.restype = w.HWND
        user.SetWindowPos.argtypes = [w.HWND, w.HWND, c.c_int, c.c_int, c.c_int, c.c_int, w.UINT]
        user.ShowWindow.argtypes = [w.HWND, c.c_int]
        user.DestroyWindow.argtypes = [w.HWND]
        user.GetMessageW.argtypes = [c.POINTER(w.MSG), w.HWND, w.UINT, w.UINT]
        user.TranslateMessage.argtypes = [c.POINTER(w.MSG)]
        user.DispatchMessageW.argtypes = [c.POINTER(w.MSG)]

        rect = w.RECT()
        if not user.GetWindowRect(self.target_hwnd, c.byref(rect)):
            self.ready.set()
            return
        width = max(100, rect.right - rect.left)
        height = max(100, rect.bottom - rect.top)
        hinst = kernel.GetModuleHandleW(None)
        hwnd = user.CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            "STATIC",
            "ReniumFpsCover",
            WS_POPUP,
            rect.left,
            rect.top,
            width,
            height,
            None,
            None,
            hinst,
            None,
        )
        if not hwnd:
            self.ready.set()
            return
        self.hwnd = int(hwnd)
        insert_after = user.GetWindow(self.target_hwnd, GW_HWNDPREV)
        if insert_after:
            user.SetWindowPos(
                hwnd,
                insert_after,
                rect.left,
                rect.top,
                width,
                height,
                SWP_NOACTIVATE | SWP_SHOWWINDOW,
            )
        else:
            user.ShowWindow(hwnd, SW_SHOWNOACTIVATE)
        self.ready.set()
        msg = w.MSG()
        while user.GetMessageW(c.byref(msg), None, 0, 0) > 0:
            if msg.message == WM_CLOSE:
                user.DestroyWindow(hwnd)
                break
            user.TranslateMessage(c.byref(msg))
            user.DispatchMessageW(c.byref(msg))
        self.hwnd = 0


def list_studio_pids() -> list[int]:
    proc = subprocess.run(
        ["tasklist", "/FI", "IMAGENAME eq RobloxStudioBeta.exe", "/FO", "CSV", "/NH"],
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        creationflags=subprocess.CREATE_NO_WINDOW,
        timeout=15,
    )
    pids = []
    for line in proc.stdout.splitlines():
        parts = [p.strip('"') for p in line.split(",")]
        if len(parts) >= 2 and parts[0].lower() == "robloxstudiobeta.exe":
            try:
                pids.append(int(parts[1]))
            except ValueError:
                pass
    return pids


def parse_json_output(stdout: str, stderr: str) -> dict | list | None:
    for stream in (stdout, stderr):
        for line in reversed(stream.strip().splitlines()):
            line = line.strip()
            if not line or line.startswith("[renium]"):
                continue
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                continue
    for stream in (stdout, stderr):
        body = "\n".join(
            l for l in stream.splitlines() if not l.startswith("[renium]")
        ).strip()
        if body.startswith(("{", "[")):
            try:
                return json.loads(body)
            except json.JSONDecodeError:
                continue
    return None


class Harness:
    def __init__(self, rbx: str, place: Path, daemon_name: str, ports: str, control_port: int):
        self.rbx = rbx
        self.daemon_name = daemon_name
        self.ports = ports
        self.control_port = control_port
        self.scratch_dir = Path(tempfile.mkdtemp(prefix="renium-fps-scenario-"))
        self.global_home = self.scratch_dir / "global-home"
        (self.global_home / "AppData" / "Roaming").mkdir(parents=True, exist_ok=True)
        self.place_copy = self.scratch_dir / place.name
        shutil.copy2(place, self.place_copy)
        self.daemon_proc: subprocess.Popen | None = None
        self.studio_pids: set[int] = set()
        self.focus_violations: list[tuple[str, int]] = []
        self.global_enabled: bool = False

    def _env(self, *, global_isolated: bool = False) -> dict[str, str]:
        env = dict(os.environ)
        env["RENIUM_DAEMON_NAME"] = self.daemon_name
        env["RENIUM_PLACE"] = str(self.place_copy)
        if global_isolated:
            env["USERPROFILE"] = str(self.global_home)
            env["HOME"] = str(self.global_home)
            env["APPDATA"] = str(self.global_home / "AppData" / "Roaming")
        return env

    def check_unfocused(self, label: str) -> None:
        fg = foreground_pid()
        if fg and fg in self.studio_pids:
            self.focus_violations.append((label, fg))

    def run(
        self,
        *args: str,
        timeout: float = 120,
        check: bool = True,
        global_isolated: bool = False,
    ) -> dict:
        cmd = [self.rbx, "--output-mode", "json", *args]
        proc = subprocess.run(
            cmd,
            cwd=str(self.scratch_dir),
            env=self._env(global_isolated=global_isolated),
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            creationflags=subprocess.CREATE_NO_WINDOW,
            timeout=timeout,
        )
        self.check_unfocused(" ".join(args[:3]))
        parsed = parse_json_output(proc.stdout, proc.stderr)
        if check and (
            proc.returncode != 0
            or (isinstance(parsed, dict) and parsed.get("ok") is False)
        ):
            raise RuntimeError(
                f"rbx {' '.join(args)} failed (exit {proc.returncode}): "
                f"stdout={proc.stdout.strip()[:400]!r} stderr={proc.stderr.strip()[:400]!r}"
            )
        return parsed if isinstance(parsed, dict) else {"raw": proc.stdout, "exit": proc.returncode}

    def start(self) -> None:
        self.run("init", ".")
        self.daemon_proc = subprocess.Popen(
            [
                self.rbx,
                "bd",
                "--name",
                self.daemon_name,
                "--ports",
                self.ports,
                "--control-port",
                str(self.control_port),
            ],
            cwd=str(self.scratch_dir),
            env=self._env(),
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            creationflags=subprocess.CREATE_NO_WINDOW,
        )
        time.sleep(1.0)
        opened = self.run("so", str(self.place_copy), timeout=180)
        pid = opened.get("pid")
        if pid:
            self.studio_pids.add(int(pid))
        self.refresh_pids()

    def refresh_pids(self) -> list[dict]:
        clients = self.run("cs").get("clients") or []
        for client in clients:
            pid = client.get("pid")
            if pid:
                self.studio_pids.add(int(pid))
        return clients

    def pid_for_role(self, role: str) -> int:
        clients = self.refresh_pids()
        for client in clients:
            if client.get("role") == role and client.get("pid"):
                return int(client["pid"])
        raise RuntimeError(f"No Studio client found for role {role!r}: {clients}")

    def measure(self, mode: str) -> dict:
        if mode == "edit":
            res = self.run("l", "--edit", MEASURE_LUAU, timeout=60)
        elif mode == "client":
            res = self.run("lc", MEASURE_LUAU, "1", timeout=60)
        else:
            raise ValueError(f"Unknown measure mode {mode}")
        results = res.get("results") or []
        if not results or not isinstance(results[0], dict):
            raise RuntimeError(f"Unexpected Luau measurement result: {res}")
        return results[0]

    def close(self) -> None:
        if self.global_enabled:
            try:
                self.run(
                    "fps",
                    "default",
                    "--global",
                    check=False,
                    timeout=20,
                    global_isolated=True,
                )
            except Exception:
                pass
            self.global_enabled = False
        try:
            self.run("play", "-x", check=False, timeout=30)
        except Exception:
            pass
        try:
            self.run("sx", "--terminate", check=False, timeout=30)
        except Exception:
            pass
        if self.daemon_proc is not None:
            try:
                self.daemon_proc.terminate()
                self.daemon_proc.wait(timeout=5)
            except Exception:
                try:
                    self.daemon_proc.kill()
                except Exception:
                    pass
        shutil.rmtree(self.scratch_dir, ignore_errors=True)


def check_unlocked_rate(fps: float, frame_time_ms: float, cap: int, tolerance: float) -> bool:
    if fps > cap * (1.0 + tolerance):
        return False
    allowed = 1000.0 / frame_time_ms if frame_time_ms > 0 else float(cap)
    target = min(float(cap), allowed)
    return fps >= target * (1.0 - tolerance)


def run_surface(
    harness: Harness,
    label: str,
    measure_mode: str,
    target_pid: int,
    fps_target_args: list[str],
    cap: int,
    cap_tolerance: float,
    default_tolerance: float,
) -> dict:
    windows_notified = deliver_deactivation(target_pid)
    harness.check_unfocused(f"{label}:deactivate-before")
    before_status = harness.run("fps", "status", *fps_target_args)
    before = harness.measure(measure_mode)

    unlock_status = harness.run("fps", "unlock", *fps_target_args)
    try:
        unlocked = harness.measure(measure_mode)
        hwnds = top_level_windows(target_pid)
        if not hwnds:
            raise RuntimeError(f"No visible top-level window found for PID {target_pid}")
        with CoverWindow(hwnds[0]):
            harness.check_unfocused(f"{label}:covered")
            covered = harness.measure(measure_mode)
        deliver_deactivation(target_pid)
        harness.check_unfocused(f"{label}:deactivate-while-unlocked")
        after_late_deact = harness.measure(measure_mode)
        unlocked_status_after = harness.run("fps", "status", *fps_target_args)
    finally:
        default_status = harness.run("fps", "default", *fps_target_args)

    after = harness.measure(measure_mode)

    before_fps = float(before["fps"])
    unlocked_fps = float(unlocked["fps"])
    covered_fps = float(covered["fps"])
    late_deact_fps = float(after_late_deact["fps"])
    after_fps = float(after["fps"])

    unlocked_ft = float(unlocked["statsFrameTimeMs"])
    covered_ft = float(covered["statsFrameTimeMs"])
    late_ft = float(after_late_deact["statsFrameTimeMs"])

    held_before = before_fps <= cap * 0.40
    unlocked_ok = check_unlocked_rate(unlocked_fps, unlocked_ft, cap, cap_tolerance)
    covered_ok = check_unlocked_rate(covered_fps, covered_ft, cap, cap_tolerance)
    late_deact_ok = check_unlocked_rate(late_deact_fps, late_ft, cap, cap_tolerance)
    never_above_cap = max(unlocked_fps, covered_fps, late_deact_fps) <= cap * (1.0 + cap_tolerance)
    after_rel_diff = abs(after_fps - before_fps) / before_fps if before_fps > 0 else 1.0
    min_unlocked_fps = min(unlocked_fps, covered_fps, late_deact_fps)
    restored_default = (
        after_rel_diff <= default_tolerance
        and after_fps <= min_unlocked_fps * 0.75
        and default_status.get("hookInstalled") is False
        and default_status.get("unlocked") is False
    )
    passed = (
        held_before
        and unlocked_ok
        and covered_ok
        and late_deact_ok
        and never_above_cap
        and restored_default
    )

    return {
        "surface": label,
        "targetPid": target_pid,
        "windowsNotified": windows_notified,
        "studioCapFps": cap,
        "beforeFps": round(before_fps, 2),
        "unlockedFps": round(unlocked_fps, 2),
        "coveredFps": round(covered_fps, 2),
        "afterLateDeactivationFps": round(late_deact_fps, 2),
        "afterDefaultFps": round(after_fps, 2),
        "engineFrameTimeMs": round(unlocked_ft, 3),
        "heldBefore": held_before,
        "unlockedOk": unlocked_ok,
        "coveredOk": covered_ok,
        "lateDeactivationOk": late_deact_ok,
        "neverAboveCap": never_above_cap,
        "afterVsBeforeDiff": round(after_rel_diff, 3),
        "restoredDefault": restored_default,
        "passed": passed,
        "beforeStatus": before_status,
        "unlockStatus": unlock_status,
        "unlockedStatusAfter": unlocked_status_after,
        "defaultStatus": default_status,
    }


def main() -> int:
    parser = argparse.ArgumentParser(description="Verify unfocused Studio FPS unlock")
    parser.add_argument(
        "--rbx",
        default=os.environ.get("RENIUM_SCENARIO_RBX", "rbx"),
        help="Path to rbx executable",
    )
    parser.add_argument(
        "--place",
        required=True,
        help="Scratch .rbxl place to copy and open",
    )
    parser.add_argument("--ports", default="8883,8884")
    parser.add_argument("--control-port", type=int, default=8885)
    parser.add_argument("--cap-tolerance", type=float, default=0.05)
    parser.add_argument("--default-tolerance", type=float, default=0.30)
    parser.add_argument("--skip-global", action="store_true")
    parser.add_argument(
        "--allow-global-with-other-studios",
        action="store_true",
        help="Run the --global step even when unrelated Studio processes are already open",
    )
    args = parser.parse_args()

    place = Path(args.place)
    if not place.is_file():
        print(f"Place file not found: {place}", file=sys.stderr)
        return 2

    cap = read_studio_framerate_cap()
    preexisting_studios = set(list_studio_pids())
    daemon_name = f"studio-fps-{os.getpid()}"
    harness = Harness(args.rbx, place, daemon_name, args.ports, args.control_port)
    rows: list[dict] = []
    global_report: dict | None = None
    try:
        harness.start()
        edit_pid = harness.pid_for_role("edit")

        rows.append(
            run_surface(
                harness,
                "edit",
                "edit",
                edit_pid,
                [],
                cap,
                args.cap_tolerance,
                args.default_tolerance,
            )
        )

        harness.run("play", "-s", timeout=120)
        harness.refresh_pids()
        try:
            rows.append(
                run_surface(
                    harness,
                    "play-solo",
                    "client",
                    edit_pid,
                    [],
                    cap,
                    args.cap_tolerance,
                    args.default_tolerance,
                )
            )
        finally:
            harness.run("play", "-x", timeout=60)
            time.sleep(1.0)

        harness.run("play", "-s", "-p", "1", "--mode", "play", timeout=180)
        harness.refresh_pids()
        try:
            player_pid = harness.pid_for_role("play-client")
            rows.append(
                run_surface(
                    harness,
                    "local-server-player",
                    "client",
                    player_pid,
                    ["--player", "1"],
                    cap,
                    args.cap_tolerance,
                    args.default_tolerance,
                )
            )
            harness.run("fps", "unlock", "--server")
            harness.run("fps", "default", "--server")
        finally:
            harness.run("play", "-x", timeout=60)
            time.sleep(1.0)

        if not args.skip_global and (
            not preexisting_studios or args.allow_global_with_other_studios
        ):
            harness.global_enabled = True
            harness.run("fps", "unlock", "--global", global_isolated=True)
            try:
                harness.run("play", "-s", "-p", "1", "--mode", "play", timeout=180)
                clients = harness.refresh_pids()
                expected_pids = {int(c["pid"]) for c in clients if c.get("pid")}
                player_pid = harness.pid_for_role("play-client")
                adopted = False
                last_global = {}
                deadline = time.monotonic() + 10.0
                while time.monotonic() < deadline:
                    last_global = harness.run(
                        "fps", "status", "--global", global_isolated=True
                    )
                    unlocked_pids = {
                        int(p["pid"])
                        for p in (last_global.get("processes") or [])
                        if p.get("unlocked") is True
                    }
                    if expected_pids and expected_pids.issubset(unlocked_pids):
                        adopted = True
                        break
                    time.sleep(0.3)
                deliver_deactivation(player_pid)
                hwnds = top_level_windows(player_pid)
                with CoverWindow(hwnds[0]):
                    harness.check_unfocused("global:covered-player")
                    player_global = harness.measure("client")
                player_fps = float(player_global["fps"])
                player_ft = float(player_global["statsFrameTimeMs"])
                global_passed = adopted and check_unlocked_rate(
                    player_fps, player_ft, cap, args.cap_tolerance
                )
                global_report = {
                    "expectedPids": sorted(expected_pids),
                    "adoptedAllPlayProcesses": adopted,
                    "playerFpsUnderGlobal": round(player_fps, 2),
                    "playerFrameTimeMs": round(player_ft, 3),
                    "passed": global_passed,
                    "status": last_global,
                }
            finally:
                harness.run("play", "-x", check=False, timeout=60)
                harness.run(
                    "fps",
                    "default",
                    "--global",
                    check=False,
                    timeout=20,
                    global_isolated=True,
                )
                harness.global_enabled = False
        elif not args.skip_global and preexisting_studios:
            global_report = {
                "skipped": True,
                "reason": (
                    f"Preexisting Studio process(es) {sorted(preexisting_studios)} are open; "
                    "pass --allow-global-with-other-studios to run --global live"
                ),
                "passed": None,
            }
    finally:
        harness.close()

    print(f"\n=== Studio FPS Verification (Studio cap={cap} FPS, focus-throttled & covered) ===")
    print(
        f"{'Surface':<20} {'Before':>7} {'Unlock':>8} {'Covered':>8} {'LateDeact':>10} "
        f"{'Default':>8} {'FrameMs':>8} {'Pass':>6}"
    )
    for row in rows:
        print(
            f"{row['surface']:<20} {row['beforeFps']:>7.1f} {row['unlockedFps']:>8.1f} "
            f"{row['coveredFps']:>8.1f} {row['afterLateDeactivationFps']:>10.1f} "
            f"{row['afterDefaultFps']:>8.1f} {row['engineFrameTimeMs']:>8.3f} {str(row['passed']):>6}"
        )
    if global_report is not None:
        if global_report.get("skipped"):
            print(f"\nGlobal mode: SKIPPED ({global_report['reason']})")
        else:
            print(
                f"\nGlobal mode (--global before Play, deactivated + covered): "
                f"passed={global_report['passed']} adopted={global_report['adoptedAllPlayProcesses']} "
                f"pids={global_report['expectedPids']} player={global_report['playerFpsUnderGlobal']} FPS "
                f"(frameTime={global_report['playerFrameTimeMs']} ms)"
            )
    print(f"Focus violations: {harness.focus_violations}")

    summary = {
        "ok": (
            all(r["passed"] for r in rows)
            and not harness.focus_violations
            and (global_report is None or global_report["passed"] is not False)
        ),
        "studioCapFps": cap,
        "focusViolations": harness.focus_violations,
        "surfaces": rows,
        "global": global_report,
    }
    print(json.dumps(summary, indent=2))
    return 0 if summary["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
