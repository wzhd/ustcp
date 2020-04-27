use crate::dispatch::poll_queue::QueueUpdater;
use crate::dispatch::{packet_to_bytes, SocketHandle};
use crate::stream::{ReadinessState, TcpLock, WriteReadiness};
use smoltcp::iface::Packet;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::socket::PollAt;
use smoltcp::time::Instant;
use smoltcp::wire::IpRepr;
use smoltcp::wire::TcpRepr;
use smoltcp::Error;
use std::fmt;
use std::fmt::Formatter;

/// Reference to TcpSocket.
/// Notify readers and writers after IO activities.
pub(crate) struct Connection {
    pub socket: TcpLock,
    /// Can be used as a key in key-value storage.
    handle: SocketHandle,
    /// Contains an optional Waker which would be set by the reader
    /// after exhausting the rx buffer.
    read_readiness: ReadinessState,
    write_readiness: WriteReadiness,
    /// Send timing information to a queue to schedule the socket for future polling.
    send_poll: QueueUpdater,
}

impl fmt::Debug for Connection {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "Connection")
    }
}

impl Connection {
    pub(super) fn new(
        socket: TcpLock,
        addr: SocketHandle,
        read_readiness: ReadinessState,
        write_readiness: WriteReadiness,
        send_poll: QueueUpdater,
    ) -> Connection {
        Connection {
            socket,
            handle: addr,
            read_readiness,
            write_readiness,
            send_poll,
        }
    }
    /// Process incoming packet and queue for polling.
    pub async fn process(
        &mut self,
        timestamp: Instant,
        ip_repr: &IpRepr,
        tcp_repr: &TcpRepr<'_>,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, Error> {
        let mut socket = self.socket.lock().await;
        let reply = socket.process(timestamp, &ip_repr, &tcp_repr);
        if socket.can_recv() {
            if let Some(waker) = self.read_readiness.lock().unwrap().waker.take() {
                waker.wake();
            }
        }
        self.send_poll
            .send(self.handle.clone(), socket.poll_at())
            .await
            .expect("queue closed");
        trace!("connection reply {:?}", reply);
        reply
    }

    /// Serialise outgoing packet and queue for next dispatch.
    /// Assumes the packet will be written to network device immediately.
    pub(crate) async fn dispatch(
        &mut self,
        mut buf: &mut Vec<u8>,
        timestamp: Instant,
        capabilities: &DeviceCapabilities,
    ) -> Result<(), smoltcp::Error> {
        assert_eq!(0, buf.len(), "Given buffer should be empty.");
        let mut tcp = self.socket.lock().await;
        let r = tcp.dispatch(timestamp, capabilities, |(i, t)| {
            let p = Packet::Tcp((i, t));
            packet_to_bytes(p, &mut buf, &capabilities.checksum)?;
            let ns = buf.len();
            if ns > 0 {
                debug!("Filled tcp dispatch packet {} bytes.", ns);
            } else {
                error!("Dispatched empty tcp packet.")
            }
            Ok(())
        });
        match r {
            Ok(()) => {}
            Err(smoltcp::Error::Exhausted) => {
                // smoltcp probably just has nothing to send
            }
            Err(error) => return Err(error),
        }
        if tcp.can_send() {
            if let Some(waker) = self.write_readiness.lock().unwrap().waker.take() {
                trace!("Waking writer.");
                waker.wake();
            }
        }
        let poll_at = tcp.poll_at();
        match tcp.poll_at() {
            PollAt::Ingress => {
                trace!("tcp socket becomes inactive");
            }
            PollAt::Now => {
                trace!("tcp socket is still active");
            }
            PollAt::Time(instant) => {
                trace!("tcp socket needs to be polled at {:?}", instant);
            }
        }
        self.send_poll
            .send(self.handle.clone(), poll_at)
            .await
            .expect("Poll Queue closed");
        Ok(())
    }
}
