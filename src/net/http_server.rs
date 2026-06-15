use core::net::{IpAddr, Ipv4Addr, SocketAddr};

use edge_nal::{TcpAccept, TcpBind, WithTimeout};
use edge_nal_embassy::{Tcp, TcpBuffers};
use embassy_net::Stack;

use crate::net::{
    prov_server::HttpHandler,
    wifi_net::{SharedWifiController, TCP_SERV_SOCKET_COUNT},
};

const TCP_RX_SIZE: usize = 512;
const TCP_TX_SIZE: usize = 512;
const TCP_BUF_POOL_SIZE: usize = 1;
const SOCKET_ACCEPT_TIMEOUT_MS: u32 = 5000;
const SOCKET_READ_TIMEOUT_MS: u32 = 8000;
const HTTP_MAX_HEADER_COUNT: usize = 32;
const HTTP_BUF_SIZE: usize = 1024;

pub fn spawn(
    spawner: embassy_executor::Spawner,
    ap_stack: Stack<'static>,
    shared_controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    // Spawn HTTP server tasks
    for i in 0..TCP_SERV_SOCKET_COUNT {
        spawner.spawn(http_server_task(ap_stack, shared_controller, ap_ssid, i as u64).unwrap());
    }
}

#[embassy_executor::task(pool_size = TCP_SERV_SOCKET_COUNT)]
async fn http_server_task(
    ap_stack: Stack<'static>,
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
    task_id: u64,
) {
    let tcp_bufs = TcpBuffers::<TCP_BUF_POOL_SIZE, TCP_TX_SIZE, TCP_RX_SIZE>::new();
    let tcp = Tcp::new(ap_stack, &tcp_bufs);

    // Bind TCP listener to socket
    let sock_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 80);
    let acceptor = tcp
        .bind(sock_addr)
        .await
        .expect("Failed to bind HTTP server socket");
    let timed_acceptor = WithTimeout::new(SOCKET_ACCEPT_TIMEOUT_MS, acceptor);

    let http_handler = HttpHandler::new(controller, ap_ssid.clone());
    let mut http_buf = [0u8; HTTP_BUF_SIZE];

    // Handle incoming connections forever
    loop {
        let sock = match timed_acceptor.accept().await {
            Ok((_, sock)) => sock,
            Err(_) => continue,
        };
        edge_http::io::server::handle_connection::<_, _, HTTP_MAX_HEADER_COUNT>(
            sock,
            &mut http_buf,
            Some(SOCKET_READ_TIMEOUT_MS),
            task_id,
            &http_handler,
        )
        .await;
    }
}
