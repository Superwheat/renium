# Renium relay

A Cloudflare Worker that hosts Renium collaboration rooms. Each room is a
Durable Object holding the shared project document and its change history.

## Deploy

```bash
npm install
npx wrangler login
npm run deploy
```

The deploy prints a `*.workers.dev` address. Share the project with

```powershell
rbx collab start --relay https://renium-relay.<account>.workers.dev
```

Cloudflare's free plan covers small teams: 100,000 Durable Object requests
a day, where each incoming WebSocket message counts as one request, and 5 GB
of storage. Larger teams move the Worker to the paid plan without code changes.

## Local run

```bash
npm run dev
rbx collab start --relay http://127.0.0.1:8787
```

## Protocol

Rooms speak the Yjs sync and awareness protocol over WebSocket, the same
wire format the Renium daemon and `y-websocket` clients use. The first
connection to a room sets its token; later connections must present the
same `?token=` query value.
