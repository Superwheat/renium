use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::op;
use crate::app::timing::current_millis;
use crate::daemon::daemon_control_endpoints;
use crate::daemon::transport::{
    BoundedLineRead, DAEMON_CONTROL_CONNECT_TIMEOUT, DAEMON_CONTROL_IDLE_TIMEOUT,
    DAEMON_CONTROL_RESPONSE_TIMEOUT, MAX_DAEMON_LINE_BYTES, read_bounded_line,
};

fn transport_failure(id: u64, error: anyhow::Error) -> super::Response {
    super::Response::failure(
        id,
        Instant::now(),
        super::Failure::new("bridge_off", format!("{error:#}"), true, "bind"),
    )
}

pub(crate) fn send_request(request: &super::Request) -> Result<super::Response> {
    if let Some(response) = try_send_request(request)? {
        return Ok(response);
    }
    if request.op == op::BIND {
        crate::daemon::ensure_shared_daemon("8781,8782", 1.0)?;
    }
    try_send_request(request)?.context("Renium daemon is not running")
}

pub(crate) fn try_send_request(request: &super::Request) -> Result<Option<super::Response>> {
    let _trace_context = crate::app::output::global_log_enabled(5).then(|| {
        crate::app::timing::enter_trace_context(Some(crate::app::timing::TraceContext {
            request_id: request.id,
            context_id: request.cx,
        }))
    });
    let _trace = crate::app::timing::trace_scope(
        "daemon.rpc",
        super::opcode_by_id(request.op).map_or("unknown operation", |operation| operation.name),
    );
    let Some(stream) = connect_daemon() else {
        return Ok(None);
    };
    send_on_stream(&stream, request).map(Some)
}

fn connect_daemon() -> Option<TcpStream> {
    let _trace =
        crate::app::timing::trace_scope("daemon.connect", "endpoint discovery and connect");
    daemon_control_endpoints().into_iter().find_map(|address| {
        TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT).ok()
    })
}

fn send_on_stream(stream: &TcpStream, request: &super::Request) -> Result<super::Response> {
    let timeout = match request.op {
        op::PERFORMANCE_MONITOR
            if matches!(request.p["action"].as_str(), Some("micro" | "micro-stop")) =>
        {
            Duration::from_secs(30)
        }
        op::CAP
        | op::BIND
        | op::STUDIOS
        | op::STUDIO_STATUS
        | op::PROPERTY_ACCESS
        | op::PERFORMANCE_MONITOR => Duration::from_secs(5),
        _ => DAEMON_CONTROL_RESPONSE_TIMEOUT,
    };
    send_on_stream_with_timeout(stream, request, timeout)
}

struct DeadlineReader<'a> {
    stream: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon response deadline elapsed",
            ));
        }
        self.stream.set_read_timeout(Some(remaining))?;
        self.stream.read(bytes)
    }
}

fn send_on_stream_with_timeout(
    mut stream: &TcpStream,
    request: &super::Request,
    timeout: Duration,
) -> Result<super::Response> {
    let deadline = Instant::now() + timeout;
    let encode = crate::app::timing::trace_scope("daemon.encode", "encode and authorize request");
    let encoded = super::authorization::encode_request(request, stream.peer_addr()?.port())?;
    drop(encode);
    let send = crate::app::timing::trace_scope("daemon.send", "write request");
    stream.set_write_timeout(Some(timeout.min(DAEMON_CONTROL_IDLE_TIMEOUT)))?;
    writeln!(stream, "{}", encoded)?;
    stream.flush()?;
    drop(send);
    let wait = crate::app::timing::trace_scope("daemon.wait", "read response");
    let mut reader = BufReader::new(DeadlineReader { stream, deadline });
    let mut line = String::new();
    match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES).with_context(|| {
        format!(
            "Waiting for daemon response to operation {} ({}s limit)",
            request.op,
            timeout.as_secs_f64()
        )
    })? {
        BoundedLineRead::Line => {}
        BoundedLineRead::Eof => bail!("Renium daemon closed the connection before responding"),
        BoundedLineRead::TooLong => bail!("Renium daemon response exceeded the protocol limit"),
    }
    drop(wait);
    let _decode = crate::app::timing::trace_scope("daemon.decode", "decode response");
    let response = serde_json::from_str(line.trim()).context("Invalid Renium daemon response")?;
    print_update_notice(&response);
    Ok(response)
}

fn print_update_notice(response: &super::Response) {
    let Some(version) = response.u.as_deref() else {
        return;
    };
    crate::app::update::report_update_notice(version);
}

pub(crate) fn shared_daemon_available() -> bool {
    crate::daemon::daemon_control_endpoints()
        .into_iter()
        .any(|address| daemon_endpoint_available(address, Duration::from_millis(500)))
}

pub(crate) fn daemon_endpoint_available(address: SocketAddr, timeout: Duration) -> bool {
    let request = super::Request {
        v: super::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: op::CAP,
        cx: None,
        p: json!({}),
    };
    let Ok(stream) = TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT) else {
        return false;
    };
    send_on_stream_with_timeout(&stream, &request, timeout).is_ok_and(|response| response.ok == 1)
}

fn forward_proxy_request(
    request: &super::Request,
    control: &super::stdio_proxy::RequestControl,
    bridge_ports: &str,
    bridge_wait_seconds: f64,
) -> super::Response {
    let forward = || {
        let stream = match connect_daemon() {
            Some(stream) => stream,
            None => {
                crate::daemon::ensure_shared_daemon(bridge_ports, bridge_wait_seconds)?;
                connect_daemon().context("Renium daemon did not become available")?
            }
        };
        let stream = crate::system::net::SharedTcpStream::from(stream);
        control.attach(&stream)?;
        send_on_stream(&stream, request)
    };
    forward().unwrap_or_else(|error| transport_failure(request.id, error))
}

pub(crate) fn run_stdio_proxy(bridge_ports: String, bridge_wait_seconds: f64) -> Result<()> {
    super::stdio_proxy::run(io::stdin().lock(), io::stdout(), |request, control| {
        forward_proxy_request(request, control, &bridge_ports, bridge_wait_seconds)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn response_deadline_is_absolute_and_reports_the_waiting_operation() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut peer = listener.accept().unwrap().0;
        peer.write_all(b"ab").unwrap();
        let mut reader = DeadlineReader {
            stream: &client,
            deadline: Instant::now() + Duration::from_secs(2),
        };
        assert_eq!(reader.read(&mut [0]).unwrap(), 1);
        reader.deadline = Instant::now();
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        let mut remaining = [0];
        client.read_exact(&mut remaining).unwrap();
        assert_eq!(remaining[0], b'b');
        let error = send_on_stream_with_timeout(
            &client,
            &super::super::Request {
                v: super::super::PROTOCOL_VERSION,
                id: 1,
                op: op::STUDIO_STATUS,
                cx: None,
                p: json!({}),
            },
            Duration::from_millis(20),
        )
        .err()
        .expect("an unanswered status must time out");
        assert!(format!("{error:#}").contains("Waiting for daemon response to operation 51"));
    }
}
