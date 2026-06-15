use core::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use defmt::error;
use edge_dhcp::server::{Server, ServerOptions};
use edge_nal::UdpBind;
use edge_nal_embassy::{Udp, UdpBuffers};
use embassy_executor::Spawner;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};

const UDP_PACKET_BUF_SIZE: usize = 1024;
const UDP_RX_BUF_SIZE: usize = 512;
const UDP_TX_BUF_SIZE: usize = 512;
const UDP_BUF_POOL_SIZE: usize = 1;
const UDP_MAX_METADATA_COUNT: usize = 2;
const MAX_LEASES_COUNT: usize = 4;

pub fn spawn(spawner: Spawner, stack: Stack<'static>, ip: Ipv4Addr) {
    spawner.spawn(dhcp_server_task(stack, ip).unwrap());
}

#[embassy_executor::task]
async fn dhcp_server_task(stack: Stack<'static>, ip: Ipv4Addr) {
    let mut pack_buf: [u8; _] = [0u8; UDP_PACKET_BUF_SIZE];
    let mut gw_buf = [Ipv4Addr::UNSPECIFIED];
    let buffers = UdpBuffers::<
        UDP_BUF_POOL_SIZE,
        UDP_TX_BUF_SIZE,
        UDP_RX_BUF_SIZE,
        UDP_MAX_METADATA_COUNT,
    >::new();

    // Create and bind UDP socket
    let unbound_socket = Udp::new(stack, &buffers);
    let mut bound_socket = unbound_socket
        .bind(SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::UNSPECIFIED,
            edge_dhcp::io::DEFAULT_SERVER_PORT,
        )))
        .await
        .expect("Failed to bind DHCP server socket");

    // Run DHCP server forever
    loop {
        _ = edge_dhcp::io::server::run(
            &mut Server::<_, MAX_LEASES_COUNT>::new_with_et(ip),
            &ServerOptions::new(ip, Some(&mut gw_buf)),
            &mut bound_socket,
            &mut pack_buf,
        )
        .await
        .inspect_err(|e| error!("DHCP server error: {:?}", e));
        Timer::after(Duration::from_millis(500)).await;
    }
}
