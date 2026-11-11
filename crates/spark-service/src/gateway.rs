use crate::{
    error::{ApiError, ApiResult, json},
    host::{self, HostHandle, TempFile, UploadQuery},
    validation,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{
        DefaultBodyLimit, Path, Query, Request, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use spark_protocol::{
    Bootstrap, ChatMessage, ConnectionInfo, FileInfo, MAX_FILE_BYTES, MAX_JSON_BYTES, Profile,
    ServerEvent,
};
use std::{
    net::Ipv4Addr,
    path::{Path as FsPath, PathBuf},
    sync::{Arc, Mutex as SyncMutex},
    time::{Duration, Instant},
};
use tokio::{
    io::AsyncWriteExt,
    net::TcpListener,
    sync::{Mutex, RwLock},
};
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

#[derive(Clone, Debug)]
pub struct ServiceOptions {
    pub data_dir: PathBuf,
    pub ui_dir: PathBuf,
    pub gateway_port: u16,
}
impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("spark-data"),
            ui_dir: PathBuf::from("assets"),
            gateway_port: 0,
        }
    }
}
pub struct Service {
    pub url: String,
    pub token: String,
    state: Arc<GatewayState>,
    task: Option<tokio::task::JoinHandle<()>>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ActiveConfig {
    address: String,
    key: String,
    server_name: String,
    hosting: bool,
    generation: String,
}
struct GatewaySession {
    id: String,
    stop: CancellationToken,
}
struct GatewayState {
    root: PathBuf,
    origin: String,
    token: String,
    profile: RwLock<Profile>,
    active: RwLock<Option<ActiveConfig>>,
    hosted: Mutex<Option<HostHandle>>,
    session: Mutex<Option<GatewaySession>>,
    operation: Mutex<()>,
    client: reqwest::Client,
    stop: CancellationToken,
}
#[derive(Deserialize)]
struct NameRequest {
    name: String,
}
#[derive(Deserialize)]
struct HostRequest {
    name: String,
    code: String,
    port: Option<u16>,
}
#[derive(Deserialize)]
struct JoinRequest {
    address: String,
    code: String,
}
#[derive(Deserialize)]
struct SaveRequest {
    file_id: String,
    path: String,
    #[serde(default)]
    overwrite: bool,
}

impl Service {
    pub async fn start(options: ServiceOptions) -> anyhow::Result<Self> {
        tokio::fs::create_dir_all(&options.data_dir).await?;
        let profile_path = options.data_dir.join("profile.json");
        let profile_value = if profile_path.exists() {
            let profile: Profile = serde_json::from_slice(&tokio::fs::read(&profile_path).await?)?;
            validation::id(&profile.id).map_err(|e| anyhow::anyhow!(e.message))?;
            validation::name(&profile.name, 48).map_err(|e| anyhow::anyhow!(e.message))?;
            profile
        } else {
            let profile = Profile {
                id: uuid::Uuid::new_v4().to_string(),
                name: "Участник".into(),
            };
            save_json(&profile_path, &profile)
                .await
                .map_err(|e| anyhow::anyhow!(e.message))?;
            profile
        };
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, options.gateway_port)).await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let active = match tokio::fs::read(options.data_dir.join("connection.json")).await {
            Ok(bytes) => serde_json::from_slice::<ActiveConfig>(&bytes)
                .ok()
                .filter(|c| !c.hosting && validation::address(&c.address).is_ok()),
            Err(_) => None,
        };
        let stop = CancellationToken::new();
        let state = Arc::new(GatewayState {
            root: options.data_dir,
            origin: origin.clone(),
            token: token.clone(),
            profile: RwLock::new(profile_value),
            active: RwLock::new(active),
            hosted: Mutex::new(None),
            session: Mutex::new(None),
            operation: Mutex::new(()),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .build()?,
            stop: stop.clone(),
        });
        let app = Router::new()
            .route("/api/bootstrap", get(bootstrap))
            .route("/api/profile", put(profile))
            .route("/api/discovery", get(discover))
            .route("/api/host", post(start_host))
            .route("/api/join", post(join))
            .route("/api/disconnect", post(disconnect))
            .route("/api/events", get(events))
            .route("/api/upload", post(upload))
            .route("/api/files/{id}", get(download))
            .route("/api/save", post(save))
            .fallback_service(ServeDir::new(options.ui_dir).append_index_html_on_directories(true))
            .layer(DefaultBodyLimit::max(MAX_JSON_BYTES))
            .layer(middleware::from_fn_with_state(state.clone(), security))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await
            {
                eprintln!("Spark loopback HTTP: {e}");
            }
        });
        Ok(Self {
            url: origin,
            token,
            state,
            task: Some(task),
        })
    }
    pub async fn shutdown(mut self) {
        self.state.stop.cancel();
        self.state.disconnect_active().await;
        if let Some(task) = self.task.take() {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}
impl Drop for Service {
    fn drop(&mut self) {
        self.state.stop.cancel();
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
impl GatewayState {
    async fn config(&self) -> ApiResult<ActiveConfig> {
        self.active.read().await.clone().ok_or_else(|| {
            ApiError::new(
                StatusCode::CONFLICT,
                "Сначала создайте комнату или подключитесь к ней",
            )
        })
    }
    async fn disconnect_active(&self) {
        if let Some(session) = self.session.lock().await.take() {
            session.stop.cancel();
        }
        *self.active.write().await = None;
        if let Some(host) = self.hosted.lock().await.take() {
            host.stop().await;
        }
        let _ = tokio::fs::remove_file(self.root.join("connection.json")).await;
    }
    async fn checked_remote(
        &self,
        request: reqwest::RequestBuilder,
    ) -> ApiResult<reqwest::Response> {
        let response = request.send().await.map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("Сервер комнаты недоступен: {e}"),
            )
        })?;
        if !response.status().is_success() {
            let status = response.status();
            // Error bodies are never allowed to allocate an arbitrary amount of memory.
            let mut stream = response.bytes_stream();
            let mut bytes = Vec::new();
            while let Ok(Some(Ok(chunk))) =
                tokio::time::timeout(Duration::from_secs(5), stream.next()).await
            {
                if bytes.len() + chunk.len() > 4096 {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            let message = serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("error").and_then(|v| v.as_str()).map(str::to_owned))
                .unwrap_or_else(|| format!("Сервер вернул ошибку {status}"));
            return Err(ApiError::new(status, message));
        }
        Ok(response)
    }
    async fn file_info(&self, config: &ActiveConfig, id: &str) -> ApiResult<FileInfo> {
        validation::id(id)?;
        let response = self
            .checked_remote(
                self.client
                    .get(format!("http://{}/lan/files/{id}/info", config.address))
                    .header("x-spark-key", &config.key)
                    .timeout(Duration::from_secs(10)),
            )
            .await?;
        let info: FileInfo = remote_json(response).await?;
        if info.id != id
            || info.size > MAX_FILE_BYTES
            || info.sha256.len() != 64
            || hex::decode(&info.sha256).is_err()
        {
            return Err(ApiError::bad("Недопустимые метаданные файла"));
        }
        validation::filename(&info.name)?;
        Ok(info)
    }
}

async fn security(
    State(state): State<Arc<GatewayState>>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let expected = state.origin.trim_start_matches("http://");
    if host != expected {
        return ApiError::new(StatusCode::FORBIDDEN, "Недопустимый заголовок Host").into_response();
    }
    if request
        .headers()
        .get(header::ORIGIN)
        .is_some_and(|v| v.to_str().ok() != Some(state.origin.as_str()))
    {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "Запрос разрешён только из локального приложения Spark",
        )
        .into_response();
    }
    if request
        .headers()
        .get("sec-fetch-site")
        .is_some_and(|v| v == "cross-site")
    {
        return ApiError::new(StatusCode::FORBIDDEN, "Межсайтовый запрос отклонён").into_response();
    }
    let path = request.uri().path();
    if path.starts_with("/api/") && path != "/api/bootstrap" {
        let mut token = request
            .headers()
            .get("x-spark-token")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if token.is_none() && (path == "/api/events" || path.starts_with("/api/files/")) {
            token = request.uri().query().and_then(|q| {
                url::form_urlencoded::parse(q.as_bytes())
                    .find(|(key, _)| key == "token")
                    .map(|(_, value)| value.into_owned())
            });
        }
        if !token
            .as_deref()
            .is_some_and(|token| validation::equal_secret(token, &state.token))
        {
            return ApiError::unauthorized().into_response();
        }
    }
    let is_api = path.starts_with("/api/");
    let mut response = next.run(request).await;
    if is_api && response.status().is_client_error() && !path_security_cache(&response) {
        response = ApiError::new(
            response.status(),
            response.status().canonical_reason().unwrap_or("Ошибка API"),
        )
        .into_response();
    }
    response.headers_mut().insert(
        "x-content-type-options",
        header::HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        "referrer-policy",
        header::HeaderValue::from_static("no-referrer"),
    );
    if path_security_cache(&response) {
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
    }
    response
}
fn path_security_cache(response: &Response) -> bool {
    response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.to_str().is_ok_and(|v| v.starts_with("application/json")))
}
async fn bootstrap(State(state): State<Arc<GatewayState>>) -> Json<Bootstrap> {
    Json(Bootstrap {
        version: spark_protocol::VERSION.into(),
        token: state.token.clone(),
        profile: state.profile.read().await.clone(),
    })
}
async fn profile(
    State(state): State<Arc<GatewayState>>,
    body: Result<Json<NameRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<Profile>> {
    let name = validation::name(&json(body)?.name, 48)?;
    let mut profile = state.profile.write().await;
    let next = Profile {
        id: profile.id.clone(),
        name,
    };
    save_json(&state.root.join("profile.json"), &next).await?;
    *profile = next.clone();
    Ok(Json(next))
}
async fn discover() -> Json<Vec<spark_protocol::Discovery>> {
    Json(crate::discovery::discover().await)
}
async fn start_host(
    State(state): State<Arc<GatewayState>>,
    body: Result<Json<HostRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<ConnectionInfo>> {
    let request = json(body)?;
    let name = validation::name(&request.name, 64)?;
    validation::code(&request.code)?;
    let _operation = state.operation.lock().await;
    // Keep an existing connection usable when a new host cannot be bound.
    if state.hosted.lock().await.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Вы уже создали комнату. Сначала отключитесь от неё",
        ));
    }
    let host = host::start(
        &state.root.join("host"),
        name.clone(),
        validation::key(&request.code),
        request.port.unwrap_or(spark_protocol::DEFAULT_LAN_PORT),
        state.stop.child_token(),
    )
    .await?;
    state.disconnect_active().await;
    let address = crate::discovery::local_address(host.port);
    let config = ActiveConfig {
        address: format!("127.0.0.1:{}", host.port),
        key: validation::key(&request.code),
        server_name: name.clone(),
        hosting: true,
        generation: uuid::Uuid::new_v4().to_string(),
    };
    *state.active.write().await = Some(config);
    *state.hosted.lock().await = Some(host);
    Ok(Json(ConnectionInfo {
        server_name: name,
        address,
        hosting: true,
    }))
}
async fn join(
    State(state): State<Arc<GatewayState>>,
    body: Result<Json<JoinRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<ConnectionInfo>> {
    let request = json(body)?;
    let address = validation::address(&request.address)?;
    validation::code(&request.code)?;
    let _operation = state.operation.lock().await;
    let key = validation::key(&request.code);
    let response = state
        .checked_remote(
            state
                .client
                .get(format!("http://{address}/lan/check"))
                .header("x-spark-key", &key)
                .timeout(Duration::from_secs(7)),
        )
        .await?;
    let value: serde_json::Value = remote_json(response).await?;
    let name = validation::name(
        value
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ApiError::bad("Этот сервер не поддерживает Spark"))?,
        64,
    )?;
    let config = ActiveConfig {
        address: address.clone(),
        key,
        server_name: name.clone(),
        hosting: false,
        generation: uuid::Uuid::new_v4().to_string(),
    };
    save_json(&state.root.join("connection.pending.json"), &config).await?;
    state.disconnect_active().await;
    tokio::fs::rename(
        state.root.join("connection.pending.json"),
        state.root.join("connection.json"),
    )
    .await?;
    *state.active.write().await = Some(config);
    Ok(Json(ConnectionInfo {
        server_name: name,
        address,
        hosting: false,
    }))
}
async fn disconnect(State(state): State<Arc<GatewayState>>) -> Json<serde_json::Value> {
    let _operation = state.operation.lock().await;
    state.disconnect_active().await;
    Json(serde_json::json!({"ok":true}))
}
async fn events(
    State(state): State<Arc<GatewayState>>,
    ws: WebSocketUpgrade,
) -> ApiResult<Response> {
    let config = state.config().await?;
    Ok(ws
        .max_message_size(MAX_JSON_BYTES)
        .max_frame_size(MAX_JSON_BYTES)
        .on_upgrade(move |socket| proxy_events(state, config, socket)))
}
async fn proxy_events(state: Arc<GatewayState>, config: ActiveConfig, mut socket: WebSocket) {
    let profile = state.profile.read().await.clone();
    let session_id = uuid::Uuid::new_v4().to_string();
    let stop = CancellationToken::new();
    {
        let active = state.active.read().await;
        if !active
            .as_ref()
            .is_some_and(|c| c.generation == config.generation)
        {
            return;
        }
        let mut session = state.session.lock().await;
        if let Some(old) = session.replace(GatewaySession {
            id: session_id.clone(),
            stop: stop.clone(),
        }) {
            old.stop.cancel();
        }
    }
    let mut url = url::Url::parse(&format!("ws://{}/lan/events", config.address))
        .expect("validated host address");
    url.query_pairs_mut()
        .append_pair("id", &profile.id)
        .append_pair("name", &profile.name);
    let connection = async {
        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|e| e.to_string())?;
        request.headers_mut().insert(
            "x-spark-key",
            config
                .key
                .parse()
                .map_err(|_| "Некорректный ключ комнаты".to_owned())?,
        );
        request.headers_mut().insert(
            "x-spark-session",
            session_id
                .parse()
                .map_err(|_| "Некорректный сеанс".to_owned())?,
        );
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio_tungstenite::connect_async_with_config(
                request,
                Some(
                    tungstenite::protocol::WebSocketConfig::default()
                        .max_message_size(Some(16 * 1024 * 1024))
                        .max_frame_size(Some(16 * 1024 * 1024)),
                ),
                false,
            ),
        )
        .await
        .map_err(|_| "Сервер не ответил вовремя".to_owned())?
        .map_err(|e| e.to_string())
    };
    let remote = tokio::select! {
        _=state.stop.cancelled()=>Err("Сервис завершает работу".into()),
        _=stop.cancelled()=>Err("Подключение заменено или закрыто".into()),
        remote=connection=>remote,
    };
    let (remote, _) = match remote {
        Ok(remote) => remote,
        Err(reason) => {
            let event = ServerEvent::Disconnected {
                reason: format!("Не удалось подключиться к комнате: {reason}"),
            };
            let _ = socket
                .send(Message::Text(serde_json::to_string(&event).unwrap().into()))
                .await;
            let _ = socket.close().await;
            let mut session = state.session.lock().await;
            if session.as_ref().is_some_and(|s| s.id == session_id) {
                *session = None;
            }
            return;
        }
    };
    {
        let active = state.active.read().await;
        if !active
            .as_ref()
            .is_some_and(|c| c.generation == config.generation)
        {
            return;
        }
        if stop.is_cancelled() {
            return;
        }
    }
    let (mut local_tx, mut local_rx) = socket.split();
    let (mut remote_tx, mut remote_rx) = remote.split();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    let mut last_local = Instant::now();
    let reason = loop {
        tokio::select! {
            _=state.stop.cancelled()=>break "Сервис завершает работу".to_owned(),
            _=stop.cancelled()=>break "Подключение заменено или закрыто".to_owned(),
            _=heartbeat.tick()=>{
                if last_local.elapsed()>Duration::from_secs(60) {break "Локальное соединение потеряно".into();}
                if local_tx.send(Message::Ping(Vec::new().into())).await.is_err(){break "Локальное соединение закрыто".into();}
            }
            incoming=local_rx.next()=>match incoming {
                Some(Ok(Message::Text(text)))=>{last_local=Instant::now();if remote_tx.send(tungstenite::Message::Text(text.as_str().to_owned().into())).await.is_err(){break "Соединение с комнатой потеряно".into();}}
                Some(Ok(Message::Pong(_)))=>last_local=Instant::now(),
                Some(Ok(Message::Ping(data)))=>{if local_tx.send(Message::Pong(data)).await.is_err(){break "Локальное соединение закрыто".into();}}
                Some(Ok(Message::Close(_)))|None|Some(Err(_))=>break "Локальное соединение закрыто".into(),
                _=>{}
            },
            incoming=remote_rx.next()=>match incoming {
                Some(Ok(tungstenite::Message::Text(text)))=>{if local_tx.send(Message::Text(text.as_str().to_owned().into())).await.is_err(){break "Локальное соединение закрыто".into();}}
                Some(Ok(tungstenite::Message::Ping(data)))=>{if remote_tx.send(tungstenite::Message::Pong(data)).await.is_err(){break "Соединение с комнатой потеряно".into();}}
                Some(Ok(tungstenite::Message::Close(_)))|None|Some(Err(_))=>break "Соединение с комнатой потеряно".into(),
                _=>{}
            }
        }
    };
    let _ = tokio::time::timeout(Duration::from_secs(1), remote_tx.close()).await;
    let event = ServerEvent::Disconnected { reason };
    let _ = tokio::time::timeout(
        Duration::from_secs(1),
        local_tx.send(Message::Text(serde_json::to_string(&event).unwrap().into())),
    )
    .await;
    let _ = tokio::time::timeout(Duration::from_secs(1), local_tx.close()).await;
    let mut session = state.session.lock().await;
    if session.as_ref().is_some_and(|s| s.id == session_id) {
        *session = None;
    }
}

#[derive(Default)]
struct TransferHash {
    hash: Sha256,
    bytes: u64,
}
async fn upload(
    State(state): State<Arc<GatewayState>>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Body,
) -> ApiResult<Json<ChatMessage>> {
    let config = state.config().await?;
    let profile = state.profile.read().await.clone();
    validation::filename(&query.name)?;
    let session = state
        .session
        .lock()
        .await
        .as_ref()
        .map(|s| s.id.clone())
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "Дождитесь подключения к чату"))?;
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > MAX_FILE_BYTES) {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Максимальный размер файла — 2 ГиБ",
        ));
    }
    let hash = Arc::new(SyncMutex::new(TransferHash::default()));
    let hash_stream = hash.clone();
    let stream = body
        .into_data_stream()
        .map(move |chunk| -> Result<_, std::io::Error> {
            let chunk = chunk.map_err(std::io::Error::other)?;
            let mut hash = hash_stream
                .lock()
                .map_err(|_| std::io::Error::other("transfer lock poisoned"))?;
            hash.bytes = hash
                .bytes
                .checked_add(chunk.len() as u64)
                .ok_or_else(|| std::io::Error::other("file too large"))?;
            if hash.bytes > MAX_FILE_BYTES {
                return Err(std::io::Error::other("file exceeds 2 GiB"));
            }
            hash.hash.update(&chunk);
            Ok(chunk)
        });
    let mut url = url::Url::parse(&format!("http://{}/lan/upload", config.address))
        .map_err(ApiError::internal)?;
    url.query_pairs_mut()
        .append_pair("channel_id", &query.channel_id)
        .append_pair("name", &query.name);
    let mut request = state
        .client
        .post(url)
        .header("x-spark-key", &config.key)
        .header("x-spark-peer", profile.id)
        .header("x-spark-session", session)
        .header(
            header::CONTENT_TYPE,
            validation::mime(
                headers
                    .get(header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("application/octet-stream"),
            ),
        )
        .body(reqwest::Body::wrap_stream(stream));
    if let Some(length) = length {
        request = request.header(header::CONTENT_LENGTH, length);
    }
    if let Some(expected) = headers.get("x-spark-sha256").and_then(|v| v.to_str().ok()) {
        request = request.header("x-spark-sha256", expected);
    }
    let response = state.checked_remote(request).await?;
    let message: ChatMessage = remote_json(response).await?;
    let hash = hash.lock().map_err(ApiError::internal)?;
    let sha256 = hex::encode(hash.hash.clone().finalize());
    if !message
        .file
        .as_ref()
        .is_some_and(|f| f.size == hash.bytes && validation::equal_secret(&f.sha256, &sha256))
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Переданный файл не прошёл проверку контрольной суммы",
        ));
    }
    Ok(Json(message))
}
async fn download(
    State(state): State<Arc<GatewayState>>,
    Path(id): Path<String>,
) -> ApiResult<Response> {
    let config = state.config().await?;
    let info = state.file_info(&config, &id).await?;
    let response = state
        .checked_remote(
            state
                .client
                .get(format!("http://{}/lan/files/{id}", config.address))
                .header("x-spark-key", &config.key),
        )
        .await?;
    let mut output = Body::from_stream(response.bytes_stream()).into_response();
    output.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_str(&validation::mime(&info.mime)).map_err(ApiError::internal)?,
    );
    output.headers_mut().insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&info.size.to_string()).map_err(ApiError::internal)?,
    );
    output
        .headers_mut()
        .insert(header::CONTENT_DISPOSITION, host::attachment(&info.name)?);
    output.headers_mut().insert(
        "x-spark-sha256",
        header::HeaderValue::from_str(&info.sha256).map_err(ApiError::internal)?,
    );
    output.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    Ok(output)
}
async fn save(
    State(state): State<Arc<GatewayState>>,
    body: Result<Json<SaveRequest>, axum::extract::rejection::JsonRejection>,
) -> ApiResult<Json<serde_json::Value>> {
    let request = json(body)?;
    let config = state.config().await?;
    let path = PathBuf::from(&request.path);
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ApiError::bad(
            "Для сохранения требуется абсолютный путь из системного диалога",
        ));
    }
    let filename = path
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| ApiError::bad("Недопустимое имя файла"))?;
    validation::filename(filename)?;
    if tokio::fs::try_exists(&path).await? && !request.overwrite {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "Файл уже существует; требуется подтверждение замены в системном диалоге",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| ApiError::bad("Не указан каталог назначения"))?;
    if !tokio::fs::metadata(parent).await?.is_dir() {
        return Err(ApiError::bad("Каталог назначения не существует"));
    }
    let info = state.file_info(&config, &request.file_id).await?;
    let response = state
        .checked_remote(
            state
                .client
                .get(format!("http://{}/lan/files/{}", config.address, info.id))
                .header("x-spark-key", &config.key),
        )
        .await?;
    let mut temporary = TempFile {
        path: parent.join(format!(".spark-save-{}.part", uuid::Uuid::new_v4())),
        armed: true,
    };
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary.path)
        .await?;
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .map_err(|_| ApiError::bad("Превышено время ожидания скачивания"))?
    {
        let chunk = chunk.map_err(ApiError::internal)?;
        bytes += chunk.len() as u64;
        if bytes > info.size || bytes > MAX_FILE_BYTES {
            return Err(ApiError::bad(
                "Размер скачанного файла не совпадает с ожидаемым",
            ));
        }
        hash.update(&chunk);
        file.write_all(&chunk).await?;
    }
    let sha256 = hex::encode(hash.finalize());
    if bytes != info.size || !validation::equal_secret(&sha256, &info.sha256) {
        return Err(ApiError::bad(
            "Контрольная сумма скачанного файла не совпадает",
        ));
    }
    file.sync_all().await?;
    drop(file);
    if request.overwrite {
        tokio::fs::rename(&temporary.path, &path).await?;
    } else {
        // A hard link atomically fails if the destination appeared after the dialog.
        tokio::fs::hard_link(&temporary.path, &path)
            .await
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ApiError::new(StatusCode::CONFLICT, "Файл уже существует")
                } else {
                    ApiError::from(e)
                }
            })?;
        tokio::fs::remove_file(&temporary.path).await?;
    }
    temporary.armed = false;
    Ok(Json(
        serde_json::json!({"path":path,"sha256":sha256,"bytes":bytes}),
    ))
}
async fn save_json(path: &FsPath, value: &impl Serialize) -> ApiResult<()> {
    let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .await?;
    }
    let mut guard = TempFile {
        path: temp.clone(),
        armed: true,
    };
    file.write_all(&serde_json::to_vec(value)?).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temp, path).await?;
    guard.armed = false;
    Ok(())
}

async fn remote_json<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> ApiResult<T> {
    if response
        .content_length()
        .is_some_and(|n| n > MAX_JSON_BYTES as u64)
    {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Слишком большой JSON-ответ сервера",
        ));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "Сервер не ответил вовремя"))?
    {
        let chunk = chunk.map_err(ApiError::internal)?;
        if body.len() + chunk.len() > MAX_JSON_BYTES {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "Слишком большой JSON-ответ сервера",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "Некорректный JSON-ответ сервера"))
}
