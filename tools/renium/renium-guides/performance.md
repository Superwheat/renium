# Studio performance profiles

Profiles constrain connected Studio process trees without changing FPS, moving windows, or taking input. The selected profile is global and is reapplied when a connected Studio process is replaced.

```powershell
rbx pf ls
rbx pf use iphone-11
rbx pf show
rbx pf off
```

Built-in device names are approximate performance tiers, not hardware emulation. `pf ls` shows which tiers this computer can enforce; a tier is unavailable if it would require resources the computer cannot provide or controls the OS cannot enforce. The first `use` calibrates automatically when needed; use `pf cal` to recalibrate after hardware or power-mode changes.

Use advanced constraints for exact supported limits:

```powershell
rbx pf adv cpu=25 cores=2 headroom=1g prio=low
rbx pf adv cpu=40 cores=4 headroom=2g save=slow-test
rbx pf use slow-test
```

`cpu` is aggregate CPU percent. `cores` limits logical processors. `headroom` caps Studio above its current committed memory. `prio` is `normal`, `below`, or `low`. An absolute `mem=1g` cap can crash Studio and requires `risk=crash`.

Windows enforces profiles with Job Objects and verifies the applied limits. `pf off` removes every limit; Windows keeps neutral job membership until that Studio process exits. macOS and Linux report profiles as unavailable rather than pretending to enforce unsupported limits.
