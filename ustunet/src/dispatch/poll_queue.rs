use super::super::mpsc;
use super::SocketHandle;
use crate::sockets::AddrPair;
use crate::time::Clock;
use smoltcp::socket::PollAt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fmt::Formatter;
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
    end_receiver: mpsc::UnboundedReceiver<(SocketHandle, Shutdown)>,
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

#[derive(Debug)]
pub(super) enum SocketChange {
    Poll,
    Shutdown(Shutdown),
}

impl DispatchQueue {
    pub fn new(clock: Clock) -> (Self, QueueUpdater, ShutdownNotifierBuilder) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let (end_sender, end_receiver) = mpsc::unbounded_channel();
        let queue = Self {
            clock,
            delayed: Delays::new(),
            expired: ExpiredQueue::new(),
            receiver,
            end_receiver,
        };
        let updater = QueueUpdater { clock, sender };
        let shutdown_builder = ShutdownNotifierBuilder { sender: end_sender };
        (queue, updater, shutdown_builder)
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
    /// Remove from channel to potentially save some memory.
    fn receive_poll_times(&mut self) {
        loop {
            match self.receiver.try_recv() {
                Ok((s, p)) => self.insert(s, p),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => unreachable!("Always open in current implementation."),
            }
        }
    }
    pub(super) async fn next(&mut self, get_dispatch: bool) -> (SocketHandle, SocketChange) {
        self.receive_poll_times();
        loop {
            if get_dispatch {
                if let Some(ready) = self.expired.pop() {
                    return (ready, SocketChange::Poll);
                }
            }
            tokio::select! {
                expired = self.delayed.next(), if get_dispatch && !self.delayed.is_empty() => {
                    let s = expired.expect("DelayQueue should not be empty");
                    return (s, SocketChange::Poll)
                }
                received = self.receiver.recv() => {
                    let (s, p) = received.expect("Should not be closed.");
                    self.insert(s, p);
                }
                s = self.end_receiver.recv() => {
                    let (socket, shut) = s.unwrap();
                    return (socket, SocketChange::Shutdown(shut))
                }
            }
        }
    }
    pub(crate) fn remove(&mut self, socket: &SocketHandle) {
        self.expired.remove(socket);
        self.delayed.remove(socket);
    }
    pub(crate) fn contains(&mut self, socket: &SocketHandle) -> bool {
        self.expired.contains(socket) || self.delayed.contains_key(socket)
    }
}

pub(crate) struct ShutdownNotifier {
    socket: SocketHandle,
    rw: Shutdown,
    sender: mpsc::UnboundedSender<(SocketHandle, Shutdown)>,
}

impl ShutdownNotifier {
    pub fn notify(&mut self) {
        self.sender.send((self.socket.clone(), self.rw)).unwrap();
    }
}

pub(crate) struct ShutdownNotifierBuilder {
    sender: mpsc::UnboundedSender<(SocketHandle, Shutdown)>,
}

impl ShutdownNotifierBuilder {
    pub fn reader_build(&self, socket: SocketHandle) -> ShutdownNotifier {
        ShutdownNotifier {
            sender: self.sender.clone(),
            rw: Shutdown::Read,
            socket,
        }
    }
    pub fn writer_build(&self, socket: SocketHandle) -> ShutdownNotifier {
        ShutdownNotifier {
            sender: self.sender.clone(),
            rw: Shutdown::Write,
            socket,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Shutdown {
    Read,
    Write,
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
    fn contains_key(&self, socket: &SocketHandle) -> bool {
        self.keys.contains_key(socket)
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
    fn remove(&mut self, socket: &SocketHandle) {
        if self.dispatch_set.remove(socket) {
            let (index, _) = self
                .dispatch_queue
                .iter()
                .enumerate()
                .find(|(_index, s)| *s == socket)
                .unwrap();
            self.dispatch_queue.remove(index);
        }
    }
    fn contains(&self, socket: &SocketHandle) -> bool {
        self.dispatch_set.contains(socket)
    }
}

impl fmt::Debug for ShutdownNotifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "ShutdownNotifier({:?})", self.socket)
    }
}
