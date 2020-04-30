use smoltcp::wire::IpAddress as Address;
use std::net::SocketAddr;

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
