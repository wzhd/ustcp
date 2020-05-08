use crate::dispatch::poll_queue::{DispatchQueue, Shutdown};
use crate::dispatch::{packet_to_bytes, SocketHandle};
use crate::stream::{Inner, ReadinessState, Tcp, TcpLock, WriteReadiness};
use smoltcp::iface::Packet;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::socket::PollAt;
use smoltcp::time::Instant;
use smoltcp::wire::IpRepr;
use smoltcp::wire::TcpRepr;
use smoltcp::Error;
use std::fmt;
use std::fmt::Formatter;
use std::ops::DerefMut;

/// Reference to TcpSocket.
/// Notify readers and writers after IO activities.
pub(crate) struct Connection {
    pub(crate) socket: TcpLock,
    inner: RwStatus,
}

struct RwStatus {
    /// Can be used as a key in key-value storage.
    handle: SocketHandle,
    /// Contains an optional Waker which would be set by the reader
    /// after exhausting the rx buffer.
    read_readiness: ReadinessState,
    /// Use to wake the writer when a full tx buffer has newly freed up space
    /// and can be written to again.
    write_readiness: WriteReadiness,
    reader_dropped: bool,
    writer_dropped: bool,
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
        let inner = RwStatus {
            handle: addr,
            read_readiness,
            write_readiness,
            writer_dropped: false,
            reader_dropped: false,
        };
        Connection { socket, inner }
    }
    /// Process incoming packet and queue for polling.
    pub async fn process(
        &mut self,
        timestamp: Instant,
        ip_repr: &IpRepr,
        tcp_repr: &TcpRepr<'_>,
        send_poll: &mut DispatchQueue,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, Error> {
        let handle = self.handle().clone();
        let mut socket = self.socket.lock().await;
        let socket = &mut socket.deref_mut().tcp;
        check_acceptability(socket, ip_repr, tcp_repr)?;
        let reply = socket.process(timestamp, &ip_repr, &tcp_repr);
        if socket.can_recv() {
            self.inner.wake_reader();
        }
        self.inner.wake_to_close(socket);
        let mut poll_at = socket.poll_at();
        if self.inner.both_dropped() && poll_at == PollAt::Ingress && !send_poll.contains(&handle) {
            poll_at = PollAt::Now;
        }
        send_poll.send(handle.clone(), poll_at);
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
    ) -> bool {
        assert_eq!(0, buf.len(), "Given buffer should be empty.");
        let handle = self.handle();
        let mut guard = self.socket.lock().await;
        let tcp = &mut guard.tcp;
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
        if let Err(error) = r {
            if error != smoltcp::Error::Exhausted {
                error!("Error in dispatch {:?}.", error);
            }
        }
        self.inner.wake_to_close(tcp);
        if tcp.can_send() {
            self.inner.wake_writer();
        }
        let poll_at = tcp.poll_at();
        if self.inner.both_dropped() {
            if !tcp.remote_endpoint().is_specified() {
                debug!(
                    "Disassociated from remote endpoint, dropping connection {:?}",
                    handle
                );
                return true;
            }
            if let Err(smoltcp::Error::Exhausted) = r {
                debug!("Finished dispatching, dropping connection {:?}", handle);
                return true;
            }
            if poll_at == PollAt::Ingress {
                debug!(
                    "Dropping connection {:?} to avoid waiting indefinitely.",
                    handle
                );
                return true;
            }
        }
        guard.polling_active = poll_at == PollAt::Now;
        send_poll.send(handle, poll_at);
        false
    }
    /// Mark reader or writer as dropped.
    /// Returns whether the connection should be dropped.
    pub(crate) async fn drop_half(&mut self, rw: Shutdown, dispatch_queue: &mut DispatchQueue) {
        let handle = self.handle();
        let Self { socket, inner } = self;
        match rw {
            Shutdown::Read => inner.reader_dropped = true,
            Shutdown::Write => inner.writer_dropped = true,
        }
        if !inner.writer_dropped {
            return;
        }
        let mut guard = socket.lock().await;
        let Inner { tcp, .. } = guard.deref_mut();
        tcp.close();
        let mut poll_at = tcp.poll_at();
        if inner.both_dropped() && poll_at == PollAt::Ingress {
            poll_at = PollAt::Now;
        }
        dispatch_queue.send(handle, poll_at);
    }
    fn handle(&self) -> SocketHandle {
        self.inner.handle.clone()
    }
}

impl RwStatus {
    /// Notify reader or writer after reaching the end.
    /// TcpSocket state may change after either processing or dispatching.
    fn wake_to_close(&mut self, socket: &mut Tcp) {
        if !socket.may_recv() {
            self.wake_reader();
        }
        if !socket.may_send() {
            self.wake_writer();
        }
    }
    fn wake_reader(&mut self) {
        if !self.reader_dropped {
            self.read_readiness.lock().unwrap().wake_once();
        }
    }
    fn wake_writer(&mut self) {
        if !self.writer_dropped {
            self.write_readiness.lock().unwrap().wake_once();
        }
    }
    fn both_dropped(&self) -> bool {
        self.writer_dropped && self.reader_dropped
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
