# Renium bug reports

Report a Renium failure (an internal error, a crash, a sync that lost or mangled data, a command that misbehaves) in three steps: write the report, review it, upload it. Never upload a report you have not read.

## 1. Write

```powershell
rbx report -m "what went wrong"
```

This writes `.renium/reports/<ID>/` and `<ID>.zip` beside it; the ID looks like `RNM-20261021-AB12CD`. The folder holds `report.md` (your message, environment and sections), `report.json`, `doctor.json`, `studio.json`, the project file, `daemon.log` (last 400 lines), `crashes/*.json` and `transcript.md`: the last `rbx` commands with their output from this project's Codex or Claude Code transcripts (`--since MINUTES`, default 120; `--no-transcript` leaves them out). Values after key, token, password, secret, bearer or authorization words and home paths are masked. The masking is pattern-based and misses anything it does not recognize.

## 2. Review

Read every file in the folder before sending; the folder is exactly what will be sent. Remove anything the user would not hand to a stranger:

- credentials in any form: API keys, cookies, webhook URLs, Open Cloud keys, `.ROBLOSECURITY`, data-store values holding player data
- personal data: real names, emails, user IDs tied to people, chat text, IP addresses, machine names, paths outside the project
- the user's own work beyond what the failure needs: transcript entries that printed script sources, place data dumps, asset lists, unreleased game details
- anything the user has said is private

Edit the file in place: delete whole files, delete lines, or replace values with `[removed]`; keep `.json` files valid JSON. The upload re-zips the folder and rewrites the file list in `report.json`, so the zip from step 1 is not what is sent. Do not add files or links; the store accepts only the report layout. If you are unsure whether something may be shared, show the user the folder path and ask before uploading.

## 3. Upload

```powershell
rbx report --upload RNM-20261021-AB12CD
```

This re-zips the folder and sends it to the Renium developer's report store, which checks the bundle and files it under the same ID (`RENIUM_REPORT_ENDPOINT` overrides the address; repeat `-o DIR` if step 1 used it). Tell the user the ID. A refused or failed upload keeps the refreshed zip; the user may attach it to the GitHub issue link `report` prints or post it in the Discord server: https://discord.gg/wwTFHSSNn3

Also tell the user about any change you made to their place to get past the failure.
