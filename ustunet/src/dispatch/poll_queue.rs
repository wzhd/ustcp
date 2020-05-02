use super::super::mpsc;
use super::SocketHandle;
use crate::sockets::AddrPair;
use crate::time::Clock;
use smoltcp::socket::PollAt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;
use tokio::stream::StreamExt;
use tokio::sync::mpsc::error::SendError;
use tokio::time::{delay_queue::Key, DelayQueue};

type PollDelay = Option<Instant>;

/// Update the timing for polling a socket for retransmission.
pub(crate) type PollUpdate = (SocketHandle, PollDelay);

/// Used to send updated polling delay to the queue.
#[derive(Clone, Debug)]
pub(crate) struct QueueUpdater {
    clock: Clock,
    sender: mpsc::Sender<PollUpdate>,
}
pub(crate) struct DispatchQueue {
    clock: Clock,
    delayed: Delays,
    expired: ExpiredQueue,
}
impl QueueUpdater {
    pub async fn send(
        &mut self,
        socket: SocketHandle,
        poll_at: PollAt,
    ) -> Result<(), SendError<PollUpdate>> {
        let delay = match poll_at {
            PollAt::Now => None,
            PollAt::Time(millis) => Some(self.clock.resolve(millis)),
            PollAt::Ingress => return Ok(()),
        };
        self.sender.send((socket, delay)).await?;
        Ok(())
    }
}

impl DispatchQueue {
    pub fn new(clock: Clock) -> Self {
        Self {
            clock,
            delayed: Delays::new(),
            expired: ExpiredQueue::new(),
        }
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
            PollAt::Ingress => return,
        }
    }
    pub async fn next(&mut self) -> SocketHandle {
        if let Some(ready) = self.expired.pop() {
            return ready;
        }
        if let Some(expired) = self.delayed.next().await {
            return expired;
        }
        let () = futures::future::pending().await;
        unreachable!()
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
