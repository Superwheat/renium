use std::io::{self, BufReader, Read};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;

use anyhow::Result;

use super::transport::{BoundedLineRead, MAX_DAEMON_LINE_BYTES, read_bounded_line};
use crate::studio::bridge::BridgeRequestLease;
use crate::system::net::SharedTcpStream;

struct WakeableReader {
    stream: SharedTcpStream,
    wake: TcpStream,
    state: Arc<Mutex<ConnectionState>>,
}

impl Read for WakeableReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            match wait_for_input(&self.stream, &self.wake) {
                Ok(false) => return Ok(0),
                Ok(true) => return self.stream.read(buffer),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == io::ErrorKind::TimedOut
                        && self
                            .state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .active
                            .is_some() =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn wait_for_input(stream: &TcpStream, wake: &TcpStream) -> io::Result<bool> {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{
            POLLRDNORM, WSAGetLastError, WSAPOLLFD, WSAPoll,
        };
        let mut sockets = [stream, wake].map(|socket| WSAPOLLFD {
            fd: socket.as_raw_socket() as usize,
            events: POLLRDNORM,
            revents: 0,
        });
        let result = unsafe { WSAPoll(sockets.as_mut_ptr(), 2, 30_000) };
        if result < 0 {
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Control connection idle",
            ));
        }
        Ok(sockets[1].revents == 0)
    }
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let mut sockets = [stream, wake].map(|socket| libc::pollfd {
            fd: socket.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        let result = unsafe { libc::poll(sockets.as_mut_ptr(), 2, 30_000) };
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Control connection idle",
            ));
        }
        Ok(sockets[1].revents == 0)
    }
}

#[derive(Default)]
struct ConnectionState {
    closed: bool,
    active: Option<Arc<BridgeRequestLease>>,
}

pub(super) enum Request {
    Line(String),
    TooLong,
}

// One blocking reader per connection also observes EOF while a request executes.
// No per-request monitor, socket peeking, or millisecond wakeup loop.
pub(super) struct ControlReader {
    stream: SharedTcpStream,
    wake: TcpStream,
    requests: mpsc::Receiver<Request>,
    state: Arc<Mutex<ConnectionState>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ControlReader {
    pub(super) fn start(
        stream: SharedTcpStream,
        on_cancel: impl Fn(&BridgeRequestLease) + Send + 'static,
    ) -> Result<Self> {
        let input = stream.clone();
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let wake = TcpStream::connect(listener.local_addr()?)?;
        let wake_input = listener.accept()?.0;
        let (sender, requests) = mpsc::sync_channel(1);
        let state = Arc::new(Mutex::new(ConnectionState::default()));
        let reader_state = Arc::clone(&state);
        let thread = thread::Builder::new()
            .name("renium-control-reader".into())
            .spawn(move || {
                let _disconnect = crate::system::files::OnDrop::new(|| {
                    let active = {
                        let mut state = reader_state
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        state.closed = true;
                        state.active.take()
                    };
                    if let Some(lease) = active
                        && lease.cancel()
                    {
                        on_cancel(&lease);
                    }
                });
                let mut reader = BufReader::new(WakeableReader {
                    stream: input,
                    wake: wake_input,
                    state: Arc::clone(&reader_state),
                });
                loop {
                    let mut line = String::new();
                    let request =
                        match read_bounded_line(&mut reader, &mut line, MAX_DAEMON_LINE_BYTES) {
                            Ok(BoundedLineRead::Eof) => break,
                            Ok(BoundedLineRead::TooLong) => Request::TooLong,
                            Ok(BoundedLineRead::Line) if line.trim().is_empty() => continue,
                            Ok(BoundedLineRead::Line) => Request::Line(line),
                            Err(_) => break,
                        };
                    // Bound read-ahead; a flooding/pipelining peer must not grow memory
                    // or prevent disconnect detection while an operation is active.
                    if sender.try_send(request).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stream,
            wake,
            requests,
            state,
            thread: Some(thread),
        })
    }

    pub(super) fn next(&self) -> Option<Request> {
        self.requests.recv().ok()
    }

    pub(super) fn activate(&self, lease: Arc<BridgeRequestLease>) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.closed {
            return false;
        }
        state.active = Some(lease);
        true
    }

    pub(super) fn finish(&self) {
        if let Some(lease) = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .active
            .take()
        {
            lease.finish();
        }
    }
}

impl Drop for ControlReader {
    fn drop(&mut self) {
        let _ = self.wake.shutdown(Shutdown::Both);
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        client.set_nodelay(true).unwrap();
        (client, listener.accept().unwrap().0)
    }

    #[test]
    fn disconnect_cancels_active_request_without_polling() {
        let (mut client, server) = pair();
        let reader = ControlReader::start(server.into(), |_| {
            panic!("pending lease has no Studio operation to cancel")
        })
        .unwrap();
        client.write_all(b"request\n").unwrap();
        assert!(matches!(reader.next(), Some(Request::Line(_))));
        let lease = Arc::new(BridgeRequestLease::new("reader-test".into()));
        assert!(reader.activate(Arc::clone(&lease)));
        drop(client);
        assert!(reader.next().is_none());
        assert!(lease.is_cancelled());
        assert!(!reader.activate(Arc::new(BridgeRequestLease::new("late".into()))));
    }

    #[test]
    fn many_sequential_requests_reuse_reader_and_clean_shutdown_wakes_it() {
        let (mut client, server) = pair();
        let reader =
            ControlReader::start(server.into(), |_| panic!("finished request cancelled")).unwrap();
        for n in 0..100 {
            client.write_all(format!("{n}\n").as_bytes()).unwrap();
            assert!(matches!(reader.next(), Some(Request::Line(_))));
            assert!(reader.activate(Arc::new(BridgeRequestLease::new(n.to_string()))));
            reader.finish();
        }
        drop(reader);
    }

    #[test]
    fn shutdown_wakes_a_partial_line_without_waiting_for_the_peer() {
        let (mut client, server) = pair();
        let reader = ControlReader::start(server.into(), |_| {}).unwrap();
        client.write_all(b"unfinished").unwrap();
        drop(reader);
    }
}
