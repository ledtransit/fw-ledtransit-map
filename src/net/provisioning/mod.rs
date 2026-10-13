// WiFi provisioning: the map's own WiFi network in setup mode, with a DHCP
// server and the setup portal (see docs/WIFI_PROVISIONING.md)
mod dhcp_server;
mod http_server;
mod portal;
mod static_files;

use core::net::Ipv4Addr;

use embassy_executor::Spawner;
use embassy_net::Stack;

use crate::net::wifi_net::SharedWifiController;

const TCP_SOCKET_COUNT: usize = 4;
const DHCP_SOCKET_COUNT: usize = 1;
const DNS_SOCKET_COUNT: usize = 1;
/// Sockets of the access point's network stack
pub const SOCKET_COUNT: usize = DHCP_SOCKET_COUNT + DNS_SOCKET_COUNT + TCP_SOCKET_COUNT;

pub fn spawn(
    spawner: Spawner,
    ap_stack: Stack<'static>,
    gateway_ip: Ipv4Addr,
    controller: &'static SharedWifiController,
    ap_ssid: &'static heapless::String<32>,
) {
    dhcp_server::spawn(spawner, ap_stack, gateway_ip);
    http_server::spawn(spawner, ap_stack, controller, ap_ssid);
}
