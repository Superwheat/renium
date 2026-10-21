# Renium reports

A Cloudflare Worker that receives the bundles `rbx report --upload` sends,
checks them, and stores them in the `renium-reports` R2 bucket. The answer
carries the report ID, so "report RNM-20261021-AB12CD" is enough to find the
bundle.

## What is checked

- Content type `application/zip`, body at most `MAX_UPLOAD_BYTES` (8 MiB).
- Only the archive shape `rbx report` writes: a zip whose end record closes
  the file exactly (an archive comment is allowed, entry comments are not),
  at most 64 stored or deflated regular-file entries, no extra fields, data
  descriptors, zip64 fields, encryption, link or directory attributes, or
  multi-disk markers, local headers equal to their directory records, and
  entries laid out back to back so nothing unlisted can ride along. Entry
  timestamps are not checked.
- Every entry is inflated under hard caps: 8 MiB each, 24 MiB in total; the
  inflated size and CRC must match the directory record, the deflate stream
  must use every compressed byte, and every file must be UTF-8 text. The
  `.json` files (256 KiB each, `report.json` 512 KiB) must parse and nest at
  most 32 levels deep. A record that lies, a bomb, a binary under a report
  name, or a name that only differs by case or carries a Windows device name
  is refused before anything is stored.
- Entry names limited to the report layout: `report.md`, `report.json`,
  `doctor.json`, `studio.json`, `daemon.log`, `transcript.md`, the project
  file, and `crashes/*.json`. Anything else, a duplicate, or a path with `..`
  is refused.
- `report.md` and `report.json` are present; `report.json` (at most 512 KiB)
  parses, its `environment.id` is a `RNM-YYYYMMDD-XXXXXX` ID with a real
  date that matches the `X-Renium-Report-Id` header, its version, platform
  and generation time are well formed, and its `files` list names exactly
  the zip's other entries.
- The ID is new; a second upload of the same ID answers 409.
- At most 6 uploads a minute per address and 30 a minute overall, and a
  daily budget (`DAILY_UPLOADS`, `DAILY_BYTES`: 200 uploads, 1 GiB) kept by
  the `Quota` Durable Object; past the budget the store answers 503 and the
  client keeps its zip for a manual attachment. A missing binding makes the
  store answer 503 rather than accept uploads unprotected. The limiters run
  inside the Worker, so refused requests still count as invocations against
  the Workers plan; the budgets bound storage and R2 writes, not requests.
- Stored objects expire after a year through the bucket's lifecycle rule
  (`wrangler r2 bucket lifecycle list renium-reports`), so retained storage
  stays bounded by the daily budget times that window.

A refused upload answers with a JSON `error` that `rbx report` prints; the
local zip stays in place for a manual attachment.

## Storage layout

```
reports/<YYYY>/<MM>/<DD>/<ID>.zip   the bundle, with version, OS, SHA-256,
                                     receipt time and the report message as
                                     custom metadata
index/<ID>.json                      the same metadata plus the object key
```

Nothing identifies the sender beyond a salted hash of the address
(`IP_SALT`, optional secret).

## Look up a report

```bash
npm run fetch -- RNM-20261021-AB12CD
```

writes `downloads/<ID>/index.json` and the zip through the logged-in
wrangler account. The Worker has no download route.

## Deploy

```bash
npm install
npx wrangler login
npx wrangler r2 bucket create renium-reports
npm run deploy
```

The deploy prints the `*.workers.dev` address; `rbx report --upload` uses
`https://renium-reports.superwheat05.workers.dev` unless
`RENIUM_REPORT_ENDPOINT` names another base URL.

## Local run

```bash
npm run dev
rbx report -m "test"
RENIUM_REPORT_ENDPOINT=http://127.0.0.1:8788 rbx report --upload RNM-20261021-AB12CD
```
