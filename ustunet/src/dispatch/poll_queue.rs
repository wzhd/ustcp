use super::super::mpsc;
use super::SocketHandle;
use crate::sockets::AddrPair;
use crate::time::Clock;
use futures::future::poll_fn;

use smoltcp::socket::PollAt;
use std::collections::HashMap;

use std::time::Instant;
use tokio_util::time::delay_queue::{Expired, Key};
use tokio_util::time::DelayQueue;

pub(super) type PollReceiver = mpsc::UnboundedReceiver<PollUpdate>;
/// Update the timing for polling a socket for retransmission.
pub(crate) type PollUpdate = (SocketHandle, PollAt);

/// Used to send updated polling delay to the queue.
#[derive(Clone, Debug)]
pub(crate) struct QueueUpdater {
    sender: mpsc::UnboundedSender<PollUpdate>,
}

pub(crate) struct DispatchQueue {
    clock: Clock,
    delayed: Delays,
}

impl QueueUpdater {
    pub fn send(&mut self, socket: SocketHandle, poll_at: PollAt) {
        self.sender.send((socket, poll_at)).unwrap();
    }
}

impl DispatchQueue {
    pub fn new(clock: Clock) -> (Self, QueueUpdater, PollReceiver) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = Self {
            clock,
            delayed: Delays::new(),
        };
        let updater = QueueUpdater { sender };
        (queue, updater, receiver)
    }
    pub fn send(&mut self, socket: SocketHandle, poll_at: PollAt) {
        let instant = match poll_at {
            PollAt::Now => self.clock.origin(),
            PollAt::Time(millis) => self.clock.resolve(millis),
            PollAt::Ingress => return,
        };
        self.delayed.insert(socket, instant);
    }
    pub(crate) async fn poll(&mut self) -> Expired<SocketHandle> {
        let n = self.delayed.next().await;
        if let Some(e) = n {
            return e;
        }
        futures::future::pending().await
    }
    pub(crate) fn remove(&mut self, socket: &SocketHandle) {
        self.delayed.remove(socket);
    }
    pub(crate) fn contains(&mut self, socket: &SocketHandle) -> bool {
        self.delayed.contains_key(socket)
    }
}

struct Delays {
    queue: DelayQueue<SocketHandle>,
    keys: HashMap<AddrPair, Key>,
}

impl Delays {
    fn new() -> Delays {
        Delays {
            queue: DelayQueue::new(),
            keys: HashMap::new(),
        }
    }
    async fn next(&mut self) -> Option<Expired<SocketHandle>> {
        let n = poll_fn(|c| self.queue.poll_expired(c)).await?;
        let expired = n.unwrap();
        let s = expired.get_ref();
        self.keys.remove(s).unwrap();
        Some(expired)
    }
    fn insert(&mut self, socket: SocketHandle, instant: Instant) {
        if let Some(k) = self.keys.get(&socket) {
            self.queue.reset_at(k, instant.into());
        } else {
            let k = self.queue.insert_at(socket.clone(), instant.into());
            self.keys.insert(socket, k);
        }
    }
    fn contains_key(&self, socket: &SocketHandle) -> bool {
        self.keys.contains_key(socket)
    }
    fn remove(&mut self, socket: &SocketHandle) {
        if let Some(key) = self.keys.remove(socket) {
            self.queue.remove(&key);
        }
    }
}
