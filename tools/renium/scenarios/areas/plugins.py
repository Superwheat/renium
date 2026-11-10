"""Renium plugins: scaffold, validation, pinned installs, running commands, snapshots, process
reports and the synced project guides, plus `rbx collab` (matrix/links.md, PLUG rows). Every row keeps
the plugin registry inside the scratch copy through RENIUM_PLUGIN_HOME."""

from __future__ import annotations

import functools
import json
import os
import re
import shutil
import socket
import stat
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

from run import FIXTURES, Context, Scenario

RELAY = Path(__file__).resolve().parents[3] / "renium-relay"
MAIN = "src/ServerScriptService/Main.server.luau"
UTIL = "src/ReplicatedStorage/Shared/Util.luau"


def _write(path, text: str):
    """Python 3.9 on the Mac has no Path.write_text(newline=)."""
    with Path(path).open("w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def _home(ctx: Context):
    os.environ["RENIUM_PLUGIN_HOME"] = str(ctx.root / "plugin-home")
    os.environ["RENIUM_GLOBAL_PACKAGES_DIR"] = str(ctx.root / "global-packages")


def _scaffold(ctx: Context, name: str = "demo"):
    _home(ctx)
    created = ctx.rbx("plugin", "new", name, name, timeout=60)
    ctx.expect(created.get("compiled") is False and created.get("created"), f"plugin new: {created.json}")
    return ctx.root / name


def _build(ctx: Context, folder):
    completed = subprocess.run(["cargo", "build", "--release", "-j", "4"], cwd=folder, capture_output=True, text=True, timeout=600)
    ctx.expect(completed.returncode == 0, f"plugin build failed: {completed.stderr[-400:]}")


def _manifest(folder: Path) -> dict:
    return json.loads((folder / "renium-plugin.json").read_text(encoding="utf-8"))


def _write_manifest(folder: Path, manifest: dict):
    _write(folder / "renium-plugin.json", json.dumps(manifest, indent=2))
P1_MAIN = r'''use renium_plugin_sdk::{Invocation, Result, Value, bail, json};
use std::io::{self, BufRead, Write};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("child") {
        std::thread::sleep(Duration::from_secs(60));
        return ExitCode::SUCCESS;
    }
    let mut line = String::new();
    if io::stdin().lock().read_line(&mut line).is_err() {
        return ExitCode::FAILURE;
    }
    let invocation: Invocation = match serde_json::from_str(&line) {
        Ok(invocation) => invocation,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let mut out = io::stdout().lock();
    match invocation.command.as_str() {
        "garbage" => {
            let _ = writeln!(out, "not json");
            eprintln!("garbage diagnostics");
            return ExitCode::SUCCESS;
        }
        "okexit" => {
            let _ = writeln!(out, "{}", json!({"protocol": 1, "ok": true, "result": {}}));
            let _ = out.flush();
            std::process::exit(3);
        }
        "proto" => {
            let _ = writeln!(out, "{}", json!({"protocol": 2, "ok": true, "result": {}}));
            return ExitCode::SUCCESS;
        }
        "big" => {
            let chunk = vec![b'x'; 1 << 20];
            for _ in 0..33 {
                if out.write_all(&chunk).is_err() {
                    break;
                }
            }
            return ExitCode::SUCCESS;
        }
        _ => {}
    }
    let (response, code) = match handle(&invocation) {
        Ok(value) => (json!({"protocol": 1, "ok": true, "result": value}), ExitCode::SUCCESS),
        Err(error) => (json!({"protocol": 1, "ok": false, "error": format!("{error:#}")}), ExitCode::FAILURE),
    };
    let _ = writeln!(out, "{response}");
    code
}

fn handle(invocation: &Invocation) -> Result<Value> {
    let context = &invocation.context;
    match invocation.command.as_str() {
        "hello" => Ok(json!({"message": format!("Hello, {}!", invocation.arguments["name"].as_str().unwrap_or("World"))})),
        "sleep" => {
            let seconds = invocation.arguments["seconds"].as_i64().unwrap_or(0);
            std::thread::sleep(Duration::from_secs(seconds.max(0) as u64));
            Ok(json!({"slept": seconds}))
        }
        "spawn" => {
            let child = Command::new(std::env::current_exe()?)
                .arg("child")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            std::fs::write(context.state_directory.join("child.pid"), child.id().to_string())?;
            std::thread::sleep(Duration::from_secs(60));
            Ok(json!({}))
        }
        "fail" => bail!("boom"),
        "call" => context.renium(&["lks"]),
        "session" => Ok(json!({"session": context.session})),
        other => bail!("unknown command {other}"),
    }
}
'''

P1_COMMANDS = {
    "hello": {"description": "Say hello", "arguments": [{"name": "name", "type": "string", "description": "Who to greet", "default": "World"}]},
    "sleep": {"description": "Sleep", "arguments": [{"name": "seconds", "type": "integer", "description": "Seconds", "required": True}], "timeoutSeconds": 2},
    "spawn": {"description": "Start a child process and sleep", "timeoutSeconds": 2},
    "garbage": {"description": "Print no JSON"},
    "fail": {"description": "Fail"},
    "okexit": {"description": "Answer ok and exit 3"},
    "proto": {"description": "Answer protocol 2"},
    "big": {"description": "Print 33 MiB"},
    "call": {"description": "Call rbx lks"},
    "session": {"description": "Return the session"},
}


def _p1(ctx: Context) -> Path:
    """P1: the test plugin with one handler per protocol case, built and installed pinned."""
    folder = _scaffold(ctx)
    _write(folder / "src" / "main.rs", P1_MAIN)
    with (folder / "Cargo.toml").open("a", encoding="utf-8", newline="\n") as handle:
        handle.write('serde_json = "1.0"\n')
    manifest = _manifest(folder)
    manifest["permissions"] = ["renium"]
    manifest["commands"] = P1_COMMANDS
    _write_manifest(folder, manifest)
    _build(ctx, folder)
    ctx.rbx("plugin", "install", "demo", timeout=60)
    return folder


def _variant(ctx: Context, source: Path, name: str, change=None, guide: str | None = None) -> Path:
    """A copy of the built plugin under another name: manifest, guide and executable only."""
    folder = ctx.root / "variants" / name
    folder.mkdir(parents=True)
    manifest = _manifest(source)
    manifest["name"] = name
    if change:
        change(manifest)
    _write_manifest(folder, manifest)
    _write(folder / "AGENTS.md", guide or f"# {name}\n")
    executable = manifest["executable"]["windows" if os.name == "nt" else ("macos" if sys.platform == "darwin" else "linux")][0]
    built = source / ("target/release/demo.exe" if os.name == "nt" else "target/release/demo")
    (folder / executable).parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(built, folder / executable)
    return folder


def scaffold_check_and_refusals(ctx: Context):
    """PLUG-001/002/003/005: new writes the starter with its SDK, check validates it without a binary,
    install before a build is refused, and bad names or an existing folder are refused."""
    folder = _scaffold(ctx)
    for relative in ("renium-plugin.json", "Cargo.toml", "src/main.rs", "AGENTS.md", "sdk/src/lib.rs", "sdk/src/process.rs", "sdk/src/lease.rs"):
        ctx.expect((folder / relative).is_file(), f"starter lacks {relative}")
    lines = lambda path: path.read_bytes().replace(b"\r\n", b"\n")
    canonical = (ctx.root.parent.parent / "projest" / "tools" / "renium-plugin-sdk" / "src" / "lib.rs")
    if canonical.is_file():
        ctx.expect(lines(folder / "sdk/src/lib.rs") == lines(canonical), "starter SDK matches the repository SDK")
    checked = ctx.rbx("plugin", "check", "demo", timeout=60)
    ctx.expect(checked.get("valid") is True and checked.get("compiled") is False and "hello" in (checked.get("commands") or {}), f"check: {checked.json}")
    early = ctx.rbx("plugin", "install", "demo", ok=False, timeout=60)
    ctx.expect_in("build the plugin first", early.error, "install before build")
    exists = ctx.rbx("plugin", "new", "demo", "demo", ok=False, timeout=60)
    ctx.expect_in("already exists", exists.error, "existing folder")
    upper = ctx.rbx("plugin", "new", "Demo", "d2", ok=False, timeout=60)
    ctx.expect_in("lowercase", upper.error, "bad name")
    clash = ctx.rbx("plugin", "new", "lk", "d3", ok=False, timeout=60)
    ctx.expect_in("collides with a Renium command", clash.error, "command name clash")
    reserved = ctx.rbx("plugin", "new", "link-apply", "d4", ok=False, timeout=60)
    ctx.expect_in("collides with a Renium command", reserved.error, "long command name clash")


MANIFEST_CASES = [
    ("schema2", lambda m: m.__setitem__("schemaVersion", 2), "Unsupported plugin schemaVersion 2"),
    ("timeout0", lambda m: m["commands"]["hello"].__setitem__("timeoutSeconds", 0), "Command timeoutSeconds must be 1–3600"),
    ("timeout3601", lambda m: m["commands"]["hello"].__setitem__("timeoutSeconds", 3601), "Command timeoutSeconds must be 1–3600"),
    ("root", lambda m: m.__setitem__("permissions", ["root"]), "Unknown plugin permission"),
    ("nocommands", lambda m: m.__setitem__("commands", {}), "Plugins need 1–100 commands"),
    ("session", lambda m: m["commands"]["hello"]["arguments"].append({"name": "session", "type": "string", "description": "s"}), "Invalid, reserved or duplicate argument session"),
    ("requireddefault", lambda m: m["commands"]["hello"]["arguments"][0].__setitem__("required", True), "cannot be required and have a default"),
    ("defaulttype", lambda m: m["commands"]["hello"]["arguments"][0].__setitem__("default", 5), "Invalid default type for name"),
    ("absolute", lambda m: m.__setitem__("executable", {key: ["/abs/x"] for key in ("windows", "macos", "linux")}), "Executable must be a relative path inside the plugin"),
    ("unknownkey", lambda m: m.__setitem__("extra", 1), "Invalid renium-plugin.json"),
    ("guide", lambda m: m.__setitem__("guide", "../x"), "Plugin paths must stay inside their directory"),
]


def manifest_validation(ctx: Context):
    """PLUG-004: check refuses each invalid manifest field and an oversized manifest."""
    folder = _scaffold(ctx)
    for name, change, message in MANIFEST_CASES:
        copy = ctx.root / "cases" / name
        copy.mkdir(parents=True)
        manifest = _manifest(folder)
        change(manifest)
        _write_manifest(copy, manifest)
        shutil.copy2(folder / "AGENTS.md", copy / "AGENTS.md")
        checked = ctx.rbx("plugin", "check", str(copy), ok=False, timeout=60)
        ctx.expect_in(message, checked.error, f"check {name}")
    huge = ctx.root / "cases" / "huge"
    huge.mkdir(parents=True)
    (huge / "renium-plugin.json").write_text(json.dumps(_manifest(folder)) + " " * (1100 * 1024), encoding="utf-8")
    shutil.copy2(folder / "AGENTS.md", huge / "AGENTS.md")
    oversized = ctx.rbx("plugin", "check", str(huge), ok=False, timeout=60)
    ctx.expect_in("exceeds its size limit", oversized.error, "1.1 MiB manifest")


def windows_entry_must_be_exe(ctx: Context):
    """PLUG-006: a Windows entry point that is a batch file is refused at install."""
    folder = _scaffold(ctx)
    manifest = _manifest(folder)
    manifest["executable"]["windows"] = ["run.cmd"]
    _write_manifest(folder, manifest)
    (folder / "run.cmd").write_text("@echo off\r\n", encoding="utf-8")
    refused = ctx.rbx("plugin", "install", "demo", ok=False, timeout=60)
    ctx.expect_in("Windows plugin entry points must be .exe files", refused.error, "batch entry refused")
    ctx.expect(not (ctx.root / "plugin-home" / "installed" / "demo.json").exists(), "nothing registered")


def install_run_info_and_remove(ctx: Context):
    """PLUG-007/009/010/023: a pinned install records both hashes, info and help run no plugin code,
    the command runs with its default and a given argument, remove keeps the state."""
    folder = _scaffold(ctx)
    _build(ctx, folder)
    installed = ctx.rbx("plugin", "install", "demo", timeout=60)
    ctx.expect(installed.get("installed") == "demo" and installed.get("dev") is False and installed.get("permissions") == [] and installed.get("next") and "trusted" in str(installed.get("trust")), f"install: {installed.json}")
    registration = json.loads((ctx.root / "plugin-home" / "installed" / "demo.json").read_text(encoding="utf-8"))
    ctx.expect(len(registration.get("manifestHash", "")) == 64 and len(registration.get("executableHash", "")) == 64 and registration.get("dev") is False, f"registration: {registration}")
    info = ctx.rbx("plugin", "info", "demo", timeout=60)
    ctx.expect("hello" in json.dumps(info.get("manifest")) and info.get("guide") == (folder / "AGENTS.md").read_text(encoding="utf-8") and info.get("dev") is False, f"info: {str(info.json)[:200]}")
    ctx.expect(not str(info.get("directory", "")).startswith("\\\\?\\") and Path(info.get("directory", "")).resolve() == folder.resolve(), f"info directory: {info.get('directory')}")
    helped = ctx.rbx("demo", "--help", json_mode=False, ok=None, timeout=60)
    ctx.expect(helped.code == 0 and "hello" in helped.stdout, f"plugin help: {helped.stdout[:200]}")
    command_help = ctx.rbx("demo", "hello", "--help", json_mode=False, ok=None, timeout=60)
    ctx.expect(command_help.code == 0 and "--session" in command_help.stdout and "--name" in command_help.stdout, f"command help: {command_help.stdout[:300]}")
    ctx.expect(not (ctx.root / "plugin-home" / "state" / "demo").exists(), "help ran no plugin code")
    ran = ctx.rbx("demo", "hello", timeout=60)
    ctx.expect(ran.get("message") == "Hello, World!", f"hello: {ran.json}")
    ctx.expect((ctx.root / "plugin-home" / "state" / "demo").is_dir(), "state directory created")
    named = ctx.rbx("demo", "hello", "--name", "Ana", timeout=60)
    ctx.expect(named.get("message") == "Hello, Ana!", f"hello --name: {named.json}")
    bad = ctx.rbx("demo", "hello", "--nope", json_mode=False, ok=None, timeout=60)
    ctx.expect(bad.code != 0 and "nope" in (bad.stdout + bad.stderr), f"unknown argument: exit {bad.code} {bad.stderr[-200:]}")
    listed = ctx.rbx("plugin", "list", timeout=60)
    ctx.expect(any(p.get("name") == "demo" and p.get("version") == "0.1.0" for p in listed.get("plugins") or []), f"list: {listed.json}")
    removed = ctx.rbx("plugin", "remove", "demo", timeout=60)
    ctx.expect(removed.get("removed") == "demo" and removed.get("statePreserved") is True, f"remove: {removed.json}")
    ctx.expect((ctx.root / "plugin-home" / "state" / "demo").is_dir(), "state kept")
    gone = ctx.rbx("demo", "hello", ok=False, json_mode=False, timeout=60)
    ctx.expect(gone.code != 0, "removed plugin command fails")
    again = ctx.rbx("plugin", "remove", "demo", ok=False, timeout=60)
    ctx.expect_in("Plugin 'demo' is not installed", again.error, "second remove")


def pinned_install_detects_changes(ctx: Context):
    """PLUG-012/013: a pinned install refuses a changed manifest or executable until reinstalled, --dev
    follows changes, and a renamed manifest asks for a reinstall."""
    folder = _scaffold(ctx)
    _build(ctx, folder)
    ctx.rbx("plugin", "install", "demo", timeout=60)
    manifest = folder / "renium-plugin.json"
    text = manifest.read_text(encoding="utf-8")
    manifest.write_text(text.replace("A Renium plugin", "A changed plugin"), encoding="utf-8")
    changed = ctx.rbx("demo", "hello", ok=False, timeout=60)
    ctx.expect_in("manifest changed", changed.error, "manifest change detected")
    ctx.rbx("plugin", "install", "demo", timeout=60)
    ctx.expect(ctx.rbx("demo", "hello", timeout=60).get("message") == "Hello, World!", "reinstall accepts the change")
    main = folder / "src/main.rs"
    main.write_text(main.read_text(encoding="utf-8").replace("Hello, ", "Hi, "), encoding="utf-8")
    _build(ctx, folder)
    rebuilt = ctx.rbx("demo", "hello", ok=False, timeout=60)
    ctx.expect_in("executable changed", rebuilt.error, "executable change detected")
    ctx.rbx("plugin", "install", "demo", "--dev", timeout=60)
    ctx.expect(ctx.rbx("demo", "hello", timeout=60).get("message") == "Hi, World!", "--dev runs the rebuilt binary")
    manifest.write_text(manifest.read_text(encoding="utf-8").replace('"demo"', '"demo3"', 1), encoding="utf-8")
    renamed = ctx.rbx("demo", "hello", ok=False, timeout=60)
    ctx.expect_in("renamed", renamed.error, "rename detected")


def plugin_protocol_and_limits(ctx: Context):
    """PLUG-011/014/015: arguments are checked before the plugin starts; a command past its timeout has its
    whole process tree stopped; malformed responses are reported."""
    _p1(ctx)
    state = ctx.root / "plugin-home" / "state" / "demo"
    for args, needle in ((("sleep", "--seconds", "abc"), "invalid value 'abc'"), (("hello", "--nope"), "unexpected argument '--nope'"), ((), "requires a subcommand"), (("--project", "X", "hello"), "unexpected argument '--project'")):
        refused = ctx.rbx("demo", *args, ok=False, timeout=60)
        ctx.expect(refused.code != 0 and needle in refused.error, f"demo {' '.join(args)}: exit {refused.code} {refused.error[:200]}")
    ctx.expect(not state.exists(), "argument errors never launched the plugin")
    started = time.monotonic()
    spawned = ctx.rbx("demo", "spawn", ok=False, timeout=60)
    ctx.expect_in("Plugin command exceeded 2.0s; its process tree was stopped", spawned.error, "timeout")
    ctx.expect(time.monotonic() - started < 10, f"timeout took {time.monotonic() - started:.1f}s")
    # On a loaded machine the plugin may be stopped before it spawned its child.
    if (state / "child.pid").is_file():
        child = (state / "child.pid").read_text(encoding="utf-8").strip()
        ctx.expect(ctx.rbx("plugin", "process", child, timeout=60).get("alive") is False, f"child {child} survived the timeout")
    else:
        ctx.log.append("# the plugin was stopped before it spawned its child; no child to check")
    for command, message in (("garbage", "Plugin returned no JSON response"), ("fail", "boom"), ("okexit", "Plugin reported success but exited with exit"), ("proto", "Unsupported plugin response protocol"), ("big", "Plugin command output exceeded 32 MiB")):
        failed = ctx.rbx("demo", command, ok=False, timeout=120)
        ctx.expect_in(message, failed.error, command)
        if command == "garbage":
            ctx.expect_in("Plugin diagnostics: garbage diagnostics", failed.error, "diagnostics forwarded")


def plugin_permissions_binding_and_session(ctx: Context):
    """PLUG-008/016/017/018/024: list shows a broken registration; ctx.renium needs the renium permission
    and keeps the host's project binding without refreshing guides; the session comes from --session,
    RENIUM_SESSION_ID, CODEX_THREAD_ID in that order; check and install refuse a guide over 64 KiB,
    which info could not show (D-L3)."""
    folder = _p1(ctx)
    ctx.rbx("lkp", "--link-folder", "links", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Shared"]', timeout=60)
    bare = _variant(ctx, folder, "bare", lambda m: m.__setitem__("permissions", []))
    ctx.rbx("plugin", "install", str(bare), timeout=60)
    denied = ctx.rbx("bare", "call", ok=False, timeout=60)
    ctx.expect_in("Plugin does not declare the renium permission", denied.error, "permission")
    guide = ctx.root / "RENIUM" / "plugins.md"
    guide.write_bytes(guide.read_bytes() + b"\nedited\n")
    elsewhere = ctx.root / "elsewhere"
    elsewhere.mkdir()
    called = ctx.rbx("--project", str(ctx.root / "renium.project.jsonc"), "demo", "call", cwd=elsewhere, timeout=120)
    ctx.expect(called.get("linkCount") == 1, f"demo call: {called.json}")
    ctx.expect(guide.read_bytes().endswith(b"\nedited\n"), "plugin calls skip the guide refresh")
    saved = {key: os.environ.get(key) for key in ("RENIUM_SESSION_ID", "CODEX_THREAD_ID")}
    try:
        os.environ["RENIUM_SESSION_ID"], os.environ["CODEX_THREAD_ID"] = "b", "c"
        ctx.expect(ctx.rbx("demo", "session", "--session", "a", timeout=60).get("session") == "a", "--session wins")
        ctx.expect(ctx.rbx("demo", "session", timeout=60).get("session") == "b", "RENIUM_SESSION_ID next")
        del os.environ["RENIUM_SESSION_ID"]
        ctx.expect(ctx.rbx("demo", "session", timeout=60).get("session") == "c", "CODEX_THREAD_ID last")
        del os.environ["CODEX_THREAD_ID"]
        ctx.expect(ctx.rbx("demo", "session", timeout=60).get("session") is None, "no session")
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    demo2 = _variant(ctx, folder, "demo2")
    ctx.rbx("plugin", "install", str(demo2), timeout=60)
    shutil.rmtree(demo2)
    listed = ctx.rbx("plugin", "list", timeout=60).get("plugins") or []
    names = [entry.get("name") for entry in listed]
    ctx.expect(names == sorted(names) and set(names) == {"bare", "demo", "demo2"}, f"list: {listed}")
    entries = {entry.get("name"): entry for entry in listed}
    ctx.expect(set(entries["demo"]) == {"name", "version", "description", "dev"} and set(entries["demo2"]) == {"name", "error"}, f"list entries: {listed}")
    big = _variant(ctx, folder, "bigguide", guide="# bigguide\n" + "x" * (70 * 1024) + "\n")
    for command in ("check", "install"):
        refused = ctx.rbx("plugin", command, str(big), ok=False, timeout=60)
        ctx.expect_in("The plugin guide AGENTS.md is 71692 bytes; a guide can be at most 65536 bytes (64 KiB)", refused.error, f"{command} refuses a guide info could not show (D-L3)")
    ctx.expect("bigguide" not in {entry.get("name") for entry in ctx.rbx("plugin", "list", timeout=60).get("plugins") or []}, "the refused plugin is not installed")


def snapshot_process_and_runtime_check(ctx: Context):
    """PLUG-019/020/022: snapshot exports the projection into a new project, process reports liveness
    and guards the Studio kill, runtime-check confirms the daemon can hold leases."""
    _home(ctx)
    snapped = ctx.rbx("plugin", "snapshot", "snap", timeout=120)
    ctx.expect(snapped.get("project") and snapped.get("sourceProject") and snapped.get("source") and (ctx.root / "snap" / "instances" / "Workspace.renium").is_file(), f"snapshot: {snapped.json}")
    ctx.expect(not (ctx.root / "snap" / "src" / ".renium").exists(), "no state under the snapshot's src")
    again = ctx.rbx("plugin", "snapshot", "snap", ok=False, timeout=60)
    ctx.expect_in("must not exist", again.error, "existing destination")
    inside = ctx.rbx("plugin", "snapshot", "src/inside", ok=False, timeout=60)
    ctx.expect_in("cannot be inside the projected source tree", inside.error, "destination inside src")
    alive = ctx.rbx("plugin", "process", str(os.getpid()), timeout=60)
    ctx.expect(alive.get("alive") is True and alive.get("studio") is False, f"process: {alive.json}")
    guarded = ctx.rbx("plugin", "process", str(os.getpid()), "--terminate-studio", "--started-unix", "1", ok=False, timeout=60)
    ctx.expect_in("is not the Roblox Studio that started at 1; nothing was terminated", guarded.error, "guarded kill")
    alone = ctx.rbx("plugin", "process", str(os.getpid()), "--terminate-studio", ok=False, timeout=60)
    ctx.expect(alone.code == 2 and "--started-unix" in alone.stderr, f"--terminate-studio alone: exit {alone.code} {alone.stderr[:200]}")
    runtime = ctx.rbx("plugin", "runtime-check", timeout=60)
    ctx.expect(runtime.get("ready") is True and runtime.get("pluginResourceLeases") == 1, f"runtime-check: {runtime.json}")


def terminate_the_studio_the_runner_launched(ctx: Context):
    """PLUG-021: process --terminate-studio with the recorded start time closes only that Studio."""
    _home(ctx)
    ctx.open_studio()
    clients = [c for c in ctx.rbx("status", timeout=60).get("clients") or [] if c.get("role") == "edit"]
    ctx.expect(len(clients) == 1 and clients[0].get("pid"), f"edit client: {clients}")
    pid = str(clients[0]["pid"])
    report = ctx.rbx("plugin", "process", pid, timeout=60)
    ctx.expect(report.get("studio") is True and report.get("startedUnix"), f"process: {report.json}")
    others = [p.get("pid") for p in (ctx.rbx("status", "--all", ok=None, timeout=60).pointer("/diagnosis/studioProcesses") or []) if p.get("pid") and str(p.get("pid")) != pid]
    killed = ctx.rbx("plugin", "process", pid, "--terminate-studio", "--started-unix", str(report["startedUnix"]), timeout=60)
    ctx.studio_open = False
    ctx.expect(killed.get("terminated") is True, f"terminate: {killed.json}")
    ctx.expect(ctx.rbx("plugin", "process", pid, timeout=60).get("alive") is False, "Studio gone")
    survivors = [other for other in others if ctx.rbx("plugin", "process", str(other), timeout=60).get("alive") is True]
    ctx.expect(survivors == others, f"other Studios terminated: {sorted(set(others) - set(survivors))}")


def synced_guides_refresh(ctx: Context):
    """PLUG-025: an edited synced guide is restored by the next command that refreshes guides, with a
    stderr note; a CRLF copy is left alone; an extra file in RENIUM stays until a refresh runs."""
    _home(ctx)
    guide = ctx.root / "RENIUM" / "plugins.md"
    original = guide.read_bytes()
    guide.write_bytes(original + b"\nedited\n")
    refreshed = ctx.rbx("lks", ok=None, timeout=60)
    ctx.expect_in("updated RENIUM/plugins.md", refreshed.stderr, "refresh note on stderr")
    ctx.expect(guide.read_bytes() == original, "guide restored")
    links_guide = ctx.root / "RENIUM" / "links.md"
    crlf = links_guide.read_bytes().replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
    links_guide.write_bytes(crlf)
    quiet = ctx.rbx("lks", ok=None, timeout=60)
    ctx.expect("updated RENIUM/links.md" not in quiet.stderr and links_guide.read_bytes() == crlf, "CRLF copy kept")
    extra = ctx.root / "RENIUM" / "extra.md"
    extra.write_text("# extra\n", encoding="utf-8")
    ctx.rbx("lks", ok=None, timeout=60)
    ctx.expect(extra.is_file(), "extra guide kept while nothing needs a refresh")
    guide.write_bytes(original + b"\nedited\n")
    ctx.rbx("lks", ok=None, timeout=60)
    ctx.expect(not extra.exists() and guide.read_bytes() == original, "a refresh removes the extra guide")


# ---------------------------------------------------------------- H. rbx collab

INVITE = re.compile(r"^ws://127\.0\.0\.1:\d+/\?token=[0-9a-f]{32}$")


def _collab(ctx: Context, root: Path, *args: str, **kwargs):
    kwargs.setdefault("timeout", 90)
    return ctx.rbx("collab", *args, cwd=root, **kwargs)


def _ready(ctx: Context, root: Path, seconds: float = 40):
    deadline = time.monotonic() + seconds
    while True:
        status = _collab(ctx, root, "status", ok=None)
        if status.get("ready") is True:
            return status
        if time.monotonic() >= deadline:
            ctx.expect(False, f"collab session at {root} never became ready: {status.json}")
        time.sleep(0.5)


def _until(ctx: Context, check, seconds: float, message):
    """Polls check until it returns something truthy; message may be a callable, read at the deadline."""
    deadline = time.monotonic() + seconds
    while True:
        value = check()
        if value:
            return value
        if time.monotonic() >= deadline:
            ctx.expect(False, message() if callable(message) else message)
        time.sleep(0.5)


def _stop(ctx: Context, *roots: Path):
    for root in roots:
        if root.is_dir():
            _collab(ctx, root, "stop", ok=None)


def _project_files(root: Path) -> dict:
    files = {"renium.project.jsonc": (root / "renium.project.jsonc").read_bytes()}
    for folder in ("src", "instances"):
        for path in sorted((root / folder).rglob("*")):
            if path.is_file() and not path.name.endswith(".lock"):
                files[path.relative_to(root).as_posix()] = path.read_bytes()
    return files


def _start_host(ctx: Context, *extra: str):
    _home(ctx)
    started = _collab(ctx, ctx.root, "start", *extra, "--name", "host", "-r", str(ctx.root))
    return started, _ready(ctx, ctx.root)


def _join(ctx: Context, invite: str, name: str):
    root = ctx.root / name
    _collab(ctx, ctx.root, "join", invite, "--name", name, "-r", str(root))
    return root, _ready(ctx, root)


def collab_local_host_and_guest(ctx: Context):
    """PLUG-040/041/042/044/048: a local host shares the project files, a guest fills an empty folder from
    the invite, bad invites and second starts are refused, invite prints the link and stop ends a session."""
    guest = ctx.root / "guest"
    try:
        started, host = _start_host(ctx, "--local")
        ctx.expect(started.get("running") is True and started.get("role") == "host" and started.get("tunnel") is False and INVITE.match(str(started.get("invite"))), f"start: {started.json}")
        ctx.expect(host.get("files") == len(_project_files(ctx.root)), f"host files {host.get('files')} vs {len(_project_files(ctx.root))}")
        ctx.expect((ctx.root / ".renium" / "collab.json").is_file(), "collab.json written")
        again = _collab(ctx, ctx.root, "start", "--local", "-r", str(ctx.root), ok=False)
        ctx.expect_in("A collaboration session is already running for", again.error, "second start")
        empty = ctx.root / "empty"
        empty.mkdir()
        bare = _collab(ctx, empty, "start", "--local", "-r", str(empty), ok=False)
        ctx.expect_in("has no renium.project.jsonc; start from a project or join an invite", bare.error, "start without a project")
        for invite, message in (("ftp://x", "Invites are wss:// or https:// links"), ("ws://127.0.0.1:1/", "The invite has no token")):
            refused = _collab(ctx, ctx.root, "join", invite, "-r", str(ctx.root / "bad"), ok=False)
            ctx.expect_in(message, refused.error, f"join {invite}")
        _, joined = _join(ctx, started["invite"], "guest")
        ctx.expect(joined.get("role") == "guest" and joined.get("connected") is True, f"guest: {joined.json}")
        ctx.expect(_project_files(guest) == _project_files(ctx.root), "guest files equal the host's")
        state = {p.name for p in (guest / ".renium").iterdir()}
        ctx.expect("collab.json" in state and state <= {".gitignore", "collab.json", "map-cache.json"}, f"guest .renium: {sorted(state)}")
        users = lambda root: sorted(p.get("user") for p in _collab(ctx, root, "status").get("participants") or [])
        _until(ctx, lambda: users(ctx.root) == ["guest", "host"] and users(guest) == ["guest", "host"], 30, lambda: f"participants: host {users(ctx.root)} guest {users(guest)}")
        printed = _collab(ctx, ctx.root, "invite", json_mode=False)
        ctx.expect(printed.stdout.strip() == started["invite"], f"invite: {printed.stdout!r}")
        ctx.expect(_collab(ctx, guest, "stop").get("stopped") is True, "first stop")
        ctx.expect(_collab(ctx, guest, "stop").get("stopped") is False, "second stop")
        ctx.expect(not (guest / ".renium" / "collab.json").exists(), "guest collab.json removed")
        none = _collab(ctx, guest, "invite", ok=False)
        ctx.expect_in("No collaboration session is running for this project", none.error, "invite without a session")
    finally:
        _stop(ctx, ctx.root, guest)


def collab_join_overwrites(ctx: Context):
    """PLUG-043: joining into an existing project folder replaces its files with the room's."""
    guest = ctx.root / "guest"
    shutil.copytree(FIXTURES["small"], guest, ignore=shutil.ignore_patterns(".renium", "*.lock", "*.rbxl.lock"))
    (guest / MAIN).write_text('print("guest")\n', encoding="utf-8")
    (guest / "src" / "Extra.luau").write_text("return 1\n", encoding="utf-8")
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        _until(ctx, lambda: (guest / MAIN).read_bytes() == (ctx.root / MAIN).read_bytes() and not (guest / "src" / "Extra.luau").exists(), 30, "the guest folder kept its own files")
    finally:
        _stop(ctx, ctx.root, guest)


def collab_edits_travel(ctx: Context):
    """PLUG-045/047/046: edits travel both ways, a store edit replaces the whole store file without
    mirroring lock files, and concurrent edits of different lines of one file merge in both folders."""
    guest = ctx.root / "guest"
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        ctx.write(MAIN, 'print("host edit")\n')
        (guest / UTIL).write_text("return { version = 2 }\n", encoding="utf-8")
        _until(ctx, lambda: (guest / MAIN).read_text(encoding="utf-8") == 'print("host edit")\n' and ctx.read(UTIL) == "return { version = 2 }\n", 30, "edits did not travel both ways")
        stats = _collab(ctx, ctx.root, "status")
        ctx.expect((stats.get("localChanges") or 0) >= 1 and (stats.get("remoteChanges") or 0) >= 1, f"host counters: {stats.json}")
        ctx.rbx("bs", "Workspace", "Props.Sign", "-p", "Value", "--str", "hi", cwd=guest, timeout=60)
        store = "instances/Workspace.renium"
        _until(ctx, lambda: (ctx.root / store).read_bytes() == (guest / store).read_bytes(), 30, "the store edit never reached the host")
        ctx.expect(ctx.rbx("bg", "Workspace", "Props.Sign", "Value", timeout=60).json == "hi", "host store holds the edit")
        locks = [p for root in (ctx.root, guest) for p in (root / "instances").glob("*.lock")]
        ctx.expect(not locks, f"lock files mirrored: {locks}")
        lines = [f"line {n}" for n in range(1, 6)]
        ctx.write(MAIN, "\n".join(lines) + "\n")
        _until(ctx, lambda: (guest / MAIN).read_text(encoding="utf-8") == "\n".join(lines) + "\n", 30, "five lines never reached the guest")
        host_lines, guest_lines = list(lines), list(lines)
        host_lines[0], guest_lines[4] = "line 1 host", "line 5 guest"
        ctx.write(MAIN, "\n".join(host_lines) + "\n")
        (guest / MAIN).write_text("\n".join(guest_lines) + "\n", encoding="utf-8")
        merged = "\n".join(["line 1 host"] + lines[1:4] + ["line 5 guest"]) + "\n"
        _until(ctx, lambda: ctx.read(MAIN) == merged and (guest / MAIN).read_text(encoding="utf-8") == merged, 30, lambda: f"concurrent line edits did not merge after 30 s (D-L12): host {ctx.read(MAIN)!r} guest {(guest / MAIN).read_text(encoding='utf-8')!r}")
    finally:
        _stop(ctx, ctx.root, guest)


def collab_relay_preferences(ctx: Context):
    """PLUG-050: relay set/show/clear validate and store the preference (kept inside the scratch copy
    through LOCALAPPDATA); start --relay without one configured is refused."""
    _home(ctx)
    real = _collab(ctx, ctx.root, "relay", "show")
    appdata = ctx.root / "appdata"
    (appdata / "Renium").mkdir(parents=True)
    for name in ("update-check.json", "update-refresh.pending"):
        source = Path(os.environ.get("LOCALAPPDATA", "")) / "Renium" / name
        if source.is_file():
            shutil.copy2(source, appdata / "Renium" / name)
    saved = os.environ.get("LOCALAPPDATA")
    os.environ["LOCALAPPDATA"] = str(appdata)
    try:
        ctx.expect(_collab(ctx, ctx.root, "relay", "show").get("relay") is None, "no relay at first")
        bad = _collab(ctx, ctx.root, "relay", "set", "ftp://x", ok=False)
        ctx.expect_in("Relay URLs start with https://", bad.error, "bad scheme")
        ctx.expect(_collab(ctx, ctx.root, "relay", "set", "http://127.0.0.1:8787/").get("relay") == "http://127.0.0.1:8787", "set trims the slash")
        ctx.expect(_collab(ctx, ctx.root, "relay", "show").get("relay") == "http://127.0.0.1:8787", "show")
        ctx.expect((appdata / "Renium" / "collab.json").is_file(), "preference stored in the redirected data folder")
        ctx.expect(_collab(ctx, ctx.root, "relay", "clear").get("relay") is None, "clear")
        ctx.expect(_collab(ctx, ctx.root, "relay", "show").get("relay") is None, "cleared")
    finally:
        os.environ["LOCALAPPDATA"] = saved or ""
    if real.get("relay") is None:
        try:
            refused = _collab(ctx, ctx.root, "start", "--relay", "-r", str(ctx.root), ok=False)
            ctx.expect_in("No relay is configured yet", refused.error, "start --relay without a relay")
            ctx.expect("defaultRelay" not in (_collab(ctx, ctx.root, "status").json or {}), "status shows no default relay")
        finally:
            _stop(ctx, ctx.root)


def _free_port() -> int:
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def _start_relay(ctx: Context):
    """C2: the relay Worker under wrangler dev, from a copy inside the scratch copy (its runtime packages
    copied, wrangler run from the repository's node_modules)."""
    relay = ctx.root / "relay"
    for name in ("src", "package.json", "wrangler.toml", "tsconfig.json"):
        source = RELAY / name
        (shutil.copytree if source.is_dir() else shutil.copy2)(source, relay / name)
    for package in ("lib0", "y-protocols", "yjs", "isomorphic.js"):
        shutil.copytree(RELAY / "node_modules" / package, relay / "node_modules" / package)
    port = _free_port()
    env = dict(os.environ, WRANGLER_SEND_METRICS="false", WRANGLER_LOG_PATH=str(ctx.root / "wrangler-logs"), XDG_CONFIG_HOME=str(ctx.root / "xdg"))
    log = (ctx.root / "wrangler.out").open("w", encoding="utf-8")
    process = subprocess.Popen(["node", str(RELAY / "node_modules" / "wrangler" / "bin" / "wrangler.js"), "dev", "--port", str(port), "--ip", "127.0.0.1"], cwd=relay, env=env, stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT, start_new_session=os.name != "nt")
    url = f"http://127.0.0.1:{port}"
    deadline = time.monotonic() + 90
    while True:
        try:
            with urllib.request.urlopen(url + "/", timeout=2) as response:
                if response.status == 200:
                    return process, url, log
        except OSError:
            pass
        if time.monotonic() >= deadline or process.poll() is not None:
            _stop_relay(process, log)
            ctx.expect(False, f"the local relay never answered: {(ctx.root / 'wrangler.out').read_text(encoding='utf-8', errors='replace')[-600:]}")
        time.sleep(0.5)


def _stop_relay(process: subprocess.Popen, log):
    if os.name == "nt":
        subprocess.run(["taskkill", "/F", "/T", "/PID", str(process.pid)], capture_output=True, timeout=60)
    else:
        try:
            os.killpg(process.pid, 9)
        except OSError:
            pass
    try:
        process.wait(timeout=30)
    except subprocess.TimeoutExpired:
        pass
    log.close()


def collab_local_relay(ctx: Context):
    """PLUG-051/052: a relay keeps the room after the host stops, so a later guest gets the room's copy
    without the host's post-stop edit; a wrong token is refused by the relay and the guest keeps retrying."""
    process, url, log = _start_relay(ctx)
    guests = [ctx.root / name for name in ("g1", "g2", "g3", "g4")]
    try:
        started, _ = _start_host(ctx, "--relay", url)
        invite = str(started.get("invite"))
        ctx.expect(re.match(rf"^ws://127\.0\.0\.1:{url.rsplit(':', 1)[1]}/rooms/[0-9a-f]{{24}}\?token=[0-9a-f]{{32}}$", invite), f"relay invite: {invite}")
        _join(ctx, invite, "g1")
        original = ctx.read(MAIN)
        _collab(ctx, ctx.root, "stop")
        ctx.write(MAIN, 'print("after stop")\n')
        g2, _ = _join(ctx, invite, "g2")
        ctx.expect((g2 / MAIN).read_text(encoding="utf-8") == original, f"second guest got {(g2 / MAIN).read_text(encoding='utf-8')!r}")
        room = invite.split("?token=")[0]
        for name, token, code in (("g3", "badbadbadbad", "403"), ("g4", "bad", "401")):
            _collab(ctx, ctx.root, "join", f"{room}?token={token}", "--name", name, "-r", str(ctx.root / name))
        for name, code in (("g3", "403"), ("g4", "401")):
            status = _until(ctx, lambda: (lambda s: s if "Still trying to reach the room" in str(s.get("error")) else None)(_collab(ctx, ctx.root / name, "status").json or {}), 40, f"{name} never reported the refused room")
            ctx.expect(status.get("connected") is False and code in str(status.get("error")), f"{name}: {status}")
            ctx.expect(sorted(p.name for p in (ctx.root / name).iterdir()) == [".renium"], f"{name} folder untouched")
    finally:
        _stop(ctx, ctx.root, *guests)
        _stop_relay(process, log)


def collab_shares_link_files(ctx: Context):
    """PLUG-054 (D-L1): a guest of a project with links receives the link manifest and the link folder."""
    _home(ctx)
    ctx.rbx("lkp", "--link-folder", "links", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Shared"]', timeout=60)
    ctx.rbx("lka", "--id", "shared", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","SharedCopy"]', timeout=60)
    ctx.write("links/Logger.luau", 'return "v1"\n')
    ctx.rbx("lka", "--id", "logger", "--source", "links/Logger.luau", "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Logger"]', timeout=60)
    ctx.rbx("lk", timeout=60)
    guest = ctx.root / "guest"
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        time.sleep(2)
        present = {name: (guest / name).exists() for name in ("renium-link.json", "links/shared.renium", "links/Logger.luau")}
        status = ctx.rbx("lks", cwd=guest, ok=None, timeout=60)
        ctx.expect(all(present.values()) and status.get("linkCount") == 2, f"guest lacks the link files (D-L1): {present}, lks {status.json or status.error[:200]}")
    finally:
        _stop(ctx, ctx.root, guest)


def _five_lines(ctx: Context, guest: Path) -> list[str]:
    lines = [f"line {n}" for n in range(1, 6)]
    ctx.write(MAIN, "\n".join(lines) + "\n")
    _until(ctx, lambda: (guest / MAIN).read_text(encoding="utf-8") == "\n".join(lines) + "\n", 30, "five lines never reached the guest")
    return lines


def _both_hold(ctx: Context, guest: Path, accepted: set, what: str):
    _until(ctx, lambda: ctx.read(MAIN) in accepted and ctx.read(MAIN) == (guest / MAIN).read_text(encoding="utf-8"), 30, lambda: f"{what}: host {ctx.read(MAIN)!r} guest {(guest / MAIN).read_text(encoding='utf-8')!r}")


def collab_restore_after_remote_edit(ctx: Context):
    """PLUG-055: a guest save that restores exactly the text a host edit replaced moments before is kept as
    the guest's choice and reaches the host; before, the save was read as a stale copy of the older text
    and the host's edit was written back over it."""
    guest = ctx.root / "guest"
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        lines = _five_lines(ctx, guest)
        original = "\n".join(lines) + "\n"
        edited = "\n".join(["line 1 host"] + lines[1:]) + "\n"
        ctx.write(MAIN, edited)
        _until(ctx, lambda: (guest / MAIN).read_text(encoding="utf-8") == edited, 30, "the host edit never reached the guest")
        _write(guest / MAIN, original)
        _both_hold(ctx, guest, {original}, "the guest's restore was undone")
        time.sleep(3)
        _both_hold(ctx, guest, {original}, "the restore did not hold")
    finally:
        _stop(ctx, ctx.root, guest)


def collab_simultaneous_same_line_edits(ctx: Context):
    """PLUG-056: host and guest save different versions of the same line within a millisecond, so both saves
    can be folded into the room before either reaches the other; both folders end with both versions between
    markers (either side may lead), never with the two lines one after the other."""
    guest = ctx.root / "guest"
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        lines = _five_lines(ctx, guest)
        host_lines, guest_lines = list(lines), list(lines)
        host_lines[2], guest_lines[2] = "line 3 host", "line 3 guest"
        ctx.write(MAIN, "\n".join(host_lines) + "\n")
        _write(guest / MAIN, "\n".join(guest_lines) + "\n")
        marked = lambda first, second: "\n".join(lines[:2] + [f"<<<<<<< {first}", f"line 3 {first}", "=======", f"line 3 {second}", ">>>>>>> room"] + lines[3:]) + "\n"
        _both_hold(ctx, guest, {marked("host", "guest"), marked("guest", "host")}, "same-line edits did not end between markers")
    finally:
        _stop(ctx, ctx.root, guest)


def collab_link_added_while_running(ctx: Context):
    """PLUG-057: a file link added with rbx lka while the session runs, whose source was saved earlier as an
    unshared file, reaches the guest together with renium-link.json; before, the first manifest save
    published nothing and the source never followed."""
    guest = ctx.root / "guest"
    source = "links/Logger.luau"
    try:
        started, _ = _start_host(ctx, "--local")
        _join(ctx, started["invite"], "guest")
        ctx.expect(not (guest / "renium-link.json").exists(), "the guest starts without a manifest")
        ctx.write(source, 'return "v1"\n')
        time.sleep(2)
        ctx.expect(not (guest / source).exists(), "an unlinked file stays local")
        ctx.rbx("lka", "--id", "logger", "--source", source, "--service", "ReplicatedStorage", "--path", '["ReplicatedStorage","Logger"]', timeout=60)
        _until(ctx, lambda: (guest / "renium-link.json").is_file() and (guest / source).is_file() and (guest / source).read_bytes() == (ctx.root / source).read_bytes(), 30, lambda: f"the late link did not reach the guest: manifest {(guest / 'renium-link.json').is_file()}, source {(guest / source).is_file()}")
        status = ctx.rbx("lks", cwd=guest, ok=None, timeout=60)
        ctx.expect(status.get("linkCount") == 1, f"guest lks: {status.json or status.error[:200]}")
    finally:
        _stop(ctx, ctx.root, guest)


SCENARIOS = [
    Scenario("PLUG-001", "scaffold, check and refusals", "small", scaffold_check_and_refusals, needs_studio=False),
    Scenario("PLUG-004", "manifest validation", "small", manifest_validation, needs_studio=False),
    Scenario("PLUG-006", "Windows entry must be .exe", "small", windows_entry_must_be_exe, needs_studio=False, platforms=("nt",)),
    Scenario("PLUG-007", "install, run, info and remove", "small", install_run_info_and_remove, needs_studio=False),
    Scenario("PLUG-011", "plugin protocol and limits", "small", plugin_protocol_and_limits, needs_studio=False),
    Scenario("PLUG-012", "pinned install detects changes", "small", pinned_install_detects_changes, needs_studio=False),
    Scenario("PLUG-016", "permissions, binding, session, list, big guide", "small", plugin_permissions_binding_and_session, needs_studio=False),
    Scenario("PLUG-019", "snapshot, process and runtime-check", "small", snapshot_process_and_runtime_check, needs_studio=False),
    Scenario("PLUG-021", "terminate the Studio the runner launched", "small", terminate_the_studio_the_runner_launched),
    Scenario("PLUG-025", "synced guides refresh", "small", synced_guides_refresh, needs_studio=False),
    Scenario("PLUG-040", "collab local host and guest", "small", collab_local_host_and_guest, needs_studio=False),
    Scenario("PLUG-043", "collab join overwrites", "small", collab_join_overwrites, needs_studio=False),
    Scenario("PLUG-045", "collab edits travel", "small", collab_edits_travel, needs_studio=False),
    Scenario("PLUG-050", "collab relay preferences", "small", collab_relay_preferences, needs_studio=False, platforms=("nt",)),
    Scenario("PLUG-051", "collab local relay", "small", collab_local_relay, needs_studio=False),
    Scenario("PLUG-054", "collab shares link files", "small", collab_shares_link_files, needs_studio=False),
    Scenario("PLUG-055", "collab restore after a remote edit", "small", collab_restore_after_remote_edit, needs_studio=False),
    Scenario("PLUG-056", "collab simultaneous same-line edits", "small", collab_simultaneous_same_line_edits, needs_studio=False),
    Scenario("PLUG-057", "collab link added while running", "small", collab_link_added_while_running, needs_studio=False),
]


def _leaves_files_writable(run):
    """A link applied in a row leaves read-only mirrors; make every file under the scratch copy writable
    again when a row ends, so the session runner can restore the copy from the fixture."""
    @functools.wraps(run)
    def wrapper(ctx: Context):
        try:
            run(ctx)
        finally:
            for path in ctx.root.rglob("*"):
                try:
                    if path.is_file() and not os.access(path, os.W_OK):
                        os.chmod(path, stat.S_IREAD | stat.S_IWRITE)
                except OSError:
                    pass
    return wrapper


for _scenario in SCENARIOS:
    _scenario.run = _leaves_files_writable(_scenario.run)
