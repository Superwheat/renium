#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import itertools
import json
from dataclasses import dataclass, field
from typing import Any

import websockets


@dataclass
class ChannelState:
    port: int
    websocket: Any | None = None
    pending: dict[str, asyncio.Future] = field(default_factory=dict)


class BridgeServer:
    def __init__(self, host: str, ports: list[int]) -> None:
        self.host = host
        self.ports = ports
        self.channels: dict[int, ChannelState] = {p: ChannelState(port=p) for p in ports}
        self._id_counter = itertools.count(1)
        self._servers: list[Any] = []
        self._last_duplicate_log: dict[int, float] = {}

    async def start(self) -> None:
        for port in self.ports:
            async def handler(websocket, _path=None, *, _port=port):
                await self._handle_connection(_port, websocket)

            server = await websockets.serve(handler, self.host, port, max_size=None)
            self._servers.append(server)
            print(f"[bridge] listening on ws://{self.host}:{port}")

    async def stop(self) -> None:
        for state in self.channels.values():
            if state.websocket is not None:
                await state.websocket.close()
            for fut in list(state.pending.values()):
                if not fut.done():
                    fut.set_exception(RuntimeError("bridge stopped"))
            state.pending.clear()

        for server in self._servers:
            server.close()
            await server.wait_closed()
        self._servers.clear()

    def status_text(self) -> str:
        lines = []
        for port in self.ports:
            state = self.channels[port]
            lines.append(f"port {port}: {'connected' if state.websocket else 'disconnected'}")
        return "\n".join(lines)

    async def call(self, port: int, method: str, params: dict[str, Any], timeout: float = 30.0) -> Any:
        if port not in self.channels:
            raise RuntimeError(f"Unknown channel port: {port}")
        state = self.channels[port]
        ws = state.websocket
        if ws is None:
            raise RuntimeError(f"No plugin connected on port {port}")

        req_id = f"{port}:{next(self._id_counter)}"
        fut: asyncio.Future = asyncio.get_running_loop().create_future()
        state.pending[req_id] = fut

        payload = {"id": req_id, "method": method, "params": params}
        await ws.send(json.dumps(payload, separators=(",", ":"), ensure_ascii=False))
        return await asyncio.wait_for(fut, timeout=timeout)

    async def _handle_connection(self, port: int, websocket: Any) -> None:
        state = self.channels[port]

        if state.websocket is not None and state.websocket is not websocket:
            old_closed = bool(getattr(state.websocket, "closed", False))
            if not old_closed:
                now = asyncio.get_running_loop().time()
                last = self._last_duplicate_log.get(port, 0.0)
                if now - last >= 2.0:
                    self._last_duplicate_log[port] = now
                    print(f"[bridge] duplicate channel attempt on port {port}; keeping current channel")
                await websocket.close(code=1013, reason="bridge channel already active")
                return

        state.websocket = websocket
        print(f"[bridge] channel connected on port {port}")

        try:
            async for raw in websocket:
                await self._on_message(port, raw)
        finally:
            if state.websocket is websocket:
                state.websocket = None
            for req_id, fut in list(state.pending.items()):
                if not fut.done():
                    fut.set_exception(RuntimeError(f"channel {port} disconnected"))
                state.pending.pop(req_id, None)
            print(f"[bridge] channel disconnected on port {port}")

    async def _on_message(self, port: int, raw: str) -> None:
        try:
            msg = json.loads(raw)
        except json.JSONDecodeError:
            print(f"[bridge] invalid json from {port}: {raw[:160]!r}")
            return

        state = self.channels[port]
        req_id = msg.get("id")
        if req_id is None:
            event = msg.get("event")
            if event:
                print(f"[bridge] event from {port}: {event}")
            return

        fut = state.pending.pop(str(req_id), None)
        if fut is None:
            return

        if msg.get("ok") is True:
            fut.set_result(msg.get("result"))
        else:
            fut.set_exception(RuntimeError(str(msg.get("error", "unknown bridge error"))))


async def repl(server: BridgeServer) -> None:
    print("[bridge] commands: status | ping [port] | prepare <service> [port] | batch <service> <start> <count> [port] | quit")
    while True:
        line = await asyncio.to_thread(input, "bridge> ")
        line = line.strip()
        if not line:
            continue
        if line in {"quit", "exit"}:
            return

        parts = line.split()
        cmd = parts[0].lower()

        try:
            if cmd == "status":
                print(server.status_text())
                continue

            if cmd == "ping":
                port = int(parts[1]) if len(parts) > 1 else server.ports[0]
                result = await server.call(port, "ping", {})
                print(result)
                continue

            if cmd == "prepare":
                if len(parts) < 2:
                    print("usage: prepare <service> [port]")
                    continue
                service = parts[1]
                port = int(parts[2]) if len(parts) > 2 else server.ports[0]
                result = await server.call(port, "prepare", {"service": service}, timeout=120.0)
                print(result)
                continue

            if cmd == "batch":
                if len(parts) < 4:
                    print("usage: batch <service> <start> <count> [port]")
                    continue
                service = parts[1]
                start = int(parts[2])
                count = int(parts[3])
                port = int(parts[4]) if len(parts) > 4 else server.ports[0]
                result = await server.call(
                    port,
                    "getInstanceBatchChunk",
                    {
                        "service": service,
                        "startIndex": start,
                        "maxCount": count,
                        "chunkStart": 1,
                        "maxLen": 20000,
                    },
                    timeout=120.0,
                )
                if isinstance(result, dict):
                    print({k: result.get(k) for k in ("start", "nextStart", "total")})
                    print(f"chunk bytes={len(str(result.get('chunk', '')))}")
                else:
                    print(type(result), result)
                continue

            print("unknown command")
        except Exception as exc:
            print(f"[bridge] error: {exc}")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Local daemon for Roblox ParallelExportBridge plugin")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--ports", default="8781,8782,8783,8784")
    return parser.parse_args()


async def main_async() -> int:
    args = parse_args()
    ports = [int(p.strip()) for p in args.ports.split(",") if p.strip()]
    if not ports:
        raise RuntimeError("No ports specified")

    server = BridgeServer(host=args.host, ports=ports)
    await server.start()
    try:
        await repl(server)
    finally:
        await server.stop()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(asyncio.run(main_async()))
    except KeyboardInterrupt:
        raise SystemExit(130)
