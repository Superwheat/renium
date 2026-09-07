use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::net::Shutdown;
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Instant;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::{Failure, Request, Response};
use crate::daemon::transport::{BoundedLineRead, MAX_DAEMON_LINE_BYTES, read_bounded_line};
use crate::system::net::SharedTcpStream;

const WORKERS: usize = 8;
const QUEUED_REQUESTS: usize = 16;

#[derive(Default)]
pub(super) struct RequestControl(Mutex<(bool, Option<SharedTcpStream>)>);

impl RequestControl {
    pub(super) fn attach(&self, stream: &SharedTcpStream) -> Result<()> {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if state.0 {
            bail!("Request was cancelled before dispatch");
        }
        state.1 = Some(stream.clone());
        Ok(())
    }

    fn cancel(&self) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.0 = true;
        if let Some(stream) = state.1.take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).0
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRequest {
    cancel: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProxyResponse<'a> {
    #[serde(flatten)]
    response: &'a Response,
    proxy_cancellation: bool,
}

fn respond(output: &Mutex<impl Write>, response: &Response) -> Result<()> {
    let mut output = output.lock().unwrap_or_else(PoisonError::into_inner);
    serde_json::to_writer(
        &mut *output,
        &ProxyResponse {
            response,
            proxy_cancellation: true,
        },
    )?;
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

// The reader remains available to cancel requests even when all workers are busy.
pub(super) fn run(
    mut reader: impl BufRead,
    output: impl Write + Send,
    forward: impl Fn(&Request, &RequestControl) -> Response + Sync,
) -> Result<()> {
    let output = Mutex::new(output);
    let active = Mutex::new(HashMap::<u64, Arc<RequestControl>>::new());
    let (sender, receiver) = mpsc::sync_channel::<(Request, Arc<RequestControl>)>(QUEUED_REQUESTS);
    let receiver = Mutex::new(receiver);
    std::thread::scope(|scope| {
        for _ in 0..WORKERS {
            scope.spawn(|| {
                loop {
                    let work = receiver
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .recv();
                    let Ok((request, control)) = work else { break };
                    if !control.cancelled() {
                        let response = forward(&request, &control);
                        let _ = respond(&output, &response);
                    }
                    active
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&request.id);
                }
            });
        }
        let result = (|| {
            let mut line = String::new();
            loop {
                match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES)? {
                    BoundedLineRead::Eof => break,
                    BoundedLineRead::TooLong => {
                        respond(
                            &output,
                            &super::runtime::oversized_automation_request_response(),
                        )?;
                        continue;
                    }
                    BoundedLineRead::Line => {}
                }
                if line.trim().is_empty() {
                    continue;
                }
                if let Ok(cancel) = serde_json::from_str::<CancelRequest>(&line) {
                    if let Some(control) = active
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .get(&cancel.cancel)
                    {
                        control.cancel();
                    }
                    continue;
                }
                let request = match serde_json::from_str::<Request>(&line) {
                    Ok(request) => request,
                    Err(error) => {
                        respond(
                            &output,
                            &Response::failure(
                                0,
                                Instant::now(),
                                Failure::new(
                                    "bad_req",
                                    format!("Invalid request JSON: {error}"),
                                    false,
                                    "cap",
                                ),
                            ),
                        )?;
                        continue;
                    }
                };
                let id = request.id;
                let control = Arc::new(RequestControl::default());
                {
                    let mut active = active.lock().unwrap_or_else(PoisonError::into_inner);
                    if active.contains_key(&id) {
                        bail!("Duplicate active request ID {id}");
                    }
                    active.insert(id, Arc::clone(&control));
                }
                if sender.try_send((request, control)).is_err() {
                    active
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .remove(&id);
                    respond(
                        &output,
                        &Response::failure(
                            id,
                            Instant::now(),
                            Failure::new(
                                "busy",
                                "Too many pending editor requests; retry after the current operation.",
                                true,
                                "retry",
                            ),
                        ),
                    )?;
                }
            }
            Ok(())
        })();
        for control in active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
        {
            control.cancel();
        }
        drop(sender);
        result
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufReader, Read};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Condvar;
    use std::time::Duration;

    fn sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let peer = listener.accept().unwrap().0;
        for stream in [&client, &peer] {
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
        }
        (client, peer)
    }

    #[test]
    fn cancellation_closes_only_its_request_and_prevents_late_dispatch() {
        let (client, mut peer) = sockets();
        let mut client = SharedTcpStream::from(client);
        let (other, mut other_peer) = sockets();
        let mut other = SharedTcpStream::from(other);
        let cancelled = RequestControl::default();
        cancelled.attach(&client).unwrap();
        cancelled.cancel();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        assert!(cancelled.attach(&other).is_err());
        other.write_all(b"ok").unwrap();
        let mut bytes = [0; 2];
        other_peer.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ok");
        assert!(client.write_all(b"cancelled").is_err());
    }

    #[test]
    fn burst_is_bounded_and_queued_cancellation_does_not_block_other_requests() {
        let (mut client, peer) = sockets();
        let mut responses = BufReader::new(client.try_clone().unwrap());
        let (entered, observed) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_gate = Arc::clone(&gate);
        let proxy = std::thread::spawn(move || {
            run(
                BufReader::new(peer.try_clone().unwrap()),
                peer,
                |request, _| {
                    entered.send(request.id).unwrap();
                    if request.id <= WORKERS as u64 {
                        let (lock, ready) = &*worker_gate;
                        let (_guard, wait) = ready
                            .wait_timeout_while(
                                lock.lock().unwrap(),
                                Duration::from_secs(3),
                                |open| !*open,
                            )
                            .unwrap();
                        assert!(!wait.timed_out(), "test did not release occupied workers");
                    }
                    Response::success(request.id, Instant::now(), json!({}))
                },
            )
        });
        let send = |client: &mut TcpStream, id| {
            writeln!(client, "{}", json!({"v": 1, "id": id, "op": 0, "p": {}})).unwrap();
        };
        for id in 1..=WORKERS as u64 {
            send(&mut client, id);
        }
        for _ in 0..WORKERS {
            observed.recv_timeout(Duration::from_secs(3)).unwrap();
        }
        for id in WORKERS + 1..=WORKERS + QUEUED_REQUESTS {
            send(&mut client, id as u64);
        }
        writeln!(client, "{}", json!({"cancel": WORKERS + 1})).unwrap();
        send(&mut client, 100);
        let mut line = String::new();
        responses.read_line(&mut line).unwrap();
        let busy: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(busy["id"], 100);
        assert_eq!(busy["e"]["c"], "busy");
        assert_eq!(busy["proxyCancellation"], true);
        assert!(
            observed.try_recv().is_err(),
            "more than eight workers were admitted"
        );
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        let mut completed = Vec::new();
        for _ in 0..WORKERS + QUEUED_REQUESTS - 1 {
            line.clear();
            responses.read_line(&mut line).unwrap();
            let response: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(response["ok"], 1);
            completed.push(response["id"].as_u64().unwrap());
        }
        assert!(!completed.contains(&((WORKERS + 1) as u64)));
        assert_eq!(observed.try_iter().count(), QUEUED_REQUESTS - 1);
        client.shutdown(Shutdown::Both).unwrap();
        proxy.join().unwrap().unwrap();
    }
}
