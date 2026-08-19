# Capture and device simulation

Read `RENIUM.md` first. Read `RENIUM/playtest.md` too when capturing or configuring a running test.

```powershell
rbx shot --studio -o studio.png
rbx shot -p 2 -o client.png
rbx device list
rbx device set "iPhone 16 Pro" --orientation portrait --scaling fit
rbx device stop
```

Screenshots and H.264 MP4 recordings capture only the selected Studio or client window.

Use device simulation only for mobile, resolution, or safe-area checks. Configure or stop it in Edit mode, never during Play, and never use it to repair a hidden normal viewport.

`device set` returns the resulting state, so don't call `device status` immediately afterward. Use `device status` to read existing state later. `device list` returns selection fields; use `device list --details` or `device status --details` only when native dimensions or density are needed.

Start, act, and end in one shell call so planning time isn't recorded:

```powershell
rbx record-start -p 2 -o test.mp4
rbx key W --hold-ms 700 -p 2
rbx record-end
```

`record-end` stops the sole active recording; an optional recording ID checks that it is the expected one. End before screenshots, console reads, or other verification.
