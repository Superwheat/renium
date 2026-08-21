use std::collections::HashSet;
use std::fs;
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

pub(crate) mod transport;

use crate::app::timing::current_millis;
use crate::app::update;
use crate::automation;
use crate::automation::client::{
    run_stdio_proxy, send_request, shared_daemon_available, try_send_request,
};
use crate::automation::runtime::{
    automation_parse_response, oversized_automation_request_response, run_automation_stdio,
};
use crate::bytecode::explorer::watch_parent_and_exit;
use crate::cli::args::CursorPollArgs;
use crate::cli::{BridgeDaemonArgs, BridgeGetSourceArgs};
use crate::daemon::transport::{
    BoundedLineRead, DAEMON_CONTROL_IDLE_TIMEOUT, DAEMON_DISCOVERY_MAX_AGE_MS,
    DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS, DEFAULT_DAEMON_CONTROL_PORT,
    MAX_DAEMON_CONTROL_CONNECTIONS, MAX_DAEMON_LINE_BYTES, host_port, is_loopback_endpoint,
    normalize_loopback_host, read_bounded_line,
};
use crate::snapshot::export::{fetch_text_chunks, parse_bridge_ports};
use crate::studio::bridge::{BridgeServer, clamp_bridge_chunk_size};
use crate::studio::target::place_filter;
use crate::system::files::{absolutize_for_daemon, fnv1a_hex, sanitize_ascii_identifier};

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
    if editor_stdio && shared_daemon_available() {
        return run_stdio_proxy();
    }
    let lifecycle_lock = update::acquire_lifecycle_lock()?;
    if editor_stdio && shared_daemon_available() {
        drop(lifecycle_lock);
        return run_stdio_proxy();
    }
    #[cfg(windows)]
    crate::studio::input::watch_auto_recovery_dialogs();
    let automation_state = Arc::new(automation::State::default());
    check_for_available_update(Arc::clone(&automation_state));
    let bridge_host = normalize_loopback_host(&args.bridge.host)?;
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, listen_metrics) =
        BridgeServer::listen_daemon(&bridge_host, &ports, args.bridge.wait_seconds)?;
    let bridge = Arc::new(bridge);
    spawn_daemon_control_server(
        &bridge_host,
        args.control_port,
        &name,
        Arc::clone(&bridge),
        Arc::clone(&automation_state),
        args.bridge.wait_seconds,
    )?;
    write_daemon_discovery_file(&name, &bridge_host, args.control_port, &ports)?;
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

fn check_for_available_update(state: Arc<automation::State>) {
    thread::spawn(move || match update::available_release_version() {
        Ok(version) => {
            if let Some(version) = version.as_deref() {
                eprintln!("[renium] update available: {version}; run `rbx update` to install it");
            }
            state.set_available_update(version);
        }
        Err(error) => eprintln!("[renium] update check failed: {error:#}"),
    });
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
                    let bridge = Arc::clone(&bridge);
                    let state = Arc::clone(&state);
                    let active_connections = Arc::clone(&active_connections);
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
    bridge: &Arc<BridgeServer>,
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
        if !out.is_empty() {
            return out;
        }
    }

    let env_host = std::env::var("RENIUM_DAEMON_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    if let Ok(raw_port) = std::env::var("RENIUM_DAEMON_CONTROL_PORT")
        && let Ok(port) = raw_port.trim().parse::<u16>()
    {
        push_daemon_endpoint(&mut out, &mut seen, &host_port(env_host.trim(), port));
        if !out.is_empty() {
            return out;
        }
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

fn try_bind_daemon_context(project_root: Option<&Path>) -> Result<Option<Value>> {
    let root = project_root
        .map_or_else(
            || std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            absolutize_for_daemon,
        )
        .display()
        .to_string();
    let Some(bind) = try_send_request(&automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: automation::op::BIND,
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
    bind.r
        .context("Daemon bind response omitted its context")
        .map(Some)
}

#[cfg(unix)]
fn detach_daemon(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(unix)]
fn spawn_shared_daemon(executable: &Path, ports: &str, wait_seconds: f64) -> io::Result<()> {
    let mut command = Command::new(executable);
    command
        .arg("bridge-daemon")
        .arg("--ports")
        .arg(ports)
        .arg("--wait-seconds")
        .arg(wait_seconds.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_daemon(&mut command);
    command.spawn().map(|_| ())
}

#[cfg(windows)]
fn spawn_shared_daemon(executable: &Path, ports: &str, wait_seconds: f64) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Start-Process -FilePath $env:RENIUM_DAEMON_EXECUTABLE -ArgumentList @('bridge-daemon','--ports',$env:RENIUM_DAEMON_PORTS,'--wait-seconds',$env:RENIUM_DAEMON_WAIT) -WindowStyle Hidden",
        ])
        .env("RENIUM_DAEMON_EXECUTABLE", executable)
        .env("RENIUM_DAEMON_PORTS", ports)
        .env("RENIUM_DAEMON_WAIT", wait_seconds.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("failed to start the shared Renium daemon"))
    }
}

pub(crate) fn start_shared_daemon(ports: &str, wait_seconds: f64) -> bool {
    let Ok(executable) = std::env::current_exe() else {
        return false;
    };
    if spawn_shared_daemon(&executable, ports, wait_seconds).is_err() {
        return false;
    }
    let deadline = Instant::now() + Duration::from_secs(1);
    while Instant::now() < deadline {
        if shared_daemon_available() {
            return true;
        }
        thread::sleep(Duration::from_millis(25));
    }
    shared_daemon_available()
}

pub(crate) fn try_daemon_project_root(project_root: &Path) -> Result<Option<PathBuf>> {
    try_bind_daemon_context(Some(project_root))?
        .map(|context| {
            context
                .get("root")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .context("Daemon bind response omitted the project root")
        })
        .transpose()
}

pub(super) fn try_daemon_control_request(
    operation: u16,
    project_root: Option<&Path>,
    mut parameters: Value,
    approved: bool,
) -> Result<Option<Value>> {
    let opcode = automation::opcode_by_id(operation)?;
    let bridge_wait_seconds = parameters
        .get("bridgeWaitSeconds")
        .and_then(Value::as_f64)
        .unwrap_or(8.0)
        .clamp(1.0, 30.0);
    let bridge_ports = parameters
        .get("bridgePorts")
        .and_then(Value::as_str)
        .unwrap_or("8781,8782")
        .to_string();
    let object = parameters
        .as_object_mut()
        .context("Daemon operation parameters must be a JSON object")?;
    if operation == automation::op::STUDIOS {
        let Some(response) = try_send_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: operation,
            cx: None,
            p: std::mem::take(&mut parameters),
        })?
        else {
            return Ok(None);
        };
        if response.ok == 0 {
            let error = response
                .e
                .context("Daemon request failed without an error")?;
            bail!("{}", error.m);
        }
        return Ok(Some(response.r.unwrap_or_else(|| json!({}))));
    }
    let needs_runtime = opcode.runtime
        || matches!(
            operation,
            automation::op::SET_PROPERTY | automation::op::REMOVE
        ) && object.get("editor").and_then(Value::as_bool) == Some(true);
    let mut context = try_bind_daemon_context(project_root)?;
    let ready = |context: &Option<Value>| {
        context.as_ref().is_some_and(|context| {
            !needs_runtime || context.get("runtimeId").and_then(Value::as_str).is_some()
        })
    };
    if needs_runtime && context.is_none() {
        start_shared_daemon(&bridge_ports, bridge_wait_seconds);
    }
    if needs_runtime && !ready(&context) && shared_daemon_available() {
        let deadline = Instant::now() + Duration::from_secs_f64(bridge_wait_seconds);
        let mut bind_error = None;
        while !ready(&context) && Instant::now() < deadline {
            match try_bind_daemon_context(project_root) {
                Ok(bound) => context = bound,
                Err(error) => bind_error = Some(error),
            }
            if !ready(&context) {
                thread::sleep(Duration::from_millis(50));
            }
        }
        if !ready(&context) {
            if let Some(error) = bind_error {
                return Err(error);
            }
            bail!("No Studio runtime connected to this project within {bridge_wait_seconds:.1}s");
        }
    }
    let Some(context) = context else {
        return Ok(None);
    };
    let context_id = context
        .get("id")
        .and_then(Value::as_u64)
        .context("Daemon bind response omitted the context ID")?;
    let requires_review =
        approved || object.get("destructive").and_then(Value::as_bool) == Some(true);
    let response = if requires_review {
        let prepare = send_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: automation::op::REVIEW_PREPARE,
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
        send_request(&automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: automation::op::REVIEW_APPLY,
            cx: Some(context_id),
            p: json!({ "reviewId": review_id }),
        })?
    } else {
        send_request(&automation::Request {
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
