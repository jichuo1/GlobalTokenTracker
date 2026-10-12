//! Plain or TLS byte streams with bounded, deadline-limited reads — shared
//! by the server, the pull client and both ends of pairing.

use rustls::{ClientConnection, ServerConnection, StreamOwned};
use serde_json::Value;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant};

pub(crate) const IO_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum Stream {
    Plain(TcpStream),
    TlsServer(Box<StreamOwned<ServerConnection, TcpStream>>),
    TlsClient(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Stream {
    pub(crate) fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            Self::TlsServer(s) => &s.sock,
            Self::TlsClient(s) => &s.sock,
        }
    }

    /// TLS close_notify, then half-close; briefly discard what the other
    /// side is still sending so the kernel does not answer it with RST (which
    /// can destroy a response in flight).
    pub(crate) fn close(&mut self, drain: u64) {
        match self {
            Self::Plain(_) => {}
            Self::TlsServer(s) => s.conn.send_close_notify(),
            Self::TlsClient(s) => s.conn.send_close_notify(),
        }
        let _ = self.flush();
        let tcp = self.tcp();
        let _ = tcp.shutdown(Shutdown::Write);
        let _ = tcp.set_read_timeout(Some(Duration::from_millis(500)));
        let _ = std::io::copy(&mut tcp.take(drain), &mut std::io::sink());
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::TlsServer(s) => s.read(buf),
            Self::TlsClient(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::TlsServer(s) => s.write(buf),
            Self::TlsClient(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::TlsServer(s) => s.flush(),
            Self::TlsClient(s) => s.flush(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WireError {
    TooLarge,
    Timeout,
    Closed,
}

/// A stream plus read-ahead, so bytes read past a delimiter are kept.
pub(crate) struct Wire {
    pub(crate) stream: Stream,
    buf: Vec<u8>,
}

impl Wire {
    pub(crate) fn new(stream: Stream) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    /// One read into the buffer, waiting at most until `deadline`.
    fn fill(&mut self, deadline: Instant) -> Result<(), WireError> {
        let mut chunk = [0u8; 4096];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(WireError::Timeout);
            }
            let _ = self
                .stream
                .tcp()
                .set_read_timeout(Some(left.min(IO_TIMEOUT)));
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(WireError::Closed),
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(());
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(WireError::Closed),
            }
        }
    }

    /// Bytes up to (excluding) `delim`, at most `limit` of them.
    pub(crate) fn read_until(
        &mut self,
        delim: &[u8],
        limit: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, WireError> {
        let mut scanned: usize = 0;
        loop {
            let from = scanned.saturating_sub(delim.len() - 1);
            if let Some(pos) = self.buf[from..]
                .windows(delim.len())
                .position(|w| w == delim)
                .map(|p| from + p)
            {
                if pos > limit {
                    return Err(WireError::TooLarge);
                }
                let rest = self.buf.split_off(pos + delim.len());
                let mut line = std::mem::replace(&mut self.buf, rest);
                line.truncate(pos);
                return Ok(line);
            }
            if self.buf.len() > limit {
                return Err(WireError::TooLarge);
            }
            scanned = self.buf.len();
            self.fill(deadline)?;
        }
    }

    /// Everything until EOF, at most `cap` bytes.
    pub(crate) fn read_to_end(
        &mut self,
        cap: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, WireError> {
        loop {
            if self.buf.len() > cap {
                return Err(WireError::TooLarge);
            }
            match self.fill(deadline) {
                Ok(()) => {}
                Err(WireError::Closed) => return Ok(std::mem::take(&mut self.buf)),
                Err(e) => return Err(e),
            }
        }
    }

    /// One newline-terminated JSON message (pairing).
    pub(crate) fn recv_json(&mut self, deadline: Instant) -> Result<Value, WireError> {
        let line = self.read_until(b"\n", 4096, deadline)?;
        serde_json::from_slice(&line).map_err(|_| WireError::Closed)
    }

    pub(crate) fn send_json(&mut self, value: &Value) -> std::io::Result<()> {
        let mut line = serde_json::to_vec(value)?;
        line.push(b'\n');
        self.stream.write_all(&line)?;
        self.stream.flush()
    }
}
