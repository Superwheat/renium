use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::ops::Deref;
use std::sync::Arc;

// Share cancellation/read/write access without duplicating an OS socket. On
// Windows, duplicated sockets can be inherited by children and outlive us.
#[derive(Clone)]
pub(crate) struct SharedTcpStream(Arc<TcpStream>);

impl From<TcpStream> for SharedTcpStream {
    fn from(stream: TcpStream) -> Self {
        Self(Arc::new(stream))
    }
}

impl Deref for SharedTcpStream {
    type Target = TcpStream;

    fn deref(&self) -> &TcpStream {
        &self.0
    }
}

impl Read for SharedTcpStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        (&*self.0).read(bytes)
    }
}

impl Write for SharedTcpStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        (&*self.0).write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        (&*self.0).flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Shutdown, TcpListener};
    use std::time::Duration;

    fn pair() -> (SharedTcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        (stream.into(), listener.accept().unwrap().0)
    }

    #[test]
    fn shutdown_interrupts_a_shared_blocking_read() {
        let (stream, _peer) = pair();
        let mut reader = stream.clone();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || sent.send(reader.read(&mut [0])).unwrap());
        stream.shutdown(Shutdown::Both).unwrap();
        match received.recv_timeout(Duration::from_secs(1)).unwrap() {
            Ok(count) => assert_eq!(count, 0),
            Err(error) => assert!(!matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            )),
        }
        thread.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn launched_child_does_not_keep_shared_sockets_alive() {
        use std::os::windows::io::AsRawSocket;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

        let (stream, mut peer) = pair();
        let cancel = stream.clone();
        assert_eq!(stream.as_raw_socket(), cancel.as_raw_socket());
        let child = Command::new("cmd.exe")
            .args(["/d", "/c", "set /p renium_wait="])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .unwrap();
        let _cleanup = crate::system::files::OnDrop::new(move || {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        });
        drop(stream);
        drop(cancel);
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        assert_eq!(peer.read(&mut [0]).unwrap(), 0, "child inherited a socket");
    }
}
