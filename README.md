# ustunet

A pure Rust TCP library that can be simply used in place of TCP implementations provided by operating systems,
enabling services to be built with uncommon flexibility.

## Usage

At a high level,
it provides APIs similar to any asynchronous TCP socket library.

To start listening, use `TcpListener`.
But instead of binding to a single `SocketAddr`,
give it the name of a TUN network interface:

    use ustunet::TcpListener;
    
    let mut listener = TcpListener::bind("tuna").unwrap();

And it will process all IP packets going to the interface,
accepting incoming TCP connections destined to any `SocketAddr`.
It's like binding to all possible IP and ports.
(But without the complexity or the cost.
Only one file descriptor is used.)

Accept `TcpStream`s:

    while let Some(tcp_stream) = listener.next().await {
    }

Use `tokio::io::AsyncRead` and `AsyncWrite` implementations:

    let mut buf = [0u8; 1024];
    let n = tcp_stream.read(&mut buf).await.unwrap();

    tcp_stream.write("Aloha!".as_bytes()).await.unwrap();

Since the listener is not bound to an address,
the `SocketAddr` actually used by a connection can be of more interest.

    let server_addr = socket.local_addr();
    println!("Provided service on port {}, IP: {}.", server_addr.port(), server_addr.ip());

Similarly, call `peer_addr` to get the address of the host that initiated the connection.

### Adding the dependency

Add to `Cargo.toml`

    [dependencies.ustunet]
    git = "https://github.com/ustcp/ustcp.git"

## Setting up a TUN network interface

TUN allows a user to send and receive IP packets on the interface with no permission requirements at runtime.

Create a TUN interface with name set to `tuna`, and ownership given to user `yuki`.
And bring it up.

    sudo ip tuntap add dev tuna mode tun user yuki
    sudo ip link set up dev tuna

Give it an IP address (and a subnet mask) that's not currently in use:

    sudo ip addr add 192.168.9.100/24 dev tuna

With the subnet mask `/24`,
ustunet will be able to respond on any address in the range from `192.168.9.1` to `192.168.9.254`.

The command can be repeated to add more addresses,
but adding routes can be more flexible.
Configure any address in the subnet as a gateway for a different range of IPs
to instruct the operating systems to contact those addresses also via `tuna`.

    sudo ip route add 10.33.0.0/16 via 192.168.9.4 dev tuna

