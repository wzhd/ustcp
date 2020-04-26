use super::mpsc::{self};
use crate::sockets::{AddrPair, SocketPool};
use crate::stream::TcpStream;
use crate::time::Clock;
use futures::future::{Fuse, FusedFuture};
use futures::{future::FutureExt, pin_mut, select};
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

type SMResult<T> = Result<T, smoltcp::Error>;
type ProcessingReply = Option<(IpRepr, TcpRepr<'static>)>;

/// Used to locate a socket in the storage.
/// The address pair can be used but there are other options.
pub(crate) type SocketHandle = AddrPair;
type ReadResult = (io::Result<usize>, ReadHalf<AsyncFd>, Vec<u8>);

pub struct Interface {
    sockets: SocketPool,
    capabilities: DeviceCapabilities,
    clock: Clock,
    /// Receive next socket to be polled for dispatch
    poll_recv: Option<mpsc::Receiver<SocketHandle>>,
}

enum SelectValue {
    Read(ReadResult),
    Written((io::Result<usize>, Writer)),
    NextPoll((SocketHandle, mpsc::Receiver<SocketHandle>)),
}

impl Interface {
    pub fn new(capabilities: DeviceCapabilities) -> (Interface, mpsc::Receiver<TcpStream>) {
        let clock = Clock::new();
        let (send_poll, receive_poll) = poll_queue::poll_queue(clock);
        let (pool, incoming) = SocketPool::new(send_poll);
        let interface = Interface {
            sockets: pool,
            capabilities,
            clock: Clock::new(),
            poll_recv: Some(receive_poll),
        };
        (interface, incoming)
    }

    async fn read(mut reader: ReadHalf<AsyncFd>, mut buf: Vec<u8>) -> ReadResult {
        let n = reader.read(&mut buf).fuse().await;
        (n, reader, buf)
    }
    /// Keeps reading, processing, and writing
    pub async fn poll(
        &mut self,
        read_half: ReadHalf<AsyncFd>,
        write_half: WriteHalf<AsyncFd>,
    ) -> io::Result<()> {
        let mut ingress_replies = vec![];
        let mut next_poll = None;
        let read = Self::read(read_half, vec![0u8; 2048]).fuse();
        let write_fut = Fuse::terminated();
        let next_poll_future = Fuse::terminated();
        let mut writer = Some(Writer::new(write_half));
        pin_mut!(read, write_fut, next_poll_future);
        let mut timestamp = self.clock.timestamp();
        loop {
            if next_poll.is_none() {
                if let Some(mut receiver) = self.poll_recv.take() {
                    assert!(next_poll_future.is_terminated(), "Receiving in progress.");
                    let future = async move {
                        let o = receiver.recv().await.expect("No more sockets to poll.");
                        (o, receiver)
                    };
                    next_poll_future.set(future.fuse());
                }
            }
            if write_fut.is_terminated() {
                let mut w = writer.take().unwrap();
                if w.buf.is_empty() {
                    while let Some(r) = ingress_replies.pop() {
                        packet_to_bytes(r, &mut w.buf, &self.capabilities.checksum).unwrap();
                        if !w.buf.is_empty() {
                            break;
                        }
                    }
                }
                if w.buf.is_empty() {
                    if let Some(addr) = next_poll.take() {
                        if let Err(e) = self
                            .sockets
                            .dispatch(&mut w.buf, timestamp, addr, &self.capabilities)
                            .await
                        {
                            error!("Tcp dispatch error {:?}", e);
                        }
                    }
                }
                if w.buf.is_empty() {
                    debug!("nothing to dispatch");
                    writer = Some(w);
                } else {
                    write_fut.set(w.write().fuse());
                }
            }
            let v = select! {
                n = read => SelectValue::Read((n)),
                n = write_fut => SelectValue::Written(n),
                p = next_poll_future => SelectValue::NextPoll(p),
            };
            debug!("selected value {:?}", v);
            timestamp = self.clock.timestamp();
            match v {
                SelectValue::Read((n, read_half, buf)) => {
                    let n = n?;
                    trace!("ingress  packet size: {}", n);
                    match self.process(&buf[..n], timestamp).await {
                        Ok(reply) => {
                            if let Some(repr) = reply {
                                debug!("ingress reply: {:?}", repr);
                                let packet = Packet::Tcp(repr);
                                ingress_replies.push(packet);
                            }
                        }
                        Err(e) => {
                            println!("process error {:?}", e);
                        }
                    }
                    read.set(Self::read(read_half, buf).fuse());
                }
                SelectValue::Written((n, wr)) => {
                    let _n = n?;
                    writer = Some(wr);
                }
                SelectValue::NextPoll((s, r)) => {
                    assert!(self.poll_recv.is_none());
                    self.poll_recv = Some(r);
                    assert!(next_poll.is_none());
                    next_poll = Some(s);
                }
            }
        }
    }
    /// Process incoming packet.
    pub async fn process<T: AsRef<[u8]> + ?Sized>(
        &mut self,
        packet: &T,
        timestamp: Instant,
    ) -> Result<ProcessingReply, smoltcp::Error> {
        self.process_ipv4(packet, timestamp).await
    }

    /// Process incoming ipv4 packet.
    async fn process_ipv4<T: AsRef<[u8]> + ?Sized>(
        &mut self,
        payload: &T,
        timestamp: Instant,
    ) -> SMResult<ProcessingReply> {
        let ipv4_packet = Ipv4Packet::new_checked(payload)?;
        let checksum_caps = self.capabilities.checksum.clone();
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
            IpProtocol::Tcp => self.process_tcp(timestamp, ip_repr, ip_payload).await,
            _ => {
                debug!("ipv4 not tcp");
                Ok(None)
            }
        }
    }

    /// Process incoming tcp packet.
    async fn process_tcp(
        &mut self,
        timestamp: Instant,
        ip_repr: IpRepr,
        ip_payload: &[u8],
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, smoltcp::Error> {
        trace!("processing tcp to {:?}", ip_repr);
        let tcp_packet = TcpPacket::new_checked(ip_payload)?;
        let checksum_caps = &self.capabilities.checksum;
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let tcp_repr = TcpRepr::parse(&tcp_packet, &src_addr, &dst_addr, checksum_caps)?;
        let reply = self.sockets.process(ip_repr, tcp_repr, timestamp).await?;
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

struct Writer {
    fd: WriteHalf<AsyncFd>,
    pub buf: Vec<u8>,
}

impl Writer {
    fn new(fd: WriteHalf<AsyncFd>) -> Writer {
        Writer {
            fd,
            buf: Vec::with_capacity(2048),
        }
    }
    async fn write(mut self) -> (io::Result<usize>, Self) {
        let n = self.fd.write(&self.buf).await;
        self.buf.clear();
        (n, self)
    }
}

impl fmt::Debug for SelectValue {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use SelectValue::*;
        match self {
            Read((n, _, _)) => write!(f, "Read({:?} bytes)", n),
            Written((n, _)) => write!(f, "Written({:?} bytes)", n),
            NextPoll((s, _)) => write!(f, "Polling({:?})", s),
        }
    }
}
