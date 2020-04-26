//! TcpStream and private structures.
use super::SocketLock;
use super::SocketLockGuard;
use crate::dispatch::poll_queue::QueueUpdater;
use crate::sockets::AddrPair;
use crate::stream::internal::Connection;
use futures::task::Poll;
use futures::{ready, FutureExt};
use smoltcp::socket::{TcpSocket, TcpState};
use std::fmt;
use std::fmt::Formatter;
use std::future::Future;
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

type SocketPointer = Arc<SocketLock<TcpSocket<'static>>>;
type LockedSocket = SocketLockGuard<TcpSocket<'static>>;
type LockOutput = (SocketPointer, LockedSocket);
type LockFuture = Pin<Box<dyn Future<Output = LockOutput> + Send>>;

pub struct TcpStream {
    pub writer: WriteHalf,
    pub reader: ReadHalf,
    /// Local and peer address.
    // Immutable.
    addr: AddrPair,
}

pub struct ReadHalf {
    socket_locking: LockFuture,
    shared_state: ReadinessState,
}

pub struct WriteHalf {
    socket_locking: LockFuture,
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
        socket: SocketPointer,
        poll_queue: QueueUpdater,
        addr: AddrPair,
    ) -> (TcpStream, Connection) {
        let (reader, set_ready) = ReadHalf::new(socket.clone());
        let (writer, write_readiness) = WriteHalf::new(socket.clone());
        let tcp = TcpStream {
            reader,
            writer,
            addr: addr.clone(),
        };
        let connection = Connection::new(socket, addr, set_ready, write_readiness, poll_queue);
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

async fn lock_socket(socket: SocketPointer) -> LockOutput {
    let locked = { socket.lock().await };
    (socket, locked)
}

impl ReadHalf {
    fn new(socket: SocketPointer) -> (Self, ReadinessState) {
        let state = SharedState { waker: None };
        let shared_state = Arc::new(std_sync::Mutex::new(state));
        let s = ReadHalf {
            socket_locking: lock_socket(socket).boxed(),
            shared_state: shared_state.clone(),
        };
        (s, shared_state)
    }
    fn read_impl(mut s: LockedSocket, buf: &mut [u8]) -> io::Result<Option<usize>> {
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
    fn new(socket: SocketPointer) -> (Self, WriteReadiness) {
        let shared_state = Arc::new(std_sync::Mutex::new(SharedState { waker: None }));
        let s = Self {
            socket_locking: lock_socket(socket).boxed(),
            shared_state: shared_state.clone(),
        };
        (s, shared_state)
    }
}

impl AsyncRead for ReadHalf {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> task::Poll<io::Result<usize>> {
        let (pointer, socket) = ready!(self.socket_locking.as_mut().poll(cx));
        self.socket_locking = lock_socket(pointer).boxed();
        let read_result = Self::read_impl(socket, buf);
        trace!("read result: {:?}", read_result);
        let size = read_result?;
        trace!("read size: {:?}", size);
        if let Some(n) = size {
            return task::Poll::Ready(Ok(n));
        }
        trace!("set waker");
        self.shared_state.lock().unwrap().waker = Some(cx.waker().clone());
        task::Poll::Pending
    }
}

impl AsyncWrite for WriteHalf {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> task::Poll<io::Result<usize>> {
        let (pointer, mut s): (SocketPointer, LockedSocket) =
            ready!(self.socket_locking.as_mut().poll(cx));
        self.socket_locking = lock_socket(pointer).boxed();
        if s.can_send() {
            let s = s
                .send_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            trace!("Written {} bytes.", s);
            return Poll::Ready(Ok(s));
        }
        trace!("Setting waker for writer.");
        self.shared_state.lock().unwrap().waker = Some(cx.waker().clone());
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
