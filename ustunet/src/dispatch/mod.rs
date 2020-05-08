use super::mpsc::{self};
use crate::sockets::{AddrPair, SocketPool};
use crate::stream::TcpStream;
use crate::time::Clock;
use futures::TryFutureExt;
use smoltcp::iface::Packet;
use smoltcp::phy::{ChecksumCapabilities, DeviceCapabilities};
use smoltcp::time::Instant;
use smoltcp::wire::IpProtocol;
use smoltcp::wire::{IpRepr, Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr};
use smoltcp::Error;
use std::fmt;
use std::fmt::Formatter;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_fd::AsyncFd;

pub(crate) mod poll_queue;

use crate::dispatch::poll_queue::SocketChange;
use poll_queue::DispatchQueue;

type SMResult<T> = Result<T, smoltcp::Error>;
type ProcessingReply = Option<(IpRepr, TcpRepr<'static>)>;

/// Used to locate a socket in the storage.
/// The address pair can be used but there are other options.
pub(crate) type SocketHandle = AddrPair;

pub struct Interface {
    sockets: SocketPool,
    capabilities: DeviceCapabilities,
    clock: Clock,
    /// Receive next socket to be polled for dispatch
    queue: DispatchQueue,
}

enum SelectValue {
    Read(usize),
    Written(usize),
    NextPoll((SocketHandle, SocketChange)),
}

impl Interface {
    pub fn new(capabilities: DeviceCapabilities) -> (Interface, mpsc::Receiver<TcpStream>) {
        let clock = Clock::new();
        let (queue, queue_sender, shutdown_builder) = DispatchQueue::new(clock);
        let (pool, incoming) = SocketPool::new(queue_sender, shutdown_builder);
        let interface = Interface {
            sockets: pool,
            capabilities,
            clock: Clock::new(),
            queue,
        };
        (interface, incoming)
    }

    /// Keeps reading, processing, and writing
    pub async fn poll(
        &mut self,
        mut reader: ReadHalf<AsyncFd>,
        mut writer: WriteHalf<AsyncFd>,
    ) -> io::Result<()> {
        let Self {
            sockets,
            capabilities,
            clock,
            queue,
        } = self;
        let mut ingress_replies = vec![];
        let mut next_poll = None;
        let mut read_buf = vec![0u8; 2048];
        let mut write_buf = Vec::with_capacity(2048);
        let mut timestamp = clock.timestamp();
        loop {
            if write_buf.is_empty() {
                while let Some(r) = ingress_replies.pop() {
                    packet_to_bytes(r, &mut write_buf, &capabilities.checksum).unwrap();
                    if !write_buf.is_empty() {
                        break;
                    }
                }
            }
            if write_buf.is_empty() {
                if let Some(addr) = next_poll.take() {
                    sockets
                        .dispatch(&mut write_buf, timestamp, addr, &capabilities, queue)
                        .await;
                }
            }
            let rf = reader.read(&mut read_buf).map_err(|e| {
                error!("Read error: {:?}", e);
                e
            });
            let v = tokio::select! {
                n = rf => SelectValue::Read(n?),
                n = writer.write(&write_buf), if !write_buf.is_empty() => SelectValue::Written(n?),
                p = queue.next(next_poll.is_none()) => SelectValue::NextPoll(p),
            };
            trace!("selected value {:?}", v);
            timestamp = clock.timestamp();
            match v {
                SelectValue::Read(n) => {
                    trace!("ingress  packet size: {}", n);
                    match Self::process(
                        &capabilities.checksum,
                        sockets,
                        &read_buf[..n],
                        timestamp,
                        queue,
                    )
                    .await
                    {
                        Ok(reply) => {
                            if let Some(repr) = reply {
                                debug!("ingress reply: {:?}", repr);
                                let packet = Packet::Tcp(repr);
                                ingress_replies.push(packet);
                            }
                        }
                        Err(e) => {
                            if e != smoltcp::Error::Dropped {
                                warn!("process error {:?}", e);
                            }
                        }
                    }
                }
                SelectValue::Written(_n) => {
                    write_buf.clear();
                }
                SelectValue::NextPoll((s, change)) => match change {
                    SocketChange::Poll => {
                        assert!(next_poll.is_none());
                        next_poll = Some(s);
                    }
                    SocketChange::Shutdown(rw) => {
                        sockets.drop_tcp_half(&s, rw, queue).await;
                    }
                },
            }
            tokio::task::yield_now().await;
        }
    }
    /// Process incoming packet.
    pub(crate) async fn process<T: AsRef<[u8]> + ?Sized>(
        checksum_caps: &ChecksumCapabilities,
        sockets: &mut SocketPool,
        packet: &T,
        timestamp: Instant,
        queue: &mut DispatchQueue,
    ) -> Result<ProcessingReply, smoltcp::Error> {
        Self::process_ipv4(checksum_caps, sockets, packet, timestamp, queue).await
    }

    /// Process incoming ipv4 packet.
    async fn process_ipv4<T: AsRef<[u8]> + ?Sized>(
        checksum_caps: &ChecksumCapabilities,
        sockets: &mut SocketPool,
        payload: &T,
        timestamp: Instant,
        queue: &mut DispatchQueue,
    ) -> SMResult<ProcessingReply> {
        let ipv4_packet = Ipv4Packet::new_checked(payload)?;
        let ipv4_repr = Ipv4Repr::parse(&ipv4_packet, &checksum_caps)?;

        if !ipv4_repr.src_addr.is_unicast() {
            // Discard packets with non-unicast source addresses.
            debug!("non-unicast source address");
            return Err(Error::Malformed);
        }

        let ip_repr = IpRepr::Ipv4(ipv4_repr);
        let ip_payload = ipv4_packet.payload();

        trace!("processing ip4 to {:?}", ipv4_repr.dst_addr);

        match ipv4_repr.protocol {
            IpProtocol::Tcp => {
                Self::process_tcp(
                    checksum_caps,
                    sockets,
                    timestamp,
                    ip_repr,
                    ip_payload,
                    queue,
                )
                .await
            }
            _ => {
                debug!("ipv4 not tcp");
                Ok(None)
            }
        }
    }

    /// Process incoming tcp packet.
    async fn process_tcp(
        checksum_caps: &ChecksumCapabilities,
        sockets: &mut SocketPool,
        timestamp: Instant,
        ip_repr: IpRepr,
        ip_payload: &[u8],
        queue: &mut DispatchQueue,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, smoltcp::Error> {
        trace!("processing tcp to {:?}", ip_repr);
        let tcp_packet = TcpPacket::new_checked(ip_payload)?;
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let tcp_repr = TcpRepr::parse(&tcp_packet, &src_addr, &dst_addr, checksum_caps)?;
        let reply = sockets.process(ip_repr, tcp_repr, timestamp, queue).await?;
        Ok(reply)
    }
}

pub(crate) fn packet_to_bytes(
    packet: Packet<'_>,
    mut buffer: &mut Vec<u8>,
    checksum_caps: &ChecksumCapabilities,
) -> Result<(), smoltcp::Error> {
    assert_eq!(0, buffer.len(), "Given buffer should be empty.");
    trace!("serialising packet {:?}", packet);
    match packet {
        Packet::Tcp((ip_repr, tcp_repr)) => {
            let ip_repr = ip_repr.lower(&[])?;
            let l = ip_repr.total_len();
            buffer.resize(l, 0);
            ip_repr.emit(&mut buffer, checksum_caps);
            let payload = &mut buffer[ip_repr.buffer_len()..];
            tcp_repr.emit(
                &mut TcpPacket::new_unchecked(payload),
                &ip_repr.src_addr(),
                &ip_repr.dst_addr(),
                &checksum_caps,
            );
        }
        p => {
            trace!("other packet {:?}", p);
        }
    }
    Ok(())
}

impl fmt::Debug for SelectValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use SelectValue::*;
        match self {
            Read(n) => write!(f, "Read({:?} bytes)", n),
            Written(n) => write!(f, "Written({:?} bytes)", n),
            NextPoll(s) => write!(f, "Polling({:?})", s),
        }
    }
}
