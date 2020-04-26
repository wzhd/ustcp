use super::super::mpsc;
use super::SocketHandle;
use crate::sockets::AddrPair;
use crate::time::Clock;
use futures::future::Fuse;
use futures::{future::FutureExt, pin_mut, select};
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

#[derive(Debug)]
enum Selected {
    Expiration(Option<SocketHandle>),
    Update(PollUpdate),
    /// Finished using the sender
    Sent(mpsc::Sender<SocketHandle>),
}

pub(super) fn poll_queue(clock: Clock) -> (QueueUpdater, mpsc::Receiver<SocketHandle>) {
    let mut dispatch_queue = DispatchQueue::new();
    let mut delays = Delays::new();
    let (send_update, mut recv_update) = mpsc::channel::<PollUpdate>(8);
    let (send_ready, receive_ready) = mpsc::channel(1);
    tokio::spawn(async move {
        let mut sender = Some(send_ready);
        let send_fut = Fuse::terminated();
        pin_mut!(send_fut);
        loop {
            if let Some(mut s) = sender.take() {
                if let Some(ready) = dispatch_queue.pop() {
                    let fut = async {
                        s.send(ready).await.unwrap();
                        s
                    };
                    send_fut.set(fut.fuse());
                } else {
                    sender = Some(s);
                }
            }
            let selected = {
                let delay_fut = Fuse::terminated();
                let update_fut = recv_update.recv().fuse();
                pin_mut!(delay_fut);
                pin_mut!(update_fut);
                if !delays.is_empty() {
                    delay_fut.set(delays.next().fuse());
                }
                select! {
                    x = delay_fut => { Selected::Expiration(x) },
                    p = update_fut => {
                        if let Some(p) = p { Selected::Update(p) } else { break }
                    },
                    s = send_fut => { Selected::Sent(s) },
                }
            };
            trace!("selected value {:?}", selected);
            match selected {
                Selected::Expiration(expired) => {
                    if let Some(s) = expired {
                        dispatch_queue.push(s);
                    }
                }
                Selected::Update((socket, poll)) => match poll {
                    None => {
                        dispatch_queue.push(socket);
                    }
                    Some(instant) => {
                        delays.insert(socket, instant);
                    }
                },
                Selected::Sent(s) => sender = Some(s),
            }
        }
    });
    let updater = QueueUpdater {
        clock,
        sender: send_update,
    };
    (updater, receive_ready)
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
}

struct DispatchQueue {
    /// Connections that likely have something to send immediately
    dispatch_queue: VecDeque<AddrPair>,
    /// Deduplicate
    dispatch_set: HashSet<AddrPair>,
}

impl DispatchQueue {
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
