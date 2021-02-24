use super::mpsc::{self};
use crate::sockets::{AddrPair, SocketPool};
use crate::stream::TcpStream;
use crate::time::Clock;
use smoltcp::iface::IpPacket as Packet;
use smoltcp::phy::{ChecksumCapabilities, DeviceCapabilities};
use smoltcp::wire::{IpProtocol, Ipv6Packet, Ipv6Repr};
use smoltcp::wire::{IpRepr, Ipv4Packet, Ipv4Repr, TcpPacket, TcpRepr};
use smoltcp::Error;
use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio_fd::AsyncFd;

pub(crate) mod poll_queue;
mod shutdown;

use crate::dispatch::poll_queue::PollReceiver;
use crate::dispatch::shutdown::shutdown_channel;
use crate::util::{Selected, Selector};
use futures::future::Either;
use futures::pin_mut;
use poll_queue::DispatchQueue;
pub(crate) use shutdown::{Close, CloseSender, HalfCloseSender};
use smoltcp::socket::PollAt;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;

type SMResult<T> = Result<T, smoltcp::Error>;
type ProcessingReply = Option<(IpRepr, TcpRepr<'static>)>;

/// Used to locate a socket in the storage.
/// The address pair can be used but there are other options.
pub(crate) type SocketHandle = AddrPair;

pub struct Interface {
    sockets: SocketPool,
    checksum_caps: ChecksumCapabilities,
}

type ReadBuf = Box<[u8]>;
async fn read_future(
    mut reader: ReadHalf<AsyncFd>,
    mut buf: ReadBuf,
) -> (io::Result<usize>, ReadHalf<AsyncFd>, ReadBuf) {
    let n = reader.read(&mut buf).await;
    (n, reader, buf)
}

async fn write_future(
    mut writer: WriteHalf<AsyncFd>,
    buf: Vec<u8>,
) -> (io::Result<usize>, WriteHalf<AsyncFd>, Vec<u8>) {
    let n = writer.write(&buf).await;
    (n, writer, buf)
}

type CloseReceiver = mpsc::UnboundedReceiver<(SocketHandle, Close)>;

async fn recv_close(mut chan: CloseReceiver) -> (SocketHandle, Close, CloseReceiver) {
    let (s, c) = chan.recv().await.expect("Channel closed.");
    (s, c, chan)
}
async fn recv_poll(mut chan: PollReceiver) -> (SocketHandle, PollAt, PollReceiver) {
    let (s, c) = chan.recv().await.unwrap();
    (s, c, chan)
}

pub(crate) fn start_io(
    dev_caps: DeviceCapabilities,
    reader: ReadHalf<AsyncFd>,
    writer: WriteHalf<AsyncFd>,
) -> (JoinHandle<io::Result<()>>, mpsc::Receiver<TcpStream>) {
    let (tx, incoming) = mpsc::channel(1);
    let t = tokio::spawn(async move { io_loop(dev_caps, reader, writer, tx).await });
    (t, incoming)
}

async fn io_loop(
    dev_caps: DeviceCapabilities,
    reader: ReadHalf<AsyncFd>,
    writer: WriteHalf<AsyncFd>,
    tx: Sender<TcpStream>,
) -> io::Result<()> {
    let clock = Clock::new();
    let (shutdown_builder, closing) = shutdown_channel();
    let (queue, queue_sender, poll_recv) = DispatchQueue::new(clock);
    let sockets = SocketPool::new(
        queue_sender,
        shutdown_builder,
        dev_caps.clone(),
        tx,
        queue,
        clock,
    );
    let mut ingress_replies = vec![];
    let mut writing = Some((writer, Vec::with_capacity(2048)));
    let mut selector = Selector::new();
    selector.insert_b(read_future(reader, vec![0u8; 2048].into_boxed_slice()));
    selector.insert_c(recv_close(closing));
    selector.insert_d(recv_poll(poll_recv));
    let mut next_poll = None;
    let capabilities = dev_caps.clone();
    let mut interface = Interface {
        sockets,
        checksum_caps: dev_caps.checksum.clone(),
    };
    loop {
        assert!(selector.b().is_some());
        if let Some((writer, mut write_buf)) = writing.take() {
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
                    interface.dispatch(&mut write_buf, addr).await;
                }
            }
            if !write_buf.is_empty() {
                let f = write_future(writer, write_buf);
                selector.insert_a(f);
            } else {
                info!("nothing to write");
                writing = Some((writer, write_buf));
            }
        };
        let se = selector.select();
        let s = {
            let nopoll = next_poll.is_none();
            if nopoll {
                let wp = interface.sockets.queue.poll();
                pin_mut!(se);
                pin_mut!(wp);
                match futures::future::select(se, wp).await {
                    Either::Left((s, _w)) => s,
                    Either::Right((expired, _se)) => {
                        let addr = expired.into_inner();
                        next_poll = Some(addr);
                        continue;
                    }
                }
            } else {
                se.await
            }
        };
        match s {
            Selected::A((n, w, mut b)) => {
                let n = n?;
                info!("Written {} bytes", n);
                b.clear();
                writing = Some((w, b));
            }
            Selected::B((n, r, read_buf)) => {
                let n = n?;
                info!("ingress  packet size: {}", n);
                if n > 0 {
                    match interface.process(&read_buf[..n]).await {
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
                selector.insert_b(read_future(r, read_buf));
            }
            Selected::C((s, c, r)) => {
                info!("Closing {:?}'s {:?}", s, c);
                interface.sockets.drop_tcp_half(&s, c).await;
                selector.insert_c(recv_close(r));
            }
            Selected::D((s, t, p)) => {
                interface.sockets.queue.send(s, t);
                selector.insert_d(recv_poll(p));
            }
        }
        tokio::task::yield_now().await;
    }
}
impl Interface {
    pub(crate) async fn dispatch(&mut self, write_buf: &mut Vec<u8>, addr: AddrPair) {
        self.sockets.dispatch(write_buf, addr).await;
    }
    /// Process incoming packet.
    pub(crate) async fn process<T: AsRef<[u8]> + ?Sized>(
        &mut self,
        packet: &T,
    ) -> Result<ProcessingReply, smoltcp::Error> {
        let packet = packet.as_ref();
        let v = packet[0] / 16;
        if v == 4 {
            self.process_ipv4(packet).await
        } else if v == 6 {
            self.process_ipv6(packet).await
        } else {
            Err(smoltcp::Error::Dropped)
        }
    }

    /// Process incoming ipv4 packet.
    async fn process_ipv4(&mut self, payload: &[u8]) -> SMResult<ProcessingReply> {
        let ipv4_packet = Ipv4Packet::new_checked(payload)?;
        let ipv4_repr = Ipv4Repr::parse(&ipv4_packet, &self.checksum_caps)?;

        if !ipv4_repr.src_addr.is_unicast() {
            // Discard packets with non-unicast source addresses.
            debug!("non-unicast source address");
            return Err(Error::Malformed);
        }

        let ip_repr = IpRepr::Ipv4(ipv4_repr);
        let ip_payload = ipv4_packet.payload();

        trace!("processing ip4 to {:?}", ipv4_repr.dst_addr);

        match ipv4_repr.protocol {
            IpProtocol::Tcp => self.process_tcp(ip_repr, ip_payload).await,
            _ => {
                debug!("ipv4 not tcp");
                Ok(None)
            }
        }
    }

    async fn process_ipv6(&mut self, payload: &[u8]) -> SMResult<ProcessingReply> {
        let ipv6_packet = Ipv6Packet::new_checked(payload)?;
        let ipv6_repr = Ipv6Repr::parse(&ipv6_packet)?;

        if !ipv6_repr.src_addr.is_unicast() {
            // Discard packets with non-unicast source addresses.
            debug!("non-unicast source address");
            return Err(Error::Malformed);
        }

        let ip_repr = IpRepr::Ipv6(ipv6_repr);
        let ip_payload = ipv6_packet.payload();

        trace!("processing ip6 to {:?}", ipv6_repr.dst_addr);

        match ipv6_repr.next_header {
            IpProtocol::Tcp => self.process_tcp(ip_repr, ip_payload).await,
            _ => {
                debug!("ipv6 not tcp");
                Ok(None)
            }
        }
    }
    /// Process incoming tcp packet.
    async fn process_tcp(
        &mut self,
        ip_repr: IpRepr,
        ip_payload: &[u8],
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, smoltcp::Error> {
        trace!("processing tcp to {:?}", ip_repr);
        let tcp_packet = TcpPacket::new_checked(ip_payload)?;
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let tcp_repr = TcpRepr::parse(&tcp_packet, &src_addr, &dst_addr, &self.checksum_caps)?;
        let reply = self.sockets.process(ip_repr, tcp_repr).await?;
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
