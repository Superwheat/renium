use std::io::{self, BufReader, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
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
        crate::daemon::start_shared_daemon("8781,8782", 1.0);
    }
    try_send_request(request)?.context("Renium daemon is not running")
}

pub(crate) fn try_send_request(request: &super::Request) -> Result<Option<super::Response>> {
    let Some(stream) = daemon_control_endpoints().into_iter().find_map(|address| {
        TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT).ok()
    }) else {
        return Ok(None);
    };
    send_on_stream(stream, request).map(Some)
}

fn send_on_stream(mut stream: TcpStream, request: &super::Request) -> Result<super::Response> {
    send_on_stream_with_timeout(&mut stream, request, DAEMON_CONTROL_RESPONSE_TIMEOUT)
}

fn send_on_stream_with_timeout(
    stream: &mut TcpStream,
    request: &super::Request,
    timeout: Duration,
) -> Result<super::Response> {
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(DAEMON_CONTROL_IDLE_TIMEOUT));
    writeln!(stream, "{}", serde_json::to_string(request)?)?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
        BoundedLineRead::Line => {}
        BoundedLineRead::Eof => bail!("Renium daemon closed the connection before responding"),
        BoundedLineRead::TooLong => bail!("Renium daemon response exceeded the protocol limit"),
    }
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
    let request = super::Request {
        v: super::PROTOCOL_VERSION,
        id: current_millis().min(u128::from(u64::MAX)) as u64,
        op: op::CAP,
        cx: None,
        p: json!({}),
    };
    daemon_control_endpoints().into_iter().any(|address| {
        let Ok(mut stream) = TcpStream::connect_timeout(&address, DAEMON_CONTROL_CONNECT_TIMEOUT)
        else {
            return false;
        };
        send_on_stream_with_timeout(&mut stream, &request, Duration::from_millis(500))
            .is_ok_and(|response| response.ok == 1)
    })
}

pub(crate) fn run_stdio_proxy() -> Result<()> {
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let mut line = String::new();
    let stdout_gate = Arc::new(Mutex::new(()));
    loop {
        match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
            BoundedLineRead::Eof => break,
            BoundedLineRead::Line => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let request = trimmed.to_string();
                let stdout_gate = Arc::clone(&stdout_gate);
                std::thread::spawn(move || {
                    let response = match serde_json::from_str::<super::Request>(&request) {
                        Ok(request) => match try_send_request(&request) {
                            Ok(Some(response)) => response,
                            Ok(None) => transport_failure(
                                request.id,
                                anyhow::anyhow!("Renium daemon is not running"),
                            ),
                            Err(error) => transport_failure(request.id, error),
                        },
                        Err(error) => super::Response::failure(
                            0,
                            Instant::now(),
                            super::Failure::new(
                                "bad_req",
                                format!("Invalid request JSON: {error}"),
                                false,
                                "cap",
                            ),
                        ),
                    };
                    let Ok(response) = serde_json::to_string(&response) else {
                        return;
                    };
                    let _guard = stdout_gate
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    std::println!("{response}");
                    let _ = io::stdout().flush();
                });
            }
            BoundedLineRead::TooLong => {
                let response = super::runtime::oversized_automation_request_response();
                std::println!("{}", serde_json::to_string(&response)?);
                io::stdout().flush()?;
            }
        }
    }
    Ok(())
}
