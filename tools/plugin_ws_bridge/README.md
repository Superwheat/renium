# Renium Studio plugin

`Renium.project.json` is the source of truth for the Studio plugin. The two
generated convenience artifacts are:

- `Renium.rbxm` — binary model for normal Studio installation.
- `Renium.rbxmx` — XML model for inspection and source-control review.

Do not hand-edit either bundle. Build both from the repository root with:

```powershell
.\tools\build-release.ps1 -LocalBuild
```

That command uses the Rojo version pinned in `aftman.toml`, verifies that the
CLI, extension, and plugin versions match, and replaces the local bundles only
after both outputs build successfully. The versioned copies and checksums are
written under `dist/`.

For a public release, run the same command without `-LocalBuild`; its clean
checkout, product-license, and VS Code publisher checks are intentional release
gates.
