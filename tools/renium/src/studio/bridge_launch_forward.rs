use std::io::{Read, Write};
use std::net::SocketAddr;

use super::*;
use crate::project::workflows::{launch_ticket_pending, take_launch_ticket};

const YIELD_REQUEST: &[u8] = b"RENIUM-YIELD\n";

struct Forwarder {
    awaited: Vec<u32>,
    running: bool,
}

static FORWARDER: Mutex<Forwarder> = Mutex::new(Forwarder {
    awaited: Vec::new(),
    running: false,
});

/// A Studio this daemon launched connects to the plugin's saved ports, not to
/// this daemon's. While no daemon listens on the saved ports, this daemon
/// stands in on them until the Studio's launch ticket is taken: a ticketed
/// Studio is sent on to its daemon and every other connection is turned away,
/// so a named daemon's launch needs no default daemon.
pub(crate) fn forward_launched_studio(pid: u32, saved_ports: &[u16]) {
    let mut forwarder = FORWARDER.lock_recover();
    if !forwarder.awaited.contains(&pid) {
        forwarder.awaited.push(pid);
    }
    if forwarder.running {
        return;
    }
    forwarder.running = true;
    drop(forwarder);
    let ports = saved_ports.to_vec();
    thread::spawn(move || {
        if stand_in(&ports, still_awaited) {
            let mut forwarder = FORWARDER.lock_recover();
            forwarder.awaited.clear();
            forwarder.running = false;
        }
    });
}

fn still_awaited() -> bool {
    let mut forwarder = FORWARDER.lock_recover();
    forwarder.awaited.retain(|pid| launch_ticket_pending(*pid));
    forwarder.running = !forwarder.awaited.is_empty();
    forwarder.running
}

/// Listens on whichever of `ports` are free while `awaited` holds, and
/// returns true when a starting daemon asked for the ports instead. A port
/// another process holds is tried again: that process may be a daemon that
/// takes the tickets itself, or a stand-in about to finish.
fn stand_in(ports: &[u16], awaited: impl Fn() -> bool) -> bool {
    let yielded = Arc::new(AtomicBool::new(false));
    let session_id = format!("launch-forward-{:x}", std::process::id());
    let mut listeners: Vec<(u16, TcpListener)> = Vec::new();
    let mut checked: Option<Instant> = None;
    while !yielded.load(Ordering::Relaxed) {
        if checked.is_none_or(|at| at.elapsed() >= Duration::from_millis(500)) {
            checked = Some(Instant::now());
            if !awaited() {
                return false;
            }
            for port in ports {
                if listeners.iter().any(|(bound, _)| bound == port) {
                    continue;
                }
                if let Ok(listener) = TcpListener::bind(("127.0.0.1", *port))
                    && listener.set_nonblocking(true).is_ok()
                {
                    listeners.push((*port, listener));
                }
            }
        }
        for (port, listener) in &listeners {
            while let Ok((stream, addr)) = listener.accept() {
                let port = *port;
                let session_id = session_id.clone();
                let yielded = Arc::clone(&yielded);
                thread::spawn(move || {
                    forward_connection(stream, addr, port, &session_id, &yielded)
                });
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    true
}

fn forward_connection(
    stream: TcpStream,
    addr: SocketAddr,
    port: u16,
    session_id: &str,
    yielded: &AtomicBool,
) {
    let peer = addr.to_string();
    let ticketed = BridgeServer::studio_pid_for_peer(&peer)
        .ok()
        .filter(|pid| launch_ticket_pending(*pid));
    let Some(pid) = ticketed else {
        if asks_to_yield(stream) {
            yielded.store(true, Ordering::Relaxed);
        }
        return;
    };
    let outcome = (|| -> Result<Vec<u16>> {
        let mut socket =
            BridgeServer::accept_ready_socket("127.0.0.1", port, stream, peer, session_id)?;
        anyhow::ensure!(
            socket.role == BRIDGE_ROLE_EDIT,
            "its {} connection is not an Edit window",
            socket.role
        );
        let ports = take_launch_ticket(pid).context("its launch ticket was already taken")?;
        if socket.bridge_info.registration_ack {
            let ack = json!({
                "id": 0, "method": "bridgeRegistered", "params": {},
                "session_id": session_id,
            });
            socket.socket.send(Message::Text(ack.to_string().into()))?;
        }
        let answer = BridgeServer::call_on_socket_with_timeout(
            &mut socket,
            2,
            "redirectBridgePorts",
            &json!({ "ports": ports }),
            Some(Duration::from_secs(5)),
            None,
        )?;
        anyhow::ensure!(
            answer.get("ok").and_then(Value::as_bool) != Some(false),
            "it refused the redirect: {answer}"
        );
        Ok(ports)
    })();
    match outcome {
        Ok(ports) => crate::app::output::log_global(
            3,
            format_args!("[renium] launched Studio {pid} moves to bridge ports {ports:?}"),
        ),
        Err(error) => crate::app::output::log_global(
            3,
            format_args!(
                "[renium] launched Studio {pid} was not sent on from port {port}: {error:#}"
            ),
        ),
    }
}

/// Whether an unticketed connection is a starting daemon asking for the port.
/// A Studio that no daemon launched opens with an HTTP request instead and is
/// dropped, as if nothing listened.
fn asks_to_yield(mut stream: TcpStream) -> bool {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
    let mut request = [0u8; YIELD_REQUEST.len()];
    stream.read_exact(&mut request).is_ok() && request == YIELD_REQUEST
}

/// Asks a stand-in on `port` to give the port up; true when something there
/// took the request and closed the connection.
fn ask_to_yield(host: &str, port: u16) -> bool {
    let Ok(address) = format!("{host}:{port}").parse::<SocketAddr>() else {
        return false;
    };
    let Ok(mut stream) = TcpStream::connect_timeout(&address, Duration::from_millis(250)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
    if stream.write_all(YIELD_REQUEST).is_err() {
        return false;
    }
    let mut rest = Vec::new();
    let _ = stream.read_to_end(&mut rest);
    true
}

/// Binds a bridge port. A daemon standing in on it for a Studio launch gives
/// it up when asked; the launch tickets stay for the daemon that binds.
pub(super) fn bind_bridge_port(host: &str, port: u16) -> io::Result<TcpListener> {
    let first = TcpListener::bind((host, port));
    match &first {
        Err(error) if error.kind() == io::ErrorKind::AddrInUse && ask_to_yield(host, port) => {}
        _ => return first,
    }
    let started = Instant::now();
    loop {
        thread::sleep(Duration::from_millis(25));
        match TcpListener::bind((host, port)) {
            Err(error)
                if error.kind() == io::ErrorKind::AddrInUse
                    && started.elapsed() < Duration::from_secs(1) => {}
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stand_in_turns_strangers_away_and_gives_its_port_to_a_starting_daemon() {
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let serving = thread::spawn(move || stand_in(&[port], || true));
        let address = SocketAddr::from(([127, 0, 0, 1], port));
        let started = Instant::now();
        let mut stranger = loop {
            match TcpStream::connect(address) {
                Ok(stream) => break stream,
                Err(error) if started.elapsed() > Duration::from_secs(5) => panic!("{error}"),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        };
        stranger
            .write_all(b"GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n")
            .unwrap();
        stranger
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut answer = Vec::new();
        let _ = stranger.read_to_end(&mut answer);
        assert!(answer.is_empty(), "a stranger gets no handshake");
        assert!(
            TcpListener::bind(address).is_err(),
            "the stand-in keeps the port"
        );

        let listener = bind_bridge_port("127.0.0.1", port).expect("the stand-in yields");
        assert!(serving.join().unwrap(), "the stand-in reports the yield");
        drop(listener);
    }
}
