use smoltcp::wire::IpAddress as Address;
use std::fmt;
use std::fmt::Formatter;
use std::net::SocketAddr;
use std::ops::{Deref, DerefMut};
use std::option::Option::Some;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::oneshot;

pub fn convert_to_socket_address(
    address: Address,
    port: u16,
) -> Result<SocketAddr, smoltcp::Error> {
    let socket_address = match address {
        Address::Ipv4(v4) => SocketAddr::from((v4.0, port)),
        Address::Ipv6(v6) => {
            let mut b = [0u16; 8];
            v6.write_parts(&mut b);
            SocketAddr::from((b, port))
        }
        _ => return Err(smoltcp::Error::Unrecognized),
    };
    Ok(socket_address)
}

enum MutexMessage<T> {
    Get(oneshot::Sender<T>),
    Release(T),
}

#[derive(Clone)]
/// The only reason this is used instead of asynchronous mutexes is to get a
/// Future not tied to the lifetime of the mutex, so that it can be easily
/// stored and polled.
pub(crate) struct UsLock<T> {
    sender: UnboundedSender<MutexMessage<T>>,
}

pub(crate) struct UsLockGuard<T> {
    value: Option<T>,
    sender: UnboundedSender<MutexMessage<T>>,
}

impl<T: Send + 'static> UsLock<T> {
    #[deprecated]
    pub fn new(value: T) -> UsLock<T> {
        let (sender, mut receiver) = unbounded_channel();
        tokio::spawn(async move {
            use std::collections::VecDeque;
            use MutexMessage::*;
            let mut value = Some(value);
            let mut queued = VecDeque::new();
            while let Some(m) = receiver.recv().await {
                match m {
                    Get(sender) => {
                        if let Some(v) = value.take() {
                            if let Err(e) = sender.send(v) {
                                debug!("send failed");
                                value = Some(e);
                            }
                        } else {
                            queued.push_back(sender);
                        }
                    }
                    Release(val) => {
                        if let Some(v) = queued.pop_front() {
                            if let Err(e) = v.send(val) {
                                warn!("send error");
                                value = Some(e);
                            }
                        } else {
                            assert!(value.is_none());
                            value = Some(val);
                        }
                    }
                }
            }
        });
        UsLock { sender }
    }
    pub async fn lock(&self) -> UsLockGuard<T> {
        let (sender, receiver) = oneshot::channel();
        let m = MutexMessage::Get(sender);
        self.sender.send(m).expect("send");
        let value = receiver.await.expect("Lock disappeared.");
        UsLockGuard {
            value: Some(value),
            sender: self.sender.clone(),
        }
    }
}

impl<T> fmt::Debug for MutexMessage<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "MutexMessage")
    }
}

impl<T> Deref for UsLockGuard<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().unwrap()
    }
}

impl<T> DerefMut for UsLockGuard<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.value.as_mut().unwrap()
    }
}

impl<T> Drop for UsLockGuard<T> {
    fn drop(&mut self) {
        self.sender
            .send(MutexMessage::Release(self.value.take().unwrap()))
            .unwrap();
    }
}
