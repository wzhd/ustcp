//! Store TcpSockets and deliver packets by source and destination SocketAddr for processing.
use std::collections::HashMap;
use std::net::SocketAddr;

pub use super::util::convert_to_socket_address;
use smoltcp::socket::TcpSocketBuffer;
use smoltcp::socket::{TcpSocket, TcpState};

use crate::stream::TcpStream;
use log::{error, info};

use super::mpsc::{self};

use crate::dispatch::poll_queue::{DispatchQueue, QueueUpdater};
use crate::stream::internal::Connection;

use crate::dispatch::{Close, CloseSender, SocketHandle};
use crate::time::Clock;
use smoltcp::phy::DeviceCapabilities;
use smoltcp::time::Duration;
use smoltcp::wire::{IpRepr, TcpControl, TcpRepr};
use std::fmt;
use std::fmt::Formatter;
use tokio::sync::mpsc::Sender;

/// An extensible set of sockets.
pub(crate) struct SocketPool {
    pub(crate) queue: DispatchQueue,
    sockets: HashMap<AddrPair, Connection>,
    /// Received tcp connections.
    new_conns: mpsc::Sender<TcpStream>,
    /// Queue a socket to be polled for egress after a period.
    send_poll: QueueUpdater,
    shutdown_builder: CloseSender,
    capabilities: DeviceCapabilities,
    clock: Clock,
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
        shutdown_builder: CloseSender,
        capabilities: DeviceCapabilities,
        tx: Sender<TcpStream>,
        queue: DispatchQueue,
        clock: Clock,
    ) -> SocketPool {
        let sockets = HashMap::new();
        SocketPool {
            sockets,
            new_conns: tx,
            send_poll,
            shutdown_builder,
            capabilities,
            clock,
            queue,
        }
    }

    /// Find or create socket and process the incoming packet.
    pub(crate) async fn process(
        &mut self,
        ip_repr: IpRepr,
        tcp_repr: TcpRepr<'_>,
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
        let ts = self.clock.timestamp();
        self.create_syn(&pair, tcp_repr).await?;
        let socket = self.sockets.get_mut(&pair).unwrap();
        let reply = socket
            .process(ts, &ip_repr, &tcp_repr, &mut self.queue)
            .await?;
        Ok(reply)
    }
    pub(crate) async fn dispatch(&mut self, buf: &mut Vec<u8>, addr: AddrPair) {
        assert_eq!(0, buf.len(), "Given buffer should be empty.");
        let timestamp = self.clock.timestamp();
        let socket = self
            .sockets
            .get_mut(&addr)
            .expect("Dispatching should not happen after dropping.");
        let drop = socket
            .dispatch(buf, timestamp, &self.capabilities, &mut self.queue)
            .await;
        if drop {
            debug!("Removing socket {:?} from collection.", addr);
            self.sockets.remove(&addr).unwrap();
            self.queue.remove(&addr);
        }
    }
    /// Mark reader or writer as dropped.
    pub(crate) async fn drop_tcp_half(&mut self, socket: &SocketHandle, rw: Close) {
        debug!("Dropping {:?} of {:?}", rw, socket);
        let connection = self
            .sockets
            .get_mut(&socket)
            .expect("Socket only gets removed after dropping both reader and writer.");
        connection.drop_half(rw, &mut self.queue).await;
    }

    async fn create_syn(
        &mut self,
        pair: &AddrPair,
        tcp_repr: TcpRepr<'_>,
    ) -> Result<(), smoltcp::Error> {
        if let Some(s) = self.sockets.get_mut(pair) {
            if tcp_repr.control == TcpControl::Syn {
                {
                    let k = s.socket.lock().await;
                    let s = k.tcp.state();
                    if s != TcpState::SynReceived {
                        warn!("Received syn in state {:?} on {:?}", s, pair);
                    }
                }
            }
        } else if tcp_repr.control == TcpControl::Syn {
            debug!("creating socket {:?}", pair);
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
        } else {
            debug!("No known socket for {:?}.", pair);
            return Err(smoltcp::Error::Dropped);
        }
        Ok(())
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
    socket.set_ack_delay(Some(Duration::from_millis(0)));
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
