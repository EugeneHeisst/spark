use crate::{
    error::{ApiError, ApiResult},
    store::Store,
    validation,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        Path, Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use spark_protocol::{
    ChatMessage, ClientCommand, FileInfo, MAX_CHAT_CHARS, MAX_FILE_BYTES, MAX_JSON_BYTES, Peer,
    ServerEvent,
};
use std::{
    collections::HashMap,
    path::{Path as FsPath, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{Mutex, mpsc},
};
use tokio_util::{io::ReaderStream, sync::CancellationToken};

pub struct HostHandle {
    pub port: u16,
    pub shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    discovery: Option<tokio::task::JoinHandle<()>>,
}
impl HostHandle {
    pub async fn stop(mut self) {
        self.shutdown.cancel();
        if let Some(mut discovery) = self.discovery.take()
            && tokio::time::timeout(Duration::from_secs(1), &mut discovery)
                .await
                .is_err()
        {
            discovery.abort();
        }
        if tokio::time::timeout(Duration::from_secs(3), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}
pub struct HostState {
    pub id: String,
    pub name: String,
    key: String,
    pub store: Store,
    peers: Mutex<HashMap<String, ConnectedPeer>>,
    pub events: Mutex<()>,
    shutdown: CancellationToken,
}
struct ConnectedPeer {
    peer: Peer,
    session: String,
    tx: mpsc::Sender<ServerEvent>,
    cancel: CancellationToken,
}
#[derive(Deserialize)]
struct PeerQuery {
    id: String,
    name: String,
}
#[derive(Deserialize)]
pub struct UploadQuery {
    pub channel_id: String,
    pub name: String,
}

pub async fn start(
    root: &FsPath,
    name: String,
    key: String,
    port: u16,
    shutdown: CancellationToken,
) -> ApiResult<HostHandle> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port))
        .await
        .map_err(|e| ApiError::bad(format!("Не удалось открыть порт комнаты: {e}")))?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(HostState {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        key,
        store: Store::open(root)?,
        peers: Mutex::new(HashMap::new()),
        events: Mutex::new(()),
        shutdown: shutdown.clone(),
    });
    let app = Router::new()
        .route("/lan/check", get(check))
        .route("/lan/events", get(events))
        .route("/lan/upload", post(upload))
        .route("/lan/files/{id}", get(download))
        .route("/lan/files/{id}/info", get(file_info))
        .fallback(|| async {
            ApiError::new(StatusCode::NOT_FOUND, "Маршрут не найден")
        })
        .layer(axum::extract::DefaultBodyLimit::max(MAX_JSON_BYTES))
        .with_state(state.clone());
    let stop = shutdown.clone();
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
        {
            eprintln!("Spark LAN HTTP: {e}");
        }
    });
    let discovery = crate::discovery::advertise(state.clone(), port, shutdown.clone()).await;
    Ok(HostHandle {
        port,
        shutdown,
        task,
        discovery,
    })
}

impl HostState {
    fn authorize(&self, headers: &HeaderMap) -> ApiResult<()> {
        if self.shutdown.is_cancelled() {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Комната закрыта",
            ));
        }
        let provided = headers
            .get("x-spark-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if validation::equal_secret(provided, &self.key) {
            Ok(())
        } else {
            Err(ApiError::unauthorized())
        }
    }
    async fn identity(&self, headers: &HeaderMap) -> ApiResult<Peer> {
        self.authorize(headers)?;
        let id = headers
            .get("x-spark-peer")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let session = headers
            .get("x-spark-session")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        self.current_peer(id, session).await
    }
    async fn current_peer(&self, id: &str, session: &str) -> ApiResult<Peer> {
        self.peers
            .lock()
            .await
            .get(id)
            .filter(|p| validation::equal_secret(&p.session, session))
            .map(|p| p.peer.clone())
            .ok_or_else(ApiError::unauthorized)
    }
    async fn transfer_cancel(&self, headers: &HeaderMap) -> ApiResult<CancellationToken> {
        self.authorize(headers)?;
        let id = headers
            .get("x-spark-peer")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let session = headers
            .get("x-spark-session")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        self.peers
            .lock()
            .await
            .get(id)
            .filter(|peer| validation::equal_secret(&peer.session, session))
            .map(|peer| peer.cancel.child_token())
            .ok_or_else(ApiError::unauthorized)
    }
    async fn broadcast(&self, event: ServerEvent, except: Option<&str>) {
        let peers = self.peers.lock().await;
        for (id, connected) in peers.iter() {
            if except == Some(id.as_str()) {
                continue;
            }
            if connected.tx.try_send(event.clone()).is_err() {
                connected.cancel.cancel();
            }
        }
    }
    async fn send(&self, id: &str, event: ServerEvent) {
        if let Some(peer) = self.peers.lock().await.get(id)
            && peer.tx.try_send(event).is_err()
        {
            peer.cancel.cancel();
        }
    }
    pub async fn peer_count(&self) -> usize {
        self.peers.lock().await.len()
    }
    async fn remove(&self, id: &str, session: &str) {
        let _event = self.events.lock().await;
        let removed = {
            let mut peers = self.peers.lock().await;
            if peers.get(id).is_some_and(|peer| peer.session == session) {
                peers.remove(id);
                true
            } else {
                false
            }
        };
        if removed {
            self.broadcast(ServerEvent::PeerLeft { peer_id: id.into() }, None)
                .await;
        }
    }
    async fn command(&self, id: &str, session: &str, command: ClientCommand) -> ApiResult<()> {
        let _event = self.events.lock().await;
        let sender = self.current_peer(id, session).await?;
        match command {
            ClientCommand::Chat { channel_id, text } => {
                self.store.channel(&channel_id, "text")?;
                let text = text.trim().to_owned();
                if text.is_empty() || text.chars().count() > MAX_CHAT_CHARS || text.contains('\0') {
                    return Err(ApiError::bad(
                        "Сообщение должно содержать от 1 до 8000 символов",
                    ));
                }
                let message = self.store.insert_message(ChatMessage {
                    id: uuid::Uuid::new_v4().to_string(),
                    channel_id,
                    sender_id: id.into(),
                    sender_name: sender.name,
                    text,
                    created_at: 0,
                    file: None,
                })?;
                self.broadcast(ServerEvent::Chat { message }, None).await;
            }
            ClientCommand::CreateChannel { name, channel_kind } => {
                if !["text", "voice"].contains(&channel_kind.as_str()) {
                    return Err(ApiError::bad("Тип канала должен быть text или voice"));
                }
                let channel = self
                    .store
                    .create_channel(validation::name(&name, 64)?, channel_kind)?;
                self.broadcast(ServerEvent::ChannelCreated { channel }, None)
                    .await;
            }
            ClientCommand::JoinVoice { channel_id } => {
                self.store.channel(&channel_id, "voice")?;
                let peer = {
                    let mut peers = self.peers.lock().await;
                    let connected = peers.get_mut(id).ok_or_else(ApiError::unauthorized)?;
                    connected.peer.voice_channel = Some(channel_id);
                    connected.peer.muted = false;
                    connected.peer.camera = false;
                    connected.peer.screen = false;
                    connected.peer.clone()
                };
                self.broadcast(ServerEvent::Presence { peer }, None).await;
            }
            ClientCommand::LeaveVoice => {
                let peer = {
                    let mut peers = self.peers.lock().await;
                    let connected = peers.get_mut(id).ok_or_else(ApiError::unauthorized)?;
                    connected.peer.voice_channel = None;
                    connected.peer.muted = false;
                    connected.peer.camera = false;
                    connected.peer.screen = false;
                    connected.peer.clone()
                };
                self.broadcast(ServerEvent::Presence { peer }, None).await;
            }
            ClientCommand::MediaState {
                muted,
                camera,
                screen,
            } => {
                if sender.voice_channel.is_none() {
                    return Err(ApiError::bad("Сначала войдите в голосовой канал"));
                }
                let peer = {
                    let mut peers = self.peers.lock().await;
                    let connected = peers.get_mut(id).ok_or_else(ApiError::unauthorized)?;
                    connected.peer.muted = muted;
                    connected.peer.camera = camera;
                    connected.peer.screen = screen;
                    connected.peer.clone()
                };
                self.broadcast(ServerEvent::Presence { peer }, None).await;
            }
            ClientCommand::Signal { to, payload } => {
                if to == id {
                    return Err(ApiError::bad("Нельзя отправить сигнал самому себе"));
                }
                let peers = self.peers.lock().await;
                let target = peers
                    .get(&to)
                    .ok_or_else(|| ApiError::bad("Участник не в сети"))?;
                if sender.voice_channel.is_none()
                    || sender.voice_channel != target.peer.voice_channel
                {
                    return Err(ApiError::bad(
                        "Сигналы разрешены только внутри одного голосового канала",
                    ));
                }
                if !payload.is_object() {
                    return Err(ApiError::bad("Сигнал должен быть JSON-объектом"));
                }
                if target
                    .tx
                    .try_send(ServerEvent::Signal {
                        from: id.into(),
                        payload,
                    })
                    .is_err()
                {
                    target.cancel.cancel();
                }
            }
            ClientCommand::History { channel_id, before } => {
                self.store.channel(&channel_id, "text")?;
                let messages = self.store.history(Some(&channel_id), before)?;
                self.send(
                    id,
                    ServerEvent::History {
                        channel_id,
                        messages,
                    },
                )
                .await;
            }
            ClientCommand::Invite {
                to,
                channel_id,
                video,
            } => {
                self.store.channel(&channel_id, "voice")?;
                if !self.peers.lock().await.contains_key(&to) {
                    return Err(ApiError::bad("Участник не в сети"));
                }
                self.send(
                    &to,
                    ServerEvent::Invite {
                        from: sender,
                        channel_id,
                        video,
                    },
                )
                .await;
            }
        }
        Ok(())
    }
}

async fn check(
    State(host): State<Arc<HostState>>,
    headers: HeaderMap,
) -> ApiResult<Json<serde_json::Value>> {
    host.authorize(&headers)?;
    Ok(Json(serde_json::json!({"name":host.name,"id":host.id})))
}
async fn events(
    State(host): State<Arc<HostState>>,
    headers: HeaderMap,
    Query(query): Query<PeerQuery>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    host.authorize(&headers)?;
    validation::id(&query.id)?;
    let name = validation::name(&query.name, 48)?;
    let session = headers
        .get("x-spark-session")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?
        .to_owned();
    validation::id(&session)?;
    Ok(ws
        .max_message_size(MAX_JSON_BYTES)
        .max_frame_size(MAX_JSON_BYTES)
        .on_upgrade(move |socket| host_socket(host, socket, query.id, name, session)))
}
async fn host_socket(
    host: Arc<HostState>,
    socket: WebSocket,
    id: String,
    name: String,
    session: String,
) {
    let (tx, mut rx) = mpsc::channel(256);
    let cancel = CancellationToken::new();
    {
        let _event = host.events.lock().await;
        let mut peers = host.peers.lock().await;
        if peers.len() >= 128 && !peers.contains_key(&id) {
            return;
        }
        let peer = Peer {
            id: id.clone(),
            name,
            voice_channel: None,
            muted: false,
            camera: false,
            screen: false,
        };
        if let Some(old) = peers.insert(
            id.clone(),
            ConnectedPeer {
                peer: peer.clone(),
                session: session.clone(),
                tx: tx.clone(),
                cancel: cancel.clone(),
            },
        ) {
            old.cancel.cancel();
        }
        let welcome = (|| -> ApiResult<ServerEvent> {
            Ok(ServerEvent::Welcome {
                self_id: id.clone(),
                server_name: host.name.clone(),
                channels: host.store.channels()?,
                peers: peers.values().map(|c| c.peer.clone()).collect(),
                messages: host.store.history(None, None)?,
            })
        })();
        drop(peers);
        match welcome {
            Ok(welcome) => {
                let _ = tx.try_send(welcome);
                host.broadcast(ServerEvent::PeerJoined { peer }, Some(&id))
                    .await;
            }
            Err(e) => {
                eprintln!("Spark welcome: {}", e.message);
                cancel.cancel();
            }
        }
    }
    let (mut writer, mut reader) = socket.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_seen = Instant::now();
    let mut commands = 0u32;
    let mut window = Instant::now();
    loop {
        tokio::select! {
            _=host.shutdown.cancelled()=>break,
            _=cancel.cancelled()=>break,
            _=heartbeat.tick()=>{
                if last_seen.elapsed()>Duration::from_secs(60) { break; }
                if !crate::gateway::send_ws(&mut writer,Message::Ping(Vec::new().into())).await { break; }
            }
            Some(event)=rx.recv()=>{
                let Ok(json)=serde_json::to_string(&event) else { break; };
                if !matches!(tokio::time::timeout(Duration::from_secs(10),writer.send(Message::Text(json.into()))).await,Ok(Ok(()))) { break; }
            }
            message=reader.next()=>{
                match message {
                    Some(Ok(Message::Text(text)))=>{
                        last_seen=Instant::now();
                        if window.elapsed()>Duration::from_secs(1) { window=Instant::now();commands=0; }
                        commands+=1;
                        if commands>100 { let _=tx.try_send(ServerEvent::Error { code:"rate_limit".into(),message:"Слишком много команд".into() });break; }
                        let result=match serde_json::from_str::<ClientCommand>(&text) {
                            Ok(command)=>host.command(&id,&session,command).await,
                            Err(_)=>Err(ApiError::bad("Некорректная команда")),
                        };
                        if let Err(e)=result { let _=tx.try_send(ServerEvent::Error { code:e.status.as_u16().to_string(),message:e.message }); }
                    }
                    Some(Ok(Message::Pong(_)))=>last_seen=Instant::now(),
                    Some(Ok(Message::Ping(data)))=>{ last_seen=Instant::now();if !crate::gateway::send_ws(&mut writer,Message::Pong(data)).await { break; } }
                    Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break,
                    _=>{}
                }
            }
        }
    }
    cancel.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(1), writer.close()).await;
    host.remove(&id, &session).await;
}

/// Removes only a randomly named private incomplete transfer, including on cancellation.
pub struct TempFile {
    pub path: PathBuf,
    pub armed: bool,
}
impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

async fn upload(
    State(host): State<Arc<HostState>>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> ApiResult<Json<ChatMessage>> {
    let sender = host.identity(&headers).await?;
    let transfer_cancel = host.transfer_cancel(&headers).await?;
    host.store.channel(&query.channel_id, "text")?;
    let name = validation::filename(&query.name)?;
    if headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|n| n > MAX_FILE_BYTES)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Максимальный размер файла — 2 ГиБ",
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    let mut temporary = TempFile {
        path: host.store.files_dir.join(format!(".upload-{id}.part")),
        armed: true,
    };
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary.path)
        .await?;
    let mut stream = body.into_data_stream();
    let mut hash = Sha256::new();
    let mut size = 0u64;
    loop {
        let chunk = tokio::select! {
            _=host.shutdown.cancelled()=>return Err(ApiError::new(StatusCode::SERVICE_UNAVAILABLE,"Комната закрыта")),
            _=transfer_cancel.cancelled()=>return Err(ApiError::unauthorized()),
            chunk=tokio::time::timeout(Duration::from_secs(60),stream.next())=>chunk.map_err(|_|ApiError::bad("Превышено время ожидания загрузки"))?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(ApiError::internal)?;
        size = size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| ApiError::bad("Размер файла превышен"))?;
        if size > MAX_FILE_BYTES {
            return Err(ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Максимальный размер файла — 2 ГиБ",
            ));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    let sha256 = hex::encode(hash.finalize());
    if let Some(expected) = headers.get("x-spark-sha256").and_then(|h| h.to_str().ok())
        && !validation::equal_secret(expected, &sha256)
    {
        return Err(ApiError::bad("Контрольная сумма файла не совпадает"));
    }
    let _event = host.events.lock().await;
    host.identity(&headers).await?;
    let final_path = host.store.files_dir.join(&id);
    tokio::fs::rename(&temporary.path, &final_path).await?;
    temporary.armed = false;
    let message = host.store.insert_message(ChatMessage {
        id: uuid::Uuid::new_v4().to_string(),
        channel_id: query.channel_id,
        sender_id: sender.id,
        sender_name: sender.name,
        text: String::new(),
        created_at: 0,
        file: Some(FileInfo {
            id,
            name,
            size,
            sha256,
            mime: validation::mime(
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or("application/octet-stream"),
            ),
        }),
    });
    match message {
        Ok(message) => {
            host.broadcast(
                ServerEvent::Chat {
                    message: message.clone(),
                },
                None,
            )
            .await;
            Ok(Json(message))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&final_path).await;
            Err(e)
        }
    }
}
async fn file_info(
    State(host): State<Arc<HostState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<FileInfo>> {
    host.authorize(&headers)?;
    Ok(Json(host.store.file(&id)?))
}
async fn download(
    State(host): State<Arc<HostState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    host.authorize(&headers)?;
    let info = host.store.file(&id)?;
    let file = tokio::fs::File::open(host.store.files_dir.join(&info.id)).await?;
    let mut response = Body::from_stream(
        ReaderStream::new(file).take_until(host.shutdown.clone().cancelled_owned()),
    )
    .into_response();
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&info.mime).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&info.size.to_string()).map_err(ApiError::internal)?,
    );
    response.headers_mut().insert(
        "x-spark-sha256",
        header::HeaderValue::from_str(&info.sha256).map_err(ApiError::internal)?,
    );
    response
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, attachment(&info.name)?);
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(response)
}
pub fn attachment(name: &str) -> ApiResult<header::HeaderValue> {
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
    header::HeaderValue::from_str(&format!(
        "attachment; filename=\"download\"; filename*=UTF-8''{}",
        encoded.replace('+', "%20")
    ))
    .map_err(ApiError::internal)
}
