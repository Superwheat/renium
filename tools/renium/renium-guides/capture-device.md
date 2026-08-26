# Capture and device simulation

Also read `RENIUM/playtest.md` during Play.

```powershell
rbx sc --studio -o studio.png
rbx sc -p 2 -o client.png
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx dev stop
```

Screenshots and H.264 MP4 recordings capture only the selected window.

Use device simulation only for mobile, resolution, or safe-area checks. Configure it in Edit mode, never during Play.

`dev set` returns the new state. Use `dev status` later, and `--details` only for native dimensions or density.

Start, act, and end without pauses:

```powershell
rbx rs -p 2 -o test.mp4
rbx ky W --hold-ms 700 -p 2
rbx re
```

`re` stops the active recording; an optional ID verifies it. Stop before other checks.
