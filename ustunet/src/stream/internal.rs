use crate::dispatch::poll_queue::DispatchQueue;
use crate::dispatch::{packet_to_bytes, SocketHandle};
use crate::stream::{ReadinessState, Tcp, TcpLock, WriteReadiness};
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
    ) -> Connection {
        Connection {
            socket,
            handle: addr,
            read_readiness,
            write_readiness,
        }
    }
    /// Process incoming packet and queue for polling.
    pub async fn process(
        &mut self,
        timestamp: Instant,
        ip_repr: &IpRepr,
        tcp_repr: &TcpRepr<'_>,
        send_poll: &mut DispatchQueue,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, Error> {
        use std::ops::Deref;
        let mut socket = self.socket.lock().await;
        check_acceptability(socket.deref(), ip_repr, tcp_repr)?;
        let reply = socket.process(timestamp, &ip_repr, &tcp_repr);
        if socket.can_recv() {
            if let Some(waker) = self.read_readiness.lock().unwrap().waker.take() {
                waker.wake();
            }
        }
        send_poll.send(self.handle.clone(), socket.poll_at());
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
        send_poll: &mut DispatchQueue,
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
        send_poll.send(self.handle.clone(), poll_at);
        Ok(())
    }
}

fn check_acceptability(tcp: &Tcp, ip_repr: &IpRepr, repr: &TcpRepr) -> Result<(), smoltcp::Error> {
    use smoltcp::socket::TcpState as State;
    use smoltcp::Error::Dropped;
    use std::borrow::Borrow;
    let error = Err(Dropped);

    let tcp = tcp.borrow();
    let state = tcp.state();
    let local_endpoint = tcp.local_endpoint();
    let remote_endpoint = tcp.remote_endpoint();
    if state == State::Closed {
        warn!(
            "Socket closed, could not accept packet with source {:?}:{} and destination {:?}:{}.",
            ip_repr.src_addr(),
            repr.src_port,
            ip_repr.dst_addr(),
            repr.dst_port,
        );
        return Err(Dropped);
    }

    // If we're still listening for SYNs and the packet has an ACK, it cannot
    // be destined to this socket, but another one may well listen on the same
    // local endpoint.
    if state == State::Listen && repr.ack_number.is_some() {
        warn!(
            "Socket listening for SYN could not accept ACK, from {:?} to {:?}",
            remote_endpoint, local_endpoint
        );
        return Err(Dropped);
    }

    // Reject packets with a wrong destination.
    if local_endpoint.port != repr.dst_port {
        warn!(
            "Destination {:?} does not match local_endpoint {:?}",
            repr.dst_port, local_endpoint
        );
        return error;
    }
    if !local_endpoint.addr.is_unspecified() && local_endpoint.addr != ip_repr.dst_addr() {
        warn!(
            "Destination address {:?} does not match local endpoint {:?}",
            ip_repr.dst_addr(),
            local_endpoint.addr
        );
        return error;
    }

    // Reject packets from a source to which we aren't connected.
    if remote_endpoint.port != 0 && remote_endpoint.port != repr.src_port {
        warn!(
            "Packet source port {} does not match remote endpoint {:?}",
            repr.src_port, remote_endpoint
        );
        return error;
    }
    if !remote_endpoint.addr.is_unspecified() && remote_endpoint.addr != ip_repr.src_addr() {
        warn!(
            "Packet source address {:?} does not match remote address {:?}",
            ip_repr.src_addr(),
            remote_endpoint.addr
        );
        return error;
    }

    Ok(())
}
