//! Store TcpSockets and deliver packets by source and destination SocketAddr for processing.
use std::collections::HashMap;
use std::net::SocketAddr;

pub use super::util::convert_to_socket_address;
use smoltcp::socket::TcpSocket;
use smoltcp::socket::TcpSocketBuffer;

use crate::stream::TcpStream;
use log::{error, info};

use super::mpsc::{self};

use crate::dispatch::poll_queue::{DispatchQueue, QueueUpdater, Shutdown, ShutdownNotifierBuilder};
use crate::stream::internal::Connection;

use crate::dispatch::SocketHandle;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::time::Instant;
use smoltcp::wire::{IpRepr, TcpControl, TcpRepr};
use std::fmt;
use std::fmt::Formatter;

/// An extensible set of sockets.
#[allow(unused)]
pub(crate) struct SocketPool {
    sockets: HashMap<AddrPair, Connection>,
    /// Received tcp connections.
    new_conns: mpsc::Sender<TcpStream>,
    /// Queue a socket to be polled for egress after a period.
    send_poll: QueueUpdater,
    shutdown_builder: ShutdownNotifierBuilder,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
/// Client and server. Peer and local.
pub struct AddrPair {
    pub peer: SocketAddr,
    pub local: SocketAddr,
}

impl SocketPool {
    /// Create a socket set using the provided storage.
    pub fn new(
        send_poll: QueueUpdater,
        shutdown_builder: ShutdownNotifierBuilder,
    ) -> (SocketPool, mpsc::Receiver<TcpStream>) {
        let sockets = HashMap::new();
        let (tx, rx) = mpsc::channel(1);
        let s = SocketPool {
            sockets,
            new_conns: tx,
            send_poll,
            shutdown_builder,
        };
        (s, rx)
    }

    /// Find or create socket and process the incoming packet.
    pub(crate) async fn process(
        &mut self,
        ip_repr: IpRepr,
        tcp_repr: TcpRepr<'_>,
        timestamp: Instant,
        send_poll: &mut DispatchQueue,
    ) -> Result<Option<(IpRepr, TcpRepr<'static>)>, smoltcp::Error> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let src = convert_to_socket_address(src_addr, tcp_repr.src_port)?;
        let dst = convert_to_socket_address(dst_addr, tcp_repr.dst_port)?;
        let pair = AddrPair {
            peer: src,
            local: dst,
        };
        if tcp_repr.control == TcpControl::Syn && tcp_repr.ack_number.is_some() {
            warn!(
                "Syn with ack number {:?} is not expected.",
                tcp_repr.ack_number
            );
            return Err(smoltcp::Error::Dropped);
        }
        let socket = if let Some(s) = self.sockets.get_mut(&pair) {
            s
        } else if tcp_repr.control == TcpControl::Syn {
            debug!("creating socket {:?}", pair);
            self.new_connection(pair).await?
        } else {
            debug!("No known socket for {:?}.", pair);
            return Err(smoltcp::Error::Dropped);
        };
        let reply = socket
            .process(timestamp, &ip_repr, &tcp_repr, send_poll)
            .await?;
        Ok(reply)
    }
    pub(crate) async fn dispatch(
        &mut self,
        buf: &mut Vec<u8>,
        timestamp: Instant,
        addr: AddrPair,
        capabilities: &DeviceCapabilities,
        send_poll: &mut DispatchQueue,
    ) {
        assert_eq!(0, buf.len(), "Given buffer should be empty.");
        let socket = self
            .sockets
            .get_mut(&addr)
            .expect("Dispatching should not happen after dropping.");
        let drop = socket
            .dispatch(buf, timestamp, capabilities, send_poll)
            .await;
        if drop {
            debug!("Removing socket {:?} from collection.", addr);
            self.sockets.remove(&addr).unwrap();
            send_poll.remove(&addr);
        }
    }
    /// Create a new connection in response to SYN.
    async fn new_connection(&mut self, pair: AddrPair) -> Result<&mut Connection, smoltcp::Error> {
        let socket = open_socket(pair.local)?;
        let (tcp, connection) = TcpStream::new(
            socket,
            self.send_poll.clone(),
            pair.clone(),
            &self.shutdown_builder,
        );
        self.new_conns.send(tcp).await.unwrap_or_else(|error| {
            error!("tcp source {:?}", error);
        });
        let prev = self.sockets.insert(pair.clone(), connection);
        assert!(prev.is_none());
        Ok(self.sockets.get_mut(&pair).unwrap())
    }
    /// Mark reader or writer as dropped.
    pub(crate) async fn drop_tcp_half(
        &mut self,
        socket: &SocketHandle,
        rw: Shutdown,
        dispatch_queue: &mut DispatchQueue,
    ) {
        debug!("Dropping {:?} of {:?}", rw, socket);
        let connection = self
            .sockets
            .get_mut(&socket)
            .expect("Socket only gets removed after dropping both reader and writer.");
        connection.drop_half(rw, dispatch_queue).await;
    }
}

fn open_socket(local: SocketAddr) -> Result<TcpSocket<'static>, smoltcp::Error> {
    let mut socket = create_tcp_socket();
    if !socket.is_open() {
        info!("opening tcp listener for {:?}", local);
        socket.listen(local).map_err(|e| {
            error!("tcp can't listen {:?}", e);
            e
        })?;
    }
    Ok(socket)
}

const RX_BUF_SIZE: usize = 32768;
const TX_BUF_SIZE: usize = RX_BUF_SIZE;
fn create_tcp_socket<'a>() -> TcpSocket<'a> {
    let tcp1_rx_buffer = TcpSocketBuffer::new(vec![0; RX_BUF_SIZE]);
    let tcp1_tx_buffer = TcpSocketBuffer::new(vec![0; TX_BUF_SIZE]);
    TcpSocket::new(tcp1_rx_buffer, tcp1_tx_buffer)
}

impl fmt::Debug for SocketPool {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "SocketPool")?;
        Ok(())
    }
}
