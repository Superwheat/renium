#!/usr/bin/env python3
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import time
from pathlib import Path
from typing import Any

try:
    import tkinter as tk
    from tkinter import filedialog
except Exception:
    tk = None
    filedialog = None

from mcp_sync_export import MCPClient, ensure_active_studio, execute_luau, load_server_command

SYNC_RUNTIME_LUA = r'''
local HttpService = game:GetService("HttpService")

do
    local existing = _G.CDX_FS_SYNC
    local incoming = {}
    if type(existing) == "table" and type(existing._incoming) == "table" then
        incoming = existing._incoming
    end

    local function decodeValue(instance, propertyName, value)
        if type(value) ~= "table" then
            local okCurrent, currentValue = pcall(function()
                return instance[propertyName]
            end)
            if okCurrent and typeof(currentValue) == "EnumItem" and type(value) == "string" then
                local enumItem = currentValue.EnumType[value]
                if enumItem then
                    return enumItem
                end
            end
            return value
        end

        if value.Vector2 then
            local v = value.Vector2
            return Vector2.new(v[1], v[2])
        elseif value.Vector3 then
            local v = value.Vector3
            return Vector3.new(v[1], v[2], v[3])
        elseif value.UDim then
            local v = value.UDim
            return UDim.new(v[1], v[2])
        elseif value.UDim2 then
            local v = value.UDim2
            return UDim2.new(v[1], v[2], v[3], v[4])
        elseif value.Color3 then
            local v = value.Color3
            return Color3.new(v[1], v[2], v[3])
        elseif value.CFrame then
            local v = value.CFrame
            return CFrame.new(table.unpack(v))
        elseif value.Rect then
            local v = value.Rect
            return Rect.new(v[1], v[2], v[3], v[4])
        end

        return value
    end

    local function applyProperties(instance, properties)
        if type(properties) ~= "table" then
            return
        end

        for propertyName, rawValue in pairs(properties) do
            if propertyName == "Attributes" and type(rawValue) == "table" then
                for attrName, attrValue in pairs(rawValue) do
                    local typedAttr = attrValue
                    if type(attrValue) == "table" then
                        if attrValue.String ~= nil then
                            typedAttr = attrValue.String
                        elseif attrValue.Bool ~= nil then
                            typedAttr = attrValue.Bool
                        elseif attrValue.Float64 ~= nil then
                            typedAttr = attrValue.Float64
                        end
                    end
                    pcall(function()
                        instance:SetAttribute(attrName, typedAttr)
                    end)
                end
            else
                local decoded = decodeValue(instance, propertyName, rawValue)
                pcall(function()
                    instance[propertyName] = decoded
                end)
            end
        end
    end

    local function clearChildren(instance)
        for _, child in ipairs(instance:GetChildren()) do
            child:Destroy()
        end
    end

    local function buildChild(node, parent)
        local className = node.className or "Folder"
        local instance = Instance.new(className)
        instance.Name = node.name or className
        applyProperties(instance, node.properties)
        if type(node.source) == "string" and instance:IsA("LuaSourceContainer") then
            pcall(function()
                instance.Source = node.source
            end)
        end
        instance.Parent = parent

        if type(node.children) == "table" then
            for _, child in ipairs(node.children) do
                buildChild(child, instance)
            end
        end
    end

    local function applyRoot(serviceName, payload)
        local service = game:FindFirstChild(serviceName)
        if not service then
            error("Service not found: " .. tostring(serviceName))
        end

        applyProperties(service, payload.properties)
        clearChildren(service)

        if type(payload.children) == "table" then
            for _, child in ipairs(payload.children) do
                buildChild(child, service)
            end
        end
    end

    local M = {}

    function M.begin(syncId)
        incoming[syncId] = {}
        return true
    end

    function M.push(syncId, chunk)
        local parts = incoming[syncId]
        if not parts then
            error("Sync not initialized: " .. tostring(syncId))
        end
        table.insert(parts, chunk)
        return #parts
    end

    function M.apply(syncId, serviceName)
        local parts = incoming[syncId]
        if not parts then
            error("Sync not initialized: " .. tostring(syncId))
        end
        local raw = table.concat(parts)
        incoming[syncId] = nil

        local ok, payload = pcall(function()
            return HttpService:JSONDecode(raw)
        end)
        if not ok then
            error("Invalid payload json")
        end

        applyRoot(serviceName, payload)
        return true
    end

    M._incoming = incoming
    _G.CDX_FS_SYNC = M
end

return "ok"
'''

CONFIG_VERSION = 1
DEFAULT_CONFIG_PATH = Path("tools/editor_to_studio_sync.json")


def choose_directory(initial_dir: Path) -> Path:
    if tk is None or filedialog is None:
        raise RuntimeError("Folder picker UI is unavailable on this Python runtime.")

    root = tk.Tk()
    root.withdraw()
    root.attributes("-topmost", True)
    selected = filedialog.askdirectory(
        initialdir=str(initial_dir),
        title="Select folder to sync to Roblox Studio (service folder)",
    )
    root.destroy()

    if not selected:
        raise SystemExit("Folder selection canceled. Exiting.")
    return Path(selected).resolve()


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def load_json(path: Path) -> dict[str, Any]:
    return json.loads(read_text(path))


def sanitize_name(stem: str) -> str:
    return stem


def script_class_and_name(file_name: str) -> tuple[str, str] | None:
    for suffix, class_name in (
        (".server.luau", "Script"),
        (".client.luau", "LocalScript"),
        (".luau", "ModuleScript"),
        (".server.lua", "Script"),
        (".client.lua", "LocalScript"),
        (".lua", "ModuleScript"),
    ):
        if file_name.endswith(suffix):
            return class_name, file_name[: -len(suffix)]
    return None


def load_meta_properties(meta_path: Path) -> tuple[str | None, dict[str, Any]]:
    if not meta_path.exists():
        return None, {}

    raw = load_json(meta_path)
    class_name = raw.get("className")
    properties = raw.get("properties")
    if not isinstance(properties, dict):
        properties = {}
    return class_name if isinstance(class_name, str) else None, properties


def build_node_from_script_file(script_path: Path) -> dict[str, Any]:
    class_info = script_class_and_name(script_path.name)
    if class_info is None:
        raise RuntimeError(f"Unsupported script file: {script_path}")
    class_name, instance_name = class_info

    meta_path = script_path.with_name(f"{instance_name}.meta.json")
    meta_class, properties = load_meta_properties(meta_path)
    if meta_class:
        class_name = meta_class

    return {
        "name": sanitize_name(instance_name),
        "className": class_name,
        "properties": properties,
        "source": read_text(script_path),
        "children": [],
    }


def build_node_from_dir(dir_path: Path, *, name_override: str | None = None, root_service_name: str | None = None) -> dict[str, Any]:
    init_script = None
    init_class = None
    for file_name, class_name in (
        ("init.server.luau", "Script"),
        ("init.client.luau", "LocalScript"),
        ("init.luau", "ModuleScript"),
        ("init.server.lua", "Script"),
        ("init.client.lua", "LocalScript"),
        ("init.lua", "ModuleScript"),
    ):
        candidate = dir_path / file_name
        if candidate.exists():
            init_script = candidate
            init_class = class_name
            break

    meta_class, properties = load_meta_properties(dir_path / "init.meta.json")

    if root_service_name is not None:
        class_name = root_service_name
    elif meta_class:
        class_name = meta_class
    elif init_class:
        class_name = init_class
    else:
        class_name = "Folder"

    node_name = name_override or dir_path.name
    node: dict[str, Any] = {
        "name": sanitize_name(node_name),
        "className": class_name,
        "properties": properties,
        "children": [],
    }
    if init_script is not None:
        node["source"] = read_text(init_script)

    children: list[dict[str, Any]] = []
    used_script_stems: set[str] = set()

    for child in sorted(dir_path.iterdir(), key=lambda p: p.name.lower()):
        if child.name.startswith("."):
            continue
        if child.name in {
            "init.meta.json",
            "init.luau",
            "init.server.luau",
            "init.client.luau",
            "init.lua",
            "init.server.lua",
            "init.client.lua",
        }:
            continue

        if child.is_dir():
            children.append(build_node_from_dir(child))
            continue

        if child.suffix.lower() == ".json" and child.name.endswith(".meta.json"):
            # Sidecar metadata handled while processing script file.
            continue

        class_info = script_class_and_name(child.name)
        if class_info is None:
            continue

        script_node = build_node_from_script_file(child)
        stem = script_node["name"]
        if stem in used_script_stems:
            continue
        used_script_stems.add(stem)
        children.append(script_node)

    node["children"] = children
    return node


def compute_tree_fingerprint(root: Path) -> str:
    sha = hashlib.sha256()
    for p in sorted(root.rglob("*"), key=lambda x: str(x).lower()):
        rel = str(p.relative_to(root)).replace("\\", "/")
        if p.is_dir():
            sha.update(f"D:{rel}\n".encode("utf-8"))
            continue
        if p.name.startswith("."):
            continue
        if p.suffix.lower() not in {".luau", ".lua", ".json"}:
            continue
        stat = p.stat()
        sha.update(f"F:{rel}:{stat.st_size}:{stat.st_mtime_ns}\n".encode("utf-8"))
    return sha.hexdigest()


def push_payload(client: MCPClient, service_name: str, payload: dict[str, Any], chunk_size: int = 1800) -> None:
    execute_luau(client, SYNC_RUNTIME_LUA)
    raw = json.dumps(payload, separators=(",", ":"), ensure_ascii=False)
    sync_id = f"sync_{int(time.time()*1000)}"
    execute_luau(client, f"return _G.CDX_FS_SYNC.begin({json.dumps(sync_id)})")
    for i in range(0, len(raw), chunk_size):
        chunk = raw[i : i + chunk_size]
        execute_luau(client, f"return _G.CDX_FS_SYNC.push({json.dumps(sync_id)}, {json.dumps(chunk)})")
    execute_luau(client, f"return _G.CDX_FS_SYNC.apply({json.dumps(sync_id)}, {json.dumps(service_name)})")


def load_config(path: Path) -> dict[str, Any]:
    if not path.exists():
        raise RuntimeError(f"Config not found: {path}")
    raw = load_json(path)
    if not isinstance(raw, dict):
        raise RuntimeError("Invalid config format")
    return raw


def save_config(path: Path, config: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(config, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def init_config(config_path: Path, project_root: Path) -> None:
    selected = choose_directory(project_root)
    service_name = selected.name
    cfg = {
        "configVersion": CONFIG_VERSION,
        "watchDirectory": str(selected),
        "service": service_name,
        "pollIntervalSeconds": 0.75,
        "chunkSize": 1800,
    }
    save_config(config_path, cfg)
    print(f"[watch-sync] config written: {config_path}")
    print(f"[watch-sync] watchDirectory: {selected}")
    print(f"[watch-sync] service: {service_name}")


def run_watch(args: argparse.Namespace) -> int:
    project_root = args.project_root.resolve()
    config_path = args.config.resolve()
    if args.init_config:
        init_config(config_path, project_root)
        if not args.watch:
            return 0

    config = load_config(config_path)
    watch_directory = Path(str(config.get("watchDirectory", ""))).resolve()
    service_name = str(config.get("service", "")).strip()
    poll_interval = float(config.get("pollIntervalSeconds", 0.75))
    chunk_size = int(config.get("chunkSize", 1800))

    if not watch_directory.exists() or not watch_directory.is_dir():
        raise RuntimeError(f"watchDirectory does not exist or is not a directory: {watch_directory}")
    if not service_name:
        raise RuntimeError("service is missing in config")

    command = load_server_command(args.config_toml, args.server)
    client = MCPClient(command, cwd=project_root)
    try:
        client.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "editor-to-studio-sync", "version": "1.0.0"},
            },
        )
        client.notify("notifications/initialized", {})
        ensure_active_studio(client, wait_seconds=args.ws_wait_seconds)

        last_fingerprint = ""
        print(f"[watch-sync] watching: {watch_directory}")
        print(f"[watch-sync] service: {service_name}")
        print("[watch-sync] press Ctrl+C to stop")

        while True:
            try:
                fingerprint = compute_tree_fingerprint(watch_directory)
                if fingerprint != last_fingerprint:
                    payload = build_node_from_dir(
                        watch_directory,
                        name_override=service_name,
                        root_service_name=service_name,
                    )
                    push_payload(client, service_name=service_name, payload=payload, chunk_size=chunk_size)
                    last_fingerprint = fingerprint
                    print(f"[watch-sync] synced {service_name} at {time.strftime('%H:%M:%S')}")
            except KeyboardInterrupt:
                raise
            except Exception as exc:
                print(f"[watch-sync] sync error: {exc}")
            time.sleep(poll_interval)
    except KeyboardInterrupt:
        print("[watch-sync] stopped")
        return 0
    finally:
        client.close()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Watch editor files and sync a selected service folder to Roblox Studio via MCP")
    parser.add_argument("--project-root", type=Path, default=Path.cwd())
    parser.add_argument("--config", type=Path, default=DEFAULT_CONFIG_PATH)
    parser.add_argument("--config-toml", type=Path, default=Path.home() / ".codex" / "config.toml")
    parser.add_argument("--server", default="Roblox_Studio")
    parser.add_argument("--ws-wait-seconds", type=float, default=15.0)
    parser.add_argument("--init-config", action="store_true")
    parser.add_argument("--watch", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not args.init_config and not args.watch:
        args.init_config = True
        args.watch = True
    return run_watch(args)


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as err:
        print(f"[watch-sync] error: {err}", file=sys.stderr)
        raise

