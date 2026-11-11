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
pub async fn advertise(host: Arc<HostState>, port: u16, stop: CancellationToken) {
    let socket = match socket() {
        Ok(socket) => socket,
        Err(e) => {
            eprintln!("Spark discovery unavailable (manual join still works): {e}");
            return;
        }
    };
    tokio::spawn(async move {
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
    });
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
    let Ok(socket) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await else {
        return Vec::new();
    };
    let _ = socket.set_broadcast(true);
    let _ = socket
        .send_to(QUERY, (Ipv4Addr::BROADCAST, DISCOVERY_PORT))
        .await;
    let _ = socket
        .send_to(QUERY, (Ipv4Addr::LOCALHOST, DISCOVERY_PORT))
        .await;
    let deadline = tokio::time::Instant::now() + Duration::from_millis(650);
    let mut found = HashMap::new();
    let mut buffer = [0u8; 2048];
    loop {
        let Ok(Ok((len, source))) =
            tokio::time::timeout_at(deadline, socket.recv_from(&mut buffer)).await
        else {
            break;
        };
        if let Ok(ad) = serde_json::from_slice::<Advertisement>(&buffer[..len]) {
            if ad.protocol != "spark/1" || ad.port == 0 || ad.name.len() > 256 || ad.id.len() > 64 {
                continue;
            }
            found.entry(ad.id.clone()).or_insert(Discovery {
                id: ad.id,
                name: ad.name,
                address: SocketAddr::new(source.ip(), ad.port).to_string(),
                peers: ad.peers.min(128),
            });
        }
    }
    let mut found: Vec<_> = found.into_values().collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

pub fn local_address(port: u16) -> String {
    let ip = (|| -> std::io::Result<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        // UDP connect only asks the OS for its preferred route. No packet is sent.
        socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
        Ok(socket.local_addr()?.ip())
    })()
    .unwrap_or(Ipv4Addr::LOCALHOST.into());
    SocketAddr::new(ip, port).to_string()
}
