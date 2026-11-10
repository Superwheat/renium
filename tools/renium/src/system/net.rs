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
