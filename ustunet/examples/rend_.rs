#[macro_use]
extern crate log;
use argh::FromArgs;
use futures::StreamExt;
use std::io;
use std::net::SocketAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};
use tracing_subscriber;
use tracing_subscriber::EnvFilter;
use ustunet;
use ustunet::TcpListener;

#[derive(FromArgs)]
/// Reach one server with arbitrary socket addresses.
struct ConnectUp {
    /// address of server to connect to
    #[argh(positional)]
    server: SocketAddr,

    /// tun device owned by current user
    #[argh(option)]
    tun: String,
}

#[tokio::main]
async fn main() {
    let _subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    let up: ConnectUp = argh::from_env();
    let server = up.server;
    let mut listener = TcpListener::bind(&up.tun).unwrap();
    println!("Listening on {}", up.tun);
    while let Some(socket) = listener.next().await {
        tokio::spawn(async move {
            match copy_to_server(server, socket).await {
                Ok((s, r)) => info!("Received {} bytes, sent {}", r, s),
                Err(error) => error!("Error while copying: {:?}", error),
            }
        });
    }
}

async fn copy_to_server(
    remote: SocketAddr,
    socket: ustunet::stream::TcpStream,
) -> io::Result<(u64, u64)> {
    let peer = socket.peer_addr();
    info!(
        "Accepted new tcp stream from {:?} to {:?}",
        socket.peer_addr(),
        socket.local_addr()
    );
    let server = TcpStream::connect(&remote).await?;
    info!("Connected to {:?}", remote);
    let (mut s_reader, mut s_writer) = server.into_split();
    let (mut client_reader, mut client_writer) = socket.split();
    let sent = tokio::spawn(async move {
        let n = copy(&mut s_reader, &mut client_writer, "srv", peer).await;
        info!("Served bytes: {:?}", n);
        n
    });
    let recv = tokio::spawn(async move {
        let n = copy(&mut client_reader, &mut s_writer, "req", peer).await;
        info!("Request bytes: {:?}", n);
        n
    });
    let (sent, recv) = tokio::join!(sent, recv);
    let sent = sent??;
    let recv = recv??;
    Ok((sent as u64, recv as u64))
}

async fn copy<'a, R, W>(
    reader: &'a mut R,
    writer: &'a mut W,
    des: &str,
    addr: SocketAddr,
) -> io::Result<usize>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut n = 0;
    let mut nr = 0;
    let mut buf = [0; 2048];
    let d = Duration::from_secs(60);
    loop {
        let r = match timeout(d, reader.read(&mut buf)).await {
            Ok(f) => {
                let i = f?;
                nr += i;
                if i == 0 {
                    break;
                }
                i
            }
            Err(_t) => {
                error!("timeout {} at reading {}, {}", des, nr, addr);
                break;
            }
        };
        match timeout(d, writer.write_all(&buf[..r])).await {
            Ok(_f) => {
                n += r;
            }
            Err(_t) => {
                error!("timeout {} at writing {}, {}", des, n, addr);
                break;
            }
        };
    }
    Ok(n)
}
