import { DurableObject } from "cloudflare:workers";
import * as Y from "yjs";
import * as syncProtocol from "y-protocols/sync";
import * as awarenessProtocol from "y-protocols/awareness";
import * as encoding from "lib0/encoding";
import * as decoding from "lib0/decoding";

export interface Env {
  ROOMS: DurableObjectNamespace<Room>;
}

const MESSAGE_SYNC = 0;
const MESSAGE_AWARENESS = 1;
const MESSAGE_AUTH = 2;
const MESSAGE_QUERY_AWARENESS = 3;
const COMPACT_AFTER_UPDATES = 256;
const ROOM_PATH = /^\/rooms\/([A-Za-z0-9_-]{6,64})\/?$/;

interface Attachment {
  clients: number[];
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const match = ROOM_PATH.exec(url.pathname);
    if (!match) {
      return new Response("Renium relay. Rooms live at /rooms/<id>?token=<token>.", {
        status: url.pathname === "/" ? 200 : 404,
        headers: { "content-type": "text/plain" },
      });
    }
    if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
      return new Response("Rooms accept WebSocket connections only", { status: 426 });
    }
    const id = env.ROOMS.idFromName(match[1]);
    return env.ROOMS.get(id).fetch(request);
  },
};

export class Room extends DurableObject<Env> {
  private doc: Y.Doc | undefined;
  private awareness: awarenessProtocol.Awareness | undefined;
  private updatesSinceCompaction = 0;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    ctx.storage.sql.exec(
      "CREATE TABLE IF NOT EXISTS updates (seq INTEGER PRIMARY KEY AUTOINCREMENT, data BLOB NOT NULL)",
    );
    ctx.storage.sql.exec("CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL)");
  }

  private metaValue(key: string): string | undefined {
    const row = this.ctx.storage.sql.exec("SELECT value FROM meta WHERE key = ?", key).toArray()[0];
    return row ? String(row.value) : undefined;
  }

  private setMeta(key: string, value: string): void {
    this.ctx.storage.sql.exec("INSERT OR REPLACE INTO meta (key, value) VALUES (?, ?)", key, value);
  }

  private ensureDoc(): { doc: Y.Doc; awareness: awarenessProtocol.Awareness } {
    if (this.doc && this.awareness) {
      return { doc: this.doc, awareness: this.awareness };
    }
    const doc = new Y.Doc();
    const rows = this.ctx.storage.sql.exec("SELECT data FROM updates ORDER BY seq").toArray();
    for (const row of rows) {
      Y.applyUpdate(doc, new Uint8Array(row.data as ArrayBuffer));
    }
    this.updatesSinceCompaction = rows.length;
    doc.on("update", (update: Uint8Array, origin: unknown) => {
      this.persist(update);
      const encoder = encoding.createEncoder();
      encoding.writeVarUint(encoder, MESSAGE_SYNC);
      syncProtocol.writeUpdate(encoder, update);
      this.broadcast(encoding.toUint8Array(encoder), origin);
    });
    const awareness = new awarenessProtocol.Awareness(doc);
    awareness.setLocalState(null);
    awareness.on(
      "update",
      ({ added, updated, removed }: { added: number[]; updated: number[]; removed: number[] }, origin: unknown) => {
        const changed = added.concat(updated, removed);
        if (changed.length === 0) {
          return;
        }
        const encoder = encoding.createEncoder();
        encoding.writeVarUint(encoder, MESSAGE_AWARENESS);
        encoding.writeVarUint8Array(encoder, awarenessProtocol.encodeAwarenessUpdate(awareness, changed));
        this.broadcast(encoding.toUint8Array(encoder), origin);
      },
    );
    this.doc = doc;
    this.awareness = awareness;
    return { doc, awareness };
  }

  private persist(update: Uint8Array): void {
    this.ctx.storage.sql.exec("INSERT INTO updates (data) VALUES (?)", update.buffer.slice(update.byteOffset, update.byteOffset + update.byteLength));
    this.updatesSinceCompaction += 1;
    if (this.updatesSinceCompaction >= COMPACT_AFTER_UPDATES && this.doc) {
      const snapshot = Y.encodeStateAsUpdate(this.doc);
      this.ctx.storage.sql.exec("DELETE FROM updates");
      this.ctx.storage.sql.exec("INSERT INTO updates (data) VALUES (?)", snapshot.buffer.slice(snapshot.byteOffset, snapshot.byteOffset + snapshot.byteLength));
      this.updatesSinceCompaction = 1;
    }
  }

  private broadcast(bytes: Uint8Array, origin: unknown): void {
    for (const socket of this.ctx.getWebSockets()) {
      if (socket === origin) {
        continue;
      }
      try {
        socket.send(bytes);
      } catch {
        // A closed socket is removed by webSocketClose.
      }
    }
  }

  async fetch(request: Request): Promise<Response> {
    const url = new URL(request.url);
    const presented = url.searchParams.get("token") ?? "";
    if (presented.length < 8) {
      return new Response("A room token is required", { status: 401 });
    }
    const stored = this.metaValue("token");
    if (stored === undefined) {
      this.setMeta("token", presented);
      this.setMeta("created", new Date().toISOString());
    } else if (stored !== presented) {
      return new Response("Invalid room token", { status: 403 });
    }
    const pair = new WebSocketPair();
    const [client, server] = [pair[0], pair[1]];
    this.ctx.acceptWebSocket(server);
    server.serializeAttachment({ clients: [] } satisfies Attachment);
    const { doc, awareness } = this.ensureDoc();
    const encoder = encoding.createEncoder();
    encoding.writeVarUint(encoder, MESSAGE_SYNC);
    syncProtocol.writeSyncStep1(encoder, doc);
    server.send(encoding.toUint8Array(encoder));
    const states = Array.from(awareness.getStates().keys()).filter((id) => id !== awareness.clientID);
    if (states.length > 0) {
      const awarenessEncoder = encoding.createEncoder();
      encoding.writeVarUint(awarenessEncoder, MESSAGE_AWARENESS);
      encoding.writeVarUint8Array(awarenessEncoder, awarenessProtocol.encodeAwarenessUpdate(awareness, states));
      server.send(encoding.toUint8Array(awarenessEncoder));
    }
    return new Response(null, { status: 101, webSocket: client });
  }

  async webSocketMessage(socket: WebSocket, message: ArrayBuffer | string): Promise<void> {
    if (typeof message === "string") {
      return;
    }
    const { doc, awareness } = this.ensureDoc();
    const decoder = decoding.createDecoder(new Uint8Array(message));
    while (decoding.hasContent(decoder)) {
      const type = decoding.readVarUint(decoder);
      switch (type) {
        case MESSAGE_SYNC: {
          const encoder = encoding.createEncoder();
          encoding.writeVarUint(encoder, MESSAGE_SYNC);
          syncProtocol.readSyncMessage(decoder, encoder, doc, socket);
          if (encoding.length(encoder) > 1) {
            socket.send(encoding.toUint8Array(encoder));
          }
          break;
        }
        case MESSAGE_AWARENESS: {
          const update = decoding.readVarUint8Array(decoder);
          const before = new Set(awareness.getStates().keys());
          awarenessProtocol.applyAwarenessUpdate(awareness, update, socket);
          const attachment = (socket.deserializeAttachment() as Attachment | null) ?? { clients: [] };
          const owned = new Set(attachment.clients);
          for (const id of awareness.getStates().keys()) {
            if (!before.has(id)) {
              owned.add(id);
            }
          }
          socket.serializeAttachment({ clients: Array.from(owned) } satisfies Attachment);
          break;
        }
        case MESSAGE_QUERY_AWARENESS: {
          const encoder = encoding.createEncoder();
          encoding.writeVarUint(encoder, MESSAGE_AWARENESS);
          encoding.writeVarUint8Array(
            encoder,
            awarenessProtocol.encodeAwarenessUpdate(awareness, Array.from(awareness.getStates().keys())),
          );
          socket.send(encoding.toUint8Array(encoder));
          break;
        }
        case MESSAGE_AUTH:
          break;
        default:
          return;
      }
    }
  }

  async webSocketClose(socket: WebSocket): Promise<void> {
    this.forget(socket);
  }

  async webSocketError(socket: WebSocket): Promise<void> {
    this.forget(socket);
  }

  private forget(socket: WebSocket): void {
    const attachment = socket.deserializeAttachment() as Attachment | null;
    if (attachment && attachment.clients.length > 0 && this.awareness) {
      awarenessProtocol.removeAwarenessStates(this.awareness, attachment.clients, socket);
    }
  }
}
