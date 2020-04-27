//! Store TcpSockets and deliver packets by source and destination SocketAddr for processing.
use std::collections::HashMap;
use std::net::SocketAddr;

pub use super::util::convert_to_socket_address;
use smoltcp::socket::TcpSocket;
use smoltcp::socket::TcpSocketBuffer;

use crate::stream::TcpStream;
use log::{error, info};

use super::mpsc::{self};

use crate::dispatch::poll_queue::QueueUpdater;
use crate::stream::internal::Connection;

use crate::SocketLock;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::time::Instant;
use smoltcp::wire::{IpRepr, TcpControl, TcpRepr};

/// An extensible set of sockets.
#[derive(Debug)]
#[allow(unused)]
pub(crate) struct SocketPool {
    sockets: HashMap<AddrPair, Connection>,
    /// Received tcp connections.
    new_conns: mpsc::Sender<TcpStream>,
    /// Queue a socket to be polled for egress after a period.
    send_poll: QueueUpdater,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
/// Client and server. Peer and local.
pub struct AddrPair {
    pub peer: SocketAddr,
    pub local: SocketAddr,
}

impl SocketPool {
    /// Create a socket set using the provided storage.
    pub fn new(send_poll: QueueUpdater) -> (SocketPool, mpsc::Receiver<TcpStream>) {
        let sockets = HashMap::new();
        let (tx, rx) = mpsc::channel(1);
        let s = SocketPool {
            sockets,
            new_conns: tx,
            send_poll,
        };
        (s, rx)
    }

    /// Find or create socket and process the incoming packet.
    pub(crate) async fn process(
        &mut self,
        ip_repr: IpRepr,
        tcp_repr: TcpRepr<'_>,
        timestamp: Instant,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, smoltcp::Error> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let src = convert_to_socket_address(src_addr, tcp_repr.src_port)?;
        let dst = convert_to_socket_address(dst_addr, tcp_repr.dst_port)?;
        let pair = AddrPair {
            peer: src,
            local: dst,
        };
        let socket = if tcp_repr.control != TcpControl::Syn {
            self.sockets.get_mut(&pair).ok_or_else(|| {
                warn!("No known socket for {:?}.", pair);
                smoltcp::Error::Dropped
            })?
        } else if let Some(num) = tcp_repr.ack_number {
            warn!("Syn with ack number ({:?}) is not expected.", num);
            return Err(smoltcp::Error::Dropped);
        } else {
            debug!("creating socket {:?}", pair);
            self.new_connection(pair).await?
        };
        let reply = socket.process(timestamp, &ip_repr, &tcp_repr).await?;
        Ok(reply)
    }
    pub(crate) async fn dispatch(
        &mut self,
        buf: &mut Vec<u8>,
        timestamp: Instant,
        addr: AddrPair,
        capabilities: &DeviceCapabilities,
    ) -> Result<(), smoltcp::Error> {
        assert_eq!(0, buf.len(), "Given buffer should be empty.");
        let socket = self.sockets.get_mut(&addr).ok_or_else(|| {
            warn!("Address {:?} does not belong to a known socket.", addr);
            smoltcp::Error::Dropped
        })?;
        socket.dispatch(buf, timestamp, capabilities).await
    }
    /// Create a new connection in response to SYN.
    async fn new_connection(&mut self, pair: AddrPair) -> Result<&mut Connection, smoltcp::Error> {
        if self.sockets.contains_key(&pair) {
            warn!(
                "Connection {:?} already exists and closing a socket has not been implemented.",
                pair
            );
            return Err(smoltcp::Error::Dropped);
        }
        let socket = new_conn(pair.local)?;
        let (tcp, connection) = TcpStream::new(socket, self.send_poll.clone(), pair.clone());
        self.new_conns.send(tcp).await.unwrap_or_else(|error| {
            error!("tcp source {:?}", error);
        });
        self.sockets.insert(pair.clone(), connection);
        Ok(self.sockets.get_mut(&pair).unwrap())
    }
}

type LockTcp = SocketLock<TcpSocket<'static>>;

fn new_conn(local: SocketAddr) -> Result<(LockTcp, LockTcp, LockTcp), smoltcp::Error> {
    let mut socket = create_tcp_socket();
    if !socket.is_open() {
        info!("opening tcp listener for {:?}", local);
        socket.listen(local).map_err(|e| {
            error!("tcp can't listen {:?}", e);
            e
        })?;
    }
    let socket = SocketLock::new(socket);
    Ok(socket)
}

fn create_tcp_socket<'a>() -> TcpSocket<'a> {
    let tcp1_rx_buffer = TcpSocketBuffer::new(vec![0; 2048]);
    let tcp1_tx_buffer = TcpSocketBuffer::new(vec![0; 2048]);
    TcpSocket::new(tcp1_rx_buffer, tcp1_tx_buffer)
}
