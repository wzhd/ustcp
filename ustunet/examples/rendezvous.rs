use argh::FromArgs;
use futures::StreamExt;
use std::io;
use std::net::SocketAddr;
use std::str::FromStr;
use tokio::net::TcpStream;
use tracing_subscriber;
use tracing_subscriber::EnvFilter;
use ustunet;
use ustunet::TcpListener;

#[derive(FromArgs)]
/// Reach one server with arbitrary socket addresses.
struct ConnectUp {
    /// address of server to connect to
    #[argh(positional, default = "default_server_address()")]
    server: SocketAddr,

    /// tun device owned by current user
    #[argh(option, default = "\"tuna\".into()")]
    tun: String,
}
fn default_server_address() -> SocketAddr {
    SocketAddr::from_str("127.0.0.1:5201").unwrap()
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
                Ok((s, r)) => println!("Received {} bytes, sent {}", r, s),
                Err(error) => println!("Error while copying: {:?}", error),
            }
        });
    }
}

async fn copy_to_server(
    remote: SocketAddr,
    mut socket: ustunet::stream::TcpStream,
) -> io::Result<(u64, u64)> {
    println!(
        "Accepted new tcp stream from {:?} to {:?}",
        socket.peer_addr(),
        socket.local_addr()
    );
    let mut server = TcpStream::connect(&remote).await?;
    println!("Connected to {:?}", remote);
    let (mut reader, mut writer) = server.split();
    let sent = tokio::io::copy(&mut reader, &mut socket.writer);
    let recv = tokio::io::copy(&mut socket.reader, &mut writer);
    let (sent, recv) = tokio::join!(sent, recv);
    let sent = sent?;
    let recv = recv?;
    Ok((sent, recv))
}
