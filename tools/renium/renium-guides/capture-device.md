# Screenshots, recordings, and devices

Capture only for visual evidence: a screenshot for one state, a recording for motion, a transition or a short-lived effect (never screenshots timed with sleeps). Open every capture with your image tool; a path in JSON isn't seeing it. Save captures outside the project or delete them after. Capturing needs no Play: `--studio` captures the Studio window (Edit, or its play view in ordinary Play).

## Check what the player sees

When the user describes what they saw, or you must judge if a change is visible, measuring state can't tell: a player sees through their camera and frame. Reproduce it as the player does and look:

```powershell
rbx rs -p 1 -o clips/jump.mp4 --max-seconds 6  # record the client the player is in
rbx inp -p 1 key E                              # the real input, not a function call
rbx re                                          # stop; returns the overview image
rbx rf clips/jump.mp4 --page 1                  # open the frames and look
```

If the user can't see it, record from their viewpoint before touching code again; don't re-measure internals. If you still need a number, measure on screen (`Camera:WorldToViewportPoint`) or relative to `workspace.CurrentCamera.CFrame`, never world or chassis coordinates alone.

## Capture

```powershell
rbx sc --studio -o studio.png
rbx sc -p 1 -o client.png
rbx rs --studio -o clips/edit.mp4
rbx re
```

`rbx gfx` reports Studio's edit and play render quality (1-21, `0` = automatic) and the in-game slider (1-10, read only). Quality decides what a capture shows: for lighting, materials or effects, run `rbx gfx max` (Edit), `rbx gfx max --player 1` (client) or `rbx gfx set --play 21`. These Studio-wide settings survive restarts, so always `rbx gfx restore` (same `--player`) after the capture. When measuring performance, change it only if the task names a level.

`sc --camera-position -10,5,0 --look-at -20,0,0` accepts negative coordinates. `moved: false`: the view was not held (`--output-mode json` adds requested and actual camera).

Captures target one window, not the desktop. A viewport resized mid-recording is fitted to the original canvas (see `resizedFrames`).
Recordings default to 60 FPS (`--fps 1..60`), 60 s (`--max-seconds 1..300`), quality 80 (`--quality 0..100`). Clips for people are 60 FPS: on Windows run `rbx fps unlock` first (unfocused Studio renders one frame in four) and read `achievedFps` from `re`. `throttled: true`: `note` says what was slow; fix it and re-record. Never hand over a choppy clip or lower `--fps` to hide it.
On Windows `rs` also captures the Studio or client process's sound (game and UI at the game's mix, AAC 48 kHz stereo, synced in the MP4); macOS, Linux: no audio yet. `--no-audio`: video only. Sound is recorded while `rbx audio auto` (unfocused Studio) or `rbx audio mute` silences the speakers, without changing the mode; the Windows volume mixer doesn't change its level. `re` returns `audio` (true if a track with captured sound was written) and, if sound wasn't captured cleanly, `audioError` saying why (late start, dropped packets, encoder kept video only); the video is saved either way. Check both before handing over a clip needing sound.
Minimize only filler clients: Windows and Roblox throttle minimized or fully covered windows, so keep ones you record, test or read from behind other windows (`rbx so` opens Studio that way; `rs` restores a minimized one without focus).

For an already running client:

```powershell
rbx rs -p 1 -o clips/movement.mp4
rbx inp -p 1 hold W 700
rbx re
```

Call `re` to finalize, even after the duration limit; a recording ID stops only that one.

## Review what was recorded

`re` returns the video and a timestamped overview PNG in `review.path`. `sampled: true`: the overview omits frames, so it can't rule out a brief glitch.

```powershell
rbx rf clips/movement.mp4
rbx rf clips/movement.mp4 --page 1
rbx rf clips/movement.mp4 --page 2
rbx rf clips/movement.mp4 --frame 15
```

- Default: up to 12 frames across the clip, first and last included.
- `--page N`: 12 consecutive frames, no gaps between pages; when the whole recording matters, follow `nextPage`/`totalPages` to every frame.
- `--frame N`: one frame at full resolution, no labels; use it for small UI details.

Frame/page numbers start at 1. PNG labels show frame number and elapsed time; JSON lists exact `timestampMs`, dimensions and total captured frames.

Images go in `VIDEO.mp4.review/` (`-o IMAGE.png` overrides). `re --no-review` saves only the video. If the overview fails, `reviewError` says why; the MP4 still works with `rf`.

## Device simulation

```powershell
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx dev stop
```

For mobile layout, resolution or safe areas. Configure in Edit, not Play. `dev set` returns the state; later, `dev status` (`--details` only for native dimensions/density). Resource limits are separate: see the performance guide.
