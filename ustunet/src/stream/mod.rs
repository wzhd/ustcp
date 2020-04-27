//! TcpStream and private structures.
use super::SocketLock;
use crate::dispatch::poll_queue::QueueUpdater;
use crate::sockets::AddrPair;
use crate::stream::internal::Connection;
use crate::SocketLockGuard;
use futures::ready;
use futures::task::Poll;
use smoltcp::socket::{TcpSocket, TcpState};
use std::fmt;
use std::fmt::Formatter;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync as std_sync;
use std::sync::Arc;
use std::task;
use std::task::{Context, Waker};
use tokio::io::{AsyncRead, AsyncWrite};

pub(crate) type ReadinessState = Arc<std_sync::Mutex<SharedState>>;
pub(crate) type WriteReadiness = Arc<std_sync::Mutex<SharedState>>;

pub(crate) mod internal;

type Tcp = TcpSocket<'static>;
type TcpLock = SocketLock<Tcp>;

pub struct TcpStream {
    pub writer: WriteHalf,
    pub reader: ReadHalf,
    /// Local and peer address.
    // Immutable.
    addr: AddrPair,
}

pub struct ReadHalf {
    mutex: TcpLock,
    shared_state: ReadinessState,
}

pub struct WriteHalf {
    mutex: TcpLock,
    shared_state: ReadinessState,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub waker: Option<Waker>,
}

impl TcpStream {
    /// Returns a TcpStream and a struct containing references used
    /// internally to move data between buffers and network interfaces.
    pub(crate) fn new(
        tcp_locks: (TcpLock, TcpLock, TcpLock),
        poll_queue: QueueUpdater,
        addr: AddrPair,
    ) -> (TcpStream, Connection) {
        let (reader, set_ready) = ReadHalf::new(tcp_locks.0);
        let (writer, write_readiness) = WriteHalf::new(tcp_locks.1);
        let tcp = TcpStream {
            reader,
            writer,
            addr: addr.clone(),
        };
        let connection = Connection::new(tcp_locks.2, addr, set_ready, write_readiness, poll_queue);
        (tcp, connection)
    }

    /// Returns the local address that this TcpStream is bound to.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr.local
    }

    /// Returns the remote address that this stream is connected to.
    pub fn peer_addr(&self) -> SocketAddr {
        self.addr.peer
    }

    pub fn split(self) -> (ReadHalf, WriteHalf) {
        (self.reader, self.writer)
    }
}

impl ReadHalf {
    fn new(socket: TcpLock) -> (Self, ReadinessState) {
        let state = SharedState { waker: None };
        let shared_state = Arc::new(std_sync::Mutex::new(state));
        let s = ReadHalf {
            mutex: socket,
            shared_state: shared_state.clone(),
        };
        (s, shared_state)
    }
    fn read_impl(mut s: SocketLockGuard<'_, Tcp>, buf: &mut [u8]) -> io::Result<Option<usize>> {
        if s.can_recv() {
            let n = s
                .recv_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            if n > 0 {
                trace!("recv_slice {}", n);
                return Ok(Some(n));
            } else {
                trace!("recv_slice empty");
            }
        }
        if s.state() == TcpState::Closed {
            // over
            debug!("no more data to receive");
            return Ok(Some(0));
        }
        Ok(None)
    }
}

impl WriteHalf {
    fn new(socket: TcpLock) -> (Self, WriteReadiness) {
        let shared_state = Arc::new(std_sync::Mutex::new(SharedState { waker: None }));
        let s = Self {
            mutex: socket,
            shared_state: shared_state.clone(),
        };
        (s, shared_state)
    }
}

impl AsyncRead for ReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> task::Poll<io::Result<usize>> {
        let Self {
            ref mut mutex,
            ref mut shared_state,
        } = self.get_mut();
        let l = mutex.poll_lock(cx);
        let guard = ready!(l);
        let read_result = Self::read_impl(guard, buf);
        trace!("read result: {:?}", read_result);
        let size = read_result?;
        trace!("read size: {:?}", size);
        if let Some(n) = size {
            return task::Poll::Ready(Ok(n));
        }
        trace!("set waker");
        shared_state.lock().unwrap().waker = Some(cx.waker().clone());
        task::Poll::Pending
    }
}

impl AsyncWrite for WriteHalf {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> task::Poll<io::Result<usize>> {
        let Self {
            ref mut mutex,
            ref mut shared_state,
        } = self.get_mut();
        let p = mutex.poll_lock(cx);
        let mut s = ready!(p);
        if s.can_send() {
            let s = s
                .send_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            trace!("Written {} bytes.", s);
            return Poll::Ready(Ok(s));
        }

        trace!("Setting waker for writer.");
        shared_state.lock().unwrap().waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        unimplemented!()
    }
}

impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "UsTcpStream({:?})", self.addr)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn t1() {}
}
