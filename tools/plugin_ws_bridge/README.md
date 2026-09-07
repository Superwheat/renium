# Renium Studio plugin

Connects Studio to the Renium CLI and editor extension.
Install it with Renium; `rbx setup --repair` repairs the local plugin.
Restart Studio after replacing the plugin.

Sources are bundled by `Renium.project.json`. From the repository root:

```powershell
./tools/build-release.ps1 -LocalBuild
```

The pinned Rojo build produces `Renium.rbxm` (installation) and
`Renium.rbxmx` (inspectable XML) under `dist/`.
Generated bundles are not stored in Git.

[Usage and troubleshooting](../renium/README.md).
