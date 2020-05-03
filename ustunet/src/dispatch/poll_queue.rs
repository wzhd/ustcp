use super::super::mpsc;
use super::SocketHandle;
use crate::sockets::AddrPair;
use crate::time::Clock;
use smoltcp::socket::PollAt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::time::{delay_queue::Key, DelayQueue};

type PollDelay = Option<Instant>;

/// Update the timing for polling a socket for retransmission.
pub(crate) type PollUpdate = (SocketHandle, PollDelay);

/// Used to send updated polling delay to the queue.
#[derive(Clone, Debug)]
pub(crate) struct QueueUpdater {
    clock: Clock,
    sender: mpsc::UnboundedSender<PollUpdate>,
}

pub(crate) struct DispatchQueue {
    clock: Clock,
    delayed: Delays,
    expired: ExpiredQueue,
    receiver: mpsc::UnboundedReceiver<PollUpdate>,
}

impl QueueUpdater {
    pub fn send(&mut self, socket: SocketHandle, poll_at: PollAt) {
        let delay = match poll_at {
            PollAt::Now => None,
            PollAt::Time(millis) => Some(self.clock.resolve(millis)),
            PollAt::Ingress => return,
        };
        self.sender.send((socket, delay)).unwrap();
    }
}

impl DispatchQueue {
    pub fn new(clock: Clock) -> (Self, QueueUpdater) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let queue = Self {
            clock,
            delayed: Delays::new(),
            expired: ExpiredQueue::new(),
            receiver,
        };
        let updater = QueueUpdater { clock, sender };
        (queue, updater)
    }
    pub fn send(&mut self, socket: SocketHandle, poll_at: PollAt) {
        match poll_at {
            PollAt::Now => {
                self.delayed.remove(&socket);
                self.expired.push(socket);
            }
            PollAt::Time(millis) => {
                let instant = self.clock.resolve(millis);
                self.delayed.insert(socket, instant);
            }
            PollAt::Ingress => (),
        }
    }
    fn insert(&mut self, socket: SocketHandle, time: Option<Instant>) {
        if let Some(time) = time {
            self.delayed.insert(socket, time);
        } else {
            self.delayed.remove(&socket);
            self.expired.push(socket);
        }
    }
    pub async fn next(&mut self) -> SocketHandle {
        loop {
            match self.receiver.try_recv() {
                Ok((s, p)) => self.insert(s, p),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => unreachable!("Always open in current implementation."),
            }
        }
        loop {
            if let Some(ready) = self.expired.pop() {
                return ready;
            }
            tokio::select! {
                expired = self.delayed.next(), if !self.delayed.is_empty() => {
                    return expired.expect("DelayQueue should not be empty")
                }
                received = self.receiver.recv() => {
                    let (s, p) = received.expect("Should not be closed.");
                    self.insert(s, p);
                }
            }
        }
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
    fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
    async fn next(&mut self) -> Option<SocketHandle> {
        let n = self.queue.next().await;
        if n.is_none() {
            debug!("empty queue");
        }
        let result = n?;
        let expired = result.expect("");
        let s = expired.into_inner();
        self.keys.remove(&s);
        Some(s)
    }
    fn insert(&mut self, socket: SocketHandle, instant: Instant) {
        if let Some(k) = self.keys.get(&socket) {
            self.queue.reset_at(k, instant.into());
        } else {
            let k = self.queue.insert_at(socket.clone(), instant.into());
            self.keys.insert(socket, k);
        }
    }
    fn remove(&mut self, socket: &SocketHandle) {
        if let Some(key) = self.keys.remove(socket) {
            self.queue.remove(&key);
        }
    }
}

/// No delay needed
struct ExpiredQueue {
    /// Connections that likely have something to send immediately
    dispatch_queue: VecDeque<AddrPair>,
    /// Deduplicate
    dispatch_set: HashSet<AddrPair>,
}

impl ExpiredQueue {
    fn new() -> Self {
        Self {
            dispatch_queue: VecDeque::new(),
            dispatch_set: HashSet::new(),
        }
    }
    fn pop(&mut self) -> Option<AddrPair> {
        let p = self.dispatch_queue.pop_front();
        if let Some(ref p) = p {
            self.dispatch_set.remove(&p);
        }
        p
    }
    fn push(&mut self, addr: AddrPair) {
        if self.dispatch_set.insert(addr.clone()) {
            self.dispatch_queue.push_back(addr);
        }
    }
}
