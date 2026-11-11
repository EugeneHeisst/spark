use crate::host::HostState;
use serde::{Deserialize, Serialize};
use spark_protocol::{DISCOVERY_PORT, Discovery};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;

const QUERY: &[u8] = b"SPARK_DISCOVER_V1";
#[derive(Serialize, Deserialize)]
struct Advertisement {
    protocol: String,
    id: String,
    name: String,
    port: u16,
    peers: usize,
}
pub async fn advertise(
    host: Arc<HostState>,
    port: u16,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let socket = match socket() {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("Spark discovery unavailable (manual join still works): {e}");
            return None;
        }
    };
    Some(tokio::spawn(async move {
        let mut buffer = [0u8; 2048];
        loop {
            tokio::select! {
                _=stop.cancelled()=>break,
                packet=socket.recv_from(&mut buffer)=>{
                    let Ok((len,peer))=packet else {break;};
                    if &buffer[..len]!=QUERY {continue;}
                    let data=Advertisement { protocol:"spark/1".into(),id:host.id.clone(),name:host.name.clone(),port,peers:host.peer_count().await };
                    if let Ok(data)=serde_json::to_vec(&data) { let _=socket.send_to(&data,peer).await; }
                }
            }
        }
    }))
}
fn socket() -> std::io::Result<UdpSocket> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT).into())?;
    UdpSocket::from_std(socket.into())
}
pub async fn discover() -> Vec<Discovery> {
    let mut destinations: Vec<_> = interfaces()
        .into_iter()
        .filter_map(|interface| {
            interface
                .broadcast
                .map(|broadcast| (interface.ip, broadcast))
        })
        .collect();
    destinations.push((Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST));
    destinations.sort_unstable();
    destinations.dedup();
    let mut sockets = Vec::new();
    for (local, destination) in destinations.into_iter().take(64) {
        let Ok(socket) = UdpSocket::bind((local, 0)).await else {
            continue;
        };
        let _ = socket.set_broadcast(true);
        if socket
            .send_to(QUERY, (destination, DISCOVERY_PORT))
            .await
            .is_ok()
        {
            sockets.push(socket);
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(650);
    let batches =
        futures_util::future::join_all(sockets.into_iter().map(|socket| receive(socket, deadline)))
            .await;
    let mut found = HashMap::new();
    for (ad, source) in batches.into_iter().flatten() {
        if found.len() >= 256 {
            break;
        }
        found.entry(ad.id.clone()).or_insert(Discovery {
            id: ad.id,
            name: ad.name,
            address: SocketAddr::new(source.ip(), ad.port).to_string(),
            peers: ad.peers.min(128),
        });
    }
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

async fn receive(
    socket: UdpSocket,
    deadline: tokio::time::Instant,
) -> Vec<(Advertisement, SocketAddr)> {
    let mut found = Vec::new();
    let mut buffer = [0u8; 2048];
    while found.len() < 256 {
        let Ok(Ok((len, source))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        else {
            break;
        };
        if let Ok(ad) = serde_json::from_slice::<Advertisement>(&buffer[..len]) {
            if ad.protocol != "spark/1"
                || ad.port == 0
                || crate::validation::name(&ad.name, 64).is_err()
                || crate::validation::id(&ad.id).is_err()
            {
                continue;
            }
            found.push((ad, source));
        }
    }
    found
}

pub fn local_address(port: u16) -> String {
    let route_ip = (|| -> std::io::Result<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        // UDP connect only asks the OS for its preferred route. No packet is sent.
        socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
        Ok(socket.local_addr()?.ip())
    })()
    .unwrap_or(Ipv4Addr::LOCALHOST.into());
    let ip = preferred_address(interfaces(), route_ip);
    SocketAddr::new(ip, port).to_string()
}

#[derive(Clone)]
struct LanInterface {
    ip: Ipv4Addr,
    broadcast: Option<Ipv4Addr>,
    virtual_adapter: bool,
}
fn interfaces() -> Vec<LanInterface> {
    if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| {
            if interface.is_loopback()
                || interface.is_p2p()
                || !matches!(
                    interface.oper_status,
                    if_addrs::IfOperStatus::Up | if_addrs::IfOperStatus::Unknown
                )
            {
                return None;
            }
            let if_addrs::IfAddr::V4(address) = interface.addr else {
                return None;
            };
            if address.ip.is_unspecified() || address.ip.is_multicast() || address.prefixlen > 30 {
                return None;
            }
            let name = interface.name.to_ascii_lowercase();
            let virtual_adapter = [
                "docker",
                "br-",
                "veth",
                "virbr",
                "vmnet",
                "vbox",
                "tun",
                "tap",
                "wg",
                "tailscale",
                "zt",
                "ham",
                "wintun",
            ]
            .iter()
            .any(|prefix| name.starts_with(prefix))
                || [
                    "wireguard",
                    "tailscale",
                    "zerotier",
                    "vpn",
                    "virtual",
                    "hyper-v",
                    "vethernet",
                ]
                .iter()
                .any(|part| name.contains(part));
            Some(LanInterface {
                ip: address.ip,
                broadcast: address.broadcast.or_else(|| {
                    Some(Ipv4Addr::from(
                        u32::from(address.ip) | !u32::from(address.netmask),
                    ))
                }),
                virtual_adapter,
            })
        })
        .collect()
}
fn preferred_address(
    mut interfaces: Vec<LanInterface>,
    route_ip: std::net::IpAddr,
) -> std::net::IpAddr {
    interfaces.sort_by_key(|interface| {
        (
            interface.virtual_adapter,
            interface.ip.is_link_local(),
            !interface.ip.is_private(),
            std::net::IpAddr::V4(interface.ip) != route_ip,
        )
    });
    interfaces
        .first()
        .map(|interface| interface.ip.into())
        .unwrap_or(route_ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_vpn_default_route_does_not_hide_the_physical_lan_address() {
        let lan: Ipv4Addr = "192.168.1.23".parse().unwrap();
        let vpn: Ipv4Addr = "10.30.0.2".parse().unwrap();
        let docker: Ipv4Addr = "172.17.0.1".parse().unwrap();
        let interfaces = vec![
            LanInterface {
                ip: vpn,
                broadcast: None,
                virtual_adapter: true,
            },
            LanInterface {
                ip: docker,
                broadcast: None,
                virtual_adapter: true,
            },
            LanInterface {
                ip: lan,
                broadcast: Some("192.168.1.255".parse().unwrap()),
                virtual_adapter: false,
            },
        ];
        assert_eq!(
            preferred_address(interfaces, vpn.into()),
            std::net::IpAddr::V4(lan)
        );
    }
}
