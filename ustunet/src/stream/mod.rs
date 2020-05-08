//! TcpStream and private structures.
use super::SocketLock;
use crate::dispatch::poll_queue::{QueueUpdater, ShutdownNotifier, ShutdownNotifierBuilder};
use crate::dispatch::SocketHandle;
use crate::sockets::AddrPair;
use crate::stream::internal::Connection;
use futures::future::poll_fn;
use futures::ready;
use futures::task::Poll;
use smoltcp::socket::TcpSocket;
use smoltcp::socket::TcpState;
use std::fmt;
use std::fmt::Formatter;
use std::io;
use std::net::SocketAddr;
use std::ops::DerefMut;
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
type TcpLock = SocketLock<Inner>;

pub(crate) struct Inner {
    tcp: Tcp,
    /// Whether the connection is in queue to be dispatched.
    /// If true, data in the tx buffer will be sent out.
    /// If false, the dispatch queue should be notified
    /// after writing to the tx buffer.
    polling_active: bool,
}

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
    eof_reached: bool,
    /// Send a message when getting dropped.
    shutdown_notifier: ShutdownNotifier,
}

pub struct WriteHalf {
    mutex: TcpLock,
    shared_state: ReadinessState,
    handle: SocketHandle,
    notifier: QueueUpdater,
    shutdown_notifier: ShutdownNotifier,
}

#[derive(Debug)]
pub(crate) struct SharedState {
    pub waker: Option<Waker>,
}

impl SharedState {
    pub(crate) fn wake_once(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

impl TcpStream {
    /// Returns a TcpStream and a struct containing references used
    /// internally to move data between buffers and network interfaces.
    pub(crate) fn new(
        tcp: Tcp,
        poll_queue: QueueUpdater,
        addr: AddrPair,
        shutdown_builder: &ShutdownNotifierBuilder,
    ) -> (TcpStream, Connection) {
        let inner = Inner {
            tcp,
            polling_active: false,
        };
        let tcp_locks = SocketLock::new(inner);
        let (reader, set_ready) =
            ReadHalf::new(tcp_locks.0, shutdown_builder.reader_build(addr.clone()));
        let (writer, write_readiness) = WriteHalf::new(
            tcp_locks.1,
            poll_queue,
            addr.clone(),
            shutdown_builder.writer_build(addr.clone()),
        );
        let tcp = TcpStream {
            reader,
            writer,
            addr: addr.clone(),
        };
        let connection = Connection::new(tcp_locks.2, addr, set_ready, write_readiness);
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

    /// Close the transmit half of the full-duplex connection.
    pub async fn close(&mut self) {
        self.writer.close().await;
    }
}

impl ReadHalf {
    fn new(socket: TcpLock, shutdown_notifier: ShutdownNotifier) -> (Self, ReadinessState) {
        let state = SharedState { waker: None };
        let shared_state = Arc::new(std_sync::Mutex::new(state));
        let s = ReadHalf {
            mutex: socket,
            shared_state: shared_state.clone(),
            eof_reached: false,
            shutdown_notifier,
        };
        (s, shared_state)
    }
}

impl WriteHalf {
    fn new(
        socket: TcpLock,
        notifier: QueueUpdater,
        handle: SocketHandle,
        shutdown_notifier: ShutdownNotifier,
    ) -> (Self, WriteReadiness) {
        let shared_state = Arc::new(std_sync::Mutex::new(SharedState { waker: None }));
        let s = Self {
            mutex: socket,
            shared_state: shared_state.clone(),
            notifier,
            handle,
            shutdown_notifier,
        };
        (s, shared_state)
    }

    /// Close the transmit half of the full-duplex connection.
    pub async fn close(&mut self) {
        poll_fn(|cx| self.poll_close(cx)).await
    }
    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut guard = ready!(self.mutex.poll_lock(cx));
        let Inner {
            tcp,
            polling_active,
        } = guard.deref_mut();
        tcp.close();
        if !*polling_active {
            self.notifier.send(self.handle.clone(), tcp.poll_at());
        };
        Poll::Ready(())
    }
}

impl AsyncRead for ReadHalf {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        use std::borrow::BorrowMut;
        let Self {
            ref mut mutex,
            ref mut shared_state,
            ref mut eof_reached,
            ..
        } = self.get_mut();
        let l = mutex.poll_lock(cx);
        let mut guard = ready!(l);
        let tcp = &mut guard.borrow_mut().tcp;
        if tcp.can_recv() {
            // Actually there should not be any error when can_recv is true.
            let n = tcp
                .recv_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            if n > 0 {
                trace!("recv_slice {}", n);
                return Poll::Ready(Ok(n));
            } else {
                error!("Read zero when can_recv is true.");
            }
        }
        let state = tcp.state();
        if tcp.may_recv() ||
            // Reader could be used before the handshake is finished.
            state == TcpState::SynReceived
        {
            shared_state.lock().unwrap().waker = Some(cx.waker().clone());
            return task::Poll::Pending;
        } else {
            debug!("Socket may not receive in state {:?}", state);
        }
        if *eof_reached {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        *eof_reached = true;
        Poll::Ready(Ok(0))
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
            ref mut notifier,
            handle,
            ..
        } = self.get_mut();
        let p = mutex.poll_lock(cx);
        let mut guard = ready!(p);
        let inactive = !guard.polling_active;
        let s = &mut guard.tcp;
        let n = if s.can_send() {
            let s = s
                .send_slice(buf)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
            trace!("Written {} bytes.", s);
            s
        } else {
            0
        };
        if inactive {
            notifier.send(handle.clone(), s.poll_at());
        }
        if n > 0 {
            return Poll::Ready(Ok(n));
        }
        if !s.may_send() {
            return Poll::Ready(Err(io::ErrorKind::ConnectionAborted.into()));
        }
        trace!("Setting waker for writer.");
        shared_state.lock().unwrap().waker = Some(cx.waker().clone());
        Poll::Pending
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        ready!(self.poll_close(cx));
        Poll::Ready(Ok(()))
    }
}

impl Drop for ReadHalf {
    fn drop(&mut self) {
        debug!("Drop ReadHalf {:?}", self.shutdown_notifier);
        self.shutdown_notifier.notify();
    }
}

impl Drop for WriteHalf {
    fn drop(&mut self) {
        debug!("Drop WriteHalf {:?}", self.shutdown_notifier);
        self.shutdown_notifier.notify();
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
