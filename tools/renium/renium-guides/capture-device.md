# Capture and device simulation

Read `RENIUM.md` first. Read `RENIUM/playtest.md` too when capturing or configuring a running test.

```powershell
rbx sc --studio -o studio.png
rbx sc -p 2 -o client.png
rbx dev list
rbx dev set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx dev stop
```

Screenshots and H.264 MP4 recordings capture only the selected Studio or client window.

Use device simulation only for mobile, resolution, or safe-area checks. Configure or stop it in Edit mode, never during Play, and never use it to repair a hidden normal viewport.

`dev set` returns the resulting state, so don't call `dev status` immediately afterward. Use `dev status` to read existing state later. `dev list` returns selection fields; use `dev list --details` or `dev status --details` only when native dimensions or density are needed.

Start, act, and end in one shell call so planning time isn't recorded:

```powershell
rbx rs -p 2 -o test.mp4
rbx ky W --hold-ms 700 -p 2
rbx re
```

`re` stops the sole active recording; an optional recording ID checks that it is the expected one. End before screenshots, console reads, or other verification.
