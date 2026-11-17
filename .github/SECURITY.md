# Security policy

Renium runs a local daemon and a Roblox Studio plugin that talk over localhost, executes Luau in Studio on behalf of a developer, and holds Open Cloud API keys in the operating system's credential store. Reports about any of that are welcome.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting for this repository (Security tab, "Report a vulnerability"), or message the maintainer privately on the Renium Discord linked from the README. Do not open a public issue for a security problem.

Include the Renium version (`rbx --version`), the operating system, the Studio version if Studio is involved, and steps or a script that reproduces the problem. Proof-of-concept material that touches a Roblox account or experience you do not own is not needed and not wanted.

## What to expect

- Acknowledgement within 3 days.
- A fix or a documented mitigation for confirmed reports in a released version, normally within 30 days; the release notes credit the reporter unless they ask otherwise.
- A GitHub security advisory, with a CVE requested through GitHub, for confirmed vulnerabilities that affect released versions.

## Supported versions

Only the latest release receives security fixes. `rbx upd` installs it.

## Security maintainer

Superwheat (GitHub: @Superwheat) maintains Renium and handles security reports.
