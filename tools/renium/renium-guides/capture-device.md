# Screenshots, recordings, and devices

Capture only when the task needs visual evidence. Use a screenshot for one state and a recording for motion or a transition. Capturing does not require starting Play; use `--studio` for Edit. Read the Play guide only for runtime work.

## Capture

```powershell
rbx sc --studio -o studio.png
rbx sc -p 1 -o client.png
rbx rs --studio -o clips/edit.mp4
rbx re
```

Screenshots and silent H.264 MP4 recordings target one window, not the desktop.
Recordings default to 12 FPS, 60 seconds maximum, and quality 80.
`rs` accepts `--fps 1..30`, `--max-seconds 1..300`, and `--quality 0..100`.
Choose FPS for the event: quick transitions may need 30 FPS. Frames between captures cannot be recovered.

For an already running client:

```powershell
rbx rs -p 1 -o clips/movement.mp4
rbx ky W --hold-ms 700 -p 1
rbx re
```

Start, perform the action, then end without unrelated checks or artificial pauses.
`re` stops and finalizes the recording; call it even after the duration limit.
An optional recording ID prevents stopping the wrong recording.

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
Inspect relevant pages and full-size frames for small UI details. Use every page if the task requires checking the entire recording.

Review is offline and needs no Studio, play session, FFmpeg, or custom contact-sheet script.
Images go in `VIDEO.mp4.review/`; `-o IMAGE.png` overrides the output.
`re --no-review` saves only the video. If overview generation fails, `reviewError` explains why and the saved MP4 remains usable with `rf`.
Review supports Renium's H.264 MP4s, not arbitrary video codecs.

## Device simulation

```powershell
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx dev stop
```

Use this for mobile layout, resolution, or safe areas. Configure it in Edit, not Play.
`dev set` returns the state; use `dev status` later and `--details` only for native dimensions/density.
Resource limits are separate: see the performance guide.
