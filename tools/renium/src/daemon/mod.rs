use std::collections::HashSet;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

mod control_reader;
pub(crate) mod memory;
pub(crate) mod transport;

use crate::app::timing::current_millis;
use crate::app::update;
use crate::automation;
use crate::automation::client::{
    DeadlineReader, run_stdio_proxy, send_request, shared_daemon_available, try_send_request,
};
use crate::automation::runtime::{
    automation_parse_response_with_lease, oversized_automation_request_response,
};
use crate::bytecode::explorer::watch_parent_and_exit;
use crate::cli::BridgeDaemonArgs;
use crate::cli::args::CursorPollArgs;
use crate::daemon::transport::{
    DAEMON_CONTROL_IDLE_TIMEOUT, DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS, DEFAULT_DAEMON_CONTROL_PORT,
    MAX_DAEMON_CONTROL_CONNECTIONS, host_port, is_loopback_endpoint, normalize_loopback_host,
};
use crate::snapshot::export::parse_bridge_ports;
use crate::studio::bridge::{BridgeRequestLease, BridgeServer};
use crate::studio::target::place_filter;
use crate::system::files::{absolutize_for_daemon, sanitize_ascii_identifier};

const SHARED_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_CONTROL_LEASE_ID: AtomicU64 = AtomicU64::new(1);
static CONTROL_LEASE_BOOT_ID: LazyLock<String> =
    LazyLock::new(|| format!("{:x}-{:x}", current_millis(), std::process::id()));

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
    // The command that started this daemon may carry a place pin; requests
    // bring their own, and threads outside a request must not inherit it.
    crate::studio::target::set_place_filter(None);
    let name = args
        .name
        .clone()
        .or_else(|| std::env::var("RENIUM_DAEMON_NAME").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "default".to_string());
    // Resolve --name before discovery, including the editor proxy path. A
    // missing named daemon must never connect to the default daemon instead.
    unsafe { std::env::set_var("RENIUM_DAEMON_NAME", &name) };
    let editor_stdio = args.editor_stdio;
    crate::app::context::set_automation_stdio(editor_stdio);
    if editor_stdio {
        if !shared_daemon_available()
            && !start_shared_daemon_on(
                &args.bridge.ports,
                args.bridge.wait_seconds,
                args.control_port,
            )
        {
            bail!("Renium could not start the shared daemon");
        }
        if let Some(parent_pid) = args.parent_pid {
            watch_parent_and_exit(parent_pid);
        }
        return run_stdio_proxy(args.bridge.ports, args.bridge.wait_seconds);
    }
    let lifecycle_lock = update::acquire_lifecycle_lock()?;
    if shared_daemon_available() {
        drop(lifecycle_lock);
        return Ok(());
    }
    if let Some(parent_pid) = args.parent_pid {
        watch_parent_and_exit(parent_pid);
    }
    crate::app::output::open_daemon_log();
    release_freed_memory_promptly();
    #[cfg(windows)]
    if is_default_daemon() {
        crate::studio::input::watch_auto_recovery_dialogs();
    }
    let automation_state = Arc::new(automation::State::default());
    check_for_available_update(Arc::clone(&automation_state));
    let bridge_host = normalize_loopback_host(&args.host)?;
    let ports = parse_bridge_ports(&args.bridge.ports)?;
    let (bridge, listen_metrics) =
        BridgeServer::listen_daemon(&bridge_host, &ports, args.bridge.wait_seconds)?;
    let bridge = Arc::new(bridge);
    memory::mark_started();
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
    if let Ok(dir) = crate::app::update::user_data_dir() {
        let _ = std::fs::remove_dir_all(dir.join("cache").join("play-teleport"));
    }
    let mut audio_check = Instant::now();
    while bridge.alive.load(Ordering::Relaxed) {
        if is_default_daemon() && audio_check.elapsed() >= Duration::from_secs(2) {
            let _ = crate::studio::audio::global::resume();
            audio_check = Instant::now();
        }
        memory::release_when_idle();
        thread::sleep(Duration::from_millis(250));
    }
    bridge.alive.store(false, Ordering::Relaxed);
    println!("[renium] daemon stopped");
    Ok(())
}

fn release_freed_memory_promptly() {
    const MI_OPTION_PURGE_DECOMMITS: libmimalloc_sys::mi_option_t = 5;
    const MI_OPTION_ABANDONED_PAGE_PURGE: libmimalloc_sys::mi_option_t = 12;
    const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
    unsafe {
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DECOMMITS, 1);
        libmimalloc_sys::mi_option_set(MI_OPTION_ABANDONED_PAGE_PURGE, 1);
        libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0);
    }
}

fn check_for_available_update(state: Arc<automation::State>) {
    thread::spawn(move || match update::available_release_version() {
        Ok(version) => {
            if let Some(version) = version.as_deref() {
                eprintln!("[renium] update available: {version}; run `rbx upd`");
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
    state
        .authority
        .set(automation::authorization::Authority::initialize(
            listener.local_addr()?.port(),
        )?)
        .map_err(|_| anyhow::anyhow!("Daemon control authority was already initialized"))?;
    println!("[renium] daemon control listening on {bind_host}:{port}");
    let active_connections = Arc::new(AtomicUsize::new(0));
    thread::spawn(move || {
        while bridge.alive.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let Some(stream) = admit_control_connection(stream, &active_connections) else {
                        continue;
                    };
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

/// Counts a new control connection in, or answers it `busy` at the limit.
fn admit_control_connection(stream: TcpStream, active: &AtomicUsize) -> Option<TcpStream> {
    if active.fetch_add(1, Ordering::Relaxed) < MAX_DAEMON_CONTROL_CONNECTIONS {
        return Some(stream);
    }
    active.fetch_sub(1, Ordering::Relaxed);
    eprintln!(
        "[renium] warning: rejecting daemon control connection; {MAX_DAEMON_CONTROL_CONNECTIONS} connection limit reached"
    );
    refuse_control_connection(&stream);
    None
}

/// A connection over the limit gets this long to deliver its request; the
/// accept loop waits on it, and a caller writes its request as it connects.
const REFUSED_REQUEST_WAIT: Duration = Duration::from_millis(500);

/// Answers the request of a connection over the limit with a `busy` failure.
/// The request is read first: closing a socket that still holds unread data
/// resets the connection, and the caller would get a socket error in place
/// of the answer.
fn refuse_control_connection(stream: &TcpStream) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_write_timeout(Some(REFUSED_REQUEST_WAIT));
    let mut request = String::new();
    let _ = transport::read_bounded_line(
        &mut io::BufReader::new(DeadlineReader {
            stream,
            deadline: Instant::now() + REFUSED_REQUEST_WAIT,
        }),
        &mut request,
        transport::MAX_DAEMON_LINE_BYTES,
    );
    let id =
        serde_json::from_str::<RequestEnvelope>(request.trim()).map_or(0, |request| request.id);
    let response = automation::Response::failure(
        id,
        Instant::now(),
        automation::Failure::new(
            "busy",
            format!(
                "The Renium daemon has {MAX_DAEMON_CONTROL_CONNECTIONS} open connections, its limit; retry when a running command finishes"
            ),
            true,
            "retry",
        ),
    );
    if let Ok(line) = serde_json::to_string(&response) {
        let mut stream = stream;
        let _ = writeln!(stream, "{line}");
    }
}

thread_local! {
    static REQUEST_NOTICE_SINK: std::cell::RefCell<Option<(crate::system::net::SharedTcpStream, u64)>> =
        const { std::cell::RefCell::new(None) };
}

/// Sends an interim notice line to the caller of the request this thread is
/// serving, when that caller asked for notices (`p.notices`); a request
/// served without one, or outside the daemon, gets nothing.
pub(crate) fn emit_request_notice(text: &str) {
    let sink = REQUEST_NOTICE_SINK.with(|sink| sink.borrow().clone());
    let Some((mut stream, id)) = sink else {
        return;
    };
    let Ok(line) = serde_json::to_string(&automation::Response::notice(id, text)) else {
        return;
    };
    let _ = writeln!(stream, "{line}").and_then(|()| stream.flush());
}

#[derive(serde::Deserialize)]
struct RequestEnvelope {
    #[serde(default)]
    id: u64,
    #[serde(default)]
    p: Value,
}

fn request_notice_sink(
    line: &str,
    stream: &crate::system::net::SharedTcpStream,
) -> Option<(crate::system::net::SharedTcpStream, u64)> {
    let envelope = serde_json::from_str::<RequestEnvelope>(line).ok()?;
    (envelope.p.get("notices").and_then(Value::as_bool) == Some(true))
        .then(|| (stream.clone(), envelope.id))
}

/// Every wire envelope asks for notices itself, at the top level the
/// connection reader looks at; the operation's own parameters stay free of
/// transport fields, since a review stores them and runs them later on the
/// apply request's connection.
fn request_with_notices(mut parameters: Value) -> Value {
    if let Some(object) = parameters.as_object_mut() {
        object.insert("notices".to_string(), Value::Bool(true));
    }
    parameters
}

fn review_prepare_parameters(operation: u16, parameters: Value) -> Value {
    request_with_notices(json!({ "op": operation, "p": parameters }))
}

fn review_apply_parameters(review_id: &str) -> Value {
    request_with_notices(json!({ "reviewId": review_id }))
}

/// A command's wire requests: a reviewed operation travels as a review-prepare
/// carrying its parameters and a review-apply naming the receipt, any other
/// operation as itself.
fn send_operation_requests(
    operation: u16,
    context_id: u64,
    parameters: Value,
    requires_review: bool,
    mut send: impl FnMut(automation::Request) -> Result<automation::Response>,
) -> Result<automation::Response> {
    let request = |op, p| automation::Request {
        v: automation::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op,
        cx: Some(context_id),
        p,
    };
    if !requires_review {
        return send(request(operation, request_with_notices(parameters)));
    }
    let prepare = send(request(
        automation::op::REVIEW_PREPARE,
        review_prepare_parameters(operation, parameters),
    ))?;
    if prepare.ok == 0 {
        let error = prepare
            .e
            .context("Review preparation failed without an error")?;
        return Err(automation::DaemonFailure::from(error).into());
    }
    let review_id = prepare
        .r
        .as_ref()
        .and_then(|result| result.get("reviewId"))
        .and_then(Value::as_str)
        .context("Review preparation omitted reviewId")?;
    send(request(
        automation::op::REVIEW_APPLY,
        review_apply_parameters(review_id),
    ))
}

fn handle_daemon_control_connection(
    stream: TcpStream,
    bridge: &Arc<BridgeServer>,
    state: &automation::State,
    bridge_wait_seconds: f64,
) -> Result<()> {
    let _ = stream.set_read_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    let _ = stream.set_write_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    let stream = crate::system::net::SharedTcpStream::from(stream);
    let monitor_bridge = Arc::clone(bridge);
    let reader = control_reader::ControlReader::start(stream.clone(), move |lease| {
        let _ = monitor_bridge.cancel_request_lease(lease.id());
    })?;
    let notice_stream = stream.clone();
    let mut writer = BufWriter::new(stream);
    while let Some(request) = reader.next() {
        let line = match request {
            control_reader::Request::Line(line) => line,
            control_reader::Request::TooLong => {
                let response = oversized_automation_request_response();
                writeln!(writer, "{}", serde_json::to_string(&response)?)?;
                writer.flush()?;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lease_id = format!(
            "{}-{:x}",
            *CONTROL_LEASE_BOOT_ID,
            NEXT_CONTROL_LEASE_ID.fetch_add(1, Ordering::Relaxed)
        );
        let request_lease = Arc::new(BridgeRequestLease::new(lease_id));
        if !reader.activate(Arc::clone(&request_lease)) {
            break;
        }
        REQUEST_NOTICE_SINK.with(|sink| {
            *sink.borrow_mut() = request_notice_sink(trimmed, &notice_stream);
        });
        let response = automation_parse_response_with_lease(
            trimmed,
            state,
            bridge,
            bridge_wait_seconds,
            Some(Arc::clone(&request_lease)),
        );
        REQUEST_NOTICE_SINK.with(|sink| sink.borrow_mut().take());
        let response = serde_json::to_string(&response)?;
        let write_result = (|| -> io::Result<()> {
            writeln!(writer, "{response}")?;
            writer.flush()
        })();
        reader.finish();
        write_result?;
    }
    Ok(())
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

/// The bridge ports a named daemon published in its discovery file, for the
/// launch ticket of a Studio the CLI opens on that daemon's behalf.
pub(crate) fn named_daemon_bridge_ports(name: &str) -> Option<Vec<u16>> {
    daemon_discovery_write_paths(name)
        .into_iter()
        .find_map(|path| {
            let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
            let ports = value
                .get("bridgePorts")?
                .as_array()?
                .iter()
                .filter_map(|port| u16::try_from(port.as_u64()?).ok())
                .collect::<Vec<_>>();
            (ports.len() == 2).then_some(ports)
        })
}

/// The control port a named daemon published in its discovery file.
pub(crate) fn named_daemon_control_port(name: &str) -> Option<u16> {
    daemon_discovery_write_paths(name)
        .into_iter()
        .find_map(|path| {
            let value: Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
            u16::try_from(value.get("controlPort")?.as_u64()?).ok()
        })
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

    if is_default_daemon() {
        push_daemon_endpoint(
            &mut out,
            &mut seen,
            &host_port("127.0.0.1", DEFAULT_DAEMON_CONTROL_PORT),
        );
    }
    out
}

pub(crate) fn is_default_daemon() -> bool {
    default_daemon_selection(
        std::env::var("RENIUM_DAEMON_NAME").ok().as_deref(),
        std::env::var("RENIUM_DAEMON_FILE").ok().as_deref(),
    )
}

fn default_daemon_selection(name: Option<&str>, file: Option<&str>) -> bool {
    file.is_none_or(|file| file.trim().is_empty())
        && name.is_none_or(|name| matches!(name.trim(), "" | "default"))
}

pub(super) fn daemon_discovery_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(path) = std::env::var("RENIUM_DAEMON_FILE") {
        let path = path.trim();
        if !path.is_empty() {
            paths.push(PathBuf::from(path));
            return paths;
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
    // The record is written once, when the daemon starts, and holds for as
    // long as that process runs, so its age says nothing about it.
    let updated_ms = value.get("updatedUnixMs").and_then(Value::as_u64)? as u128;
    if updated_ms > current_millis().saturating_add(DAEMON_DISCOVERY_MAX_FUTURE_SKEW_MS)
        || !is_recorded_process(pid, process_start_identity)
    {
        return None;
    }
    host_port(&host, port)
        .to_socket_addrs()
        .ok()?
        .find(is_loopback_endpoint)
}

/// Whether `pid` is still the process that recorded `start_identity` for
/// itself, not a later process the system gave the same number.
pub(crate) fn is_recorded_process(pid: u32, start_identity: &str) -> bool {
    is_process_alive(pid) && update::process_start_identity(pid).as_deref() == Some(start_identity)
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
        let exists = unsafe {
            libc::kill(pid as libc::pid_t, 0) == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
        };
        exists && !unix_zombie(pid)
    }
}

/// A killed process stays listed until its parent reaps it; such a zombie
/// answers signals but runs nothing.
#[cfg(not(windows))]
fn unix_zombie(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .is_some_and(|state| state.trim_start().starts_with('Z'))
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

fn try_bind_daemon_context(
    project_root: Option<&Path>,
    project_only: bool,
    experience_only: bool,
) -> Result<Option<Value>> {
    let resource_lease = crate::plugins::environment_claim()?;
    let project = crate::app::context::project_override().map(|path| absolutize_for_daemon(&path));
    let root = project_root
        .or_else(|| daemon_project_root(project.as_deref()))
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
        p: json!({ "root": root, "project": project, "place": place_filter(), "resourceLease": resource_lease, "projectOnly": project_only, "experienceOnly": experience_only }),
    })?
    else {
        return Ok(None);
    };
    if bind.ok == 0 {
        let error = bind.e.context("Daemon bind failed without an error")?;
        return Err(automation::DaemonFailure::from(error).into());
    }
    let response = bind.r.context("Daemon bind response omitted its context")?;
    crate::plugins::verify_lease_ack(resource_lease.is_some(), &response)?;
    Ok(Some(response))
}

pub(crate) fn daemon_project_root(project: Option<&Path>) -> Option<&Path> {
    project.map(|path| {
        if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        }
    })
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
fn spawn_shared_daemon(
    executable: &Path,
    ports: &str,
    wait_seconds: f64,
    control_port: u16,
) -> io::Result<()> {
    let mut command = Command::new(executable);
    command
        .arg("bd")
        .arg("--ports")
        .arg(ports)
        .arg("--wait-seconds")
        .arg(wait_seconds.to_string())
        .arg("--control-port")
        .arg(control_port.to_string())
        .env_remove("RENIUM_RESOURCE_LEASE")
        .env_remove("RENIUM_PLACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_daemon(&mut command);
    command.spawn().map(|_| ())
}

#[cfg(windows)]
fn spawn_shared_daemon(
    executable: &Path,
    ports: &str,
    wait_seconds: f64,
    control_port: u16,
) -> io::Result<()> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x08000000;
    // The shared daemon must outlive the job containing a plugin command.
    let detach_flags = if std::env::var_os("RENIUM_PLUGIN_CHILD").is_some_and(|value| value == "1")
    {
        0x01000000
    } else {
        0
    };
    let status = Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            "Start-Process -FilePath $env:RENIUM_DAEMON_EXECUTABLE -ArgumentList @('bd','--ports',$env:RENIUM_DAEMON_PORTS,'--wait-seconds',$env:RENIUM_DAEMON_WAIT,'--control-port',$env:RENIUM_DAEMON_CONTROL_PORT) -WindowStyle Hidden",
        ])
        .env("RENIUM_DAEMON_EXECUTABLE", executable)
        .env("RENIUM_DAEMON_PORTS", ports)
        .env("RENIUM_DAEMON_WAIT", wait_seconds.to_string())
        .env("RENIUM_DAEMON_CONTROL_PORT", control_port.to_string())
        .env_remove("RENIUM_RESOURCE_LEASE")
        .env_remove("RENIUM_PLACE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW | detach_flags)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other("failed to start the shared Renium daemon"))
    }
}

pub(crate) fn ensure_shared_daemon(ports: &str, wait_seconds: f64) -> Result<()> {
    let explicit_control_port = std::env::var("RENIUM_DAEMON_CONTROL_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .filter(|port| *port != 0);
    if let Some(control_port) = explicit_control_port {
        return ensure_shared_daemon_on(ports, wait_seconds, control_port);
    }
    if is_default_daemon() {
        return ensure_shared_daemon_on(ports, wait_seconds, DEFAULT_DAEMON_CONTROL_PORT);
    }
    ensure_named_daemon(|ports, control_port| {
        ensure_shared_daemon_on(ports, wait_seconds, control_port)
    })
}

const NAMED_DAEMON_FIRST_CONTROL_PORT: u16 = 8790;

/// Named daemons start one at a time on this machine: two starts probing the
/// same free block in the same instant both spawned a daemon on it, the
/// loser's daemon exited at its bind and the loser's command talked to the
/// winner. The lock is held from the probe until the daemon answers.
fn named_daemon_start_lock() -> Option<fs::File> {
    let path = local_app_data_daemon_path()?.with_file_name("named-daemon-start.lock");
    fs::create_dir_all(path.parent()?).ok()?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(90);
    while file.try_lock().is_err() {
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
    }
    Some(file)
}
const NAMED_DAEMON_START_ATTEMPTS: usize = 4;

/// A named daemon started on demand takes the first free port block. Its
/// probe listeners hold the block until the daemon is spawned, so a start
/// running beside this one allocates past it; a start that spawned at the
/// same moment can still win the child's bind, after which the child exits,
/// so a daemon that did not become ready on a block someone else now holds
/// is started again on the next free block.
fn ensure_named_daemon(mut start: impl FnMut(&str, u16) -> Result<()>) -> Result<()> {
    let _one_start_at_a_time = named_daemon_start_lock();
    let mut first = NAMED_DAEMON_FIRST_CONTROL_PORT;
    let mut attempt = 1;
    loop {
        let block = free_named_daemon_ports_from(first)?;
        let control_port = block.control_port;
        let ports = block.ports.clone();
        drop(block);
        let started = start(&ports, control_port);
        if started.is_ok()
            || attempt >= NAMED_DAEMON_START_ATTEMPTS
            || !named_daemon_block_taken(control_port)
        {
            return started;
        }
        first = control_port.saturating_add(10);
        attempt += 1;
    }
}

struct NamedDaemonPorts {
    control_port: u16,
    ports: String,
    _reserved: Vec<TcpListener>,
}

/// Ports for a named daemon started on demand: the first free block of a
/// control port and two bridge ports from `first` up, in steps of ten, so it
/// never fights the default daemon's 8780-8782 or another named daemon's.
/// The block stays bound while the value lives.
fn free_named_daemon_ports_from(first: u16) -> Result<NamedDaemonPorts> {
    let mut control_port = first;
    while control_port <= u16::MAX - 2 {
        let reserved = (control_port..=control_port + 2)
            .map(|port| TcpListener::bind(("127.0.0.1", port)))
            .collect::<io::Result<Vec<_>>>();
        if let Ok(reserved) = reserved {
            return Ok(NamedDaemonPorts {
                control_port,
                ports: format!("{},{}", control_port + 1, control_port + 2),
                _reserved: reserved,
            });
        }
        control_port = control_port.saturating_add(10);
    }
    bail!(
        "No free control port for a named daemon from {NAMED_DAEMON_FIRST_CONTROL_PORT} up; pass RENIUM_DAEMON_CONTROL_PORT"
    )
}

fn named_daemon_block_taken(control_port: u16) -> bool {
    (control_port..=control_port + 2).any(|port| TcpListener::bind(("127.0.0.1", port)).is_err())
}

#[cfg(test)]
mod named_daemon_port_tests {
    use super::*;
    use crate::system::LockRecover;
    use std::sync::Mutex;

    /// The port blocks are process-wide; the tests that bind them take this
    /// lock so one test's reservation is not another's collision.
    static PORT_BLOCKS: Mutex<()> = Mutex::new(());

    #[test]
    fn named_daemons_take_a_free_block_from_8790_up() {
        let _serial = PORT_BLOCKS.lock_recover();
        let block = free_named_daemon_ports_from(NAMED_DAEMON_FIRST_CONTROL_PORT).unwrap();
        let control_port = block.control_port;
        assert!(control_port >= NAMED_DAEMON_FIRST_CONTROL_PORT);
        assert_eq!(
            (control_port - NAMED_DAEMON_FIRST_CONTROL_PORT) % 10,
            0,
            "{control_port}"
        );
        assert_eq!(
            block.ports,
            format!("{},{}", control_port + 1, control_port + 2)
        );
    }

    #[test]
    fn simultaneous_named_starts_allocate_different_blocks() {
        let _serial = PORT_BLOCKS.lock_recover();
        let first = free_named_daemon_ports_from(NAMED_DAEMON_FIRST_CONTROL_PORT).unwrap();
        let second = free_named_daemon_ports_from(NAMED_DAEMON_FIRST_CONTROL_PORT).unwrap();
        assert!(
            second.control_port > first.control_port,
            "{} then {}",
            first.control_port,
            second.control_port
        );
        assert_eq!((second.control_port - first.control_port) % 10, 0);
        drop(first);
        let third = free_named_daemon_ports_from(NAMED_DAEMON_FIRST_CONTROL_PORT).unwrap();
        assert!(third.control_port < second.control_port);
    }

    #[test]
    fn a_named_start_that_lost_its_block_moves_to_the_next_free_one() {
        let _serial = PORT_BLOCKS.lock_recover();
        let mut attempts = Vec::new();
        let mut winner = None;
        ensure_named_daemon(|ports, control_port| {
            assert_eq!(ports, format!("{},{}", control_port + 1, control_port + 2));
            attempts.push(control_port);
            if attempts.len() > 1 {
                return Ok(());
            }
            winner = Some(
                (control_port..=control_port + 2)
                    .map(|port| TcpListener::bind(("127.0.0.1", port)).unwrap())
                    .collect::<Vec<_>>(),
            );
            bail!("Renium daemon did not become ready within 5s on control port {control_port}")
        })
        .unwrap();
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert!(attempts[1] > attempts[0], "{attempts:?}");
        drop(winner);
    }

    #[test]
    fn a_named_start_that_failed_on_a_free_block_reports_its_error() {
        let _serial = PORT_BLOCKS.lock_recover();
        let mut attempts = 0;
        let error = ensure_named_daemon(|_, control_port| {
            attempts += 1;
            bail!("Renium daemon did not become ready within 5s on control port {control_port}")
        })
        .unwrap_err();
        assert_eq!(attempts, 1);
        assert!(error.to_string().contains("did not become ready"));
    }
}

fn start_shared_daemon_on(ports: &str, wait_seconds: f64, control_port: u16) -> bool {
    ensure_shared_daemon_on(ports, wait_seconds, control_port).is_ok()
}

fn ensure_shared_daemon_on(ports: &str, wait_seconds: f64, control_port: u16) -> Result<()> {
    let executable = std::env::current_exe().context("Cannot locate the Renium executable")?;
    spawn_shared_daemon(&executable, ports, wait_seconds, control_port)
        .context("Could not start the Renium daemon")?;
    // A named daemon is ready only once its own discovery record names this
    // control port: another daemon that won the same block answers on it
    // too, and that answer must not pass for this one.
    let name = std::env::var("RENIUM_DAEMON_NAME")
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty() && name != "default");
    let ready = || match &name {
        None => shared_daemon_available(),
        Some(name) => {
            named_daemon_control_port(name) == Some(control_port)
                && crate::automation::client::daemon_endpoint_available(
                    std::net::SocketAddr::from(([127, 0, 0, 1], control_port)),
                    Duration::from_millis(500),
                )
        }
    };
    let deadline = Instant::now() + SHARED_DAEMON_START_TIMEOUT;
    while Instant::now() < deadline {
        if ready() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    if ready() {
        Ok(())
    } else {
        bail!("Renium daemon did not become ready within 15s on control port {control_port}")
    }
}

pub(crate) fn try_daemon_project_root(project_root: &Path) -> Result<Option<PathBuf>> {
    try_bind_daemon_context(Some(project_root), false, false)?
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
    parameters: Value,
) -> Result<Option<Value>> {
    daemon_control_request_inner(operation, None, parameters, false, false)
}

pub(crate) fn try_scoped_daemon_control_request(
    operation: u16,
    root: &Path,
    parameters: Value,
) -> Result<Option<Value>> {
    daemon_control_request_inner(operation, Some(root), parameters, false, false)
}

pub(crate) fn daemon_control_request(
    operation: u16,
    project_root: Option<&Path>,
    parameters: Value,
    approved: bool,
) -> Result<Value> {
    daemon_control_request_inner(operation, project_root, parameters, approved, true)?
        .context("Renium daemon did not accept the command")
}

fn daemon_control_request_inner(
    operation: u16,
    project_root: Option<&Path>,
    mut parameters: Value,
    approved: bool,
    required: bool,
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
    let image_upload_needs_runtime = operation == automation::op::IMAGE_UPLOAD
        && (crate::cloud::assets::studio_upload(&parameters)
            || parameters.get("userId").and_then(Value::as_u64).is_none()
                && parameters.get("groupId").and_then(Value::as_u64).is_none());
    let object = parameters
        .as_object_mut()
        .context("Daemon operation parameters must be a JSON object")?;
    let direct_package = matches!(
        operation,
        automation::op::PACKAGE_DESYNC
            | automation::op::PACKAGE_PUBLISH
            | automation::op::PACKAGE_UPDATE
    ) && object.get("pid").and_then(Value::as_u64).is_some();
    if matches!(
        operation,
        automation::op::CAP
            | automation::op::STUDIOS
            | automation::op::PERFORMANCE_PROFILE
            | automation::op::COLLAB_START
            | automation::op::COLLAB_JOIN
            | automation::op::COLLAB_STOP
            | automation::op::COLLAB_STATUS
            | automation::op::COLLAB_AWARENESS
    ) || direct_package
        || operation == automation::op::STUDIO_AUDIO
            && object.get("global").and_then(Value::as_bool) == Some(true)
    {
        let request = automation::Request {
            v: automation::PROTOCOL_VERSION,
            id: current_millis().min(u128::from(u64::MAX)) as u64,
            op: operation,
            cx: None,
            p: request_with_notices(std::mem::take(&mut parameters)),
        };
        let mut response = try_send_request(&request)?;
        if response.is_none() && required {
            ensure_shared_daemon(&bridge_ports, bridge_wait_seconds)?;
            response = try_send_request(&request)?;
        }
        let Some(response) = response else {
            return Ok(None);
        };
        if response.ok == 0 {
            let error = response
                .e
                .context("Daemon request failed without an error")?;
            return Err(automation::DaemonFailure::from(error).into());
        }
        return Ok(Some(response.r.unwrap_or_else(|| json!({}))));
    }
    let needs_runtime = opcode.runtime
        || image_upload_needs_runtime
        || matches!(
            operation,
            automation::op::SET_PROPERTY | automation::op::REMOVE
        ) && object.get("editor").and_then(Value::as_bool) == Some(true);
    // An explicit file is already the open target. Binding it must not first
    // select some unrelated, already-connected runtime from this workspace.
    let experience_only = matches!(
        operation,
        automation::op::PLACE_ADD | automation::op::PLACE_RENAME | automation::op::PLACE_REORDER
    );
    let project_only = experience_only
        || operation == automation::op::STUDIO_OPEN
            && object.get("file").and_then(Value::as_str).is_some();
    let mut context = try_bind_daemon_context(project_root, project_only, experience_only)?;
    let ready = |context: &Option<Value>| {
        context.as_ref().is_some_and(|context| {
            !needs_runtime || context.get("runtimeId").and_then(Value::as_str).is_some()
        })
    };
    let daemon_started = context.is_none() && (required || needs_runtime);
    if daemon_started {
        ensure_shared_daemon(&bridge_ports, bridge_wait_seconds)?;
        context = try_bind_daemon_context(project_root, project_only, experience_only)?;
    }
    if needs_runtime && !ready(&context) && shared_daemon_available() {
        let reconnect_grace = if daemon_started {
            Duration::from_secs(2)
        } else {
            Duration::ZERO
        };
        let wait = Duration::from_secs_f64(bridge_wait_seconds) + reconnect_grace;
        let deadline = Instant::now() + wait;
        let mut bind_error = None;
        let mut delay = Duration::from_millis(50);
        let mut next_process_check = Instant::now() + reconnect_grace;
        let mut empty_probes = 0;
        let mut no_studio = false;
        while !ready(&context) && Instant::now() < deadline {
            match try_bind_daemon_context(project_root, project_only, experience_only) {
                Ok(bound) => context = bound,
                Err(error) => bind_error = Some(error),
            }
            if ready(&context) {
                break;
            }
            // A Studio that is not running cannot connect within the wait; two
            // empty probes in a row (not one failed listing) end it early, and
            // a recent launch shows up as a process by the first check.
            if Instant::now() >= next_process_check {
                next_process_check = Instant::now() + Duration::from_millis(400);
                if crate::studio::diagnosis::studio_process_ids().is_empty() {
                    empty_probes += 1;
                    if empty_probes >= 2 {
                        no_studio = true;
                        break;
                    }
                } else {
                    empty_probes = 0;
                }
            }
            thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_millis(400));
        }
        if !ready(&context) {
            if let Some(error) = bind_error {
                return Err(error);
            }
            let clients = try_daemon_control_request(automation::op::STUDIOS, json!({}))
                .ok()
                .flatten()
                .and_then(|result| result.get("clients")?.as_array().cloned())
                .unwrap_or_default();
            let verdict = crate::studio::diagnosis::verdict(&clients, project_root);
            if no_studio {
                bail!("No Studio runtime connected to this project. {verdict}");
            }
            bail!(
                "No Studio runtime connected to this project within {:.1}s. {verdict}",
                wait.as_secs_f64()
            );
        }
    }
    let Some(context) = context else {
        return Ok(None);
    };
    let context_id = context
        .get("id")
        .and_then(Value::as_u64)
        .context("Daemon bind response omitted the context ID")?;
    let requires_review = opcode.requires_review(&parameters, approved);
    // A response deadline that passes is reported as the daemon being busy
    // with the operation it is serving, not as a socket error.
    let started = Instant::now();
    let busy = |error: anyhow::Error| {
        if crate::automation::client::is_response_timeout(&error) {
            crate::automation::client::daemon_busy_error(
                &format!("rbx {}", opcode.name),
                started.elapsed(),
            )
        } else if error.chain().any(|cause| {
            cause.downcast_ref::<std::io::Error>().is_some_and(|error| {
                matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
                )
            })
        }) {
            anyhow::anyhow!(
                "The Renium daemon closed the connection before answering `rbx {}` (it restarted or crashed mid-request); run the command again, and `rbx report` collects the crash log",
                opcode.name
            )
        } else {
            error
        }
    };
    let response = send_operation_requests(
        operation,
        context_id,
        parameters,
        requires_review,
        |request| send_request(&request).map_err(busy),
    )?;
    if response.ok == 0 {
        let error = response
            .e
            .context("Daemon request failed without an error")?;
        return Err(automation::DaemonFailure::from(error).into());
    }
    Ok(Some(response.r.unwrap_or_else(|| json!({}))))
}

#[cfg(test)]
mod notice_tests {
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

    use super::*;

    #[test]
    fn review_receipt_requests_ask_for_notices_at_the_top_level() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = crate::system::net::SharedTcpStream::from(listener.accept().unwrap().0);
        let prepare =
            review_prepare_parameters(automation::op::SET_PROPERTY, json!({ "editor": true }));
        assert_eq!(prepare["p"], json!({ "editor": true }));
        let apply = review_apply_parameters("review-7");
        assert_eq!(apply["reviewId"], json!("review-7"));
        for (id, parameters) in [(5, &prepare), (6, &apply)] {
            let line =
                serde_json::to_string(&json!({ "v": 1, "id": id, "op": 0, "p": parameters }))
                    .unwrap();
            assert_eq!(
                request_notice_sink(&line, &peer).map(|(_, id)| id),
                Some(id),
                "{line}"
            );
        }
    }

    #[test]
    fn every_wire_envelope_asks_for_notices_and_a_reviews_parameters_stay_clean() {
        let parameters = json!({ "editor": true, "property": "Value" });
        let mut sent = Vec::new();
        let response = send_operation_requests(
            automation::op::SET_PROPERTY,
            4,
            parameters.clone(),
            true,
            |request| {
                let answer = if request.op == automation::op::REVIEW_PREPARE {
                    json!({ "reviewId": "review-7" })
                } else {
                    json!({ "changed": true })
                };
                let id = request.id;
                sent.push(request);
                Ok(automation::Response::success(id, Instant::now(), answer))
            },
        )
        .unwrap();
        assert_eq!(response.r.unwrap()["changed"], json!(true));
        let [prepare, apply] = sent.as_slice() else {
            panic!("expected a prepare and an apply request")
        };
        assert_eq!(prepare.op, automation::op::REVIEW_PREPARE);
        assert_eq!(prepare.cx, Some(4));
        assert_eq!(prepare.p["notices"], json!(true));
        assert_eq!(prepare.p["op"], json!(automation::op::SET_PROPERTY));
        assert_eq!(prepare.p["p"], parameters);
        assert_eq!(apply.op, automation::op::REVIEW_APPLY);
        assert_eq!(apply.p, json!({ "reviewId": "review-7", "notices": true }));
        let mut direct = Vec::new();
        send_operation_requests(
            automation::op::SET_PROPERTY,
            4,
            parameters.clone(),
            false,
            |request| {
                direct.push(request);
                Ok(automation::Response::success(0, Instant::now(), json!({})))
            },
        )
        .unwrap();
        let [request] = direct.as_slice() else {
            panic!("expected one direct request")
        };
        assert_eq!(request.op, automation::op::SET_PROPERTY);
        assert_eq!(
            request.p,
            json!({ "editor": true, "property": "Value", "notices": true })
        );
    }

    #[test]
    fn a_reviewed_edit_sends_its_hold_notice_to_the_caller() {
        // Binding a context selects it process-wide; keep this wire test isolated.
        if std::env::var_os("RENIUM_TEST_REVIEWED_NOTICE").is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "daemon::notice_tests::a_reviewed_edit_sends_its_hold_notice_to_the_caller",
                ])
                .env("RENIUM_TEST_REVIEWED_NOTICE", "1")
                .status()
                .unwrap();
            assert!(status.success());
            return;
        }
        use crate::automation::authorization::signed_fixture_request;
        use crate::studio::bridge::routing_tests::{connect, edit_info, fixture};
        use tungstenite::{Message, WebSocket, protocol::Role};
        let root = crate::tests::support::temp_dir("reviewed-notice");
        fs::write(
            root.join("renium.project.jsonc"),
            r#"{"schemaVersion":1,"sourceRoot":"src","tree":{}}"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".renium")).unwrap();
        fs::write(
            root.join(".renium").join("config.json"),
            r#"{"schemaVersion":1,"liveSync":{"changesThreshold":0}}"#,
        )
        .unwrap();
        let bridge = Arc::new(fixture());
        let stream = connect(&bridge, 0, edit_info("reviewed-notice", 20));
        let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = Arc::clone(&methods);
        let plugin = thread::spawn(move || {
            let mut socket = WebSocket::from_raw_socket(stream, Role::Client, None);
            while let Ok(message) = socket.read() {
                let Message::Text(text) = message else {
                    continue;
                };
                let request: Value = serde_json::from_str(&text).unwrap();
                let method = request["method"].as_str().unwrap().to_string();
                let result = match method.as_str() {
                    "getEditorMutationPackages" => json!({ "packages": [] }),
                    "requestEditorPushReview" => {
                        json!({ "required": true, "reviewId": "studio-1" })
                    }
                    "getEditorPushReviewDecision" => {
                        json!({ "decided": true, "decision": "skip" })
                    }
                    _ => json!({}),
                };
                seen.lock().unwrap().push(method);
                let answer = json!({ "id": request["id"], "ok": true, "result": result });
                if socket
                    .send(Message::Text(answer.to_string().into()))
                    .is_err()
                {
                    break;
                }
            }
        });
        let state = automation::State::default();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (served, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(client.try_clone().unwrap());
        let mut writer = client;
        thread::scope(|scope| {
            scope.spawn(|| handle_daemon_control_connection(served, &bridge, &state, 0.1));
            let mut exchange = |value: Value| -> Vec<automation::Response> {
                writeln!(writer, "{}", signed_fixture_request(&state, value)).unwrap();
                writer.flush().unwrap();
                let mut responses = Vec::new();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let response: automation::Response = serde_json::from_str(&line).unwrap();
                    let interim = response.is_notice();
                    responses.push(response);
                    if !interim {
                        break;
                    }
                }
                responses
            };
            let bound = exchange(json!({
                "v": 1, "id": 1, "op": automation::op::BIND,
                "p": { "root": root, "place": "10:20" }
            }));
            let bound = bound.last().unwrap();
            assert_eq!(bound.ok, 1, "{}", serde_json::to_string(bound).unwrap());
            let context = bound.r.as_ref().unwrap()["id"].as_u64().unwrap();
            let parameters = json!({
                "editor": true, "service": "Workspace", "className": "StringValue",
                "pathSegments": ["Props", "Sign"], "pathOrdinals": [],
                "scope": "property", "property": "Value", "value": "held", "srcDir": "src"
            });
            let prepared = exchange(json!({
                "v": 1, "id": 2, "op": automation::op::REVIEW_PREPARE, "cx": context,
                "p": review_prepare_parameters(automation::op::SET_PROPERTY, parameters)
            }));
            let prepared = prepared.last().unwrap();
            assert_eq!(
                prepared.ok,
                1,
                "{}",
                serde_json::to_string(prepared).unwrap()
            );
            let review_id = prepared.r.as_ref().unwrap()["reviewId"]
                .as_str()
                .unwrap()
                .to_string();
            let applied = exchange(json!({
                "v": 1, "id": 3, "op": automation::op::REVIEW_APPLY, "cx": context,
                "p": review_apply_parameters(&review_id)
            }));
            let [notice, result] = applied.as_slice() else {
                panic!(
                    "expected the hold notice and the result: {}",
                    serde_json::to_string(&applied).unwrap()
                )
            };
            assert_eq!(notice.id, 3);
            assert!(
                notice.notice_text().is_some_and(
                    |text| text.contains("held for review in Studio (id=studio-1, changes=1)")
                ),
                "{}",
                serde_json::to_string(notice).unwrap()
            );
            assert_eq!(result.ok, 1, "{}", serde_json::to_string(result).unwrap());
            assert_eq!(result.r.as_ref().unwrap()["skippedByReview"], json!(true));
            writer.shutdown(std::net::Shutdown::Both).unwrap();
        });
        assert!(
            methods
                .lock()
                .unwrap()
                .contains(&"requestEditorPushReview".to_string())
        );
        drop(bridge);
        plugin.join().unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_request_that_asks_for_notices_receives_them_on_its_own_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = crate::system::net::SharedTcpStream::from(listener.accept().unwrap().0);
        assert!(request_notice_sink(r#"{"v":1,"id":3,"op":0,"p":{}}"#, &peer).is_none());
        let sink = request_notice_sink(r#"{"v":1,"id":3,"op":0,"p":{"notices":true}}"#, &peer);
        assert_eq!(sink.as_ref().map(|(_, id)| *id), Some(3));
        emit_request_notice("ignored without a sink");
        REQUEST_NOTICE_SINK.with(|slot| *slot.borrow_mut() = sink);
        emit_request_notice("held for review");
        REQUEST_NOTICE_SINK.with(|slot| slot.borrow_mut().take());
        emit_request_notice("ignored after the request");
        drop(peer);
        let mut lines = BufReader::new(client).lines();
        let line = lines.next().unwrap().unwrap();
        let response: automation::Response = serde_json::from_str(&line).unwrap();
        assert!(response.is_notice());
        assert_eq!(response.id, 3);
        assert_eq!(response.notice_text(), Some("held for review"));
        assert!(lines.next().is_none());
    }
}

#[cfg(test)]
mod discovery_record_tests {
    use super::*;

    #[test]
    fn a_record_stays_valid_for_as_long_as_its_daemon_runs() {
        let pid = std::process::id();
        let record = |identity: &str, updated: u128| {
            json!({
                "schemaVersion": 2,
                "name": "long",
                "host": "127.0.0.1",
                "controlPort": 8790,
                "bridgePorts": [8791, 8792],
                "pid": pid,
                "processStartIdentity": identity,
                "updatedUnixMs": updated as u64,
            })
        };
        let identity = update::process_start_identity(pid).unwrap();
        let a_month_ago = current_millis() - 30 * 24 * 60 * 60 * 1000;
        assert_eq!(
            daemon_discovery_endpoint(&record(&identity, a_month_ago)),
            Some("127.0.0.1:8790".parse().unwrap())
        );
        assert_eq!(
            daemon_discovery_endpoint(&record("another start", current_millis())),
            None
        );
    }
}

#[cfg(test)]
mod connection_limit_tests {
    use std::io::{BufRead, BufReader};

    use super::*;

    #[test]
    fn a_connection_over_the_limit_is_told_the_daemon_is_busy() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let mut client = TcpStream::connect(address).unwrap();
        writeln!(client, "{}", json!({"v": 1, "id": 7, "op": 0, "p": {}})).unwrap();
        let active = AtomicUsize::new(MAX_DAEMON_CONTROL_CONNECTIONS);
        assert!(admit_control_connection(listener.accept().unwrap().0, &active).is_none());
        assert_eq!(
            active.load(Ordering::Relaxed),
            MAX_DAEMON_CONTROL_CONNECTIONS
        );
        let mut line = String::new();
        let read = BufReader::new(&client).read_line(&mut line);
        assert!(read.is_ok(), "{read:?}");
        let response: automation::Response = serde_json::from_str(&line).expect(&line);
        assert_eq!((response.id, response.ok), (7, 0));
        let error = response.e.unwrap();
        assert_eq!((error.c.as_str(), error.rt), ("busy", 1));
        assert!(error.m.contains("16 open connections"), "{}", error.m);

        let _admitted = TcpStream::connect(address).unwrap();
        let active = AtomicUsize::new(MAX_DAEMON_CONTROL_CONNECTIONS - 1);
        assert!(admit_control_connection(listener.accept().unwrap().0, &active).is_some());
        assert_eq!(
            active.load(Ordering::Relaxed),
            MAX_DAEMON_CONTROL_CONNECTIONS
        );
    }
}

#[cfg(test)]
mod isolation_tests {
    use super::default_daemon_selection;

    #[test]
    fn explicit_daemons_cannot_use_default_discovery_or_global_watchers() {
        for (name, file) in [
            (Some("canary"), None),
            (Some(" astra "), None),
            (None, Some("scratch/daemon.json")),
            (Some("default"), Some("scratch/daemon.json")),
        ] {
            assert!(!default_daemon_selection(name, file));
        }
        for (name, file) in [
            (None, None),
            (Some(" default "), None),
            (Some(""), Some(" ")),
        ] {
            assert!(default_daemon_selection(name, file));
        }
    }
}
