# Renium Studio plugin

`Renium.project.json` defines the Studio plugin bundle. Release builds generate:

- `Renium.rbxm` — binary model for normal Studio installation.
- `Renium.rbxmx` — XML model for inspection and source-control review.

Bundles aren't stored in Git. Build them from the repository root:

```powershell
.\tools\build-release.ps1 -LocalBuild
```

This uses the pinned Rojo version, checks component versions, and writes to
`dist/`.

For a public release, omit `-LocalBuild`; release checks require a clean
checkout, license, and VS Code publisher.
