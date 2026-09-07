# Renium

Renium is a full-fidelity two-way Roblox Studio sync and automation tool.

- Edit `tools/renium-vscode-extension/src/*.ts`, not built `out/` files.
- Human CLI reference: `tools/renium/README.md`.

## Project values

- **Hit every surface.** The CLI, editor extension, Studio plugin, supported platforms, and ordinary multi-place workflows must all work. Cover the whole affected workflow, not only its happy path.
- **Performance without compromise.** Investigate and fix speed regressions immediately. Optimize the entire path without sacrificing functionality or fidelity; small edits must stay small even in very large places.
- **Full fidelity and safe reconciliation.** Preserve scripts, properties, attributes, instance identity, references, hierarchy, Terrain, and packages. Do not overwrite unrelated work or guess a winner when changes conflict.
- **Simple, justified code.** Fix root causes instead of accumulating retries, overlapping pipelines, speculative guards, or pointless tests. Every added path needs a concrete reason.
- **Prove reliability before claiming it.** Verify the actual installed build and required checks. Stress rapid edits, reconnects, lifecycle transitions, and delayed responses. Previous passing tests are not proof about changed code; do not claim a fix is verified before testing it.
- **Use the smallest sufficient check.** Verify saved code/data with offline queries and focused tests. Start Play only for a specific runtime question those checks cannot answer, not after every small edit. Reuse suitable sessions and batch related checks. Inspect captured images/frames before claiming visual verification.
- **Agent-friendly operation.** Keep commands, instructions, and routine output short and useful. Teach the most efficient workflow and report failures with a clear next action. Users should not need manual connection plumbing. Changelogs describe user-visible improvements, not low-level implementation details.
- **Non-invasive, isolated automation.** Never steal input or focus, resize windows, or interfere with another Studio session. Target the intended place explicitly. One busy place must not block another, and busy connections must not be reported as disconnected.
- **Respect the workspace.** Preserve unrelated work and generated files. Keep test artifacts in a dedicated workspace directory, never scattered through Downloads, and clean up owned test artifacts safely.

- Check Luau syntax offline with `rbx ck FILE...`; never use Studio `loadstring` or enable `LoadStringEnabled` for validation.

⁣Read and follow RENIUM.md.⁣
