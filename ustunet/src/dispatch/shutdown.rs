use crate::dispatch::SocketHandle;
use core::fmt;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Close {
    Read,
    Write,
}

pub(crate) struct HalfCloseSender {
    socket: SocketHandle,
    rw: Close,
    sender: CloseSender,
}

#[derive(Clone)]
pub(crate) struct CloseSender {
    tx: mpsc::UnboundedSender<(SocketHandle, Close)>,
}

pub(crate) fn shutdown_channel() -> (CloseSender, mpsc::UnboundedReceiver<(SocketHandle, Close)>) {
    let (end_sender, end_receiver) = mpsc::unbounded_channel();
    let shutdown_builder = CloseSender { tx: end_sender };
    (shutdown_builder, end_receiver)
}

impl HalfCloseSender {
    pub fn notify(&mut self) {
        self.sender.notify(self.socket.clone(), self.rw);
    }
}

impl CloseSender {
    pub fn notify(&mut self, socket: SocketHandle, rw: Close) {
        self.tx.send((socket, rw)).unwrap();
    }
    pub fn build(&self, socket: SocketHandle, rw: Close) -> HalfCloseSender {
        HalfCloseSender {
            sender: self.clone(),
            rw,
            socket,
        }
    }
}

impl fmt::Debug for HalfCloseSender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ShutdownNotifier({:?})", self.socket)
    }
}
