use super::dispatch::Interface;
use super::stream::TcpStream;
use smoltcp::phy::DeviceCapabilities;
use snafu::{ResultExt, Snafu};
use std::convert::TryFrom;
use std::os::unix::io::IntoRawFd;

use super::mpsc::Receiver;
use futures::task::Poll;
use std::task::Context;
use tokio::io::split;
use tokio_fd::AsyncFd;
use tun;
use tun::Device as TunDevice;

#[derive(Debug, Snafu)]
pub enum TunError {
    #[snafu(display("Unable to open tun device {}: {}", name, source))]
    Tun { source: tun::Error, name: String },
}

type Result<T, E = TunError> = std::result::Result<T, E>;

pub struct TcpListener {
    receiver: Receiver<TcpStream>,
}

impl TcpListener {
    pub fn bind<S: AsRef<str>>(name: S) -> Result<TcpListener> {
        let mut conf = tun::configure();
        let name = name.as_ref();
        conf.name(name);
        let d = tun::create(&conf).context(Tun { name })?;

        let mut capabilities = DeviceCapabilities::default();
        capabilities.max_transmission_unit = d.mtu().context(Tun { name })? as usize;
        let fd = d.into_raw_fd();
        let fd = AsyncFd::try_from(fd).unwrap();
        let (rd, wr) = split(fd);
        let (mut interface, connections) = Interface::new(capabilities);
        tokio::spawn(async move {
            interface.poll(rd, wr).await.expect("processing interface");
        });
        let listener = TcpListener {
            receiver: connections,
        };
        Ok(listener)
    }
    pub async fn accept(&mut self) -> Option<TcpStream> {
        self.receiver.recv().await
    }
}

impl tokio::stream::Stream for TcpListener {
    type Item = TcpStream;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        self.receiver.poll_recv(cx)
    }
}
