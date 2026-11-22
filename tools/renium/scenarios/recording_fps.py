"""Live contract for PLAY-107. Requires Windows, ffmpeg and spare CPU capacity.

Run through the build/resource lock. A changed Studio viewport identity, capture
geometry, unavailable client capture, stale pictures or slow encoding fails
this test. It never activates, moves, resizes or minimizes a window.
"""
import ctypes as c
import ctypes.wintypes as w
import json
import shutil
import subprocess
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from run import ScenarioSkip


MOTION = '''local camera=workspace.CurrentCamera
local originalType,originalFrame=camera.CameraType,camera.CFrame
local part=Instance.new("Part")
part.Name="ReniumRecordingContract"; part.Anchored=true; part.Size=Vector3.new(8,8,2)
part.Parent=workspace
local ok,detail=xpcall(function()
 camera.CameraType=Enum.CameraType.Scriptable
 camera.CFrame=CFrame.lookAt(Vector3.new(0,8,25),Vector3.new(0,4,0))
 local started=os.clock()
 while os.clock()-started<8 do
  local t=os.clock()-started
  part.CFrame=CFrame.new(math.sin(t*3)*6,4,0)*CFrame.Angles(t,t*2,0)
  part.Color=Color3.fromHSV((t*0.17)%1,1,1)
  task.wait()
 end
end,tostring)
part:Destroy(); camera.CameraType=originalType; camera.CFrame=originalFrame
assert(ok,detail)
return true'''


def hidden(args, **kwargs):
    return subprocess.run(args, stdin=subprocess.DEVNULL, capture_output=True,
                          text=True, creationflags=subprocess.CREATE_NO_WINDOW,
                          check=True, timeout=60, **kwargs)


def geometry(pid, filename):
    user = c.WinDLL("user32")
    user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    user.GetWindowTextW.argtypes = [w.HWND, w.LPWSTR, c.c_int]
    user.IsWindowVisible.argtypes = [w.HWND]
    user.IsIconic.argtypes = [w.HWND]
    user.GetClientRect.argtypes = [w.HWND, c.POINTER(w.RECT)]
    user.ClientToScreen.argtypes = [w.HWND, c.POINTER(w.POINT)]
    user.GetAncestor.argtypes = [w.HWND, w.UINT]
    user.GetAncestor.restype = w.HWND
    user.SetThreadDpiAwarenessContext.argtypes = [c.c_void_p]
    user.SetThreadDpiAwarenessContext.restype = c.c_void_p
    previous = user.SetThreadDpiAwarenessContext(c.c_void_p(-4))
    roots = []

    @c.WINFUNCTYPE(w.BOOL, w.HWND, w.LPARAM)
    def visit(hwnd, _):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        if owner.value == pid and user.IsWindowVisible(hwnd):
            title = c.create_unicode_buffer(1024)
            user.GetWindowTextW(hwnd, title, len(title))
            if Path(filename).stem in title.value:
                roots.append(hwnd)
        return True

    def client(hwnd):
        owner = w.DWORD()
        user.GetWindowThreadProcessId(hwnd, c.byref(owner))
        assert owner.value == pid
        rect, origin = w.RECT(), w.POINT()
        assert user.GetClientRect(hwnd, c.byref(rect))
        assert user.ClientToScreen(hwnd, c.byref(origin))
        return origin.x, origin.y, rect.right, rect.bottom

    try:
        user.EnumWindows(visit, 0)
        assert len(roots) == 1, "Studio root is missing or ambiguous"
        root = roots[0]
        assert not user.IsIconic(root), "Studio was minimized"
        script = f'''Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes
$root=[System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]{root})
$condition=New-Object System.Windows.Automation.PropertyCondition([System.Windows.Automation.AutomationElement]::AutomationIdProperty,'QEngineWidget')
$found=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,$condition)
$matches=@($found | Where-Object {{ $_.Current.ClassName -eq 'RBX::Studio::QEngineWidget' -and $_.Current.ProcessId -eq {pid} -and $_.Current.NativeWindowHandle -ne 0 }})
if($matches.Count -ne 1) {{ throw 'Viewport identity changed' }}
$matches[0].Current.NativeWindowHandle | ConvertTo-Json -Compress'''
        viewport = json.loads(hidden(["powershell.exe", "-NoProfile", "-NonInteractive",
                                      "-WindowStyle", "Hidden", "-Command", script]).stdout)
        assert user.GetAncestor(viewport, 2) == root and user.IsWindowVisible(viewport)
        return client(root), client(viewport)
    finally:
        if previous:
            user.SetThreadDpiAwarenessContext(previous)


def recording_keeps_its_rate_unfocused(ctx):
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        raise ScenarioSkip("ffmpeg is needed to count the distinct frames")
    opened = ctx.open_studio()
    pid = opened.get("pid")
    assert pid
    script = ctx.write("recording-motion.luau", MOTION)
    ctx.rbx("ck", str(script))
    user = c.WinDLL("user32")
    user.GetForegroundWindow.restype = w.HWND
    user.GetWindowThreadProcessId.argtypes = [w.HWND, c.POINTER(w.DWORD)]
    results = []
    failures = []
    try:
        for play in [False, True]:
            if play:
                ctx.rbx("play", "-s")
            before = geometry(pid, ctx.place)
            root, viewport = before
            bounds = viewport if play else root
            x = (viewport[0] - bounds[0] + viewport[2] // 4) & ~1
            y = (viewport[1] - bounds[1] + viewport[3] // 4) & ~1
            width, height = (viewport[2] // 2) & ~1, (viewport[3] // 2) & ~1
            video = ctx.root / ("unfocused-play.mp4" if play else "unfocused-edit.mp4")
            ctx.rbx("rs", *(["-p", "1"] if play else ["--studio"]),
                    "--fps", "60", "--quality", "100", "--max-seconds", "15", "-o", str(video))
            try:
                with ThreadPoolExecutor(max_workers=1) as pool:
                    args = ["lc", MOTION, "1"] if play else ["l", "--edit", MOTION]
                    animation = pool.submit(ctx.rbx, *args)
                    while not animation.done():
                        owner = w.DWORD()
                        user.GetWindowThreadProcessId(user.GetForegroundWindow(), c.byref(owner))
                        assert owner.value != pid, "Recording took focus"
                        time.sleep(.01)
                    animation.result()
            finally:
                ended = ctx.rbx("re", "--no-review")
            assert geometry(pid, ctx.place) == before, "Recording changed window geometry"
            assert ended.get("resizedFrames") == 0, ended.json
            assert (ended.get("width"), ended.get("height")) == (bounds[2] & ~1, bounds[3] & ~1)
            hashes = hidden([ffmpeg, "-v", "error", "-threads", "2", "-filter_threads", "1",
                             "-i", str(video), "-an", "-vf", f"crop={width}:{height}:{x}:{y}",
                             "-fps_mode", "passthrough", "-f", "framemd5", "-"]).stdout
            hashes = [line.rsplit(",", 1)[1].strip() for line in hashes.splitlines()
                      if line and not line.startswith("#")]
            results.append({"play": play, "recording": ended.json, "distinctViewportFrames": len(set(hashes))})
            if (ended.get("captureFallback") is not None or ended.get("throttled") is True
                    or ended.get("achievedFps", 0) < 45 or len(hashes) != ended.get("frames")
                    or len(set(hashes)) < len(hashes) * .8):
                failures.append(results[-1])
            ctx.write("recording-contract.json", json.dumps(results, indent=2))
    finally:
        ctx.rbx("play", "-x", ok=None)
    assert not failures, json.dumps(failures, indent=2)
    return results
