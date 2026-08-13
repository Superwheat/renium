use std::collections::HashSet;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) mod transport;

use crate::app::output::global_yes;
use crate::app::timing::current_millis;
use crate::app::update;
use crate::automation;
use crate::automation::runtime::{
    automation_parse_response, oversized_automation_request_response, run_automation_stdio,
    send_automation_control_request, try_send_automation_control_request,
};
use crate::bytecode::explorer::watch_parent_and_exit;
use crate::cli::args::CursorPollArgs;
use crate::cli::{
    ApplyEditorDeleteArgs, ApplyEditorPropertyArgs, BridgeConnectionArgs, BridgeDaemonArgs,
    BridgeGetSourceArgs, EditorMutationArgs, ExecuteLuauArgs, ExportSnapshotsArgs,
    PluginConsoleOutputArgs, PushEditorChangesArgs, StartStopPlayArgs, StudioChangeStateArgs,
    StudioDeviceArgs,
};
use crate::daemon::transport::{
    BoundedLineRead, DAEMON_CONTROL_IDLE_TIMEOUT, DAEMON_DISCOVERY_MAX_AGE_MS,
    DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS, DEFAULT_DAEMON_CONTROL_PORT,
    MAX_DAEMON_CONTROL_CONNECTIONS, MAX_DAEMON_LINE_BYTES, host_port, is_loopback_endpoint,
    normalize_loopback_host, read_bounded_line,
};
use crate::snapshot::export::{fetch_text_chunks, parse_bridge_ports};
use crate::studio::bridge::{BridgeServer, clamp_bridge_chunk_size};
use crate::studio::target::place_filter;
use crate::system::files::{
    absolutize_for_daemon, canonical_path, fnv1a_hex, sanitize_ascii_identifier,
};

#[cfg(windows)]
pub(super) fn cursor_poll(args: CursorPollArgs) -> Result<()> {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyState, VK_LBUTTON};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, WindowFromPoint,
    };

    let interval = Duration::from_millis(args.interval_ms.max(1));
    let mut stdout = io::BufWriter::new(io::stdout().lock());
    loop {
        let mut point = POINT { x: 0, y: 0 };
        let got_cursor = unsafe { GetCursorPos(&mut point) } != 0;
        if got_cursor {
            let hwnd = unsafe { WindowFromPoint(point) };
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            if !hwnd.is_null() {
                unsafe {
                    GetWindowRect(hwnd, &mut rect);
                }
            }
            let left_down = unsafe { GetKeyState(VK_LBUTTON as i32) } as u16 & 0x8000 != 0;
            if writeln!(
                stdout,
                "{},{},{},{},{},{},{}",
                point.x,
                point.y,
                rect.left,
                rect.top,
                rect.right,
                rect.bottom,
                i32::from(left_down)
            )
            .is_err()
            {
                break;
            }
            if stdout.flush().is_err() {
                break;
            }
        }
        thread::sleep(interval);
    }
    Ok(())
}

#[cfg(not(windows))]
pub(super) fn cursor_poll(_args: CursorPollArgs) -> Result<()> {
    bail!("cursor-poll is only supported on Windows")
}

pub(super) fn bridge_daemon(args: BridgeDaemonArgs) -> Result<()> {
    let editor_stdio = args.editor_stdio;
    crate::app::context::set_automation_stdio(editor_stdio);
    let name = args
        .name
        .or_else(|| std::env::var("RENIUM_DAEMON_NAME").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string());
    if let Some(parent_pid) = args.parent_pid {
        watch_parent_and_exit(parent_pid);
    }
    let lifecycle_lock = update::acquire_lifecycle_lock()?;
    let bridge_host = normalize_loopback_host(&args.bridge.host)?;
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, listen_metrics) = BridgeServer::listen_with_initial_wait(
        &bridge_host,
        &ports,
        args.bridge.wait_seconds,
        false,
    )?;
    let bridge = Arc::new(bridge);
    let automation_state = Arc::new(automation::State::default());
    if !editor_stdio {
        spawn_daemon_control_server(
            &bridge_host,
            args.control_port,
            &name,
            bridge.clone(),
            automation_state.clone(),
            args.bridge.wait_seconds,
        )?;
        write_daemon_discovery_file(&name, &bridge_host, args.control_port, &ports)?;
    }
    drop(lifecycle_lock);

    println!(
        "[renium] daemon ready: channels={}/{}, bind_ms={:.1}, warmup_handshake_ms={:.1}",
        bridge.channel_count(),
        bridge.expected_channel_count(),
        listen_metrics.bind_ms,
        listen_metrics.wait_for_channels_ms
    );
    if editor_stdio {
        run_automation_stdio(&bridge, &automation_state, args.bridge.wait_seconds)?;
        bridge.alive.store(false, Ordering::Relaxed);
        return Ok(());
    }
    while bridge.alive.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(250));
    }
    bridge.alive.store(false, Ordering::Relaxed);
    println!("[renium] daemon stopped");
    Ok(())
}

fn spawn_daemon_control_server(
    host: &str,
    port: u16,
    name: &str,
    bridge: Arc<BridgeServer>,
    state: Arc<automation::State>,
    bridge_wait_seconds: f64,
) -> Result<()> {
    let bind_host = normalize_loopback_host(host)?;
    let listener = TcpListener::bind((bind_host.as_str(), port)).with_context(|| {
        let discoveries = daemon_discovery_write_paths(name)
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
        format!(
            "Failed to bind daemon control on {bind_host}:{port}; check {discoveries}, then run `rbx daemon clean` or choose another --control-port"
        )
    })?;
    println!("[renium] daemon control listening on {bind_host}:{port}");
    let active_connections = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        while bridge.alive.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let previous = active_connections.fetch_add(1, Ordering::Relaxed);
                    if previous >= MAX_DAEMON_CONTROL_CONNECTIONS {
                        active_connections.fetch_sub(1, Ordering::Relaxed);
                        eprintln!(
                            "[renium] warning: rejecting daemon control connection; {MAX_DAEMON_CONTROL_CONNECTIONS} connection limit reached"
                        );
                        continue;
                    }
                    let _ = stream.set_nonblocking(false);
                    let bridge = bridge.clone();
                    let state = state.clone();
                    let active_connections = active_connections.clone();
                    thread::spawn(move || {
                        let result = handle_daemon_control_connection(
                            stream,
                            &bridge,
                            &state,
                            bridge_wait_seconds,
                        );
                        active_connections.fetch_sub(1, Ordering::Relaxed);
                        if let Err(err) = result {
                            eprintln!("[renium] daemon control error: {err:#}");
                        }
                    });
                }
                Err(err) => {
                    if bridge.alive.load(Ordering::Relaxed) {
                        println!("[renium] warning: daemon control accept failed: {err}");
                        thread::sleep(Duration::from_millis(25));
                    }
                }
            }
        }
    });
    Ok(())
}

fn handle_daemon_control_connection(
    stream: TcpStream,
    bridge: &BridgeServer,
    state: &automation::State,
    bridge_wait_seconds: f64,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    let reader_stream = stream
        .try_clone()
        .context("Failed to clone control stream")?;
    let mut reader = BufReader::new(reader_stream);
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)
            .context("Failed to read daemon control request")?
        {
            BoundedLineRead::Eof => break,
            BoundedLineRead::Line => {}
            BoundedLineRead::TooLong => {
                let response = oversized_automation_request_response();
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = automation_parse_response(trimmed, state, bridge, bridge_wait_seconds);
        writeln!(writer, "{}", serde_json::to_string(&response)?)?;
        writer.flush()?;
    }
    Ok(())
}

pub(super) fn bridge_get_source(args: BridgeGetSourceArgs) -> Result<()> {
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, listen_metrics) =
        BridgeServer::listen(&args.bridge.host, &ports, args.bridge.wait_seconds)?;
    let source = bridge_fetch_source(
        &bridge,
        &args.service,
        &args.source_key,
        clamp_bridge_chunk_size(args.chunk_size),
    )?;
    let expected = match &args.expect_file {
        Some(path) => Some(
            fs::read_to_string(path)
                .with_context(|| format!("Failed to read expected file {}", path.display()))?,
        ),
        None => None,
    };
    let matches_expected = expected.as_ref().is_none_or(|expected| expected == &source);
    let summary = json!({
        "ok": matches_expected,
        "service": args.service,
        "sourceKey": args.source_key,
        "sourceLen": source.len(),
        "sourceHash": fnv1a_hex(source.as_bytes()),
        "expectedLen": expected.as_ref().map(String::len),
        "expectedHash": expected.as_ref().map(|value| fnv1a_hex(value.as_bytes())),
        "matchesExpected": matches_expected,
        "channels": bridge.channel_count(),
        "handshakeMs": listen_metrics.wait_for_channels_ms,
    });
    println!("__ROBLOX_SYNC_BRIDGE_SOURCE_RESULT__ {summary}");
    if !matches_expected {
        bail!("Studio source did not match expected file");
    }
    Ok(())
}

pub(super) fn bridge_fetch_source(
    bridge: &BridgeServer,
    service: &str,
    source_key: &str,
    chunk_size: usize,
) -> Result<String> {
    let (source, _) = fetch_text_chunks(chunk_size, |start_index, max_len| {
        bridge.call_chunk(
            "getSourceChunk",
            json!({
                "service": service,
                "instancePath": source_key,
                "startIndex": start_index,
                "maxLen": max_len,
            }),
        )
    })?;
    Ok(source)
}

fn write_daemon_discovery_file(
    name: &str,
    host: &str,
    control_port: u16,
    bridge_ports: &[u16],
) -> Result<()> {
    let bind_host = normalize_loopback_host(host)?;
    let process_start_identity = update::process_start_identity(std::process::id())
        .context("Could not read the daemon process start identity")?;
    let payload = json!({
        "schemaVersion": 2,
        "name": name,
        "host": bind_host,
        "controlPort": control_port,
        "bridgePorts": bridge_ports,
        "pid": std::process::id(),
        "processStartIdentity": process_start_identity,
        "updatedUnixMs": current_millis(),
    });
    let mut text = serde_json::to_vec_pretty(&payload)?;
    text.push(b'\n');
    let paths = daemon_discovery_write_paths(name);
    if paths.is_empty() {
        bail!("No daemon discovery path is available");
    }
    let mut errors = Vec::new();
    for path in paths {
        if let Err(error) = update::install_bytes(&path, &text) {
            errors.push(format!("{}: {error:#}", path.display()));
        }
    }
    if !errors.is_empty() {
        bail!(
            "Could not publish Renium daemon discovery: {}",
            errors.join("; ")
        );
    }
    Ok(())
}

fn daemon_discovery_write_paths(name: &str) -> Vec<PathBuf> {
    if let Ok(path) = std::env::var("RENIUM_DAEMON_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            return vec![PathBuf::from(path)];
        }
    }
    let Some(default) = local_app_data_daemon_path() else {
        return Vec::new();
    };
    if name == "default" {
        return vec![default];
    }
    vec![default.with_file_name(format!("daemon-{}.json", sanitize_ascii_identifier(name)))]
}

pub(super) fn daemon_control_endpoints() -> Vec<std::net::SocketAddr> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    if let Ok(raw) = std::env::var("RENIUM_DAEMON") {
        push_daemon_endpoint(&mut out, &mut seen, raw.trim());
    }

    let env_host = std::env::var("RENIUM_DAEMON_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    if let Ok(raw_port) = std::env::var("RENIUM_DAEMON_CONTROL_PORT")
        && let Ok(port) = raw_port.trim().parse::<u16>()
    {
        push_daemon_endpoint(&mut out, &mut seen, &host_port(env_host.trim(), port));
    }

    for path in daemon_discovery_paths() {
        if let Ok(text) = fs::read_to_string(&path)
            && let Ok(value) = serde_json::from_str::<Value>(&text)
            && let Some(endpoint) = daemon_discovery_endpoint(&value)
        {
            let key = endpoint.to_string();
            if seen.insert(key) {
                out.push(endpoint);
            }
        }
    }

    push_daemon_endpoint(
        &mut out,
        &mut seen,
        &host_port("127.0.0.1", DEFAULT_DAEMON_CONTROL_PORT),
    );
    out
}

pub(super) fn daemon_discovery_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("RENIUM_DAEMON_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            paths.push(PathBuf::from(path));
        }
    }
    if let Some(path) = local_app_data_daemon_path() {
        if let Ok(name) = std::env::var("RENIUM_DAEMON_NAME") {
            let name = name.trim();
            if !name.is_empty() && name != "default" {
                paths.push(
                    path.with_file_name(format!("daemon-{}.json", sanitize_ascii_identifier(name))),
                );
                return paths;
            }
        }
        paths.push(path);
    }
    paths
}

pub(super) fn local_app_data_daemon_path() -> Option<PathBuf> {
    if let Some(base) = std::env::var_os("LOCALAPPDATA") {
        return Some(PathBuf::from(base).join("Renium").join("daemon.json"));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".renium").join("daemon.json"))
}

fn daemon_discovery_endpoint(value: &Value) -> Option<std::net::SocketAddr> {
    let schema_version = value.get("schemaVersion").and_then(Value::as_u64);
    if schema_version != Some(2) {
        return None;
    }
    let host = normalize_loopback_host(value.get("host")?.as_str()?).ok()?;
    let port = value
        .get("controlPort")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())?;
    if port == 0 {
        return None;
    }
    let pid = value
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())?;
    let process_start_identity = value.get("processStartIdentity")?.as_str()?;
    let updated_ms = value.get("updatedUnixMs").and_then(Value::as_u64)? as u128;
    let now = current_millis();
    if updated_ms > now.saturating_add(DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS)
        || now.saturating_sub(updated_ms) > DAEMON_DISCOVERY_MAX_AGE_MS
        || !is_process_alive(pid)
        || update::process_start_identity(pid).as_deref() != Some(process_start_identity)
    {
        return None;
    }
    host_port(&host, port)
        .to_socket_addrs()
        .ok()?
        .find(is_loopback_endpoint)
}

pub(super) fn is_process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
        if handle.is_null() {
            return false;
        }
        let alive = WaitForSingleObject(handle, 0) == 0x0000_0102;
        CloseHandle(handle);
        alive
    }
    #[cfg(not(windows))]
    {
        unsafe {
            libc::kill(pid as libc::pid_t, 0) == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        }
    }
}

fn push_daemon_endpoint(
    out: &mut Vec<std::net::SocketAddr>,
    seen: &mut HashSet<String>,
    raw: &str,
) {
    let text = raw.trim();
    if text.is_empty() {
        return;
    }
    if let Ok(address) = text.parse::<std::net::SocketAddr>() {
        if !is_loopback_endpoint(&address) {
            return;
        }
        let key = address.to_string();
        if seen.insert(key) {
            out.push(address);
        }
        return;
    }
    if let Ok(addresses) = text.to_socket_addrs() {
        for address in addresses {
            if !is_loopback_endpoint(&address) {
                continue;
            }
            let key = address.to_string();
            if seen.insert(key) {
                out.push(address);
            }
        }
    }
}

pub(super) fn try_daemon_control_request(
    command: &str,
    args: Vec<String>,
) -> Result<Option<Value>> {
    let has = |needle: &str| args.iter().any(|argument| argument == needle);
    let explicitly_approved = (has("--yes") || has("--apply")) && !has("--no-review");
    let (operation, mut parameters) = match command {
        "x" | "export" | "export-snapshots" => (
            if has("-i") || has("--run-import") {
                10
            } else {
                44
            },
            json!({ "a": args }),
        ),
        "push" | "push-editor-changes" => {
            let incremental = has("-p")
                || has("--changed-path")
                || has("-f")
                || has("--changed-paths-file")
                || has("-i")
                || has("--target-settings-id")
                || has("-t")
                || has("--target-property");
            (11, json!({ "a": args, "destructive": !incremental }))
        }
        "prop" | "apply-editor-property" => (31, json!({ "a": args, "editor": true })),
        "del" | "apply-editor-delete" => (36, json!({ "a": args, "editor": true })),
        "co" | "console" | "get-console-output" => (55, json!({ "a": args })),
        "lx" | "luau" | "execute-luau" => (54, json!({ "a": args })),
        "device" | "dev" | "studio-device" => (59, json!({ "a": args })),
        "play" | "start-stop-play" => (
            if has("-x") || has("--stop") { 57 } else { 56 },
            json!({ "a": args }),
        ),
        "test" => (56, json!({ "a": args, "test": true })),
        "review" | "editor-review-decision" => (81, json!({ "a": args, "studioDecision": true })),
        "clients" | "list-clients" => (50, json!({})),
        "press" => (61, json!({ "a": args })),
        "click" => (62, json!({ "a": args })),
        "key" => (63, json!({ "a": args })),
        "ui" => (60, json!({ "a": args })),
        "type" => (64, json!({ "a": args })),
        "wait-until" => (65, json!({ "a": args })),
        "goto" => (66, json!({ "a": args })),
        "shot" => (58, json!({ "a": args })),
        "st" | "studio-change-state" => (
            if has("--stop") {
                13
            } else if has("--no-start") {
                14
            } else {
                12
            },
            json!({ "a": args }),
        ),
        _ => return Ok(None),
    };
    let root = parameters
        .get("a")
        .and_then(Value::as_array)
        .and_then(|arguments| {
            arguments.iter().enumerate().find_map(|(index, argument)| {
                matches!(argument.as_str(), Some("-r" | "--root" | "--project-root"))
                    .then_some(index)
                    .and_then(|index| {
                        arguments
                            .get(index + 1)
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    })
            })
        })
        .unwrap_or_else(|| {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .display()
                .to_string()
        });
    let Some(bind) = try_send_automation_control_request(&automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: 1,
        cx: None,
        p: json!({ "root": root, "place": place_filter() }),
    })?
    else {
        return Ok(None);
    };
    if bind.ok == 0 {
        let error = bind.e.context("Daemon bind failed without an error")?;
        bail!("{}", error.m);
    }
    let context_id = bind
        .r
        .as_ref()
        .and_then(|result| result.get("id"))
        .and_then(Value::as_u64)
        .context("Daemon bind response omitted the context ID")?;
    let requires_review = parameters.get("destructive").and_then(Value::as_bool) == Some(true)
        || explicitly_approved && matches!(operation, 11 | 31);
    let response = if requires_review {
        let prepare = send_automation_control_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: 80,
            cx: Some(context_id),
            p: json!({ "op": operation, "p": parameters }),
        })?;
        if prepare.ok == 0 {
            let error = prepare
                .e
                .context("Review preparation failed without an error")?;
            bail!("{}", error.m);
        }
        let review_id = prepare
            .r
            .as_ref()
            .and_then(|result| result.get("reviewId"))
            .and_then(Value::as_str)
            .context("Review preparation omitted reviewId")?;
        send_automation_control_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: 81,
            cx: Some(context_id),
            p: json!({ "reviewId": review_id }),
        })?
    } else {
        send_automation_control_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: operation,
            cx: Some(context_id),
            p: std::mem::take(&mut parameters),
        })?
    };
    if response.ok == 0 {
        let error = response
            .e
            .context("Daemon request failed without an error")?;
        bail!("{}", error.m);
    }
    Ok(Some(response.r.unwrap_or_else(|| json!({}))))
}

pub(super) fn get_console_output_daemon_args(args: &PluginConsoleOutputArgs) -> Vec<String> {
    let mut out = vec![
        "-n".to_string(),
        args.limit.to_string(),
        "-s".to_string(),
        args.since_seq.to_string(),
    ];
    if args.clear {
        out.push("-c".to_string());
    }
    if args.from_oldest {
        out.push("--from-oldest".to_string());
    }
    if args.client {
        out.push("--client".to_string());
    }
    if let Some(player) = args.player.as_ref() {
        out.push("--player".to_string());
        out.push(player.clone());
    }
    if let Some(grep) = args.grep.as_ref() {
        out.push("--grep".to_string());
        out.push(grep.clone());
    }
    if let Some(level) = args.level.as_ref() {
        out.push("--level".to_string());
        out.push(level.clone());
    }
    out
}

pub(super) fn execute_luau_daemon_args(args: &ExecuteLuauArgs) -> Vec<String> {
    let mut out = vec!["-t".to_string(), args.timeout.to_string()];
    if args.client {
        out.push("-c".to_string());
    }
    if let Some(player) = args.player.as_ref() {
        out.push("--player".to_string());
        out.push(player.clone());
    }
    if let Some(code) = args.code.as_ref() {
        out.push("-e".to_string());
        out.push(code.clone());
        return out;
    }
    if let Some(path) = args.file.as_ref() {
        let path = canonical_path(path).unwrap_or_else(|_| path.clone());
        out.push("-f".to_string());
        out.push(path.display().to_string());
        return out;
    }
    out
}

pub(super) fn studio_device_daemon_args(args: &StudioDeviceArgs) -> Vec<String> {
    let mut out = vec![args.action.clone()];
    if let Some(device) = args.device.as_ref() {
        out.push(device.clone());
    }
    if let Some(orientation) = args.orientation.as_ref() {
        out.push("--orientation".to_string());
        out.push(orientation.clone());
    }
    if let Some(scaling_mode) = args.scaling_mode.as_ref() {
        out.push("--scaling".to_string());
        out.push(scaling_mode.clone());
    }
    if let Some(resolution) = args.resolution.as_ref() {
        out.push("--resolution".to_string());
        out.push(resolution.clone());
    }
    if let Some(pixel_density) = args.pixel_density {
        out.push("--pixel-density".to_string());
        out.push(pixel_density.to_string());
    }
    out
}

pub(super) fn start_stop_play_daemon_args(args: &StartStopPlayArgs) -> Vec<String> {
    let mut out = Vec::new();
    if args.start {
        out.push("-s".to_string());
    }
    if args.stop {
        out.push("-x".to_string());
    }
    if let Some(players) = args.players {
        out.push("--players".to_string());
        out.push(players.to_string());
    }
    if let Some(mode) = args.mode.as_ref() {
        out.push("--mode".to_string());
        out.push(mode.clone());
    }
    out
}

pub(super) fn studio_change_state_daemon_args(args: &StudioChangeStateArgs) -> Vec<String> {
    let mut out = Vec::new();
    if !args.services.trim().is_empty() {
        out.push("-s".to_string());
        out.push(args.services.clone());
    }
    if args.reset {
        out.push("--reset".to_string());
    }
    if args.replace_services {
        out.push("--replace-services".to_string());
    }
    if args.clear_pending {
        out.push("--clear-pending".to_string());
    }
    if args.no_start {
        out.push("--no-start".to_string());
    }
    if args.stop {
        out.push("--stop".to_string());
    }
    if let Some(ack_seq) = args.ack_seq {
        out.push("--ack-seq".to_string());
        out.push(ack_seq.to_string());
    }
    if !args.ack_actions.is_empty() {
        out.push("--ack-actions".to_string());
        out.push(args.ack_actions.join(","));
    }
    if args.ack_action_results != "{}" {
        out.push("--ack-action-results".to_string());
        out.push(args.ack_action_results.clone());
    }
    if let Some(runtime_id) = args.runtime_id.as_ref() {
        out.push("--runtime-id".to_string());
        out.push(runtime_id.clone());
    }
    if let Some(suppress_seconds) = args.suppress_seconds {
        out.push("--suppress-seconds".to_string());
        out.push(suppress_seconds.to_string());
    }
    if let Some(wait_seconds) = args.wait_seconds {
        out.push("--wait-seconds".to_string());
        out.push(wait_seconds.to_string());
    }
    if args.context_bound {
        out.push("--context-bound".to_string());
    }
    out
}

pub(super) fn export_snapshots_daemon_args(args: &ExportSnapshotsArgs) -> Vec<String> {
    let mut out = vec![
        "-r".to_string(),
        absolutize_for_daemon(&args.project_root)
            .display()
            .to_string(),
        "--src-dir".to_string(),
        args.src_dir.display().to_string(),
        "-d".to_string(),
        args.snapshot_dir.display().to_string(),
        "-s".to_string(),
        args.services.clone(),
        "-c".to_string(),
        args.chunk_size.to_string(),
        "-a".to_string(),
        args.adaptive_seed_batch.to_string(),
        "-w".to_string(),
        args.bridge.wait_seconds.to_string(),
        "-H".to_string(),
        args.bridge.host.clone(),
        "-P".to_string(),
        args.bridge.ports.clone(),
        "-m".to_string(),
        args.import_mode.clone(),
        "--sw".to_string(),
        args.source_workers.to_string(),
        "--iw".to_string(),
        args.instance_workers.to_string(),
        "--mw".to_string(),
        args.import_workers.to_string(),
        "--perf".to_string(),
        args.performance_mode.clone(),
    ];
    if args.run_import {
        out.push("-i".to_string());
    }
    if args.no_run_import {
        out.push("--no-import".to_string());
    }
    if args.modified_default_bypass {
        out.push("--mdb".to_string());
    }
    if args.no_modified_default_bypass {
        out.push("--no-mdb".to_string());
    }
    if args.no_adaptive_throttle {
        out.push("--no-adaptive-throttle".to_string());
    }
    if args.export_all_properties {
        out.push("--all-props".to_string());
    }
    if args.no_export_all_properties {
        out.push("--no-props".to_string());
    }
    if args.quiet_timings {
        out.push("-q".to_string());
    }
    out
}

fn editor_daemon_args(
    project_root: &Path,
    src_dir: &Path,
    bridge: &BridgeConnectionArgs,
) -> Vec<String> {
    vec![
        "-r".to_string(),
        absolutize_for_daemon(project_root).display().to_string(),
        "-d".to_string(),
        src_dir.display().to_string(),
        "-w".to_string(),
        bridge.wait_seconds.to_string(),
        "-H".to_string(),
        bridge.host.clone(),
        "-P".to_string(),
        bridge.ports.clone(),
    ]
}

pub(super) fn push_editor_changes_daemon_args(args: &PushEditorChangesArgs) -> Vec<String> {
    let mut out = editor_daemon_args(
        &args.project.project_root,
        &args.project.src_root,
        &args.bridge,
    );
    for path in &args.changed_paths {
        out.push("-p".to_string());
        out.push(path.display().to_string());
    }
    for path in &args.changed_paths_files {
        out.push("-f".to_string());
        out.push(absolutize_for_daemon(path).display().to_string());
    }
    for settings_id in &args.target_settings_ids {
        out.push("-i".to_string());
        out.push(settings_id.clone());
    }
    for path in &args.target_settings_id_files {
        out.push("-I".to_string());
        out.push(absolutize_for_daemon(path).display().to_string());
    }
    for property in &args.target_properties {
        out.push("-t".to_string());
        out.push(property.clone());
    }
    if args.upsert_instances_only {
        out.push("-u".to_string());
    }
    if args.probe_events {
        out.push("-e".to_string());
    }
    if args.verify_sources {
        out.push("--verify-sources".to_string());
    }
    if args.no_review {
        out.push("--no-review".to_string());
    }
    if args.yes || global_yes() {
        out.push("--yes".to_string());
    }
    if args.override_packages {
        out.push("--override-packages".to_string());
    }
    if let Some(cache_dir) = &args.link_cache_dir {
        out.push("--link-cache-dir".to_string());
        out.push(cache_dir.display().to_string());
    }
    out
}

pub(super) fn apply_editor_property_daemon_args(args: &ApplyEditorPropertyArgs) -> Vec<String> {
    let mut out = editor_mutation_daemon_args(&args.target);
    out.extend([
        "-S".to_string(),
        args.scope.clone(),
        "-n".to_string(),
        args.property.clone(),
        format!("--value-json={}", args.value_json),
    ]);
    if args.no_review {
        out.push("--no-review".to_string());
    }
    if args.yes || global_yes() {
        out.push("--yes".to_string());
    }
    out
}

pub(super) fn apply_editor_delete_daemon_args(args: &ApplyEditorDeleteArgs) -> Vec<String> {
    editor_mutation_daemon_args(&args.target)
}

fn editor_mutation_daemon_args(target: &EditorMutationArgs) -> Vec<String> {
    let mut out = editor_daemon_args(
        &target.project.project_root,
        &target.project.src_root,
        &target.bridge,
    );
    out.extend([
        "-s".to_string(),
        target.service.clone(),
        "-c".to_string(),
        target.class_name.clone(),
        "-p".to_string(),
        target.path_segments_json.clone(),
        "-o".to_string(),
        target.path_ordinals_json.clone(),
    ]);
    if let Some(settings_id) = target.settings_id.as_ref() {
        out.push("-i".to_string());
        out.push(settings_id.clone());
    }
    if target.override_packages {
        out.push("--override-packages".to_string());
    }
    out
}
