//! Real TCP/HTTP/WebSocket tests exercise the same boundary used by the desktop app.
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spark_protocol::{Bootstrap, ChatMessage, ClientCommand, ConnectionInfo, ServerEvent};
use spark_service::{Service, ServiceOptions};
use std::{path::Path, time::Duration};
use tempfile::TempDir;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
fn client() -> Client {
    Client::builder().no_proxy().build().unwrap()
}
async fn start(path: &Path) -> Service {
    Service::start(ServiceOptions {
        data_dir: path.to_owned(),
        ui_dir: path.join("assets"),
        gateway_port: 0,
    })
    .await
    .unwrap()
}
fn post(client: &Client, service: &Service, path: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{}{path}", service.url))
        .header("x-spark-token", &service.token)
}
async fn host(client: &Client, service: &Service) -> ConnectionInfo {
    let response = post(client, service, "/api/host")
        .json(&json!({"name":"Тестовая комната","code":"secret-code","port":0}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "{}",
        response.text().await.unwrap_or_default()
    );
    response.json().await.unwrap()
}
async fn join(client: &Client, service: &Service, info: &ConnectionInfo) {
    let response = post(client, service, "/api/join")
        .json(&json!({"address":info.address,"code":"secret-code"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "join failed");
}
async fn socket(service: &Service) -> Socket {
    let url = format!(
        "{}/api/events?token={}",
        service.url.replacen("http:", "ws:", 1),
        service.token
    );
    tokio_tungstenite::connect_async(url).await.unwrap().0
}
async fn event(socket: &mut Socket, predicate: impl Fn(&ServerEvent) -> bool) -> ServerEvent {
    tokio::time::timeout(Duration::from_secs(8), async {
        while let Some(item) = socket.next().await {
            match item.unwrap() {
                Message::Text(text) => {
                    let value: ServerEvent = serde_json::from_str(&text).unwrap();
                    if predicate(&value) {
                        return value;
                    }
                }
                Message::Ping(data) => socket.send(Message::Pong(data)).await.unwrap(),
                Message::Close(frame) => panic!("WebSocket closed unexpectedly: {frame:?}"),
                _ => {}
            }
        }
        panic!("WebSocket ended unexpectedly")
    })
    .await
    .expect("server event timed out")
}
async fn send(socket: &mut Socket, command: ClientCommand) {
    socket
        .send(Message::Text(
            serde_json::to_string(&command).unwrap().into(),
        ))
        .await
        .unwrap();
}
async fn welcome(socket: &mut Socket) -> ServerEvent {
    event(socket, |e| matches!(e, ServerEvent::Welcome { .. })).await
}

#[tokio::test]
async fn loopback_nonce_origin_code_and_input_validation() {
    let root = TempDir::new().unwrap();
    let service = start(root.path()).await;
    let client = client();
    let bootstrap: Bootstrap = client
        .get(format!("{}/api/bootstrap", service.url))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(bootstrap.token, service.token);
    let response = client
        .get(format!("{}/api/bootstrap", service.url))
        .header("origin", "http://evil.invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = client
        .get(format!("{}/api/bootstrap", service.url))
        .header("host", "evil.invalid")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = client
        .get(format!("{}/api/discovery", service.url))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = post(&client, &service, "/api/host")
        .json(&json!({"name":"room","code":"short"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    for address in [
        "http://127.0.0.1:1",
        "user@host:80",
        "host:80/path",
        "../file:80",
    ] {
        let response = post(&client, &service, "/api/join")
            .json(&json!({"address":address,"code":"secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{address}");
    }
    let response = client
        .put(format!("{}/api/profile", service.url))
        .header("x-spark-token", &service.token)
        .header("content-type", "application/json")
        .body("{".to_owned())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        response
            .json::<Value>()
            .await
            .unwrap()
            .get("error")
            .is_some()
    );
    let response = client
        .put(format!("{}/api/profile", service.url))
        .header("x-spark-token", &service.token)
        .json(&json!({"name":"x".repeat(70000)}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(
        response
            .json::<Value>()
            .await
            .unwrap()
            .get("error")
            .is_some()
    );
    let info = host(&client, &service).await;
    let response = client
        .get(format!("http://{}/lan/check", info.address))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let response = post(&client, &service, "/api/save")
        .json(&json!({"file_id":"../bad","path":"../escape"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    service.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_chat_signaling_stale_reconnect_files_save_and_restart() {
    let a_root = TempDir::new().unwrap();
    let b_root = TempDir::new().unwrap();
    let destination = TempDir::new().unwrap();
    let a = start(a_root.path()).await;
    let b = start(b_root.path()).await;
    let client = client();
    for (service, name) in [(&a, "Алиса"), (&b, "Боб")] {
        assert!(
            client
                .put(format!("{}/api/profile", service.url))
                .header("x-spark-token", &service.token)
                .json(&json!({"name":name}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    let info = host(&client, &a).await;
    let wrong = post(&client, &b, "/api/join")
        .json(&json!({"address":info.address,"code":"wrong-code"}))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    join(&client, &b, &info).await;
    let mut aw = socket(&a).await;
    let aid = match welcome(&mut aw).await {
        ServerEvent::Welcome { self_id, .. } => self_id,
        _ => unreachable!(),
    };
    let mut bw = socket(&b).await;
    let bid = match welcome(&mut bw).await {
        ServerEvent::Welcome { self_id, .. } => self_id,
        _ => unreachable!(),
    };
    send(
        &mut aw,
        ClientCommand::Chat {
            channel_id: "general".into(),
            text: "Привет из A".into(),
        },
    )
    .await;
    let msg = event(
        &mut bw,
        |e| matches!(e,ServerEvent::Chat{message} if message.text=="Привет из A"),
    )
    .await;
    assert!(
        matches!(msg,ServerEvent::Chat{message} if message.sender_id==aid && message.sender_name=="Алиса")
    );
    send(
        &mut bw,
        ClientCommand::Chat {
            channel_id: "general".into(),
            text: "Ответ из B".into(),
        },
    )
    .await;
    event(
        &mut aw,
        |e| matches!(e,ServerEvent::Chat{message} if message.text=="Ответ из B"),
    )
    .await;
    send(
        &mut aw,
        ClientCommand::JoinVoice {
            channel_id: "lounge".into(),
        },
    )
    .await;
    event(&mut aw,|e|matches!(e,ServerEvent::Presence{peer} if peer.id==aid && peer.voice_channel.as_deref()==Some("lounge"))).await;
    send(
        &mut aw,
        ClientCommand::Signal {
            to: bid.clone(),
            payload: json!({"type":"offer","sdp":"blocked"}),
        },
    )
    .await;
    event(
        &mut aw,
        |e| matches!(e,ServerEvent::Error{message,..} if message.contains("голосового")),
    )
    .await;
    send(
        &mut aw,
        ClientCommand::Invite {
            to: bid.clone(),
            channel_id: "lounge".into(),
            video: true,
        },
    )
    .await;
    event(
        &mut bw,
        |e| matches!(e,ServerEvent::Invite{from,video:true,..} if from.id==aid),
    )
    .await;
    send(
        &mut bw,
        ClientCommand::JoinVoice {
            channel_id: "lounge".into(),
        },
    )
    .await;
    event(&mut aw,|e|matches!(e,ServerEvent::Presence{peer} if peer.id==bid && peer.voice_channel.as_deref()==Some("lounge"))).await;
    send(
        &mut aw,
        ClientCommand::Signal {
            to: bid.clone(),
            payload: json!({"type":"offer","sdp":"allowed"}),
        },
    )
    .await;
    event(&mut bw,|e|matches!(e,ServerEvent::Signal{from,payload} if from==&aid && payload["sdp"]=="allowed")).await;
    send(
        &mut aw,
        ClientCommand::CreateChannel {
            name: "Дополнительный".into(),
            channel_kind: "text".into(),
        },
    )
    .await;
    let channel = match event(&mut bw, |e| matches!(e, ServerEvent::ChannelCreated { .. })).await {
        ServerEvent::ChannelCreated { channel } => channel,
        _ => unreachable!(),
    };

    // Replacing one client's socket must not let the stale socket remove its new identity.
    let mut bw2 = socket(&b).await;
    welcome(&mut bw2).await;
    drop(bw);
    send(
        &mut bw2,
        ClientCommand::Chat {
            channel_id: channel.id.clone(),
            text: "После переподключения".into(),
        },
    )
    .await;
    event(
        &mut aw,
        |e| matches!(e,ServerEvent::Chat{message} if message.text=="После переподключения"),
    )
    .await;
    let mut aw2 = socket(&a).await;
    let peers = match welcome(&mut aw2).await {
        ServerEvent::Welcome { peers, .. } => peers,
        _ => unreachable!(),
    };
    assert!(peers.iter().any(|p| p.id == bid));
    drop(aw);

    let bytes: Vec<u8> = (0..2_000_123).map(|i| (i % 251) as u8).collect();
    let expected = hex::encode(Sha256::digest(&bytes));
    let bad = post(&client, &b, "/api/upload?channel_id=general&name=bad.bin")
        .header("x-spark-sha256", "0".repeat(64))
        .body(bytes[..99].to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let traversal = post(
        &client,
        &b,
        "/api/upload?channel_id=general&name=..%2Fescape",
    )
    .body("bad")
    .send()
    .await
    .unwrap();
    assert_eq!(traversal.status(), StatusCode::BAD_REQUEST);
    let response = post(
        &client,
        &b,
        "/api/upload?channel_id=general&name=sample.bin",
    )
    .header("x-spark-sha256", &expected)
    .header("content-type", "application/octet-stream")
    .body(bytes.clone())
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "upload failed");
    let message: ChatMessage = response.json().await.unwrap();
    let file = message.file.unwrap();
    assert_eq!(file.sha256, expected);
    assert_eq!(file.size, bytes.len() as u64);
    event(&mut aw2,|e|matches!(e,ServerEvent::Chat{message} if message.file.as_ref().is_some_and(|f|f.id==file.id))).await;
    let response = client
        .get(format!("{}/api/files/{}?token={}", a.url, file.id, a.token))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    assert_eq!(response.bytes().await.unwrap().as_ref(), bytes.as_slice());
    let output = destination.path().join("saved.bin");
    let saved: Value = post(&client, &b, "/api/save")
        .json(&json!({"file_id":file.id,"path":output}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(saved["sha256"], expected);
    assert_eq!(tokio::fs::read(&output).await.unwrap(), bytes);
    let exists = post(&client, &b, "/api/save")
        .json(&json!({"file_id":file.id,"path":output}))
        .send()
        .await
        .unwrap();
    assert_eq!(exists.status(), StatusCode::CONFLICT);
    tokio::fs::write(&output, b"old bytes").await.unwrap();
    let overwrite = post(&client, &b, "/api/save")
        .json(&json!({"file_id":file.id,"path":output,"overwrite":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(overwrite.status(), StatusCode::OK);
    assert_eq!(tokio::fs::read(&output).await.unwrap(), bytes);
    let stored = a_root.path().join("host").join("files").join(&file.id);
    let mut corrupted = bytes.clone();
    corrupted[0] ^= 0xff;
    tokio::fs::write(&stored, &corrupted).await.unwrap();
    tokio::fs::write(&output, b"keep this existing file")
        .await
        .unwrap();
    let corrupt = post(&client, &b, "/api/save")
        .json(&json!({"file_id":file.id,"path":output,"overwrite":true}))
        .send()
        .await
        .unwrap();
    assert!(!corrupt.status().is_success());
    assert_eq!(
        tokio::fs::read(&output).await.unwrap(),
        b"keep this existing file"
    );
    tokio::fs::write(&stored, &bytes).await.unwrap();
    assert!(
        std::fs::read_dir(a_root.path().join("host/files"))
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".part"))
    );
    drop(aw2);
    drop(bw2);
    b.shutdown().await;
    a.shutdown().await;

    let restarted = start(a_root.path()).await;
    host(&client, &restarted).await;
    let mut ws = socket(&restarted).await;
    match welcome(&mut ws).await {
        ServerEvent::Welcome {
            channels,
            messages,
            peers,
            ..
        } => {
            assert!(channels.iter().any(|c| c.id == channel.id));
            assert!(messages.iter().any(|m| m.text == "Ответ из B"));
            assert!(
                messages
                    .iter()
                    .any(|m| m.file.as_ref().is_some_and(|f| f.sha256 == expected))
            );
            assert_eq!(peers.len(), 1);
        }
        _ => unreachable!(),
    }
    drop(ws);
    restarted.shutdown().await;
}
