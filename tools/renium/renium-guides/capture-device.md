# Screenshots, recordings, and devices

Capture only when the task needs visual evidence. Use a screenshot for one state and a recording for motion or a transition. Record short-lived effects; don't time screenshots with sleeps. Save captures outside the project or delete them when done. Capturing does not require starting Play; use `--studio` for Edit. Read the Play guide only for runtime work.

## Check what the player sees

When the user describes something they saw, or you need to judge whether a change is visible, measuring state does not answer it: what a player sees is relative to their camera and their frame. Reproduce it the way the player does and look at the result:

```powershell
rbx rs -p 1 -o clips/jump.mp4 --max-seconds 6      # record the client the player is in
rbx inp -p 1 key E                                  # the real input, not a function call
rbx re                                              # stop; returns the overview image
rbx rf clips/jump.mp4 --page 1                      # open the frames and look
```

If the user says they cannot see it, record from their viewpoint before touching code again; do not re-measure internals. When a number is still needed, measure on screen (`Camera:WorldToViewportPoint`) or relative to `workspace.CurrentCamera.CFrame`, never world or chassis coordinates alone.

## Capture

```powershell
rbx sc --studio -o studio.png
rbx sc -p 1 -o client.png
rbx rs --studio -o clips/edit.mp4
rbx re
```

Render quality decides what a capture shows. `rbx gfx` reports Studio's edit and play levels (1-21, `0` = automatic) plus the in-game slider (1-10, read only). Before visual evidence of lighting, materials or effects, raise it: `rbx gfx max` for the Edit window, `rbx gfx max --player 1` for a client, or `rbx gfx set --play 21`. These are Studio-wide settings that persist after Studio closes, so run `rbx gfx restore` (with the same `--player`) once the capture is done; never leave an override behind. Leave quality alone when measuring performance unless the task asks for a specific level.

`sc --camera-position -10,5,0 --look-at -20,0,0` accepts negative coordinates. `moved: false` means the view was not held (`--output-mode json` adds the requested and actual camera). Inspect the image itself. `--studio` captures the Studio window, including its play view during ordinary Play.

Captures target one window, not the desktop.
After a teleport resizes the viewport, a recording fits later frames into its original canvas and reports `resizedFrames`.
Recordings default to 60 FPS (`--fps 1..60`), 60 seconds (`--max-seconds 1..300`), quality 80 (`--quality 0..100`). Keep 60 FPS for clips a person will watch; size is irrelevant.
Never minimize a window whose behavior matters: the one you record, test or read from. Windows and Roblox throttle a minimized or fully covered window, so it stutters; only filler clients that merely need to exist may be minimized. Leave the windows that matter behind your other windows (`rbx so` opens it that way; `rs` restores a minimized one without focus). `re` reports `achievedFps`; `throttled: true` means the window was starved: fix it and record again.

For an already running client:

```powershell
rbx rs -p 1 -o clips/movement.mp4
rbx ky W --hold-ms 700 -p 1
rbx re
```

`re` finalizes the recording; call it even after the duration limit. A recording ID stops only that one.

## Review what was recorded

`re` returns the video and a timestamped overview PNG in `review.path`.
Open that image with your image-viewing tool. A path in JSON does not mean you have seen it.
`sampled: true` means the overview omits frames; it cannot prove a brief glitch never occurred.

```powershell
rbx rf clips/movement.mp4
rbx rf clips/movement.mp4 --page 1
rbx rf clips/movement.mp4 --page 2
rbx rf clips/movement.mp4 --frame 15
```

- Default: up to 12 frames spread across the clip, including its first and last frames.
- `--page N`: 12 consecutive frames, in order, with no gaps between pages. Read `nextPage`/`totalPages` to cover every captured frame.
- `--frame N`: one frame at full resolution, without labels obscuring it.

Frame/page numbers start at 1. PNG labels show frame number and elapsed time; JSON lists exact `timestampMs`, dimensions, and total captured frames.
Inspect full-size frames for small UI details; use every page when the whole recording matters.

Images go in `VIDEO.mp4.review/`; `-o IMAGE.png` overrides the output.
`re --no-review` saves only the video. If overview generation fails, `reviewError` explains why and the saved MP4 remains usable with `rf`.

## Device simulation

```powershell
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx dev stop
```

Use this for mobile layout, resolution, or safe areas. Configure it in Edit, not Play.
`dev set` returns the state; use `dev status` later and `--details` only for native dimensions/density.
Resource limits are separate: see the performance guide.
