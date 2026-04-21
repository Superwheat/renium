#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import concurrent.futures
import ctypes
import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Callable

try:
    import orjson as _orjson
except Exception:
    _orjson = None

JSON_BACKEND = "orjson" if _orjson is not None else "stdlib-json"
if _orjson is not None:
    JSON_DECODE_ERRORS = (json.JSONDecodeError, _orjson.JSONDecodeError)
else:
    JSON_DECODE_ERRORS = (json.JSONDecodeError,)


def json_loads_fast(text: str | bytes | bytearray) -> Any:
    if _orjson is not None:
        return _orjson.loads(text)
    return json.loads(text)


def json_dumps_compact(value: Any) -> str:
    if _orjson is not None:
        return _orjson.dumps(value).decode("utf-8")
    return json.dumps(value, separators=(",", ":"), ensure_ascii=False)


def json_dumps_pretty(value: Any) -> str:
    if _orjson is not None:
        return _orjson.dumps(value, option=_orjson.OPT_INDENT_2).decode("utf-8")
    return json.dumps(value, indent=2, ensure_ascii=False)

try:
    import tkinter as tk
    from tkinter import filedialog
except Exception:
    tk = None
    filedialog = None

try:
    import websockets
except Exception:
    websockets = None

DEFAULT_SERVICES = [
    "Workspace",
    "Players",
    "Lighting",
    "MaterialService",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
]

RUNTIME_LUA = r'''
local HttpService = game:GetService("HttpService")

do
    local existing = _G.CDX_SYNC_EXPORT
    local persistedState = {}
    if type(existing) == "table" and type(existing._state) == "table" then
        persistedState = existing._state
    end

    local ALLOWED_SERVICES = {
        Workspace = true,
        Players = true,
        Lighting = true,
        MaterialService = true,
        ReplicatedFirst = true,
        ReplicatedStorage = true,
        ServerScriptService = true,
        ServerStorage = true,
        StarterGui = true,
        StarterPack = true,
        StarterPlayer = true,
    }

    local PROPERTY_CANDIDATES = {
        "Archivable","Enabled","RunContext","Disabled","LinkedSource","Value","Name","ClassName","Parent",
        "AutoLocalize","RootLocalizationTable","BackgroundColor3","BackgroundTransparency","BorderColor3","BorderSizePixel",
        "Position","Size","AnchorPoint","Rotation","Visible","Text","TextColor3","TextSize","TextScaled","FontFace",
        "Image","ImageColor3","ImageTransparency","Color","Transparency","ZIndex","LayoutOrder","Active","Selectable",
        "CanvasSize","ScrollBarThickness","AutomaticCanvasSize","RichText","LineHeight","MaxVisibleGraphemes","SliceCenter",
        "ScaleType","TileSize","Padding","CellPadding","CellSize","FillDirection","SortOrder","HorizontalAlignment",
        "VerticalAlignment","ApplyStrokeMode","Thickness","Color3","Material","BrickColor","CanCollide","CanQuery",
        "CanTouch","Massless","Anchored","CastShadow","CFrame","Orientation","AssemblyLinearVelocity","AssemblyAngularVelocity",
        "Shape","Reflectance","TopSurface","BottomSurface","LeftSurface","RightSurface","FrontSurface","BackSurface",
        "LightInfluence","Brightness","ClockTime","FogColor","FogEnd","FogStart","GeographicLatitude","GlobalShadows",
        "EnvironmentDiffuseScale","EnvironmentSpecularScale","Ambient","OutdoorAmbient","Technology",
    }

    local function tryRead(instance, propertyName)
        return pcall(function()
            return instance[propertyName]
        end)
    end

    local function getDebugId(instance)
        local ok, debugId = pcall(function()
            return instance:GetDebugId(32)
        end)
        if ok and type(debugId) == "string" and #debugId > 0 then
            return debugId
        end
        return nil
    end

    local function serializeValue(value)
        local valueType = typeof(value)
        if valueType == "number" or valueType == "string" or valueType == "boolean" then
            return value
        elseif valueType == "Vector2" then
            return { _type = "Vector2", x = value.X, y = value.Y }
        elseif valueType == "Vector3" then
            return { _type = "Vector3", x = value.X, y = value.Y, z = value.Z }
        elseif valueType == "UDim" then
            return { _type = "UDim", scale = value.Scale, offset = value.Offset }
        elseif valueType == "UDim2" then
            return { _type = "UDim2", xScale = value.X.Scale, xOffset = value.X.Offset, yScale = value.Y.Scale, yOffset = value.Y.Offset }
        elseif valueType == "Color3" then
            return { _type = "Color3", r = value.R, g = value.G, b = value.B }
        elseif valueType == "ColorSequence" then
            local keypoints = {}
            for i, keypoint in ipairs(value.Keypoints) do
                keypoints[i] = { time = keypoint.Time, value = { r = keypoint.Value.R, g = keypoint.Value.G, b = keypoint.Value.B } }
            end
            return { _type = "ColorSequence", keypoints = keypoints }
        elseif valueType == "NumberSequence" then
            local keypoints = {}
            for i, keypoint in ipairs(value.Keypoints) do
                keypoints[i] = { time = keypoint.Time, value = keypoint.Value, envelope = keypoint.Envelope }
            end
            return { _type = "NumberSequence", keypoints = keypoints }
        elseif valueType == "CFrame" then
            return { _type = "CFrame", components = { value:GetComponents() } }
        elseif valueType == "Rect" then
            return { _type = "Rect", minX = value.Min.X, minY = value.Min.Y, maxX = value.Max.X, maxY = value.Max.Y }
        elseif valueType == "EnumItem" then
            return { _type = "EnumItem", enumType = tostring(value.EnumType), name = value.Name }
        elseif valueType == "Font" then
            return { _type = "Font", family = value.Family, weight = tostring(value.Weight), style = tostring(value.Style) }
        elseif valueType == "Instance" then
            return value:GetFullName()
        end
        return nil
    end

    local NO_DEFAULTS = {}
    local NO_PROPERTIES = {}
    local DEFAULT_PROPERTY_CACHE = {}
    local CLASS_PROPERTY_CANDIDATES_CACHE = {}
    local EXPORT_ALL_PROPERTIES = false

    local function deepEqual(a, b)
        if a == b then
            return true
        end

        if type(a) ~= type(b) then
            return false
        end

        if type(a) ~= "table" then
            return false
        end

        for k, v in pairs(a) do
            if not deepEqual(v, b[k]) then
                return false
            end
        end

        for k, _ in pairs(b) do
            if a[k] == nil then
                return false
            end
        end

        return true
    end

    local function getDefaultSerializedProperties(className)
        local cached = DEFAULT_PROPERTY_CACHE[className]
        if cached ~= nil then
            if cached == NO_DEFAULTS then
                return nil
            end
            return cached
        end

        local ok, probe = pcall(function()
            return Instance.new(className)
        end)
        if not ok or probe == nil then
            DEFAULT_PROPERTY_CACHE[className] = NO_DEFAULTS
            CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
            return nil
        end

        local defaults = {}
        local classCandidates = {}
        for _, propertyName in ipairs(PROPERTY_CANDIDATES) do
            if propertyName ~= "Source" then
                local got, value = tryRead(probe, propertyName)
                if got then
                    table.insert(classCandidates, propertyName)
                    if value ~= nil then
                        local serialized = serializeValue(value)
                        if serialized ~= nil then
                            defaults[propertyName] = serialized
                        end
                    end
                end
            end
        end

        probe:Destroy()
        if #classCandidates > 0 then
            CLASS_PROPERTY_CANDIDATES_CACHE[className] = classCandidates
        else
            CLASS_PROPERTY_CANDIDATES_CACHE[className] = NO_PROPERTIES
        end
        DEFAULT_PROPERTY_CACHE[className] = defaults
        return defaults
    end

    local function getClassPropertyCandidates(className)
        local cached = CLASS_PROPERTY_CANDIDATES_CACHE[className]
        if cached == nil then
            getDefaultSerializedProperties(className)
            cached = CLASS_PROPERTY_CANDIDATES_CACHE[className]
        end

        if cached == nil or cached == NO_PROPERTIES then
            return nil
        end

        return cached
    end

    local function tryIsPropertyModified(instance, propertyName)
        local ok, modified = pcall(function()
            return instance:IsPropertyModified(propertyName)
        end)
        if ok and type(modified) == "boolean" then
            return true, modified
        end
        return false, nil
    end

    local function exportInstanceInternal(instance, safeReads, path, parentPath, debugId, parentDebugId)
        local entry = {
            name = instance.Name,
            className = instance.ClassName,
            path = path,
            parentPath = parentPath,
            parentDebugId = parentDebugId,
            attributes = instance:GetAttributes(),
        }
        local properties = {}
        local defaultProperties = getDefaultSerializedProperties(instance.ClassName)

        if debugId ~= nil then
            entry.debugId = debugId
        end

        local propertyNames = getClassPropertyCandidates(instance.ClassName) or PROPERTY_CANDIDATES
        for _, propertyName in ipairs(propertyNames) do
            if propertyName ~= "Source" then
                local defaultSerialized = defaultProperties and defaultProperties[propertyName] or nil
                local skipRead = false
                if not EXPORT_ALL_PROPERTIES and defaultSerialized ~= nil then
                    local hasModifiedInfo, isModified = tryIsPropertyModified(instance, propertyName)
                    if hasModifiedInfo and not isModified then
                        skipRead = true
                    end
                end

                if not skipRead then
                    local value = nil
                    local hasValue = false
                    if safeReads then
                        local got, safeValue = tryRead(instance, propertyName)
                        if got then
                            value = safeValue
                            hasValue = true
                        end
                    else
                        value = instance[propertyName]
                        hasValue = true
                    end

                    if hasValue and value ~= nil then
                        local serialized = serializeValue(value)
                        if serialized ~= nil then
                            if EXPORT_ALL_PROPERTIES or defaultSerialized == nil or not deepEqual(serialized, defaultSerialized) then
                                properties[propertyName] = serialized
                            end
                        end
                    end
                end
            end
        end

        if instance:IsA("LuaSourceContainer") then
            properties.Source = "__SOURCE_EXTERNAL__"
        end

        if next(properties) ~= nil then
            entry.properties = properties
        end

        if type(entry.attributes) == "table" and next(entry.attributes) == nil then
            entry.attributes = nil
        end

        return entry
    end

    local function exportInstanceFast(instance, path, parentPath, debugId, parentDebugId)
        return exportInstanceInternal(instance, false, path, parentPath, debugId, parentDebugId)
    end

    local function exportInstanceSafe(instance, path, parentPath, debugId, parentDebugId)
        return exportInstanceInternal(instance, true, path, parentPath, debugId, parentDebugId)
    end

    local State = persistedState
    local M = {}

    local function getState(serviceName)
        local state = State[serviceName]
        if not state then
            -- Worker clients can race on shared Studio globals; recover by preparing on-demand.
            M.prepare(serviceName)
            state = State[serviceName]
        end
        if not state then
            error("State not prepared for service: " .. tostring(serviceName))
        end
        return state
    end

    local function getCachedInstancePath(state, instance)
        local cached = state.pathByInstance[instance]
        if cached ~= nil then
            return cached
        end

        local parent = instance.Parent
        local path
        if parent == nil or parent == game then
            path = instance.Name
        else
            path = getCachedInstancePath(state, parent) .. "." .. instance.Name
        end

        state.pathByInstance[instance] = path
        return path
    end

    local function getCachedParentPath(state, instance)
        local parent = instance.Parent
        if parent == nil then
            return nil
        end
        if parent == game then
            return "game"
        end
        return getCachedInstancePath(state, parent)
    end

    local function getCachedDebugId(state, instance)
        local cached = state.debugIdByInstance[instance]
        if cached ~= nil then
            if cached == false then
                return nil
            end
            return cached
        end

        local ok, debugId = pcall(function()
            return instance:GetDebugId(32)
        end)
        if ok and type(debugId) == "string" and #debugId > 0 then
            state.debugIdByInstance[instance] = debugId
            return debugId
        end

        state.debugIdByInstance[instance] = false
        return nil
    end

    local function getCachedParentDebugId(state, instance)
        local parent = instance.Parent
        if parent == nil or parent == game then
            return nil
        end
        return getCachedDebugId(state, parent)
    end

    local function ensureScriptIndex(state)
        if state.scriptPaths ~= nil and state.scriptInstances ~= nil then
            return
        end

        local scriptObjects = state.scriptObjects
        local scriptPaths = table.create(#scriptObjects)
        local scriptInstances = {}
        for i, inst in ipairs(scriptObjects) do
            local path = getCachedInstancePath(state, inst)
            scriptPaths[i] = path
            scriptInstances[path] = inst
        end
        table.sort(scriptPaths)

        state.scriptPaths = scriptPaths
        state.scriptInstances = scriptInstances
        state.scriptPathsEncoded = nil
    end

    local function chunkEncodedString(encoded, startIndex, maxLen)
        local total = #encoded
        local startPos = math.max(1, startIndex or 1)
        local take = math.max(1, maxLen or 2000)

        if startPos > total then
            return { start = startPos, nextStart = startPos, total = total, chunk = "" }
        end

        local finish = math.min(total, startPos + take - 1)
        local chunk = string.sub(encoded, startPos, finish)
        return { start = startPos, nextStart = finish + 1, total = total, chunk = chunk }
    end

    function M.prepare(serviceName)
        if not ALLOWED_SERVICES[serviceName] then
            error("Unsupported service: " .. tostring(serviceName))
        end

        local service = game:FindFirstChild(serviceName)
        if not service then
            error("Service not found: " .. serviceName)
        end

        local instances = {}
        local scriptObjects = {}
        local scriptSources = {}
        local classSeen = {}
        local classNames = {}

        local descendants = service:GetDescendants()
        local expectedCount = #descendants + 1
        instances = table.create(expectedCount)
        instances[1] = service
        local instanceCount = 1
        local scriptCount = 0

        local rootPath = service.Name
        local pathByInstance = { [service] = rootPath }
        local debugIdByInstance = {}
        local serviceDebugId = getDebugId(service)
        if serviceDebugId then
            debugIdByInstance[service] = serviceDebugId
        else
            debugIdByInstance[service] = false
        end

        local serviceClassName = service.ClassName
        classSeen[serviceClassName] = true
        classNames[1] = serviceClassName

        if service:IsA("LuaSourceContainer") then
            scriptCount = 1
            scriptObjects[scriptCount] = service
        end

        for i, inst in ipairs(descendants) do
            instanceCount = instanceCount + 1
            instances[instanceCount] = inst

            local className = inst.ClassName
            if not classSeen[className] then
                classSeen[className] = true
                classNames[#classNames + 1] = className
            end

            if inst:IsA("LuaSourceContainer") then
                scriptCount = scriptCount + 1
                scriptObjects[scriptCount] = inst
            end
        end

        State[serviceName] = {
            instances = instances,
            classNames = classNames,
            generatedAtUnix = os.time(),
            rootName = service.Name,
            rootClassName = service.ClassName,
            rootPath = rootPath,
            pathByInstance = pathByInstance,
            debugIdByInstance = debugIdByInstance,
            scriptObjects = scriptObjects,
            scriptPaths = nil,
            scriptSources = scriptSources,
            scriptInstances = nil,
            classDefaults = nil,
            classDefaultsEncoded = nil,
            scriptPathsEncoded = nil,
            batchCacheByKey = {},
            batchCacheKeys = {},
        }

        return {
            service = serviceName,
            generatedAtUnix = State[serviceName].generatedAtUnix,
            rootName = State[serviceName].rootName,
            rootClassName = State[serviceName].rootClassName,
            rootPath = State[serviceName].rootPath,
            instanceCount = instanceCount,
            scriptCount = scriptCount,
        }
    end

    function M.getInstanceBatch(serviceName, startIndex, maxCount)
        local state = getState(serviceName)
        local key = tostring(startIndex or 1) .. ":" .. tostring(maxCount or 300)
        local cachedPayload = state.batchCacheByKey[key]
        if cachedPayload then
            return cachedPayload
        end

        local instances = state.instances
        local total = #instances
        local startPos = math.max(1, startIndex or 1)
        local take = math.max(1, maxCount or 300)

        local encoded
        if startPos > total then
            encoded = HttpService:JSONEncode({ start = startPos, nextStart = startPos, total = total, items = {} })
        else
            local finish = math.min(total, startPos + take - 1)
            local count = finish - startPos + 1
            local items = table.create(count)
            for i = startPos, finish do
                local inst = instances[i]
                local path = getCachedInstancePath(state, inst)
                local parentPath = getCachedParentPath(state, inst)
                local debugId = getCachedDebugId(state, inst)
                local parentDebugId = getCachedParentDebugId(state, inst)

                local ok, entry = pcall(exportInstanceFast, inst, path, parentPath, debugId, parentDebugId)
                if not ok then
                    entry = exportInstanceSafe(inst, path, parentPath, debugId, parentDebugId)
                end
                items[#items + 1] = entry
            end
            encoded = HttpService:JSONEncode({ start = startPos, nextStart = finish + 1, total = total, items = items })
        end

        state.batchCacheByKey[key] = encoded
        state.batchCacheKeys[#state.batchCacheKeys + 1] = key
        if #state.batchCacheKeys > 12 then
            local oldestKey = table.remove(state.batchCacheKeys, 1)
            if oldestKey and oldestKey ~= key then
                state.batchCacheByKey[oldestKey] = nil
            end
        end
        return encoded
    end

    function M.getInstanceBatchChunk(serviceName, startIndex, maxCount, chunkStart, maxLen)
        local encoded = M.getInstanceBatch(serviceName, startIndex, maxCount)
        return HttpService:JSONEncode(chunkEncodedString(encoded, chunkStart, maxLen))
    end

    function M.getClassDefaults(serviceName)
        local state = getState(serviceName)
        if state.classDefaultsEncoded ~= nil then
            return state.classDefaultsEncoded
        end

        if state.classDefaults == nil then
            local classDefaults = {}
            for _, className in ipairs(state.classNames) do
                local defaults = getDefaultSerializedProperties(className)
                if defaults ~= nil and next(defaults) ~= nil then
                    classDefaults[className] = defaults
                end
            end
            state.classDefaults = classDefaults
        end

        state.classDefaultsEncoded = HttpService:JSONEncode(state.classDefaults)
        return state.classDefaultsEncoded
    end

    function M.getClassDefaultsChunk(serviceName, startIndex, maxLen)
        local encoded = M.getClassDefaults(serviceName)
        return HttpService:JSONEncode(chunkEncodedString(encoded, startIndex, maxLen))
    end

    function M.getScriptPaths(serviceName)
        local state = getState(serviceName)
        ensureScriptIndex(state)
        if state.scriptPathsEncoded == nil then
            state.scriptPathsEncoded = HttpService:JSONEncode(state.scriptPaths)
        end
        return state.scriptPathsEncoded
    end

    function M.getScriptPathsChunk(serviceName, startIndex, maxLen)
        local encoded = M.getScriptPaths(serviceName)
        return HttpService:JSONEncode(chunkEncodedString(encoded, startIndex, maxLen))
    end

    function M.getSourceChunk(serviceName, instancePath, startIndex, maxLen)
        local state = getState(serviceName)
        ensureScriptIndex(state)
        local src = state.scriptSources[instancePath]
        if src == nil then
            local scriptInstance = state.scriptInstances and state.scriptInstances[instancePath] or nil
            if scriptInstance == nil then
                return HttpService:JSONEncode({ start = 1, nextStart = 1, total = 0, chunk = "" })
            end

            local ok, loaded = pcall(function()
                return scriptInstance.Source
            end)
            src = ok and loaded or ""
            state.scriptSources[instancePath] = src
        end

        local total = #src
        local startPos = math.max(1, startIndex or 1)
        local take = math.max(1, maxLen or 2000)

        if startPos > total then
            return HttpService:JSONEncode({ start = startPos, nextStart = startPos, total = total, chunk = "" })
        end

        local finish = math.min(total, startPos + take - 1)
        local chunk = string.sub(src, startPos, finish)
        return HttpService:JSONEncode({ start = startPos, nextStart = finish + 1, total = total, chunk = chunk })
    end

    function M.release(serviceName)
        State[serviceName] = nil
        return "ok"
    end

    function M.setExportOptions(payload)
        local options = payload
        if type(payload) ~= "table" then
            options = {}
        end
        EXPORT_ALL_PROPERTIES = options.exportAllProperties == true
        return {
            exportAllProperties = EXPORT_ALL_PROPERTIES,
        }
    end

    M._state = State
    _G.CDX_SYNC_EXPORT = M
end

return "ok"
'''


class MCPClient:
    def __init__(self, command: list[str], cwd: Path) -> None:
        self._proc = subprocess.Popen(
            command,
            cwd=str(cwd),
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        if self._proc.stdin is None or self._proc.stdout is None or self._proc.stderr is None:
            raise RuntimeError("Failed to open MCP process stdio")

        self._stdin = self._proc.stdin
        self._stdout = self._proc.stdout
        self._next_id = 1

        def _drain_stderr() -> None:
            for line in self._proc.stderr:
                sys.stderr.write(f"[mcp] {line.decode(errors='replace')}")

        self._stderr_thread = threading.Thread(target=_drain_stderr, daemon=True)
        self._stderr_thread.start()

    def close(self) -> None:
        if self._proc.poll() is None:
            self._proc.terminate()
            try:
                self._proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self._proc.kill()

    def _send(self, payload: dict[str, Any]) -> None:
        # Roblox Studio MCP uses newline-delimited JSON-RPC on stdio.
        body = (json_dumps_compact(payload) + "\n").encode("utf-8")
        self._stdin.write(body)
        self._stdin.flush()

    def _read(self) -> dict[str, Any]:
        while True:
            line = self._stdout.readline()
            if line == b"":
                raise RuntimeError("MCP server closed stdout")
            text = line.decode("utf-8", errors="replace").strip()
            if not text:
                continue
            try:
                return json_loads_fast(text)
            except JSON_DECODE_ERRORS:
                # Ignore non-JSON stdout noise and continue reading.
                continue

    def request(self, method: str, params: dict[str, Any] | None = None) -> Any:
        request_id = self._next_id
        self._next_id += 1
        self._send({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": method,
            "params": params or {},
        })

        while True:
            msg = self._read()
            if msg.get("id") != request_id:
                continue
            if "error" in msg:
                raise RuntimeError(f"MCP error for {method}: {msg['error']}")
            return msg.get("result")

    def notify(self, method: str, params: dict[str, Any] | None = None) -> None:
        self._send({
            "jsonrpc": "2.0",
            "method": method,
            "params": params or {},
        })


class WSBridgeServer:
    def __init__(self, host: str, ports: list[int]) -> None:
        self.host = host
        self.ports = ports
        self.channels: dict[int, dict[str, Any]] = {
            p: {"websocket": None, "pending": {}} for p in ports
        }
        self._servers: list[Any] = []
        self._connected_event = asyncio.Event()
        self._next_id = 1
        self._last_duplicate_log: dict[int, float] = {}

    async def start(self) -> None:
        if websockets is None:
            raise RuntimeError("The 'websockets' package is required for --transport ws. Install with: pip install websockets")

        for port in self.ports:
            async def handler(websocket, _path=None, *, _port=port):
                await self._handle_connection(_port, websocket)

            server = await websockets.serve(handler, self.host, port, max_size=None)
            self._servers.append(server)
            print(f"[bridge] listening ws://{self.host}:{port}")

    async def stop(self) -> None:
        for port, channel in self.channels.items():
            websocket = channel["websocket"]
            if websocket is not None:
                try:
                    await websocket.close()
                except Exception:
                    pass
                channel["websocket"] = None

            pending: dict[str, asyncio.Future] = channel["pending"]
            for fut in list(pending.values()):
                if not fut.done():
                    fut.cancel()
            pending.clear()

        for server in self._servers:
            server.close()
            await server.wait_closed()
        self._servers.clear()

    def connected_ports(self) -> list[int]:
        return [p for p, ch in self.channels.items() if ch["websocket"] is not None]

    async def wait_for_channels(self, min_count: int, timeout: float) -> list[int]:
        min_required = max(1, min(min_count, len(self.ports)))
        deadline = asyncio.get_running_loop().time() + max(0.1, timeout)
        while True:
            connected = self.connected_ports()
            if len(connected) >= min_required:
                return connected

            remaining = deadline - asyncio.get_running_loop().time()
            if remaining <= 0:
                raise RuntimeError(
                    f"Timed out waiting for plugin bridge channels ({len(connected)}/{min_required} connected). "
                    "Open Studio and enable ParallelExportBridge plugin."
                )

            self._connected_event.clear()
            try:
                await asyncio.wait_for(self._connected_event.wait(), timeout=remaining)
            except asyncio.TimeoutError as exc:
                raise RuntimeError(
                    f"Timed out waiting for plugin bridge channels ({len(self.connected_ports())}/{min_required} connected)."
                ) from exc

    async def call(self, port: int, method: str, params: dict[str, Any], timeout: float = 60.0) -> Any:
        channel = self.channels.get(port)
        if channel is None:
            raise RuntimeError(f"Unknown bridge channel port: {port}")

        websocket = channel["websocket"]
        if websocket is None:
            raise RuntimeError(f"Bridge channel {port} is not connected")

        req_id = f"{port}:{self._next_id}"
        self._next_id += 1

        loop = asyncio.get_running_loop()
        fut: asyncio.Future = loop.create_future()
        pending: dict[str, asyncio.Future] = channel["pending"]
        pending[req_id] = fut

        payload = {"id": req_id, "method": method, "params": params}
        await websocket.send(json_dumps_compact(payload))

        try:
            return await asyncio.wait_for(fut, timeout=timeout)
        except asyncio.CancelledError as exc:
            raise RuntimeError(f"Bridge channel {port} disconnected") from exc
        finally:
            pending.pop(req_id, None)

    async def _handle_connection(self, port: int, websocket: Any) -> None:
        channel = self.channels[port]
        old = channel["websocket"]
        if old is not None and old is not websocket:
            old_closed = bool(getattr(old, "closed", False))
            if not old_closed:
                now = time.time()
                last = self._last_duplicate_log.get(port, 0.0)
                if now - last >= 2.0:
                    self._last_duplicate_log[port] = now
                    print(f"[bridge] duplicate channel attempt on {port}; keeping current channel")
                try:
                    await websocket.close(code=1013, reason="bridge channel already active")
                except Exception:
                    pass
                return

        channel["websocket"] = websocket
        self._connected_event.set()
        print(f"[bridge] channel connected on {port}")

        try:
            async for raw in websocket:
                await self._on_message(port, raw)
        finally:
            if channel["websocket"] is websocket:
                channel["websocket"] = None

            pending: dict[str, asyncio.Future] = channel["pending"]
            for fut in list(pending.values()):
                if not fut.done():
                    fut.cancel()
            pending.clear()
            print(f"[bridge] channel disconnected on {port}")

    async def _on_message(self, port: int, raw: str) -> None:
        channel = self.channels[port]
        try:
            msg = json_loads_fast(raw)
        except JSON_DECODE_ERRORS:
            return

        req_id = msg.get("id")
        if req_id is None:
            return

        pending: dict[str, asyncio.Future] = channel["pending"]
        fut = pending.pop(str(req_id), None)
        if fut is None or fut.done():
            return

        if msg.get("ok") is True:
            fut.set_result(msg.get("result"))
        else:
            fut.set_exception(RuntimeError(str(msg.get("error", "bridge error"))))


class WSBridgeRuntime:
    def __init__(self, host: str, ports: list[int]) -> None:
        self.host = host
        self.ports = ports
        self.loop = asyncio.new_event_loop()
        self.server = WSBridgeServer(host=host, ports=ports)
        self._thread = threading.Thread(target=self._run_loop, daemon=True)
        self._started = False

    def _run_loop(self) -> None:
        asyncio.set_event_loop(self.loop)
        self.loop.run_forever()

    def start(self) -> None:
        if self._started:
            return
        self._thread.start()
        asyncio.run_coroutine_threadsafe(self.server.start(), self.loop).result(timeout=15)
        self._started = True

    def stop(self) -> None:
        if not self._started:
            return
        try:
            asyncio.run_coroutine_threadsafe(self.server.stop(), self.loop).result(timeout=10)
        finally:
            self.loop.call_soon_threadsafe(self.loop.stop)
            self._thread.join(timeout=5)
            self._started = False

    def wait_for_channels(self, min_count: int, timeout: float) -> list[int]:
        fut = asyncio.run_coroutine_threadsafe(
            self.server.wait_for_channels(min_count=min_count, timeout=timeout),
            self.loop,
        )
        return fut.result(timeout=max(timeout + 2, 5))

    def connected_ports(self) -> list[int]:
        return self.server.connected_ports()

    def call(self, port: int, method: str, params: dict[str, Any], timeout: float = 60.0) -> Any:
        fut = asyncio.run_coroutine_threadsafe(
            self.server.call(port=port, method=method, params=params, timeout=timeout),
            self.loop,
        )
        return fut.result(timeout=max(timeout + 2, 5))


def _is_bridge_connection_error(exc: Exception) -> bool:
    if isinstance(exc, (TimeoutError, asyncio.TimeoutError, concurrent.futures.TimeoutError)):
        return True
    text = str(exc).lower()
    markers = (
        "bridge channel",
        "not connected",
        "disconnected",
        "connection closed",
        "connectionclosed",
        "received 1005",
        "no status received",
        "state not prepared for service",
    )
    return any(marker in text for marker in markers)


def _ordered_connected_bridge_ports(
    bridge: WSBridgeRuntime,
    preferred_ports: list[int] | None = None,
) -> list[int]:
    connected = bridge.connected_ports()
    if not preferred_ports:
        return connected
    ordered: list[int] = []
    for port in preferred_ports:
        if port in connected and port not in ordered:
            ordered.append(port)
    for port in connected:
        if port not in ordered:
            ordered.append(port)
    return ordered


class AdaptiveBridgeThrottle:
    def __init__(
        self,
        enabled: bool = True,
        low_fps: float = 35.0,
        high_fps: float = 50.0,
        max_delay_ms: float = 40.0,
        step_ms: float = 2.0,
        probe_interval_ms: float = 500.0,
    ) -> None:
        self.enabled = bool(enabled)
        self.low_fps = float(low_fps)
        self.high_fps = float(high_fps)
        if self.high_fps <= self.low_fps:
            self.high_fps = self.low_fps + 5.0
        self.max_delay_s = max(0.0, float(max_delay_ms) / 1000.0)
        self.step_s = max(0.0005, float(step_ms) / 1000.0)
        self.probe_interval_s = max(0.05, float(probe_interval_ms) / 1000.0)
        self._lock = threading.Lock()
        self._delay_s = 0.0
        self._next_probe_at = 0.0
        self._probing = False
        self._last_fps: float | None = None
        self._last_log_delay_ms = -1

    def before_bridge_call(self, method: str) -> None:
        if not self.enabled or method == "getPerformanceStats":
            return
        with self._lock:
            delay_s = self._delay_s
        if delay_s > 0:
            time.sleep(delay_s)

    def maybe_probe(self, bridge: WSBridgeRuntime, preferred_ports: list[int] | None = None) -> None:
        if not self.enabled:
            return
        now = time.monotonic()
        with self._lock:
            if self._probing or now < self._next_probe_at:
                return
            self._probing = True
            self._next_probe_at = now + self.probe_interval_s

        try:
            ports = _ordered_connected_bridge_ports(bridge, preferred_ports)
            result: Any = None
            for port in ports:
                try:
                    result = bridge.call(port=port, method="getPerformanceStats", params={}, timeout=2.0)
                    break
                except Exception:
                    continue

            if not isinstance(result, dict):
                return
            fps = result.get("fps")
            if not isinstance(fps, (int, float)) or fps <= 0:
                return

            with self._lock:
                self._last_fps = float(fps)
                if self._last_fps < self.low_fps:
                    self._delay_s = min(self.max_delay_s, self._delay_s + self.step_s)
                elif self._last_fps > self.high_fps:
                    self._delay_s = max(0.0, self._delay_s - self.step_s)
                else:
                    self._delay_s = max(0.0, self._delay_s - (self.step_s * 0.5))

                delay_ms = int(round(self._delay_s * 1000.0))
                if delay_ms != self._last_log_delay_ms and abs(delay_ms - self._last_log_delay_ms) >= 2:
                    self._last_log_delay_ms = delay_ms
                    print(f"[sync] adaptive throttle: fps={self._last_fps:.1f}, delay={delay_ms}ms")
        finally:
            with self._lock:
                self._probing = False


def bridge_call_with_failover(
    bridge: WSBridgeRuntime,
    method: str,
    params: dict[str, Any],
    timeout: float,
    preferred_ports: list[int] | None = None,
    retries: int = 8,
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
) -> tuple[Any, int]:
    last_exc: Exception | None = None
    max_attempts = max(1, retries)
    for attempt in range(max_attempts):
        ports = _ordered_connected_bridge_ports(bridge, preferred_ports)
        if not ports:
            time.sleep(min(1.0, 0.15 * (attempt + 1)))
            continue
        for port in ports:
            try:
                if adaptive_throttle is not None:
                    adaptive_throttle.before_bridge_call(method)
                result = bridge.call(port, method, params, timeout=timeout)
                if adaptive_throttle is not None and method != "getPerformanceStats":
                    adaptive_throttle.maybe_probe(bridge=bridge, preferred_ports=preferred_ports)
                return result, port
            except Exception as exc:
                last_exc = exc
                if not _is_bridge_connection_error(exc):
                    raise
        if attempt < max_attempts - 1:
            time.sleep(min(1.0, 0.15 * (attempt + 1)))

    if last_exc is not None:
        raise RuntimeError(
            f"Bridge call failed for {method} after {max_attempts} attempts: {last_exc}"
        ) from last_exc
    raise RuntimeError(f"Bridge call failed for {method}: no connected bridge channels")


def load_server_command(config_path: Path, server_name: str) -> list[str]:
    raw = tomllib.loads(config_path.read_text(encoding="utf-8"))
    servers = raw.get("mcp_servers")
    if not isinstance(servers, dict) or server_name not in servers:
        raise RuntimeError(f"mcp_servers.{server_name} not found in {config_path}")

    entry = servers[server_name]
    if not isinstance(entry, dict):
        raise RuntimeError(f"Invalid mcp server entry for {server_name}")

    command = entry.get("command")
    args = entry.get("args", [])
    if not isinstance(command, str):
        raise RuntimeError(f"Missing command for mcp server {server_name}")
    if not isinstance(args, list) or any(not isinstance(x, str) for x in args):
        raise RuntimeError(f"Invalid args for mcp server {server_name}")

    # Follow Roblox docs exactly: cmd.exe /c %LOCALAPPDATA%\\Roblox\\mcp.bat
    return [os.path.expandvars(command), *[os.path.expandvars(a) for a in args]]


def extract_text_content(result: Any) -> str:
    if not isinstance(result, dict):
        raise RuntimeError(f"Unexpected tools/call result: {result!r}")

    content = result.get("content")
    if isinstance(content, list):
        parts: list[str] = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                parts.append(str(item.get("text", "")))
        return "".join(parts)

    if "text" in result:
        return str(result["text"])

    return json_dumps_compact(result)


def call_tool(client: MCPClient, name: str, arguments: dict[str, Any]) -> str:
    result = client.request("tools/call", {"name": name, "arguments": arguments})
    return extract_text_content(result)


def execute_luau(client: MCPClient, code: str) -> str:
    return call_tool(client, "execute_luau", {"code": code})


def ensure_export_runtime_loaded(client: MCPClient, attempts: int = 3) -> None:
    last_err: Exception | None = None
    for _ in range(attempts):
        try:
            execute_luau(client, RUNTIME_LUA)
            probe = execute_luau(
                client,
                "return (_G.CDX_SYNC_EXPORT and type(_G.CDX_SYNC_EXPORT.prepare) == 'function') and 'ready' or 'missing'",
            ).strip()
            if probe == "ready":
                return
            last_err = RuntimeError(f"Export runtime probe returned: {probe!r}")
        except Exception as exc:
            last_err = exc
        time.sleep(0.25)
    if last_err is not None:
        raise RuntimeError(f"Failed to load export runtime in Studio: {last_err}") from last_err
    raise RuntimeError("Failed to load export runtime in Studio")


def get_total_ram_gb() -> float | None:
    if os.name != "nt":
        return None

    class MEMORYSTATUSEX(ctypes.Structure):
        _fields_ = [
            ("dwLength", ctypes.c_ulong),
            ("dwMemoryLoad", ctypes.c_ulong),
            ("ullTotalPhys", ctypes.c_ulonglong),
            ("ullAvailPhys", ctypes.c_ulonglong),
            ("ullTotalPageFile", ctypes.c_ulonglong),
            ("ullAvailPageFile", ctypes.c_ulonglong),
            ("ullTotalVirtual", ctypes.c_ulonglong),
            ("ullAvailVirtual", ctypes.c_ulonglong),
            ("ullAvailExtendedVirtual", ctypes.c_ulonglong),
        ]

    status = MEMORYSTATUSEX()
    status.dwLength = ctypes.sizeof(MEMORYSTATUSEX)
    ok = ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status))
    if not ok:
        return None
    return status.ullTotalPhys / (1024 ** 3)


def detect_recommended_source_workers() -> tuple[int, int, float | None]:
    cpu_count = os.cpu_count() or 4
    ram_gb = get_total_ram_gb()

    if ram_gb is None:
        ram_limit = max(12, cpu_count * 2)
    elif ram_gb >= 64:
        ram_limit = 128
    elif ram_gb >= 48:
        ram_limit = 96
    elif ram_gb >= 32:
        ram_limit = 72
    elif ram_gb >= 24:
        ram_limit = 56
    elif ram_gb >= 16:
        ram_limit = 40
    elif ram_gb >= 12:
        ram_limit = 28
    elif ram_gb >= 8:
        ram_limit = 18
    else:
        ram_limit = 10

    # IO-bound workload (multiple MCP clients waiting on RPC): allow strong oversubscription.
    cpu_limit = max(8, cpu_count * 3)
    workers = max(1, min(cpu_limit, ram_limit, 2))
    return workers, cpu_count, ram_gb


def ensure_active_studio(client: MCPClient, wait_seconds: float = 12.0, poll_interval: float = 0.5) -> None:
    deadline = time.time() + max(0.0, wait_seconds)
    last_raw = ""

    while True:
        raw = call_tool(client, "list_roblox_studios", {})
        last_raw = raw

        try:
            parsed = json_loads_fast(raw)
        except JSON_DECODE_ERRORS:
            if "Not connected to the WS host" in raw and time.time() < deadline:
                time.sleep(poll_interval)
                continue
            raise RuntimeError(
                "Roblox MCP server is reachable but not connected to Studio. "
                "Open Roblox Studio and ensure the MCP bridge/plugin is connected. "
                f"Server response: {raw!r}"
            )

        studios = parsed.get("studios")
        if isinstance(studios, list) and studios:
            active = next((s for s in studios if isinstance(s, dict) and s.get("active") is True), None)
            chosen = active or studios[0]
            studio_id = chosen.get("id")
            if not isinstance(studio_id, str) or not studio_id:
                raise RuntimeError("Invalid studio id from list_roblox_studios")
            if active is None:
                call_tool(client, "set_active_studio", {"studio_id": studio_id})
            return

        if time.time() >= deadline:
            raise RuntimeError(
                "No Roblox Studio instances found before timeout. "
                f"Last response: {last_raw!r}"
            )
        time.sleep(poll_interval)


def initialize_export_worker(
    command: list[str],
    cwd: Path,
    ws_wait_seconds: float,
) -> MCPClient:
    client = MCPClient(command, cwd=cwd)
    client.request(
        "initialize",
        {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "codex-sync-export-worker", "version": "1.0.0"},
        },
    )
    client.notify("notifications/initialized", {})
    ensure_active_studio(client, wait_seconds=ws_wait_seconds)
    ensure_export_runtime_loaded(client)
    return client


def fetch_chunked(
    fetch_fn,
    initial_chunk_size: int,
    label: str,
) -> str:
    start = 1
    out_parts: list[str] = []
    chunk_size = initial_chunk_size

    while True:
        retries = 0
        while True:
            try:
                raw = fetch_fn(start, chunk_size)
                piece = json_loads_fast(raw)
                break
            except Exception as exc:
                retries += 1
                if retries > 5:
                    raise RuntimeError(f"Failed chunk fetch for {label} at start {start}: {exc}") from exc
                chunk_size = max(256, chunk_size // 2)

        chunk = piece.get("chunk", "")
        total = int(piece.get("total", 0))
        next_start = int(piece.get("nextStart", start))

        if not isinstance(chunk, str):
            raise RuntimeError(f"Invalid chunk type for {label}")

        out_parts.append(chunk)

        if start > total or chunk == "" and next_start <= start:
            break

        if next_start <= start:
            raise RuntimeError(f"Non-progressing chunk for {label} at {start}")

        start = next_start

        if start > total:
            break

    return "".join(out_parts)


def fetch_chunked_bridge(
    fetch_fn,
    initial_chunk_size: int,
    label: str,
) -> str:
    start = 1
    out_parts: list[str] = []
    chunk_size = max(256, int(initial_chunk_size))

    while True:
        retries = 0
        while True:
            try:
                piece = fetch_fn(start, chunk_size)
                break
            except Exception as exc:
                retries += 1
                if retries > 5:
                    raise RuntimeError(f"Failed bridge chunk fetch for {label} at start {start}: {exc}") from exc
                chunk_size = max(256, chunk_size // 2)

        if not isinstance(piece, dict):
            raise RuntimeError(f"Invalid bridge chunk payload for {label}: {type(piece)}")

        chunk = piece.get("chunk", "")
        total = int(piece.get("total", 0))
        next_start = int(piece.get("nextStart", start))

        if not isinstance(chunk, str):
            raise RuntimeError(f"Invalid bridge chunk type for {label}")

        out_parts.append(chunk)

        if start > total or (chunk == "" and next_start <= start):
            break
        if next_start <= start:
            raise RuntimeError(f"Non-progressing bridge chunk for {label} at {start}")

        start = next_start
        if start > total:
            break

    return "".join(out_parts)


def fetch_json_payload_bridge(
    fetch_fn,
    initial_chunk_size: int,
    label: str,
) -> Any:
    chunk_size = max(256, int(initial_chunk_size))
    last_exc: Exception | None = None
    for attempt in range(1, 7):
        raw = fetch_chunked_bridge(fetch_fn, chunk_size, label)
        try:
            return json_loads_fast(raw)
        except JSON_DECODE_ERRORS as exc:
            last_exc = exc
            if chunk_size <= 256:
                break
            next_chunk_size = max(256, chunk_size // 2)
            print(
                f"[sync] warning: invalid JSON payload for {label} "
                f"(attempt {attempt}/6, chunk_size={chunk_size}); retrying with chunk_size={next_chunk_size}"
            )
            chunk_size = next_chunk_size

    if last_exc is not None:
        raise RuntimeError(f"Failed to parse JSON payload for {label}: {last_exc}") from last_exc
    raise RuntimeError(f"Failed to parse JSON payload for {label}")


def fetch_instances_batched(
    client: MCPClient,
    service: str,
    batch_size: int,
    chunk_size: int,
) -> tuple[list[dict[str, Any]], int]:
    start = 1
    instances: list[dict[str, Any]] = []
    total = 0

    while True:
        raw_batch = fetch_chunked(
            lambda chunk_start, chunk_take: execute_luau(
                client,
                f"return _G.CDX_SYNC_EXPORT.getInstanceBatchChunk({json.dumps(service)}, {start}, {max(1, batch_size)}, {chunk_start}, {chunk_take})",
            ),
            chunk_size,
            f"instanceBatch:{service}:{start}",
        )
        piece = json_loads_fast(raw_batch)

        items = piece.get("items", [])
        total = int(piece.get("total", 0))
        next_start = int(piece.get("nextStart", start))

        if not isinstance(items, list):
            raise RuntimeError(f"Invalid instance batch for {service}")

        batch_added = 0
        for item in items:
            if isinstance(item, dict):
                instances.append(item)
                batch_added += 1

        if total > 0:
            done = min(len(instances), total)
            print(f"[sync]   instances {done}/{total}")

        if start > total or (batch_added == 0 and next_start <= start):
            break

        if next_start <= start:
            raise RuntimeError(f"Non-progressing instance batch for {service} at {start}")

        start = next_start
        if start > total:
            break

    return instances, total


def fetch_instance_batches_for_starts(
    command: list[str],
    project_root: Path,
    service: str,
    batch_size: int,
    chunk_size: int,
    starts: list[int],
    ws_wait_seconds: float,
    worker_id: int,
) -> tuple[dict[int, list[dict[str, Any]]], int]:
    client = initialize_export_worker(
        command=command,
        cwd=project_root,
        ws_wait_seconds=ws_wait_seconds,
    )
    try:
        execute_luau(client, f"return _G.CDX_SYNC_EXPORT.prepare({json.dumps(service)})")
        out: dict[int, list[dict[str, Any]]] = {}
        total = 0
        start_count = len(starts)
        for idx, start in enumerate(starts, start=1):
            raw_batch = fetch_chunked(
                lambda chunk_start, chunk_take, s=start: execute_luau(
                    client,
                    f"return _G.CDX_SYNC_EXPORT.getInstanceBatchChunk({json.dumps(service)}, {s}, {max(1, batch_size)}, {chunk_start}, {chunk_take})",
                ),
                chunk_size,
                f"instanceBatch:{service}:w{worker_id}:{start}",
            )
            piece = json_loads_fast(raw_batch)
            items = piece.get("items", [])
            total = max(total, int(piece.get("total", 0)))
            if not isinstance(items, list):
                raise RuntimeError(f"Invalid instance batch for {service} at start {start}")

            batch_items = [item for item in items if isinstance(item, dict)]
            out[start] = batch_items

            if start_count <= 10 or idx % 10 == 0 or idx == start_count:
                print(f"[sync]   [w{worker_id}] batch {idx}/{start_count}")

        return out, total
    finally:
        client.close()


def fetch_instances_batched_parallel(
    command: list[str],
    project_root: Path,
    service: str,
    total_instances_hint: int,
    batch_size: int,
    chunk_size: int,
    instance_workers: int,
    ws_wait_seconds: float,
) -> tuple[list[dict[str, Any]], int]:
    starts = list(range(1, max(2, total_instances_hint + 1), max(1, batch_size)))
    if not starts:
        return [], 0

    worker_count = max(1, min(instance_workers, len(starts)))
    if worker_count <= 1:
        raise RuntimeError("Parallel instance fetch requested with <=1 workers")

    assignments: list[list[int]] = [[] for _ in range(worker_count)]
    for idx, start in enumerate(starts):
        assignments[idx % worker_count].append(start)

    print(f"[sync] {service}: parallel instance export with {worker_count} workers across {len(starts)} batches")

    batches_by_start: dict[int, list[dict[str, Any]]] = {}
    total = 0
    completed_batches = 0

    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
        futures = [
            pool.submit(
                fetch_instance_batches_for_starts,
                command,
                project_root,
                service,
                batch_size,
                chunk_size,
                starts_for_worker,
                ws_wait_seconds,
                worker_idx + 1,
            )
            for worker_idx, starts_for_worker in enumerate(assignments)
            if starts_for_worker
        ]
        for future in concurrent.futures.as_completed(futures):
            worker_batches, worker_total = future.result()
            total = max(total, worker_total)
            batches_by_start.update(worker_batches)
            completed_batches += len(worker_batches)
            done_instances = min(completed_batches * batch_size, total_instances_hint)
            print(f"[sync]   instances {done_instances}/{total_instances_hint}")

    instances: list[dict[str, Any]] = []
    for start in sorted(batches_by_start.keys()):
        instances.extend(batches_by_start[start])

    return instances, (total or total_instances_hint)


def fetch_instances_batched_bridge(
    bridge: WSBridgeRuntime,
    preferred_ports: list[int],
    service: str,
    batch_size: int,
    chunk_size: int,
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
) -> tuple[list[dict[str, Any]], int]:
    start = 1
    instances: list[dict[str, Any]] = []
    total = 0

    while True:
        piece = fetch_json_payload_bridge(
            lambda chunk_start, chunk_take: bridge_call_with_failover(
                bridge=bridge,
                method="getInstanceBatchChunk",
                params={
                    "service": service,
                    "startIndex": start,
                    "maxCount": max(1, batch_size),
                    "chunkStart": chunk_start,
                    "maxLen": chunk_take,
                },
                timeout=120.0,
                preferred_ports=preferred_ports,
                adaptive_throttle=adaptive_throttle,
            )[0],
            chunk_size,
            f"bridgeInstanceBatch:{service}:{start}",
        )

        items = piece.get("items", [])
        total = int(piece.get("total", 0))
        next_start = int(piece.get("nextStart", start))
        if not isinstance(items, list):
            raise RuntimeError(f"Invalid bridge instance batch for {service}")

        batch_added = 0
        for item in items:
            if isinstance(item, dict):
                instances.append(item)
                batch_added += 1

        if total > 0:
            done = min(len(instances), total)
            print(f"[sync]   instances {done}/{total}")

        if start > total or (batch_added == 0 and next_start <= start):
            break
        if next_start <= start:
            raise RuntimeError(f"Non-progressing bridge instance batch for {service} at {start}")

        start = next_start
        if start > total:
            break

    return instances, total


def fetch_instance_batches_for_starts_bridge(
    bridge: WSBridgeRuntime,
    preferred_ports: list[int],
    service: str,
    batch_size: int,
    chunk_size: int,
    starts: list[int],
    worker_id: int,
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
) -> tuple[dict[int, list[dict[str, Any]]], int]:
    out: dict[int, list[dict[str, Any]]] = {}
    total = 0
    start_count = len(starts)

    for idx, start in enumerate(starts, start=1):
        piece = fetch_json_payload_bridge(
            lambda chunk_start, chunk_take, s=start: bridge_call_with_failover(
                bridge=bridge,
                method="getInstanceBatchChunk",
                params={
                    "service": service,
                    "startIndex": s,
                    "maxCount": max(1, batch_size),
                    "chunkStart": chunk_start,
                    "maxLen": chunk_take,
                },
                timeout=120.0,
                preferred_ports=preferred_ports,
                adaptive_throttle=adaptive_throttle,
            )[0],
            chunk_size,
            f"bridgeInstanceBatch:{service}:w{worker_id}:{start}",
        )
        items = piece.get("items", [])
        total = max(total, int(piece.get("total", 0)))
        if not isinstance(items, list):
            raise RuntimeError(f"Invalid bridge instance batch for {service} at start {start}")

        out[start] = [item for item in items if isinstance(item, dict)]

        if start_count <= 10 or idx % 10 == 0 or idx == start_count:
            print(f"[sync]   [bridge-w{worker_id}] batch {idx}/{start_count}")

    return out, total


def fetch_instances_batched_parallel_bridge(
    bridge: WSBridgeRuntime,
    ports: list[int],
    service: str,
    total_instances_hint: int,
    batch_size: int,
    chunk_size: int,
    instance_workers: int,
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
) -> tuple[list[dict[str, Any]], int]:
    starts = list(range(1, max(2, total_instances_hint + 1), max(1, batch_size)))
    if not starts:
        return [], 0

    worker_count = max(1, min(instance_workers, len(ports), len(starts)))
    if worker_count <= 1:
        raise RuntimeError("Parallel bridge instance fetch requested with <=1 workers")

    assignments: list[list[int]] = [[] for _ in range(worker_count)]
    worker_ports = ports[:worker_count]
    for idx, start in enumerate(starts):
        assignments[idx % worker_count].append(start)

    print(
        f"[sync] {service}: bridge parallel instance export with {worker_count} workers "
        f"across {len(starts)} batches"
    )

    batches_by_start: dict[int, list[dict[str, Any]]] = {}
    total = 0
    completed_batches = 0

    with concurrent.futures.ThreadPoolExecutor(max_workers=worker_count) as pool:
        futures = [
            pool.submit(
                fetch_instance_batches_for_starts_bridge,
                bridge,
                [worker_ports[worker_idx], *[p for p in ports if p != worker_ports[worker_idx]]],
                service,
                batch_size,
                chunk_size,
                starts_for_worker,
                worker_idx + 1,
                adaptive_throttle,
            )
            for worker_idx, starts_for_worker in enumerate(assignments)
            if starts_for_worker
        ]
        for future in concurrent.futures.as_completed(futures):
            worker_batches, worker_total = future.result()
            total = max(total, worker_total)
            batches_by_start.update(worker_batches)
            completed_batches += len(worker_batches)
            done_instances = min(completed_batches * batch_size, total_instances_hint)
            print(f"[sync]   instances {done_instances}/{total_instances_hint}")

    instances: list[dict[str, Any]] = []
    for start in sorted(batches_by_start.keys()):
        instances.extend(batches_by_start[start])

    return instances, (total or total_instances_hint)


def merge_script_sources(snapshot: dict[str, Any], sources: dict[str, str]) -> None:
    instances = snapshot.get("instances")
    if not isinstance(instances, list):
        raise RuntimeError("Snapshot has invalid instances")

    by_path: dict[str, dict[str, Any]] = {}
    for inst in instances:
        if isinstance(inst, dict):
            path = inst.get("path")
            if isinstance(path, str):
                by_path[path] = inst

    for path, src in sources.items():
        inst = by_path.get(path)
        if not inst:
            continue
        props = inst.get("properties")
        if not isinstance(props, dict):
            props = {}
            inst["properties"] = props
        props["Source"] = src


def write_snapshot_service(
    snapshot_dir: Path,
    service: str,
    snapshot: dict[str, Any],
    instance_chunk_size: int,
) -> None:
    out_path = snapshot_dir / f"{service}.json"
    instances = snapshot.get("instances")
    script_count = 0
    metadata = snapshot.get("metadata")
    if isinstance(metadata, dict):
        script_count_value = metadata.get("scriptCount")
        if isinstance(script_count_value, int):
            script_count = script_count_value

    chunk_size = max(0, int(instance_chunk_size))
    if not isinstance(instances, list) or chunk_size <= 0 or len(instances) <= chunk_size:
        out_path.write_text(json_dumps_compact(snapshot), encoding="utf-8")
        print(f"[sync] wrote {out_path} ({script_count} scripts)")
        return

    # Remove stale chunk files for this service before writing the new manifest/chunks.
    for old_chunk in snapshot_dir.glob(f"{service}.instances.*.json"):
        try:
            old_chunk.unlink()
        except Exception:
            pass

    total_instances = len(instances)
    manifest_instances: list[Any] = []
    chunk_start_offset = 0
    if total_instances > 0 and isinstance(instances[0], dict):
        # Keep the service root in the manifest so import can discover roots immediately.
        manifest_instances = [instances[0]]
        chunk_start_offset = 1

    chunk_source = instances[chunk_start_offset:]
    if len(chunk_source) <= chunk_size:
        out_path.write_text(json_dumps_compact(snapshot), encoding="utf-8")
        print(f"[sync] wrote {out_path} ({script_count} scripts)")
        return

    total_chunks = (len(chunk_source) + chunk_size - 1) // chunk_size
    chunk_entries: list[dict[str, Any]] = []

    for chunk_idx in range(total_chunks):
        start = chunk_idx * chunk_size
        end = min(len(chunk_source), start + chunk_size)
        chunk_instances = chunk_source[start:end]
        chunk_name = f"{service}.instances.{chunk_idx + 1:04d}.json"
        chunk_path = snapshot_dir / chunk_name
        chunk_path.write_text(json_dumps_compact(chunk_instances), encoding="utf-8")
        chunk_entries.append(
            {
                "file": chunk_name,
                "count": len(chunk_instances),
                "startIndex": chunk_start_offset + start + 1,
            }
        )
        progress_idx = chunk_idx + 1
        if total_chunks <= 10 or progress_idx % 10 == 0 or progress_idx == total_chunks:
            print(
                f"[sync]   {service}: wrote chunk {progress_idx}/{total_chunks} "
                f"({len(chunk_instances)} instances)"
            )

    manifest = dict(snapshot)
    manifest["instances"] = manifest_instances
    manifest["instanceChunks"] = chunk_entries

    manifest_meta = manifest.get("metadata")
    if isinstance(manifest_meta, dict):
        manifest_meta["instanceChunked"] = True
        manifest_meta["instanceChunkSize"] = chunk_size
        manifest_meta["instanceChunkCount"] = total_chunks

    out_path.write_text(json_dumps_compact(manifest), encoding="utf-8")
    print(f"[sync] wrote {out_path} ({script_count} scripts, {total_chunks} chunks)")


def delete_snapshot_service_files(snapshot_dir: Path, service: str) -> None:
    service_file = snapshot_dir / f"{service}.json"
    if service_file.exists():
        try:
            service_file.unlink()
        except Exception:
            pass
    for chunk_file in snapshot_dir.glob(f"{service}.instances.*.json"):
        try:
            chunk_file.unlink()
        except Exception:
            pass


def run_export(
    client: MCPClient,
    services: list[str],
    snapshot_dir: Path,
    chunk_size: int,
    instance_workers: int,
    command: list[str],
    project_root: Path,
    ws_wait_seconds: float,
    export_all_properties: bool = False,
    snapshot_instance_chunk_size: int = 0,
    on_service_snapshot: Callable[[str, dict[str, Any]], None] | None = None,
) -> None:
    ensure_export_runtime_loaded(client)
    execute_luau(
        client,
        "return _G.CDX_SYNC_EXPORT.setExportOptions({ exportAllProperties = "
        + ("true" if export_all_properties else "false")
        + " })",
    )

    for service in services:
        service_done = False
        last_error: Exception | None = None
        for attempt in range(1, 4):
            try:
                ensure_export_runtime_loaded(client)
                print(f"[sync] exporting {service}... (attempt {attempt}/3)")
                prepare_started = time.time()
                prep_raw = execute_luau(client, f"return _G.CDX_SYNC_EXPORT.prepare({json.dumps(service)})")
                prep = json_loads_fast(prep_raw)
                script_count = int(prep.get("scriptCount", 0))
                instance_count = int(prep.get("instanceCount", 0))
                prepare_elapsed = time.time() - prepare_started
                print(
                    f"[sync] prepared {service}: {instance_count} instances, "
                    f"{script_count} scripts in {prepare_elapsed:.2f}s"
                )

                class_defaults_raw = fetch_chunked(
                    lambda start, size: execute_luau(
                        client,
                        f"return _G.CDX_SYNC_EXPORT.getClassDefaultsChunk({json.dumps(service)}, {start}, {size})",
                    ),
                    chunk_size,
                    f"classDefaults:{service}",
                )
                class_defaults = json_loads_fast(class_defaults_raw)
                if isinstance(class_defaults, list) and len(class_defaults) == 0:
                    class_defaults = {}
                if not isinstance(class_defaults, dict):
                    raise RuntimeError(f"Invalid class default payload for {service}")

                if instance_count >= 100000:
                    instance_batch_size = 2400
                elif instance_count >= 50000:
                    instance_batch_size = 1800
                elif instance_count >= 20000:
                    instance_batch_size = 1200
                else:
                    instance_batch_size = 600
                print(f"[sync] {service}: instance batch size {instance_batch_size}")
                if instance_workers > 1 and instance_count > instance_batch_size:
                    try:
                        instances, total_instances = fetch_instances_batched_parallel(
                            command=command,
                            project_root=project_root,
                            service=service,
                            total_instances_hint=instance_count,
                            batch_size=instance_batch_size,
                            chunk_size=chunk_size,
                            instance_workers=instance_workers,
                            ws_wait_seconds=ws_wait_seconds,
                        )
                    except Exception as exc:
                        print(f"[sync] warning: parallel instance export failed for {service}: {exc}; falling back to single-worker mode")
                        instances, total_instances = fetch_instances_batched(
                            client=client,
                            service=service,
                            batch_size=instance_batch_size,
                            chunk_size=chunk_size,
                        )
                else:
                    instances, total_instances = fetch_instances_batched(
                        client=client,
                        service=service,
                        batch_size=instance_batch_size,
                        chunk_size=chunk_size,
                    )
                if total_instances and len(instances) != total_instances:
                    print(
                        f"[sync] warning: instance count mismatch for {service} "
                        f"(received={len(instances)}, expected={total_instances})"
                    )

                service_path = prep.get("rootPath")
                if not isinstance(service_path, str) or not service_path:
                    service_path = f"game.{service}"

                service_class = prep.get("rootClassName")
                if not isinstance(service_class, str) or not service_class:
                    service_class = service

                service_name = prep.get("rootName")
                if not isinstance(service_name, str) or not service_name:
                    service_name = service

                generated_at_unix = prep.get("generatedAtUnix")
                if not isinstance(generated_at_unix, int):
                    generated_at_unix = int(time.time())

                snapshot: dict[str, Any] = {
                    "metadata": {
                        "generatedAtUnix": generated_at_unix,
                        "serviceName": service,
                        "instanceCount": len(instances),
                        "scriptCount": script_count,
                        "sourceChunked": True,
                    },
                    "classDefaults": class_defaults,
                    "services": [
                        {
                            "name": service_name,
                            "className": service_class,
                            "path": service_path,
                        }
                    ],
                    "instances": instances,
                }

                paths_raw = fetch_chunked(
                    lambda start, size: execute_luau(
                        client,
                        f"return _G.CDX_SYNC_EXPORT.getScriptPathsChunk({json.dumps(service)}, {start}, {size})",
                    ),
                    chunk_size,
                    f"scriptPaths:{service}",
                )
                script_paths = json_loads_fast(paths_raw)
                if not isinstance(script_paths, list):
                    raise RuntimeError(f"Invalid script path list for {service}")

                cleaned_paths = [p for p in script_paths if isinstance(p, str)]
                sources: dict[str, str] = {}

                # Single active MCP pipeline avoids repeating full prepare() per worker client.
                # This is materially faster for large services such as Workspace.
                for idx, path in enumerate(cleaned_paths, start=1):
                    print(f"[sync]   script {idx}/{len(cleaned_paths)}")
                    src = fetch_chunked(
                        lambda start, size, p=path: execute_luau(
                            client,
                            f"return _G.CDX_SYNC_EXPORT.getSourceChunk({json.dumps(service)}, {json.dumps(p)}, {start}, {size})",
                        ),
                        chunk_size,
                        f"source:{service}:{path}",
                    )
                    sources[path] = src

                merge_script_sources(snapshot, sources)

                if on_service_snapshot is not None:
                    on_service_snapshot(service, snapshot)
                else:
                    write_snapshot_service(
                        snapshot_dir=snapshot_dir,
                        service=service,
                        snapshot=snapshot,
                        instance_chunk_size=snapshot_instance_chunk_size,
                    )
                try:
                    execute_luau(client, f"return _G.CDX_SYNC_EXPORT.release({json.dumps(service)})")
                except Exception:
                    pass
                service_done = True
                break
            except Exception as exc:
                last_error = exc
                if attempt >= 3:
                    break
                print(f"[sync] warning: export failed for {service}: {exc}; attempting MCP/Studio rebind")
                ensure_active_studio(client, wait_seconds=ws_wait_seconds)
                ensure_export_runtime_loaded(client)
                time.sleep(1.0)

        if not service_done and last_error is not None:
            raise last_error


def run_export_ws(
    bridge: WSBridgeRuntime,
    bridge_ports: list[int],
    services: list[str],
    snapshot_dir: Path,
    chunk_size: int,
    instance_workers: int,
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
    export_all_properties: bool = False,
    snapshot_instance_chunk_size: int = 0,
    on_service_snapshot: Callable[[str, dict[str, Any]], None] | None = None,
) -> None:
    if not bridge_ports:
        raise RuntimeError("No bridge ports configured")

    configure_bridge_export_options(
        bridge=bridge,
        bridge_ports=bridge_ports,
        export_all_properties=export_all_properties,
    )

    for service in services:
        service_done = False
        last_error: Exception | None = None
        for attempt in range(1, 4):
            try:
                print(f"[sync] exporting {service}... (attempt {attempt}/3)")
                prepare_started = time.time()
                prep, prep_port = bridge_call_with_failover(
                    bridge=bridge,
                    method="prepare",
                    params={"service": service},
                    timeout=180.0,
                    preferred_ports=bridge_ports,
                    adaptive_throttle=adaptive_throttle,
                )
                if not isinstance(prep, dict):
                    raise RuntimeError(f"Invalid bridge prepare payload for {service}")

                script_count = int(prep.get("scriptCount", 0))
                instance_count = int(prep.get("instanceCount", 0))
                prepare_elapsed = time.time() - prepare_started
                print(
                    f"[sync] prepared {service}: {instance_count} instances, "
                    f"{script_count} scripts in {prepare_elapsed:.2f}s"
                )
                preferred_service_ports = [prep_port, *[p for p in bridge_ports if p != prep_port]]

                class_defaults = fetch_json_payload_bridge(
                    lambda start, size: bridge_call_with_failover(
                        bridge=bridge,
                        method="getClassDefaultsChunk",
                        params={"service": service, "startIndex": start, "maxLen": size},
                        timeout=120.0,
                        preferred_ports=preferred_service_ports,
                        adaptive_throttle=adaptive_throttle,
                    )[0],
                    chunk_size,
                    f"bridgeClassDefaults:{service}",
                )
                if isinstance(class_defaults, list) and len(class_defaults) == 0:
                    class_defaults = {}
                if not isinstance(class_defaults, dict):
                    raise RuntimeError(f"Invalid class default payload for {service}")

                if instance_count >= 100000:
                    instance_batch_size = 2400
                elif instance_count >= 50000:
                    instance_batch_size = 1800
                elif instance_count >= 20000:
                    instance_batch_size = 1200
                else:
                    instance_batch_size = 600
                print(f"[sync] {service}: instance batch size {instance_batch_size}")

                available_parallel_ports = _ordered_connected_bridge_ports(bridge, preferred_service_ports)
                if (
                    instance_workers > 1
                    and instance_count > instance_batch_size
                    and len(available_parallel_ports) > 1
                ):
                    active_parallel_workers = max(1, min(instance_workers, len(available_parallel_ports)))
                    parallel_batch_size = max(200, instance_batch_size // active_parallel_workers)
                    if parallel_batch_size != instance_batch_size:
                        print(
                            f"[sync] {service}: parallel batch size tuned from {instance_batch_size} "
                            f"to {parallel_batch_size} for {active_parallel_workers} workers"
                        )
                    try:
                        instances, total_instances = fetch_instances_batched_parallel_bridge(
                            bridge=bridge,
                            ports=available_parallel_ports,
                            service=service,
                            total_instances_hint=instance_count,
                            batch_size=parallel_batch_size,
                            chunk_size=chunk_size,
                            instance_workers=instance_workers,
                            adaptive_throttle=adaptive_throttle,
                        )
                    except Exception as exc:
                        print(
                            f"[sync] warning: bridge parallel instance export failed for {service}: {exc}; "
                            "falling back to single-worker mode"
                        )
                        instances, total_instances = fetch_instances_batched_bridge(
                            bridge=bridge,
                            preferred_ports=preferred_service_ports,
                            service=service,
                            batch_size=instance_batch_size,
                            chunk_size=chunk_size,
                            adaptive_throttle=adaptive_throttle,
                        )
                else:
                    instances, total_instances = fetch_instances_batched_bridge(
                        bridge=bridge,
                        preferred_ports=preferred_service_ports,
                        service=service,
                        batch_size=instance_batch_size,
                        chunk_size=chunk_size,
                        adaptive_throttle=adaptive_throttle,
                    )

                if total_instances and len(instances) != total_instances:
                    print(
                        f"[sync] warning: instance count mismatch for {service} "
                        f"(received={len(instances)}, expected={total_instances})"
                    )

                service_path = prep.get("rootPath")
                if not isinstance(service_path, str) or not service_path:
                    service_path = f"game.{service}"

                service_class = prep.get("rootClassName")
                if not isinstance(service_class, str) or not service_class:
                    service_class = service

                service_name = prep.get("rootName")
                if not isinstance(service_name, str) or not service_name:
                    service_name = service

                generated_at_unix = prep.get("generatedAtUnix")
                if not isinstance(generated_at_unix, int):
                    generated_at_unix = int(time.time())

                snapshot: dict[str, Any] = {
                    "metadata": {
                        "generatedAtUnix": generated_at_unix,
                        "serviceName": service,
                        "instanceCount": len(instances),
                        "scriptCount": script_count,
                        "sourceChunked": True,
                    },
                    "classDefaults": class_defaults,
                    "services": [
                        {
                            "name": service_name,
                            "className": service_class,
                            "path": service_path,
                        }
                    ],
                    "instances": instances,
                }

                script_paths = fetch_json_payload_bridge(
                    lambda start, size: bridge_call_with_failover(
                        bridge=bridge,
                        method="getScriptPathsChunk",
                        params={"service": service, "startIndex": start, "maxLen": size},
                        timeout=120.0,
                        preferred_ports=preferred_service_ports,
                        adaptive_throttle=adaptive_throttle,
                    )[0],
                    chunk_size,
                    f"bridgeScriptPaths:{service}",
                )
                if not isinstance(script_paths, list):
                    raise RuntimeError(f"Invalid script path list for {service}")

                cleaned_paths = [p for p in script_paths if isinstance(p, str)]
                sources: dict[str, str] = {}
                for idx, path in enumerate(cleaned_paths, start=1):
                    print(f"[sync]   script {idx}/{len(cleaned_paths)}")
                    src = fetch_chunked_bridge(
                        lambda start, size, p=path: bridge_call_with_failover(
                            bridge=bridge,
                            method="getSourceChunk",
                            params={
                                "service": service,
                                "instancePath": p,
                                "startIndex": start,
                                "maxLen": size,
                            },
                            timeout=120.0,
                            preferred_ports=preferred_service_ports,
                            adaptive_throttle=adaptive_throttle,
                        )[0],
                        chunk_size,
                        f"bridgeSource:{service}:{path}",
                    )
                    sources[path] = src

                merge_script_sources(snapshot, sources)

                if on_service_snapshot is not None:
                    on_service_snapshot(service, snapshot)
                else:
                    write_snapshot_service(
                        snapshot_dir=snapshot_dir,
                        service=service,
                        snapshot=snapshot,
                        instance_chunk_size=snapshot_instance_chunk_size,
                    )

                try:
                    bridge_call_with_failover(
                        bridge=bridge,
                        method="release",
                        params={"service": service},
                        timeout=30.0,
                        preferred_ports=preferred_service_ports,
                        retries=3,
                        adaptive_throttle=adaptive_throttle,
                    )
                except Exception:
                    pass

                service_done = True
                break
            except Exception as exc:
                last_error = exc
                if attempt >= 3:
                    break
                print(f"[sync] warning: bridge export failed for {service}: {exc}; retrying")
                time.sleep(1.0)

        if not service_done and last_error is not None:
            raise last_error


def update_editor_icon_settings(project_root: Path) -> None:
    vscode_dir = project_root / ".vscode"
    settings_path = vscode_dir / "settings.json"
    vscode_dir.mkdir(parents=True, exist_ok=True)

    settings: dict[str, Any] = {}
    if settings_path.exists():
        raw = settings_path.read_text(encoding="utf-8").strip()
        if raw:
            try:
                parsed = json_loads_fast(raw)
                if isinstance(parsed, dict):
                    settings = parsed
            except JSON_DECODE_ERRORS:
                # If user has invalid JSON, keep existing file untouched and skip icon updates.
                return

    material_assoc = settings.get("material-icon-theme.folders.associations")
    if not isinstance(material_assoc, dict):
        material_assoc = {}
        settings["material-icon-theme.folders.associations"] = material_assoc

    # These icon ids are verified against Material Icon Theme 5.33.x.
    material_icon_map = {
        "Workspace": "src",
        "workspace": "src",
        "Players": "client",
        "players": "client",
        "Lighting": "animation",
        "lighting": "animation",
        "MaterialService": "config",
        "materialservice": "config",
        "ReplicatedFirst": "app",
        "replicatedfirst": "app",
        "ReplicatedStorage": "database",
        "replicatedstorage": "database",
        "ServerScriptService": "server",
        "serverscriptservice": "server",
        "ServerStorage": "database",
        "serverstorage": "database",
        "StarterGui": "ui",
        "startergui": "ui",
        "StarterPack": "packages",
        "starterpack": "packages",
        "StarterPlayer": "client",
        "starterplayer": "client",
        "src": "src",
        "tools": "tools",
        "scripts": "scripts",
        "assets": "images",
        "shared": "lib",
        "snapshots": "database",
    }

    for folder_name, icon_name in material_icon_map.items():
        material_assoc[folder_name] = icon_name

    # Keep vscode-icons to ids that are known to exist in vscode-icons.
    vsicons_icon_map = {
        "Workspace": "src",
        "workspace": "src",
        "Players": "client",
        "players": "client",
        "Lighting": "theme",
        "lighting": "theme",
        "MaterialService": "config",
        "materialservice": "config",
        "ReplicatedFirst": "app",
        "replicatedfirst": "app",
        "ReplicatedStorage": "shared",
        "replicatedstorage": "shared",
        "ServerScriptService": "server",
        "serverscriptservice": "server",
        "ServerStorage": "private",
        "serverstorage": "private",
        "StarterGui": "public",
        "startergui": "public",
        "StarterPack": "plugin",
        "starterpack": "plugin",
        "StarterPlayer": "client",
        "starterplayer": "client",
        "src": "src",
        "tools": "tools",
        "scripts": "src",
        "assets": "images",
        "shared": "shared",
        "snapshots": "log",
    }

    # vscode-icons uses an array format, not an object map:
    # "vsicons.associations.folders": [{ "icon": "src", "extensions": ["Workspace"], "format": "svg" }]
    raw_vsicons = settings.get("vsicons.associations.folders")
    vsicons_by_extension: dict[str, str] = {}
    if isinstance(raw_vsicons, list):
        for entry in raw_vsicons:
            if not isinstance(entry, dict):
                continue
            icon = entry.get("icon")
            extensions = entry.get("extensions")
            if not isinstance(icon, str) or not isinstance(extensions, list):
                continue
            for ext in extensions:
                if isinstance(ext, str) and ext:
                    vsicons_by_extension[ext] = icon
    elif isinstance(raw_vsicons, dict):
        # Backward compatibility with the old incorrect object format.
        for ext, icon in raw_vsicons.items():
            if isinstance(ext, str) and isinstance(icon, str):
                vsicons_by_extension[ext] = icon

    for folder_name, icon_name in vsicons_icon_map.items():
        vsicons_by_extension[folder_name] = icon_name

    settings["vsicons.associations.folders"] = [
        {"icon": icon, "extensions": [ext], "format": "svg"}
        for ext, icon in sorted(vsicons_by_extension.items(), key=lambda kv: kv[0].lower())
    ]

    settings_path.write_text(json_dumps_pretty(settings) + "\n", encoding="utf-8")


def _http_get_text(url: str, timeout: float = 30.0) -> str:
    req = urllib.request.Request(
        url,
        headers={
            "User-Agent": "mcp-sync-export/1.0",
            "Accept": "application/json, text/plain, */*",
        },
    )
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        return resp.read().decode("utf-8", errors="replace")


def _load_studio_api_dump(project_root: Path) -> dict[str, Any]:
    version_url = "https://setup.rbxcdn.com/versionQTStudio"
    version = _http_get_text(version_url, timeout=15.0).strip()
    if not version:
        raise RuntimeError("Failed to resolve Roblox Studio version for API dump")

    cache_dir = project_root / "tools" / ".cache"
    cache_dir.mkdir(parents=True, exist_ok=True)
    dump_path = cache_dir / f"{version}-API-Dump.json"
    if dump_path.exists():
        raw = dump_path.read_text(encoding="utf-8")
    else:
        dump_url = f"https://setup.rbxcdn.com/{version}-API-Dump.json"
        raw = _http_get_text(dump_url, timeout=60.0)
        dump_path.write_text(raw, encoding="utf-8")

    parsed = json_loads_fast(raw)
    if not isinstance(parsed, dict):
        raise RuntimeError("Invalid API dump payload")
    return parsed


def _build_class_property_candidates(api_dump: dict[str, Any]) -> dict[str, list[str]]:
    classes_node = api_dump.get("Classes")
    if not isinstance(classes_node, list):
        raise RuntimeError("API dump missing Classes list")

    def normalize_property_name(name: str) -> str:
        return name.strip()

    def property_key(name: str) -> str:
        return name.casefold()

    def should_skip_property(name: str) -> bool:
        key = property_key(name)
        return key == "source" or key == "robloxlocked"

    direct_props: dict[str, list[str]] = {}
    superclass: dict[str, str] = {}

    for cls in classes_node:
        if not isinstance(cls, dict):
            continue
        class_name = cls.get("Name")
        if not isinstance(class_name, str) or not class_name:
            continue

        super_name = cls.get("Superclass")
        if isinstance(super_name, str) and super_name:
            superclass[class_name] = super_name

        members = cls.get("Members")
        props: list[str] = []
        seen_props: set[str] = set()
        if isinstance(members, list):
            for member in members:
                if not isinstance(member, dict):
                    continue
                if member.get("MemberType") != "Property":
                    continue
                name = member.get("Name")
                if not isinstance(name, str):
                    continue
                normalized = normalize_property_name(name)
                if not normalized:
                    continue
                key = property_key(normalized)
                if should_skip_property(normalized) or key in seen_props:
                    continue
                seen_props.add(key)
                props.append(normalized)
        direct_props[class_name] = props

    resolved: dict[str, list[str]] = {}
    stack: set[str] = set()

    def resolve_props(class_name: str) -> list[str]:
        cached = resolved.get(class_name)
        if cached is not None:
            return cached
        if class_name in stack:
            return []

        stack.add(class_name)
        merged: list[str] = []
        seen: set[str] = set()

        parent = superclass.get(class_name)
        if parent:
            for prop in resolve_props(parent):
                key = property_key(prop)
                if key not in seen:
                    seen.add(key)
                    merged.append(prop)

        for prop in direct_props.get(class_name, []):
            key = property_key(prop)
            if key not in seen:
                seen.add(key)
                merged.append(prop)

        stack.remove(class_name)
        resolved[class_name] = merged
        return merged

    for class_name in direct_props.keys():
        resolve_props(class_name)

    return {k: v for k, v in resolved.items() if v}


def configure_bridge_property_candidates(
    bridge: WSBridgeRuntime,
    bridge_ports: list[int],
    class_candidates: dict[str, list[str]],
    adaptive_throttle: AdaptiveBridgeThrottle | None = None,
) -> None:
    if not class_candidates:
        return

    print(f"[sync] configuring bridge property candidates ({len(class_candidates)} classes)")
    result, _ = bridge_call_with_failover(
        bridge=bridge,
        method="configurePropertyCandidates",
        params={"classes": class_candidates},
        timeout=240.0,
        preferred_ports=bridge_ports,
        retries=5,
        adaptive_throttle=adaptive_throttle,
    )
    if isinstance(result, dict):
        classes = result.get("classCount")
        props = result.get("propertyCount")
        if isinstance(classes, int) and isinstance(props, int):
            print(f"[sync] bridge property candidates ready: classes={classes}, properties={props}")


def configure_bridge_export_options(
    bridge: WSBridgeRuntime,
    bridge_ports: list[int],
    export_all_properties: bool,
) -> None:
    ports = _ordered_connected_bridge_ports(bridge, bridge_ports)
    if not ports:
        return
    configured = 0
    for port in ports:
        try:
            bridge.call(
                port=port,
                method="setExportOptions",
                params={"exportAllProperties": bool(export_all_properties)},
                timeout=30.0,
            )
            configured += 1
        except Exception as exc:
            print(f"[sync] warning: failed to configure export options on bridge port {port}: {exc}")
    if configured > 0:
        mode = "all-properties" if export_all_properties else "filtered-properties"
        print(f"[sync] bridge export mode: {mode} (configured ports={configured})")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Export Roblox services via MCP with chunk-safe script source retrieval")
    parser.add_argument("--transport", choices=["mcp", "ws"], default="ws")
    parser.add_argument("--config", type=Path, default=Path.home() / ".codex" / "config.toml")
    parser.add_argument("--server", default="Roblox_Studio")
    parser.add_argument("--project-root", type=Path, default=Path.cwd())
    parser.add_argument("--snapshot-dir", type=Path, default=Path("snapshots"))
    parser.add_argument("--services", default=",".join(DEFAULT_SERVICES))
    parser.add_argument("--chunk-size", type=int, default=12000)
    parser.add_argument("--source-workers", type=int, default=0)
    parser.add_argument("--ws-wait-seconds", type=float, default=15.0)
    parser.add_argument("--bridge-host", default="127.0.0.1")
    parser.add_argument("--bridge-ports", default="8781,8782,8783")
    parser.add_argument("--bridge-wait-seconds", type=float, default=60.0)
    parser.add_argument("--adaptive-throttle", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--throttle-low-fps", type=float, default=35.0)
    parser.add_argument("--throttle-high-fps", type=float, default=50.0)
    parser.add_argument("--throttle-max-delay-ms", type=float, default=40.0)
    parser.add_argument("--throttle-probe-interval-ms", type=float, default=500.0)
    parser.add_argument("--snapshot-instance-chunk-size", type=int, default=2000)
    parser.add_argument("--export-all-properties", action=argparse.BooleanOptionalAction, default=False)
    parser.add_argument("--property-candidates-source", choices=["api_dump", "builtin"], default="api_dump")
    parser.add_argument("--no-update-editor-icons", action="store_true")
    parser.add_argument("--run-import", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--import-mode", choices=["direct", "snapshot"], default="direct")
    parser.add_argument("--import-progress-every", type=int, default=5000)
    parser.add_argument("--import-compact-meta-json", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--import-skip-default-filtering", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--import-cli", type=Path, default=None)
    parser.add_argument("--interactive", action="store_true")
    return parser.parse_args()


def apply_interactive_options(args: argparse.Namespace) -> argparse.Namespace:
    print("MCP -> project sync interactive mode")
    print("Press Enter to accept defaults.")

    if tk is None or filedialog is None:
        raise RuntimeError("Folder picker UI is unavailable on this Python runtime.")

    current_root = str(args.project_root.resolve())
    root = tk.Tk()
    root.withdraw()
    root.attributes("-topmost", True)
    chosen_root = filedialog.askdirectory(
        initialdir=current_root,
        title="Select project folder for sync output",
    )
    root.destroy()

    if not chosen_root:
        raise SystemExit("Folder selection canceled. Exiting.")

    args.project_root = Path(chosen_root)
    print(f"Project folder: {args.project_root}")

    args.snapshot_dir = None
    args.run_import = True
    args.source_workers = 0
    print("Snapshot folder: temporary cache (auto-deleted)")
    print("Import after export: enabled")

    args.run_import = True

    return args


def run_import_snapshot(
    project_root: Path,
    snapshot_dir: Path,
    progress_every: int = 1000,
    compact_meta_json: bool = True,
    skip_default_filtering: bool = True,
    services: list[str] | None = None,
    write_project_file: bool = True,
    import_cli: Path | None = None,
) -> None:
    if isinstance(import_cli, Path):
        cli_path = import_cli.resolve()
        if not cli_path.exists():
            raise RuntimeError(f"Import CLI not found: {cli_path}")
        cmd = [
            str(cli_path),
            "import-snapshots",
            "--snapshot-dir",
            str(snapshot_dir),
            "--project-root",
            str(project_root),
        ]
        if compact_meta_json:
            cmd.append("--compact-meta-json")
        if not write_project_file:
            cmd.append("--no-project-write")
        if services:
            selected = [str(s).strip() for s in services if isinstance(s, str) and s.strip()]
            if selected:
                cmd.extend(["--services", ",".join(selected)])
        proc = subprocess.run(cmd, cwd=str(project_root))
        if proc.returncode != 0:
            raise RuntimeError(f"Import CLI failed with exit code {proc.returncode}")
        return

    local_script = (project_root / "tools" / "import_snapshot.ps1").resolve()
    bundled_script = Path(__file__).resolve().with_name("import_snapshot.ps1")
    script_path = local_script if local_script.exists() else bundled_script
    if not script_path.exists():
        raise RuntimeError(f"Missing importer script: {script_path}")

    cmd = [
        "powershell",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
        str(script_path),
        "-SnapshotDir",
        str(snapshot_dir),
        "-ProjectRoot",
        str(project_root),
        "-ProgressEvery",
        str(max(1, int(progress_every))),
    ]
    if compact_meta_json:
        cmd.append("-CompactMetaJson")
    if skip_default_filtering:
        cmd.append("-SkipDefaultFiltering")
    if services:
        cmd.append("-Services")
        cmd.extend([str(s) for s in services if isinstance(s, str) and s.strip()])
    if not write_project_file:
        cmd.append("-NoProjectWrite")
    proc = subprocess.run(cmd, cwd=str(project_root))
    if proc.returncode != 0:
        raise RuntimeError(f"Import failed with exit code {proc.returncode}")


def run_import_service_payload(
    project_root: Path,
    service: str,
    snapshot: dict[str, Any],
    compact_meta_json: bool = True,
    write_project_file: bool = False,
    import_cli: Path | None = None,
) -> None:
    if not isinstance(import_cli, Path):
        raise RuntimeError("import_cli is required for direct payload import")

    cli_path = import_cli.resolve()
    if not cli_path.exists():
        raise RuntimeError(f"Import CLI not found: {cli_path}")

    cmd = [
        str(cli_path),
        "import-service",
        "--project-root",
        str(project_root),
        "--service",
        str(service),
    ]
    if compact_meta_json:
        cmd.append("--compact-meta-json")
    if not write_project_file:
        cmd.append("--no-project-write")

    payload = json_dumps_compact(snapshot).encode("utf-8")
    proc = subprocess.run(cmd, input=payload, cwd=str(project_root))
    if proc.returncode != 0:
        raise RuntimeError(f"Direct service import failed with exit code {proc.returncode}")


def write_generated_project(project_root: Path, services: list[str]) -> None:
    tree: dict[str, Any] = {"$className": "DataModel"}
    for service in services:
        tree[service] = {"$path": f"src/{service}"}
    project = {
        "name": "projest",
        "tree": tree,
    }
    project_path = project_root / "default.project.generated.json"
    project_path.write_text(json_dumps_pretty(project) + "\n", encoding="utf-8")
    print(f"[sync] wrote {project_path}")


def parse_bridge_ports(raw: str) -> list[int]:
    ports: list[int] = []
    for piece in raw.split(","):
        text = piece.strip()
        if not text:
            continue
        value = int(text)
        if value <= 0 or value > 65535:
            raise RuntimeError(f"Invalid bridge port: {value}")
        ports.append(value)
    if not ports:
        raise RuntimeError("No bridge ports configured")
    return ports


def main() -> int:
    args = parse_args()
    if args.interactive:
        args = apply_interactive_options(args)
    print(f"[sync] json backend: {JSON_BACKEND}")

    services = [s.strip() for s in args.services.split(",") if s.strip()]
    if not services:
        raise RuntimeError("No services provided")

    project_root = args.project_root.resolve()
    project_root.mkdir(parents=True, exist_ok=True)
    direct_import_mode = bool(args.run_import) and args.import_mode == "direct"
    direct_import_via_cli = direct_import_mode and isinstance(args.import_cli, Path)
    temp_snapshot_dir: Path | None = None
    if direct_import_mode:
        if direct_import_via_cli:
            snapshot_dir = project_root / ".sync_unused"
            print("[sync] import mode: direct (service-by-service apply, no snapshot cache)")
        else:
            temp_snapshot_dir = Path(tempfile.mkdtemp(prefix="mcp_sync_service_cache_"))
            snapshot_dir = temp_snapshot_dir
            print("[sync] import mode: direct (service-by-service apply)")
            print(f"[sync] service cache dir (temp): {snapshot_dir}")
    else:
        if args.snapshot_dir is None:
            temp_snapshot_dir = Path(tempfile.mkdtemp(prefix="mcp_sync_snapshots_"))
            snapshot_dir = temp_snapshot_dir
        else:
            snapshot_dir = (project_root / args.snapshot_dir).resolve()
            snapshot_dir.mkdir(parents=True, exist_ok=True)
        if temp_snapshot_dir is not None:
            print(f"[sync] snapshot cache dir (temp): {snapshot_dir}")
        else:
            print(f"[sync] snapshot dir: {snapshot_dir}")

    def on_service_snapshot(service: str, snapshot: dict[str, Any]) -> None:
        if direct_import_mode and direct_import_via_cli:
            run_import_service_payload(
                project_root=project_root,
                service=service,
                snapshot=snapshot,
                compact_meta_json=args.import_compact_meta_json,
                write_project_file=False,
                import_cli=args.import_cli,
            )
            return

        write_snapshot_service(
            snapshot_dir=snapshot_dir,
            service=service,
            snapshot=snapshot,
            instance_chunk_size=int(args.snapshot_instance_chunk_size),
        )
        if direct_import_mode:
            run_import_snapshot(
                project_root=project_root,
                snapshot_dir=snapshot_dir,
                progress_every=args.import_progress_every,
                compact_meta_json=args.import_compact_meta_json,
                skip_default_filtering=args.import_skip_default_filtering,
                services=[service],
                write_project_file=False,
                import_cli=args.import_cli,
            )
            delete_snapshot_service_files(snapshot_dir=snapshot_dir, service=service)

    max_source_workers = 2
    if int(args.source_workers) <= 0:
        detected_workers, cpu_count, ram_gb = detect_recommended_source_workers()
        instance_workers = max(1, min(max_source_workers, detected_workers))
        ram_text = f"{ram_gb:.1f}GB" if ram_gb is not None else "unknown"
        print(
            f"[sync] source workers: {instance_workers} (instance-parallel mode; "
            f"detected capacity={detected_workers}, cpu={cpu_count}, ram={ram_text})"
        )
    else:
        instance_workers = max(1, min(max_source_workers, int(args.source_workers)))
        print(
            f"[sync] source workers: {instance_workers} (manual, max={max_source_workers})"
        )

    if args.transport == "mcp":
        command = load_server_command(args.config, args.server)
        client = MCPClient(command, cwd=project_root)
        try:
            client.request(
                "initialize",
                {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "codex-sync-export", "version": "1.0.0"},
                },
            )
            client.notify("notifications/initialized", {})
            ensure_active_studio(client, wait_seconds=args.ws_wait_seconds)
            run_export(
                client=client,
                services=services,
                snapshot_dir=snapshot_dir,
                chunk_size=args.chunk_size,
                instance_workers=instance_workers,
                command=command,
                project_root=project_root,
                ws_wait_seconds=args.ws_wait_seconds,
                export_all_properties=bool(args.export_all_properties),
                snapshot_instance_chunk_size=int(args.snapshot_instance_chunk_size),
                on_service_snapshot=on_service_snapshot,
            )
        finally:
            client.close()
    else:
        bridge_ports = parse_bridge_ports(args.bridge_ports)
        bridge = WSBridgeRuntime(host=args.bridge_host, ports=bridge_ports)
        adaptive_throttle = AdaptiveBridgeThrottle(
            enabled=bool(args.adaptive_throttle),
            low_fps=float(args.throttle_low_fps),
            high_fps=float(args.throttle_high_fps),
            max_delay_ms=float(args.throttle_max_delay_ms),
            probe_interval_ms=float(args.throttle_probe_interval_ms),
        )
        try:
            bridge.start()
            min_needed = 1
            try:
                connected = bridge.wait_for_channels(min_count=min_needed, timeout=args.bridge_wait_seconds)
            except Exception as first_wait_exc:
                retry_wait = max(20.0, float(args.bridge_wait_seconds) * 2.0)
                print(
                    f"[sync] warning: initial bridge wait failed ({first_wait_exc}); "
                    f"retrying for up to {retry_wait:.0f}s..."
                )
                connected = bridge.wait_for_channels(min_count=min_needed, timeout=retry_wait)
            connected_sorted = sorted(connected)
            active_workers = max(1, min(instance_workers, len(bridge_ports)))
            print(
                f"[sync] bridge connected channels: {connected_sorted} "
                f"(workers={active_workers}, configured_ports={bridge_ports})"
            )
            if args.adaptive_throttle:
                print(
                    "[sync] adaptive throttle: enabled "
                    f"(low_fps={args.throttle_low_fps:.1f}, high_fps={args.throttle_high_fps:.1f}, "
                    f"max_delay_ms={args.throttle_max_delay_ms:.1f})"
                )
            if args.property_candidates_source == "api_dump":
                try:
                    api_dump = _load_studio_api_dump(project_root=project_root)
                    class_candidates = _build_class_property_candidates(api_dump)
                    configure_bridge_property_candidates(
                        bridge=bridge,
                        bridge_ports=bridge_ports,
                        class_candidates=class_candidates,
                        adaptive_throttle=adaptive_throttle,
                    )
                except Exception as exc:
                    print(f"[sync] warning: failed to configure API-dump property candidates: {exc}")

            run_export_ws(
                bridge=bridge,
                bridge_ports=bridge_ports,
                services=services,
                snapshot_dir=snapshot_dir,
                chunk_size=args.chunk_size,
                instance_workers=active_workers,
                adaptive_throttle=adaptive_throttle,
                export_all_properties=bool(args.export_all_properties),
                snapshot_instance_chunk_size=int(args.snapshot_instance_chunk_size),
                on_service_snapshot=on_service_snapshot,
            )
        finally:
            bridge.stop()

    if not args.no_update_editor_icons:
        update_editor_icon_settings(project_root)

    try:
        if args.run_import:
            if direct_import_mode:
                write_generated_project(project_root=project_root, services=services)
            else:
                run_import_snapshot(
                    project_root=project_root,
                    snapshot_dir=snapshot_dir,
                    progress_every=args.import_progress_every,
                    compact_meta_json=args.import_compact_meta_json,
                    skip_default_filtering=args.import_skip_default_filtering,
                    services=services,
                    write_project_file=True,
                    import_cli=args.import_cli,
                )
    finally:
        if args.run_import and temp_snapshot_dir is not None and temp_snapshot_dir.exists():
            if direct_import_mode:
                print(f"[sync] cleaning temp service cache: {temp_snapshot_dir}")
            else:
                print(f"[sync] cleaning temp snapshot cache: {temp_snapshot_dir}")
            shutil.rmtree(temp_snapshot_dir, ignore_errors=True)

    print("[sync] done")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"[sync] error: {exc}", file=sys.stderr)
        raise
