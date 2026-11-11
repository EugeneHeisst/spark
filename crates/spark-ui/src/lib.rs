//! Spark's local-first desktop interface. All application and media logic is Rust.
mod media;
mod preferences;
mod updates;

use js_sys::{Function, Promise, Reflect};
use leptos::prelude::*;
use media::{MediaCallbacks, MediaManager, MediaState};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use spark_protocol::*;
use std::{cell::RefCell, collections::HashMap, rc::Rc};
use wasm_bindgen::{JsCast, prelude::*};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{Event, HtmlInputElement, HtmlTextAreaElement, MediaStream, MessageEvent, WebSocket};

type AppRef = Rc<RefCell<App>>;

#[derive(Default)]
struct Form {
    mode: String,
    name: String,
    room: String,
    code: String,
    address: String,
    channel_name: String,
    channel_kind: String,
    settings_name: String,
}
#[derive(Clone, Deserialize)]
struct Source {
    id: String,
    name: String,
    thumbnail: String,
}
struct Upload {
    name: String,
    percent: u32,
}
struct UploadRequest {
    xhr: web_sys::XmlHttpRequest,
    _progress: Closure<dyn FnMut(web_sys::ProgressEvent)>,
    _done: Closure<dyn FnMut(Event)>,
}
struct Invite {
    from: Peer,
    channel_id: String,
    video: bool,
}
#[derive(Default)]
struct App {
    preferences: preferences::Preferences,
    update: updates::Status,
    update_poll_busy: bool,
    update_installing: bool,
    token: String,
    profile: Option<Profile>,
    version: String,
    conn: Option<ConnectionInfo>,
    ws: Option<WebSocket>,
    generation: u32,
    reconnecting: bool,
    ready: bool,
    busy: bool,
    channels: Vec<Channel>,
    peers: Vec<Peer>,
    messages: Vec<ChatMessage>,
    selected: String,
    draft: String,
    form: Form,
    discovered: Vec<Discovery>,
    discovery_generation: u32,
    discovery_busy: bool,
    error: Option<String>,
    notice: Option<String>,
    modal: Option<String>,
    expanded_stream: Option<(String, String)>,
    invite: Option<Invite>,
    sources: Vec<Source>,
    media: Option<MediaManager>,
    media_state: MediaState,
    voice_generation: u32,
    streams: HashMap<(String, String), MediaStream>,
    connections: HashMap<String, String>,
    upload: Option<Upload>,
    upload_request: Option<UploadRequest>,
    history_busy: bool,
    cue_audio: HashMap<String, web_sys::HtmlAudioElement>,
}

fn window() -> web_sys::Window {
    web_sys::window().expect("window")
}
fn document() -> web_sys::Document {
    window().document().expect("document")
}
fn element(id: &str) -> Option<web_sys::Element> {
    document().get_element_by_id(id)
}
fn html(id: &str, content: &str) {
    if let Some(el) = element(id) {
        el.set_inner_html(content);
    }
}
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn initial(name: &str) -> String {
    esc(&name
        .chars()
        .next()
        .unwrap_or('?')
        .to_uppercase()
        .to_string())
}
fn bytes(n: u64) -> String {
    if n >= 1024 * 1024 * 1024 {
        format!("{:.1} ГБ", n as f64 / 1073741824.)
    } else if n >= 1024 * 1024 {
        format!("{:.1} МБ", n as f64 / 1048576.)
    } else if n >= 1024 {
        format!("{:.1} КБ", n as f64 / 1024.)
    } else {
        format!("{n} Б")
    }
}
fn time(ms: i64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}
fn encode(s: &str) -> String {
    js_sys::encode_uri_component(s)
        .as_string()
        .unwrap_or_default()
}
fn err_text(v: JsValue) -> String {
    v.as_string().unwrap_or_else(|| format!("{v:?}"))
}
fn error(app: &AppRef, message: impl Into<String>) {
    app.borrow_mut().error = Some(message.into().chars().take(500).collect());
    render_overlay(app);
}
fn send(app: &AppRef, cmd: ClientCommand) -> bool {
    let ws = app.borrow().ws.clone();
    if let Some(ws) = ws.filter(|s| s.ready_state() == WebSocket::OPEN) {
        match ws.send_with_str(&serde_json::to_string(&cmd).unwrap_or_default()) {
            Ok(()) => true,
            Err(e) => {
                error(app, err_text(e));
                false
            }
        }
    } else {
        error(
            app,
            "Соединение с комнатой потеряно. Дождитесь переподключения.",
        );
        false
    }
}

async fn api<T: DeserializeOwned>(
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<T, String> {
    let opts = web_sys::RequestInit::new();
    opts.set_method(method);
    if let Some(body) = body {
        opts.set_body(&JsValue::from_str(&body.to_string()));
    }
    let req = web_sys::Request::new_with_str_and_init(path, &opts).map_err(err_text)?;
    req.headers()
        .set("content-type", "application/json")
        .map_err(err_text)?;
    if !token.is_empty() {
        req.headers()
            .set("x-spark-token", token)
            .map_err(err_text)?;
    }
    let response: web_sys::Response = JsFuture::from(window().fetch_with_request(&req))
        .await
        .map_err(err_text)?
        .dyn_into()
        .map_err(err_text)?;
    let status = response.status();
    let text = JsFuture::from(response.text().map_err(err_text)?)
        .await
        .map_err(err_text)?
        .as_string()
        .unwrap_or_default();
    if !response.ok() {
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned));
        return Err(message.unwrap_or_else(|| format!("Ошибка сервиса ({status})")));
    }
    serde_json::from_str(&text).map_err(|_| "Сервис вернул некорректный ответ".into())
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(|| view! { <main id="spark" aria-label="Spark"/> });
    let app = Rc::new(RefCell::new(App::default()));
    app.borrow_mut().form.mode = "join".into();
    app.borrow_mut().form.room = "Моя локальная сеть".into();
    app.borrow_mut().form.channel_kind = "text".into();
    install_events(&app);
    html(
        "spark",
        "<div class='boot'><div class='brand-mark'>ϟ</div><h1>Spark</h1><p>Подключаем локальный сервис…</p></div>",
    );
    let a = app.clone();
    spawn_local(async move {
        let prefs = preferences::load().await;
        preferences::apply(&prefs);
        a.borrow_mut().preferences = prefs;
        match api::<Bootstrap>("", "GET", "/api/bootstrap", None).await {
            Ok(b) => {
                {
                    let mut s = a.borrow_mut();
                    s.token = b.token;
                    s.version = b.version;
                    s.form.name = b.profile.name.clone();
                    s.profile = Some(b.profile);
                    s.ready = true;
                }
                render(&a);
                discovery(&a);
                updates::start(&a);
            }
            Err(e) => {
                html(
                    "spark",
                    &format!(
                        "<div class='boot'><div class='brand-mark'>ϟ</div><h1>Сервис недоступен</h1><p>{}</p><button data-action='reload'>Попробовать снова</button></div>",
                        esc(&e)
                    ),
                );
            }
        }
    });
}

fn render(app: &AppRef) {
    if app.borrow().conn.is_none() {
        render_onboarding(app);
        return;
    }
    if element("workspace").is_none() {
        html(
            "spark",
            "<div id='workspace' class='workspace'><nav class='rail'><button class='brand-mark small' data-action='about' title='Spark'>ϟ</button><div class='rail-line'></div><div class='space-icon' title='Текущая локальная сеть'>S<span></span></div><div class='rail-bottom'><span class='lan-indicator'></span><span>LAN</span></div></nav><aside id='sidebar' class='sidebar'></aside><section class='main'><header id='chat-header' class='chat-header'></header><section id='voice-panel' class='voice-panel'></section><section id='message-list' class='message-list' aria-live='polite'></section><section id='composer' class='composer-wrap'></section></section><aside id='members' class='members'></aside></div><div id='overlay'></div><input id='file-input' type='file' hidden/>",
        );
    }
    render_sidebar(app);
    render_header(app);
    render_messages(app, true);
    render_composer(app);
    render_members(app);
    render_voice(app);
    render_overlay(app);
}

fn render_onboarding(app: &AppRef) {
    let s = app.borrow();
    let host = s.form.mode == "host";
    let content = format!(
        r#"<div class="onboarding"><div class="onboard-info"><div class="brand-lockup"><div class="brand-mark">ϟ</div><span>Spark<span class="wordmark-dot">.</span></span></div><div class="eyebrow"><span class="status-dot"></span> ВАША СЕТЬ. ВАШЕ ПРОСТРАНСТВО.</div><h1>Ближе,<br/>чем кажется<span>.</span></h1><p class="lead">Всё, что нужно для общения.<br/>Прямо в вашей локальной сети.</p><div class="feature-pills"><span>◉ Голос и видео</span><span>▱ Общий экран</span><span>↗ Файлы</span></div><div class="local-note"><span>⌁</span><p><strong>Без облака и регистрации</strong><br/>Один компьютер создаёт комнату.<br/>Остальные подключаются по локальной сети.</p></div><button class="onboard-settings" data-action="settings" data-testid="settings">⚙ Настройки</button><footer>Spark <span>v{version} · Linux / Windows</span></footer></div><section class="onboard-card"><div class="card-heading"><span class="eyebrow">НАЧНЁМ ОБЩЕНИЕ</span><h2>Ваше место в сети</h2><p>Укажите имя и подключитесь к своей команде.</p></div><label class="field">Как вас называть<input id="display-name" data-field="name" data-testid="display-name" autocomplete="nickname" maxlength="64" value="{name}" placeholder="Ваше имя"/></label><div class="tabs"><button class="{join_class}" data-action="mode-join" data-testid="join-tab">Подключиться</button><button class="{host_class}" data-action="mode-host" data-testid="host-tab">Создать комнату</button></div>{fields}<label class="field">Код комнаты<input id="room-code" data-field="code" data-testid="room-code" type="password" minlength="6" maxlength="128" value="{code}" placeholder="Не менее 6 символов" autocomplete="off"/></label><p class="field-help">{help}</p><button class="primary wide" data-action="connect" data-testid="connect" {disabled}>{button}<span>→</span></button>{discovery}<p class="trust-note">Чаты и файлы проходят через создателя комнаты.<br/>Используйте Spark в доверенной локальной сети.</p></section></div><div id="overlay"></div>"#,
        version = esc(&s.version),
        name = esc(&s.form.name),
        code = esc(&s.form.code),
        join_class = if host { "" } else { "active" },
        host_class = if host { "active" } else { "" },
        fields = if host {
            format!(
                "<label class='field'>Название комнаты<input data-field='room' data-testid='room-name' maxlength='64' value='{}' placeholder='Например: Команда дома'/></label>",
                esc(&s.form.room)
            )
        } else {
            format!(
                "<label class='field'>Адрес комнаты<input data-field='address' data-testid='room-address' value='{}' placeholder='192.168.1.10:39393' autocomplete='off'/></label>",
                esc(&s.form.address)
            )
        },
        help = if host {
            "Передайте код людям, которых хотите пригласить."
        } else {
            "Попросите адрес и код у создателя комнаты."
        },
        disabled = if s.busy { "disabled" } else { "" },
        button = if s.busy {
            "Подключаемся…"
        } else if host {
            "Создать комнату"
        } else {
            "Войти в комнату"
        },
        discovery = if host {
            String::new()
        } else {
            format!(
                "<section id='nearby-rooms'>{}</section>",
                discovery_html(&s.discovered, s.discovery_busy)
            )
        }
    );
    drop(s);
    html("spark", &content);
    render_overlay(app);
}

fn render_sidebar(app: &AppRef) {
    let s = app.borrow();
    let Some(conn) = &s.conn else {
        return;
    };
    let mut channels = String::new();
    for kind in ["text", "voice"] {
        channels.push_str(&format!("<div class='channel-heading'><span>⌄ {} КАНАЛЫ</span><button data-action='new-channel' data-id='{kind}' title='Создать канал' data-testid='new-{kind}-channel'>+</button></div>", if kind=="text" {"ТЕКСТОВЫЕ"} else {"ГОЛОСОВЫЕ"}));
        for c in s.channels.iter().filter(|c| c.kind == kind) {
            let active = if kind == "text" {
                s.selected == c.id
            } else {
                s.media_state.channel_id.as_deref() == Some(c.id.as_str())
            };
            channels.push_str(&format!("<button class='channel {}' data-action='channel' data-id='{}' data-testid='channel-{}'><span class='channel-symbol'>{}</span><span>{}</span>{}</button>", if active {"active"} else {""},esc(&c.id),esc(&c.id),if kind=="text" {"#"} else {"◖"},esc(&c.name),if active {"<span class='channel-active'></span>"} else {""}));
            if kind == "voice" {
                for p in s
                    .peers
                    .iter()
                    .filter(|p| p.voice_channel.as_deref() == Some(c.id.as_str()))
                {
                    channels.push_str(&format!("<div class='voice-member'><span class='avatar tiny'>{}</span><span>{}</span><span>{}</span></div>",initial(&p.name),esc(&p.name),if p.muted {"♩̸"} else {""}));
                }
            }
        }
    }
    let profile = s.profile.as_ref();
    let content = format!(
        "<div class='server-heading'><div><span class='eyebrow'>ЛОКАЛЬНОЕ ПРОСТРАНСТВО</span><h2>{}</h2></div><span class='server-chevron'>⌄</span></div><div class='network-badge'><span class='status-dot {}'></span><span>{}</span></div><div class='channels'>{channels}<div class='sidebar-tip'><span>ϟ</span><p>Идеи становятся ближе,<br/>когда вы на одной волне.</p></div></div><div class='server-address'><span>⌁ {}</span><button data-action='settings' title='Адрес и настройки'>ⓘ</button></div><div class='user-bar'><span class='avatar self'>{}</span><div><strong>{}</strong><small>{}</small></div><button data-action='settings' title='Настройки' data-testid='settings'>⚙</button></div>",
        esc(&conn.server_name),
        if s.reconnecting { "pending" } else { "" },
        if s.reconnecting {
            "Переподключаемся…"
        } else if conn.hosting {
            "Вы создали эту комнату"
        } else {
            "Соединение по LAN"
        },
        esc(&conn.address),
        profile.map(|p| initial(&p.name)).unwrap_or_default(),
        profile.map(|p| esc(&p.name)).unwrap_or_default(),
        if s.media_state.channel_id.is_some() {
            "В голосовом канале"
        } else {
            "В сети"
        }
    );
    drop(s);
    html("sidebar", &content);
}
fn render_header(app: &AppRef) {
    let s = app.borrow();
    let name = s
        .channels
        .iter()
        .find(|c| c.id == s.selected)
        .map(|c| c.name.as_str())
        .unwrap_or("Чат");
    let content = format!(
        "<div class='channel-title'><span>#</span><h1>{}</h1><i></i><p>Место, где начинается разговор</p></div><div class='header-actions'><span class='connection-label'><span class='status-dot {}'></span>{}</span><button data-action='settings' title='Настройки комнаты'>ⓘ</button></div>",
        esc(name),
        if s.reconnecting { "pending" } else { "" },
        if s.reconnecting {
            "Нет соединения"
        } else {
            "В локальной сети"
        }
    );
    drop(s);
    html("chat-header", &content);
}
fn render_messages(app: &AppRef, bottom: bool) {
    let s = app.borrow();
    let old = element("message-list")
        .map(|e| (e.scroll_top(), e.scroll_height(), e.client_height()))
        .unwrap_or_default();
    let name = s
        .channels
        .iter()
        .find(|c| c.id == s.selected)
        .map(|c| c.name.as_str())
        .unwrap_or("Чат");
    let mut out = format!(
        "<div class='chat-welcome'><div class='welcome-hash'>#</div><span class='eyebrow'>НАЧАЛО КАНАЛА</span><h2>Добро пожаловать в #{}</h2><p>Здесь начинается ваша история. Напишите первое сообщение.</p></div><div class='date-divider'><span>ИСТОРИЯ СООБЩЕНИЙ</span></div><button class='history-button' data-action='history' data-testid='load-history' {}>{}</button>",
        esc(name),
        if s.history_busy { "disabled" } else { "" },
        if s.history_busy {
            "Загружаем…"
        } else {
            "↑ Загрузить более ранние сообщения"
        }
    );
    for m in s.messages.iter().filter(|m| m.channel_id == s.selected) {
        let mine = s.profile.as_ref().is_some_and(|p| p.id == m.sender_id);
        let mut attachment = String::new();
        if let Some(f) = &m.file {
            attachment = format!(
                "<button class='file-card' data-action='download' data-id='{}' data-name='{}' data-testid='file-{}'><span class='file-icon'>↧</span><span><strong>{}</strong><small>{} · SHA256: {}…</small></span><span class='download-arrow'>↓</span></button>",
                esc(&f.id),
                esc(&f.name),
                esc(&f.id),
                esc(&f.name),
                bytes(f.size),
                esc(&f.sha256.chars().take(12).collect::<String>())
            );
        }
        out.push_str(&format!("<article class='message' data-testid='message-{}'><span class='avatar {}'>{}</span><div class='message-body'><div class='message-meta'><strong>{}</strong>{}<time>{}</time></div><p>{}</p>{attachment}</div></article>",esc(&m.id),if mine {"self"} else {""},initial(&m.sender_name),esc(&m.sender_name),if mine {"<span class='you-label'>ВЫ</span>"} else {""},time(m.created_at),esc(&m.text).replace('\n',"<br/>")));
    }
    drop(s);
    html("message-list", &out);
    if let Some(e) = element("message-list") {
        if bottom || old.1 - old.0 - old.2 < 100 {
            e.set_scroll_top(e.scroll_height());
        } else {
            e.set_scroll_top(old.0 + e.scroll_height() - old.1);
        }
    }
}
fn render_composer(app: &AppRef) {
    updates::report_activity(app);
    let focused = document()
        .active_element()
        .is_some_and(|e| e.id() == "message-input");
    let selection = element("message-input")
        .and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
        .map(|e| {
            (
                e.selection_start().ok().flatten(),
                e.selection_end().ok().flatten(),
            )
        });
    let s = app.borrow();
    let name = s
        .channels
        .iter()
        .find(|c| c.id == s.selected)
        .map(|c| c.name.as_str())
        .unwrap_or("чат");
    let unavailable = s.reconnecting
        || s.selected.is_empty()
        || !s
            .ws
            .as_ref()
            .is_some_and(|ws| ws.ready_state() == WebSocket::OPEN);
    let upload=s.upload.as_ref().map(|u|format!("<div class='upload-status' data-testid='upload-progress'><span id='upload-label'>↑ {} · {}%</span><button data-action='cancel-upload' title='Отменить'>×</button><progress id='upload-meter' max='100' value='{}'></progress></div>",esc(&u.name),u.percent,u.percent)).unwrap_or_default();
    let content = format!(
        "{upload}<div class='composer'><button class='attach-button' data-action='attach' data-testid='attach-file' title='Прикрепить файл' {}>+</button><textarea id='message-input' data-testid='message-input' data-field='draft' rows='1' maxlength='8000' placeholder='Написать в #{}' {}>{}</textarea><button class='send-button' data-action='send' data-testid='send-message' title='Отправить сообщение' {}>➤</button></div><div class='composer-help'><span><kbd>Enter</kbd> отправить <span>·</span> <kbd>Shift + Enter</kbd> новая строка</span><span>Всё остаётся в вашей сети <span class='mini-spark'>ϟ</span></span></div>",
        if s.upload.is_some() || unavailable {
            "disabled"
        } else {
            ""
        },
        esc(name),
        if unavailable { "disabled" } else { "" },
        esc(&s.draft),
        if unavailable { "disabled" } else { "" }
    );
    drop(s);
    html("composer", &content);
    if focused {
        if let Some(e) =
            element("message-input").and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
        {
            e.focus().ok();
            if let Some((Some(start), Some(end))) = selection {
                e.set_selection_range(start, end).ok();
            }
        }
    }
}
fn render_members(app: &AppRef) {
    let s = app.borrow();
    let mut out = format!(
        "<div class='members-heading'>В СЕТИ <span>{}</span></div>",
        s.peers.len()
    );
    for p in &s.peers {
        let mine = s.profile.as_ref().is_some_and(|x| x.id == p.id);
        out.push_str(&format!("<div class='member'><span class='avatar {}'>{}<i></i></span><div><strong>{}{}</strong><small>{}</small></div>{}</div>",if mine {"self"} else {""},initial(&p.name),esc(&p.name),if mine {" <em>вы</em>"} else {""},if p.screen {"Демонстрирует экран"} else if p.camera {"Камера включена"} else if p.voice_channel.is_some() {"В голосовом канале"} else {"В сети"},if mine {String::new()} else {format!("<button data-action='invite' data-id='{}' title='Пригласить в звонок' data-testid='invite-{}'>◖</button>",esc(&p.id),esc(&p.id))}));
    }
    out.push_str("<div class='members-note'><span>⌁</span><strong>Вместе в одной сети</strong><p>Для общения интернет<br/>не требуется.</p></div>");
    drop(s);
    html("members", &out);
}

fn connection_label(state: &str) -> &str {
    match state {
        "connected" | "local" => "На связи",
        "failed" => "Ошибка соединения",
        "disconnected" => "Связь потеряна",
        "closed" => "Отключён",
        _ => "Подключаем…",
    }
}
fn render_voice(app: &AppRef) {
    let (media_state, channels, peers, streams, connections, profile) = {
        let s = app.borrow();
        (
            s.media_state.clone(),
            s.channels.clone(),
            s.peers.clone(),
            s.streams.clone(),
            s.connections.clone(),
            s.profile.clone(),
        )
    };
    let Some(channel) = &media_state.channel_id else {
        html("voice-panel", "");
        return;
    };
    if element("call-grid").is_none() {
        html(
            "voice-panel",
            "<div id='call-heading' class='voice-heading'></div><div id='call-grid' class='video-grid'></div><div id='call-audio'></div><div id='call-controls' class='call-controls'></div>",
        );
    }
    let name = channels
        .iter()
        .find(|c| &c.id == channel)
        .map(|c| c.name.as_str())
        .unwrap_or("Звонок");
    html(
        "call-heading",
        &format!(
            "<span class='status-dot'></span><strong>{}</strong><span>Голосовой канал</span><span class='p2p-label'>P2P · WebRTC</span>",
            esc(name)
        ),
    );
    html(
        "call-controls",
        &format!(
            "<button class='{}' data-action='mute' title='Микрофон' data-testid='mute'><span>{}</span><small>{}</small></button><button class='{}' data-action='deafen' title='Звук' data-testid='deafen'><span>♬</span><small>{}</small></button><button class='{}' data-action='camera' title='Камера' data-testid='camera'><span>▣</span><small>Камера</small></button><button class='{}' data-action='screen' title='Демонстрация экрана' data-testid='screen'><span>▱</span><small>Экран</small></button><button class='end-call' data-action='leave-voice' title='Завершить звонок' data-testid='leave-voice'><span>⌁</span><small>Выйти</small></button>",
            if media_state.muted { "off" } else { "" },
            if media_state.muted { "♩̸" } else { "♩" },
            if media_state.muted {
                "Выключен"
            } else {
                "Микрофон"
            },
            if media_state.deafened { "off" } else { "" },
            if media_state.deafened {
                "Без звука"
            } else {
                "Звук"
            },
            if media_state.camera { "on" } else { "" },
            if media_state.screen { "on" } else { "" }
        ),
    );
    // Media elements survive state/presence updates. Reassigning srcObject restarts
    // playback in Chromium, so reconcile by peer/kind and replace only changed streams.
    if let Ok(nodes) = document().query_selector_all("[data-stream-key]") {
        for i in 0..nodes.length() {
            if let Some(el) = nodes
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::Element>().ok())
            {
                let peer = el.get_attribute("data-peer").unwrap_or_default();
                let kind = el.get_attribute("data-kind").unwrap_or_default();
                if !streams.contains_key(&(peer, kind)) {
                    el.remove();
                }
            }
        }
    }
    let videos = streams.keys().any(|(_, kind)| kind != "audio");
    if videos {
        if let Some(el) = element("audio-avatars") {
            el.remove();
        }
    } else {
        if element("audio-avatars").is_none() {
            if let (Some(grid), Ok(el)) = (element("call-grid"), document().create_element("div")) {
                el.set_id("audio-avatars");
                el.set_class_name("audio-avatars");
                grid.append_child(&el).ok();
            }
        }
        let mut avatars = String::new();
        for p in peers
            .iter()
            .filter(|p| p.voice_channel.as_deref() == Some(channel.as_str()))
        {
            let state = connections.get(&p.id).map(String::as_str).unwrap_or(
                if profile.as_ref().is_some_and(|x| x.id == p.id) {
                    "local"
                } else {
                    "new"
                },
            );
            avatars.push_str(&format!("<div class='audio-person'><span class='avatar large {}'>{}</span><strong>{}</strong><small data-testid='connection-{}'>{}</small></div>",if p.muted {"muted"}else{""},initial(&p.name),esc(&p.name),esc(&p.id),connection_label(state)));
        }
        html("audio-avatars", &avatars);
    }
    for ((peer, kind), stream) in &streams {
        if kind == "audio" && peer == "local" {
            continue;
        }
        let id = format!("media-{peer}-{kind}");
        if element(&id).is_none() {
            let Ok(tile) = document().create_element(if kind == "audio" { "audio" } else { "div" })
            else {
                continue;
            };
            tile.set_attribute("data-stream-key", &format!("{peer}:{kind}"))
                .ok();
            tile.set_attribute("data-peer", peer).ok();
            tile.set_attribute("data-kind", kind).ok();
            if kind == "audio" {
                tile.set_id(&id);
                tile.set_attribute("autoplay", "").ok();
                if let Some(parent) = element("call-audio") {
                    parent.append_child(&tile).ok();
                }
            } else {
                tile.set_class_name(if kind == "screen" {
                    "video-tile screen-tile"
                } else {
                    "video-tile"
                });
                let name = if peer == "local" {
                    "Вы"
                } else {
                    peers
                        .iter()
                        .find(|p| &p.id == peer)
                        .map(|p| p.name.as_str())
                        .unwrap_or("Участник")
                };
                tile.set_inner_html(&format!("<video id='{}' autoplay playsinline {} data-testid='video-{}-{}'></video><span>{}<small>{}</small></span><small class='tile-connection' id='tile-state-{}-{}'></small><button class='video-expand' data-action='view-video' data-id='{}' data-name='{}' data-testid='expand-{}-{}' title='Развернуть видео' aria-label='Развернуть видео'>⛶</button>",esc(&id),if peer=="local"{"muted"}else{""},esc(peer),esc(kind),esc(name),if kind=="screen"{" · Экран"}else{""},esc(peer),esc(kind),esc(peer),esc(kind),esc(peer),esc(kind)));
                if let Some(parent) = element("call-grid") {
                    parent.append_child(&tile).ok();
                }
            }
        }
        if let Some(el) = element(&format!("tile-state-{peer}-{kind}")) {
            let state = if peer == "local" {
                "local"
            } else {
                connections.get(peer).map(String::as_str).unwrap_or("new")
            };
            el.set_text_content(Some(connection_label(state)));
            // A peer can have camera and screen: one connection test marker per tile.
            el.set_attribute("data-testid", &format!("connection-{peer}-{kind}"))
                .ok();
        }
        if let Some(el) = element(&id).and_then(|e| e.dyn_into::<web_sys::HtmlMediaElement>().ok())
        {
            if peer == "local" {
                el.set_muted(true);
            }
            if el.src_object().as_ref().map(|current| current.id()) != Some(stream.id()) {
                el.set_src_object(Some(stream));
                if let Ok(p) = el.play() {
                    let weak = Rc::downgrade(app);
                    let el = el.clone();
                    spawn_local(async move {
                        if let Err(e) = JsFuture::from(p).await {
                            let name = Reflect::get(&e, &JsValue::from_str("name"))
                                .ok()
                                .and_then(|v| v.as_string())
                                .unwrap_or_default();
                            if name == "NotAllowedError" && el.is_connected() {
                                if let Some(a) = weak.upgrade() {
                                    a.borrow_mut().notice = Some(
                                        "Браузер приостановил звук. Нажмите «Звук» в звонке."
                                            .into(),
                                    );
                                    render_overlay(&a);
                                }
                            }
                        }
                    });
                }
            }
        }
    }
}

fn render_overlay(app: &AppRef) {
    let update_controls = updates::controls(app);
    let s = app.borrow();
    let viewer = if s.invite.is_none() && s.modal.as_deref() == Some("viewer") {
        s.expanded_stream.as_ref().and_then(|(peer, kind)| {
            s.streams.get(&(peer.clone(), kind.clone())).map(|stream| {
                // Preserve the same JS stream/track identity. MediaStream's
                // inherent clone() would create additional captured tracks.
                (peer.clone(), kind.clone(), Clone::clone(stream))
            })
        })
    } else {
        None
    };
    let mut out = String::new();
    if s.update.state == "ready"
        && s.modal.as_deref() != Some("settings")
        && s.modal.as_deref() != Some("viewer")
    {
        out.push_str(&format!("<div class='toast update-ready' role='status'><span>↓</span><p>Готово обновление Spark {}</p><button data-action='settings'>Подробнее</button></div>",esc(s.update.version.as_deref().unwrap_or(""))));
    }
    if let Some(e) = &s.error {
        out.push_str(&format!("<div class='toast error' role='alert' data-testid='error'><span>!</span><p>{}</p><button data-action='dismiss-error'>×</button></div>",esc(e)));
    }
    if let Some(n) = &s.notice {
        out.push_str(&format!("<div class='toast notice' role='status'><span>✓</span><p>{}</p><button data-action='dismiss-notice'>×</button></div>",esc(n)));
    }
    if let Some(inv) = &s.invite {
        let channel = s
            .channels
            .iter()
            .find(|c| c.id == inv.channel_id)
            .map(|c| c.name.as_str())
            .unwrap_or("Звонок");
        out.push_str(&format!("<div class='modal-shade'><section class='modal invite-modal'><span class='avatar large'>{}</span><h2>{} приглашает в звонок</h2><p>{} · {}</p><p class='field-help'>Микрофон{} включится после принятия.</p><div class='modal-actions'><button data-action='reject-invite' data-testid='reject-invite'>Отклонить</button><button class='primary' data-action='accept-invite' data-testid='accept-invite'>Принять</button></div></section></div>",initial(&inv.from.name),esc(&inv.from.name),esc(channel),if inv.video {"Видеозвонок"} else {"Голосовой звонок"},if inv.video {" и камера"} else {""}));
    } else if let Some(modal) = &s.modal {
        let content=match modal.as_str() {
            "viewer"=>viewer.as_ref().map(|(peer,kind,_)|{
                let name=if peer=="local"{"Вы"}else{s.peers.iter().find(|p|&p.id==peer).map(|p|p.name.as_str()).unwrap_or("Участник")};
                format!("<div class='viewer-heading'><span>{}</span><h2>{}</h2><button data-action='fullscreen-video' data-testid='fullscreen-video' title='На весь экран'>⛶ На весь экран</button><small>Esc — назад</small></div><video id='expanded-video' data-testid='expanded-video' autoplay playsinline muted></video>",if kind=="screen"{"ДЕМОНСТРАЦИЯ ЭКРАНА"}else{"КАМЕРА"},esc(name))
            }).unwrap_or_default(),
            "channel"=>format!("<span class='eyebrow'>ВАШЕ ПРОСТРАНСТВО</span><h2>Создать канал</h2><label class='field'>Название<input id='channel-name' data-field='channel-name' data-testid='channel-name' maxlength='64' value='{}' placeholder='Например: идеи'/></label><label class='field'>Тип канала<select data-field='channel-kind' data-testid='channel-kind'><option value='text' {}>Текстовый</option><option value='voice' {}>Голосовой</option></select></label><div class='modal-actions'><button data-action='close-modal'>Отмена</button><button class='primary' data-action='create-channel' data-testid='create-channel'>Создать</button></div>",esc(&s.form.channel_name),if s.form.channel_kind=="text" {"selected"} else {""},if s.form.channel_kind=="voice" {"selected"} else {""}),
            "settings"=>format!("<span class='eyebrow'>SPARK · {}</span><h2>Настройки</h2><label class='field'>Ваше имя<input data-field='settings-name' data-testid='settings-name' maxlength='64' value='{}'/></label>{}<div class='settings-info'><strong>Локальная комната</strong><p>{}</p><code>{}</code><small>{}</small></div><div class='modal-actions'>{}<button class='primary' data-action='save-settings' data-testid='save-settings'>Сохранить имя</button></div>",esc(&s.version),esc(&s.form.settings_name),format!("{}{}",preferences::controls(&s.preferences),update_controls),s.conn.as_ref().map(|c|esc(&c.server_name)).unwrap_or_else(||"Нет подключения".into()),s.conn.as_ref().map(|c|esc(&c.address)).unwrap_or_default(),if s.conn.as_ref().is_some_and(|c|c.hosting) {"Вы — создатель. При выходе комната остановится для всех."} else {"Настройки оформления действуют на этом компьютере."},if s.conn.is_some(){format!("<button class='danger-text' data-action='disconnect' data-testid='disconnect'>{}</button>",if s.conn.as_ref().is_some_and(|c|c.hosting){"Остановить комнату"}else{"Выйти из комнаты"})}else{String::new()}),
            "sources"=>{let mut x="<span class='eyebrow'>ДЕМОНСТРАЦИЯ ЭКРАНА</span><h2>Что показать команде?</h2><p>Выберите экран или отдельное окно.</p><div class='sources'>".to_string();for src in &s.sources { x.push_str(&format!("<button class='source' data-action='select-source' data-id='{}' data-testid='source-{}'><img src='{}' alt=''/><strong>{}</strong></button>",esc(&src.id),esc(&src.id),esc(&src.thumbnail),esc(&src.name))); }x.push_str("</div><div class='modal-actions'><button data-action='close-modal'>Отмена</button></div>");x},
            _=>"<div class='brand-mark'>ϟ</div><h2>Spark</h2><p>Общение рядом. Чаты, голос, видео и файлы в вашей локальной сети.</p><div class='modal-actions'><button class='primary' data-action='close-modal'>Понятно</button></div>".into()
        };
        out.push_str(&format!("<div class='modal-shade'><section class='modal {}'><button class='modal-close' data-action='close-modal' data-testid='{}' title='Закрыть'>×</button>{content}</section></div>",if modal=="viewer"{"video-viewer"}else if modal=="settings"{"settings-modal"}else{""},if modal=="viewer"{"close-video"}else{"close-modal"}));
    }
    drop(s);
    if viewer.is_none() && document().fullscreen_element().is_some() {
        document().exit_fullscreen();
    }
    html("overlay", &out);
    if let Some((_, _, stream)) = viewer {
        if let Some(video) =
            element("expanded-video").and_then(|el| el.dyn_into::<web_sys::HtmlMediaElement>().ok())
        {
            video.set_muted(true);
            video.set_src_object(Some(&stream));
            if let Ok(play) = video.play() {
                spawn_local(async move {
                    let _ = JsFuture::from(play).await;
                });
            }
        }
    }
}

fn install_events(app: &AppRef) {
    let root = element("spark").expect("root");
    let a = app.clone();
    let click = Closure::<dyn FnMut(Event)>::new(move |ev: Event| {
        let Some(target) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        if target.closest("#voice-panel").ok().flatten().is_some() {
            resume_media();
        }
        let Ok(Some(el)) = target.closest("[data-action]") else {
            return;
        };
        if el.has_attribute("disabled") {
            return;
        }
        ev.prevent_default();
        let action = el.get_attribute("data-action").unwrap_or_default();
        let id = el.get_attribute("data-id").unwrap_or_default();
        action_handler(
            &a,
            &action,
            &id,
            el.get_attribute("data-name").as_deref().unwrap_or(""),
        );
    });
    root.add_event_listener_with_callback("click", click.as_ref().unchecked_ref())
        .expect("click");
    click.forget();
    let a = app.clone();
    let input = Closure::<dyn FnMut(Event)>::new(move |ev: Event| {
        let Some(target) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
        else {
            return;
        };
        let value = Reflect::get(&target, &JsValue::from_str("value"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        if let Some(field) = target.get_attribute("data-field") {
            if field.starts_with("pref-") {
                let checked = target
                    .dyn_ref::<HtmlInputElement>()
                    .is_some_and(|i| i.checked());
                preferences::input(&a, &field, &value, checked);
                return;
            }
            let mut s = a.borrow_mut();
            match field.as_str() {
                "name" => s.form.name = value,
                "room" => s.form.room = value,
                "code" => s.form.code = value,
                "address" => s.form.address = value,
                "draft" => s.draft = value,
                "channel-name" => s.form.channel_name = value,
                "channel-kind" => s.form.channel_kind = value,
                "settings-name" => s.form.settings_name = value,
                _ => {}
            }
        }
        if target.id() == "message-input" {
            if let Ok(t) = target.dyn_into::<HtmlTextAreaElement>() {
                let _ = t.set_attribute("style", "height: auto");
                let _ = t.set_attribute(
                    "style",
                    &format!("height: {}px", t.scroll_height().clamp(24, 160)),
                );
            }
        }
    });
    root.add_event_listener_with_callback("input", input.as_ref().unchecked_ref())
        .expect("input");
    // Chromium fires `change` synchronously when a focused input is detached.
    // `input` already covers typing, paste and selects, without re-entering App
    // while a renderer replaces a subtree. File selection has its own listener.
    input.forget();
    let a = app.clone();
    let key =
        Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(move |ev: web_sys::KeyboardEvent| {
            let id = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .map(|t| t.id())
                .unwrap_or_default();
            if ev.key() == "Enter" && !ev.shift_key() && !ev.is_composing() && id == "message-input"
            {
                ev.prevent_default();
                send_chat(&a);
            }
            if (ev.ctrl_key() || ev.meta_key())
                && matches!(ev.key().as_str(), "+" | "=" | "-" | "0")
            {
                ev.prevent_default();
                {
                    let mut s = a.borrow_mut();
                    s.preferences.zoom = match ev.key().as_str() {
                        "0" => 100,
                        "-" => s.preferences.zoom.saturating_sub(5).max(75),
                        _ => (s.preferences.zoom + 5).min(150),
                    };
                }
                preferences::persist(&a);
                if a.borrow().modal.as_deref() == Some("settings") {
                    render_overlay(&a);
                }
            }
            if ev.key() == "Escape" {
                if document().fullscreen_element().is_some() {
                    document().exit_fullscreen();
                }
                let mut s = a.borrow_mut();
                s.modal = None;
                s.expanded_stream = None;
                drop(s);
                render_overlay(&a);
            }
        });
    document()
        .add_event_listener_with_callback("keydown", key.as_ref().unchecked_ref())
        .expect("keyboard");
    key.forget();
    let a = app.clone();
    let change = Closure::<dyn FnMut(Event)>::new(move |ev: Event| {
        let Some(input) = ev
            .target()
            .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
        else {
            return;
        };
        if input.id() == "file-input" {
            if let Some(file) = input.files().and_then(|files| files.get(0)) {
                start_upload(&a, file);
            }
            input.set_value("");
        }
    });
    root.add_event_listener_with_callback("change", change.as_ref().unchecked_ref())
        .expect("files");
    change.forget();
    let a = app.clone();
    let unload = Closure::<dyn FnMut(Event)>::new(move |_| {
        let m = a.borrow().media.clone();
        if let Some(m) = m {
            m.leave();
        }
    });
    window()
        .add_event_listener_with_callback("beforeunload", unload.as_ref().unchecked_ref())
        .ok();
    unload.forget();
}

fn action_handler(app: &AppRef, action: &str, id: &str, name: &str) {
    if app.borrow().update_installing {
        return;
    }
    match action {
        "reload" => {
            let _ = window().location().reload();
        }
        "mode-join" | "mode-host" => {
            app.borrow_mut().form.mode = if action == "mode-host" {
                "host"
            } else {
                "join"
            }
            .into();
            render_onboarding(app);
            if action == "mode-join" {
                discovery(app);
            }
        }
        "refresh" => discovery(app),
        "select-host" => {
            app.borrow_mut().form.address = id.into();
            render_onboarding(app);
            if let Some(e) =
                element("room-code").and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            {
                e.focus().ok();
            }
        }
        "connect" => connect(app),
        "send" => send_chat(app),
        "channel" => {
            let c = app.borrow().channels.iter().find(|c| c.id == id).cloned();
            if let Some(c) = c {
                if c.kind == "voice" {
                    join_voice(app, c.id, false);
                } else {
                    app.borrow_mut().selected = c.id;
                    render_sidebar(app);
                    render_header(app);
                    render_messages(app, true);
                    render_composer(app);
                }
            }
        }
        "new-channel" => {
            let mut s = app.borrow_mut();
            s.form.channel_kind = id.into();
            s.form.channel_name.clear();
            s.modal = Some("channel".into());
            drop(s);
            render_overlay(app);
            if let Some(el) =
                element("channel-name").and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
            {
                el.focus().ok();
            }
        }
        "create-channel" => {
            let s = app.borrow();
            let name = s.form.channel_name.trim().to_owned();
            let channel_kind = s.form.channel_kind.clone();
            drop(s);
            if name.is_empty() {
                error(app, "Введите название канала.");
                return;
            }
            if send(app, ClientCommand::CreateChannel { name, channel_kind }) {
                app.borrow_mut().modal = None;
                render_overlay(app);
            }
        }
        "history" => {
            let mut s = app.borrow_mut();
            if s.history_busy {
                return;
            }
            let channel_id = s.selected.clone();
            let before = s
                .messages
                .iter()
                .filter(|m| m.channel_id == channel_id)
                .map(|m| m.created_at)
                .min();
            s.history_busy = true;
            drop(s);
            if !send(app, ClientCommand::History { channel_id, before }) {
                app.borrow_mut().history_busy = false;
            }
            render_messages(app, false);
        }
        "attach" => {
            if let Some(input) =
                element("file-input").and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
            {
                input.click();
            }
        }
        "cancel-upload" => {
            let xhr = app.borrow().upload_request.as_ref().map(|r| r.xhr.clone());
            if let Some(xhr) = xhr {
                let _ = xhr.abort();
            }
        }
        "download" => download(app, id.to_owned(), name.to_owned()),
        "settings" => {
            let mut s = app.borrow_mut();
            s.form.settings_name = s
                .profile
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_default();
            s.modal = Some("settings".into());
            drop(s);
            render_overlay(app);
        }
        "reset-zoom" => {
            app.borrow_mut().preferences.zoom = 100;
            preferences::persist(app);
            render_overlay(app);
        }
        "preview-sound" => preferences::sound(app, id),
        "quit" => {
            let a = app.clone();
            spawn_local(async move {
                if let Err(e) = desktop_call("quit", &[]).await {
                    error(&a, e);
                }
            });
        }
        "fullscreen-video" => {
            if document().fullscreen_element().is_some() {
                document().exit_fullscreen();
            } else if let Some(root) = document().document_element() {
                let result = Reflect::get(&root, &JsValue::from_str("requestFullscreen"))
                    .and_then(|f| f.dyn_into::<Function>())
                    .and_then(|f| f.call0(&root));
                match result {
                    Ok(p) => {
                        let a = app.clone();
                        spawn_local(async move {
                            if let Err(e) = JsFuture::from(Promise::resolve(&p)).await {
                                error(&a, err_text(e));
                            }
                        });
                    }
                    Err(e) => error(app, err_text(e)),
                }
            }
        }
        "check-updates" => updates::action(app, false),
        "install-update" => updates::action(app, true),
        "save-settings" => save_settings(app),
        "disconnect" => disconnect(app),
        "close-modal" => {
            if document().fullscreen_element().is_some() {
                document().exit_fullscreen();
            }
            let mut s = app.borrow_mut();
            s.modal = None;
            s.expanded_stream = None;
            drop(s);
            render_overlay(app);
        }
        "view-video" => {
            let key = (id.to_owned(), name.to_owned());
            let mut s = app.borrow_mut();
            if s.streams.contains_key(&key) && matches!(name, "camera" | "screen") {
                s.expanded_stream = Some(key);
                s.modal = Some("viewer".into());
                drop(s);
                render_overlay(app);
            }
        }
        "about" => {
            app.borrow_mut().modal = Some("about".into());
            render_overlay(app);
        }
        "dismiss-error" => {
            app.borrow_mut().error = None;
            render_overlay(app);
        }
        "dismiss-notice" => {
            app.borrow_mut().notice = None;
            render_overlay(app);
        }
        "mute" => {
            let (m, value) = {
                let s = app.borrow();
                (s.media.clone(), !s.media_state.muted)
            };
            if let Some(m) = m {
                m.set_muted(value);
            }
        }
        "deafen" => {
            let (m, value) = {
                let s = app.borrow();
                (s.media.clone(), !s.media_state.deafened)
            };
            if let Some(m) = m {
                m.set_deafened(value);
            }
        }
        "leave-voice" => {
            let m = {
                let mut s = app.borrow_mut();
                s.voice_generation = s.voice_generation.wrapping_add(1);
                s.media.clone()
            };
            if let Some(m) = m {
                m.leave();
            }
            send(app, ClientCommand::LeaveVoice);
        }
        "camera" => {
            let m = app.borrow().media.clone();
            let a = app.clone();
            if let Some(m) = m {
                spawn_local(async move {
                    if let Err(e) = m.toggle_camera().await {
                        error(&a, e);
                    }
                });
            }
        }
        "screen" => screen_picker(app),
        "select-source" => select_source(app, id.to_owned()),
        "invite" => {
            let s = app.borrow();
            let channel = s.media_state.channel_id.clone().or_else(|| {
                s.channels
                    .iter()
                    .find(|c| c.kind == "voice")
                    .map(|c| c.id.clone())
            });
            drop(s);
            if let Some(channel_id) = channel {
                if send(
                    app,
                    ClientCommand::Invite {
                        to: id.into(),
                        channel_id,
                        video: false,
                    },
                ) {
                    app.borrow_mut().notice = Some("Приглашение в звонок отправлено.".into());
                    render_overlay(app);
                }
            } else {
                error(app, "Сначала создайте голосовой канал.");
            }
        }
        "reject-invite" => {
            app.borrow_mut().invite = None;
            render_overlay(app);
        }
        "accept-invite" => {
            let inv = app.borrow_mut().invite.take();
            render_overlay(app);
            if let Some(inv) = inv {
                join_voice(app, inv.channel_id, inv.video);
            }
        }
        _ => {}
    }
}

fn resume_media() {
    if let Ok(nodes) = document().query_selector_all("#voice-panel audio,#voice-panel video") {
        for i in 0..nodes.length() {
            if let Some(el) = nodes
                .item(i)
                .and_then(|n| n.dyn_into::<web_sys::HtmlMediaElement>().ok())
            {
                if el.paused() {
                    if let Ok(p) = el.play() {
                        spawn_local(async move {
                            let _ = JsFuture::from(p).await;
                        });
                    }
                }
            }
        }
    }
}

fn discovery_html(hosts: &[Discovery], busy: bool) -> String {
    let mut out = format!(
        "<div class='discovery-title'><span>{}</span><button data-action='refresh' title='Обновить комнаты' data-testid='refresh-discovery' aria-label='Обновить комнаты'>↻</button></div>",
        if busy {
            "ИЩЕМ КОМНАТЫ ПОБЛИЗОСТИ…"
        } else {
            "КОМНАТЫ ПОБЛИЗОСТИ"
        }
    );
    for h in hosts {
        out.push_str(&format!("<button class='discovery-item' data-action='select-host' data-id='{}'><span class='discovery-icon'>⌁</span><span><strong>{}</strong><small>{} · участников: {}</small></span><span>↗</span></button>",esc(&h.address),esc(&h.name),esc(&h.address),h.peers));
    }
    if hosts.is_empty() {
        out.push_str("<div class='discovery-empty'><span class='signal-icon'>⌁</span>Пока нет доступных комнат<small>Создайте свою или введите адрес вручную</small></div>");
    }
    out
}
fn render_discovery(app: &AppRef) {
    let content = {
        let s = app.borrow();
        discovery_html(&s.discovered, s.discovery_busy)
    };
    // Discovery is independent from the editable form. A response must never
    // detach a name/code/address field while the user is typing in it.
    html("nearby-rooms", &content);
}
fn discovery(app: &AppRef) {
    let (token, generation) = {
        let mut s = app.borrow_mut();
        if s.conn.is_some() {
            return;
        }
        s.discovery_generation = s.discovery_generation.wrapping_add(1);
        s.discovery_busy = true;
        (s.token.clone(), s.discovery_generation)
    };
    render_discovery(app);
    let a = app.clone();
    spawn_local(async move {
        let result = api::<Vec<Discovery>>(&token, "GET", "/api/discovery", None).await;
        if a.borrow().discovery_generation != generation {
            return;
        }
        a.borrow_mut().discovery_busy = false;
        match result {
            Ok(hosts) => {
                a.borrow_mut().discovered = hosts;
                render_discovery(&a);
            }
            Err(e) => {
                render_discovery(&a);
                error(&a, e);
            }
        }
        let weak = Rc::downgrade(&a);
        let refresh = Closure::once_into_js(move || {
            if let Some(a) = weak.upgrade() {
                let should_refresh = {
                    let s = a.borrow();
                    s.conn.is_none()
                        && s.form.mode == "join"
                        && s.discovery_generation == generation
                };
                if should_refresh {
                    discovery(&a);
                }
            }
        });
        window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(refresh.unchecked_ref(), 4000)
            .ok();
    });
}
fn capture_onboarding_fields(app: &AppRef) {
    let mut values = Vec::new();
    for field in ["name", "room", "code", "address"] {
        if let Ok(Some(el)) =
            document().query_selector(&format!("#spark input[data-field='{field}']"))
        {
            if let Ok(input) = el.dyn_into::<HtmlInputElement>() {
                values.push((field, input.value()));
            }
        }
    }
    let mut s = app.borrow_mut();
    for (field, value) in values {
        match field {
            "name" => s.form.name = value,
            "room" => s.form.room = value,
            "code" => s.form.code = value,
            "address" => s.form.address = value,
            _ => {}
        }
    }
}

fn connect(app: &AppRef) {
    if app.borrow().update_installing {
        return;
    }
    capture_onboarding_fields(app);
    let mut s = app.borrow_mut();
    if s.busy {
        return;
    }
    let name = s.form.name.trim().to_owned();
    let code = s.form.code.clone();
    let host = s.form.mode == "host";
    let room = s.form.room.trim().to_owned();
    let address = s.form.address.trim().to_owned();
    let token = s.token.clone();
    if name.is_empty() {
        drop(s);
        error(app, "Укажите имя, под которым вас увидят участники.");
        return;
    }
    if code.chars().count() < 6 {
        drop(s);
        error(app, "Код комнаты должен содержать не менее 6 символов.");
        return;
    }
    if host && room.is_empty() {
        drop(s);
        error(app, "Введите название комнаты.");
        return;
    }
    if !host && address.is_empty() {
        drop(s);
        error(
            app,
            "Введите адрес комнаты или выберите найденную поблизости.",
        );
        return;
    }
    s.busy = true;
    s.error = None;
    drop(s);
    render_onboarding(app);
    updates::report_activity(app);
    let a = app.clone();
    spawn_local(async move {
        let result = async {
            let profile =
                api::<Profile>(&token, "PUT", "/api/profile", Some(json!({"name":name}))).await?;
            a.borrow_mut().profile = Some(profile);
            let connection = if host {
                api::<ConnectionInfo>(
                    &token,
                    "POST",
                    "/api/host",
                    Some(json!({"name":room,"code":code})),
                )
                .await?
            } else {
                api::<ConnectionInfo>(
                    &token,
                    "POST",
                    "/api/join",
                    Some(json!({"address":address,"code":code})),
                )
                .await?
            };
            a.borrow_mut().conn = Some(connection);
            Ok::<(), String>(())
        }
        .await;
        a.borrow_mut().busy = false;
        match result {
            Ok(()) => {
                render(&a);
                open_socket(&a);
            }
            Err(e) => {
                render_onboarding(&a);
                error(&a, e);
            }
        }
    });
}
fn send_chat(app: &AppRef) {
    if app.borrow().update_installing {
        return;
    }
    let s = app.borrow();
    let text = s.draft.trim().to_owned();
    let channel_id = s.selected.clone();
    drop(s);
    if text.is_empty() || channel_id.is_empty() {
        return;
    }
    if send(app, ClientCommand::Chat { channel_id, text }) {
        app.borrow_mut().draft.clear();
        if let Some(input) =
            element("message-input").and_then(|e| e.dyn_into::<HtmlTextAreaElement>().ok())
        {
            input.set_value("");
            input.set_attribute("style", "height:24px").ok();
            input.focus().ok();
        }
    }
}

fn open_socket(app: &AppRef) {
    let mut s = app.borrow_mut();
    if s.conn.is_none() {
        return;
    }
    s.generation += 1;
    let generation = s.generation;
    let location = window().location();
    let host = location.host().unwrap_or_default();
    let scheme = if location.protocol().unwrap_or_default() == "https:" {
        "wss"
    } else {
        "ws"
    };
    let ws = match WebSocket::new(&format!(
        "{scheme}://{host}/api/events?token={}",
        encode(&s.token)
    )) {
        Ok(ws) => ws,
        Err(e) => {
            drop(s);
            error(app, err_text(e));
            return;
        }
    };
    s.ws = Some(ws.clone());
    drop(s);
    let weak = Rc::downgrade(app);
    let message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let Some(a) = weak.upgrade() else { return };
        if a.borrow().generation != generation {
            return;
        }
        if let Some(text) = event.data().as_string() {
            match serde_json::from_str::<ServerEvent>(&text) {
                Ok(event) => server_event(&a, event),
                Err(_) => error(&a, "Не удалось прочитать событие комнаты."),
            }
        }
    });
    ws.set_onmessage(Some(message.as_ref().unchecked_ref()));
    message.forget();
    let weak = Rc::downgrade(app);
    let close = Closure::<dyn FnMut(Event)>::new(move |_| {
        let Some(a) = weak.upgrade() else { return };
        let mut s = a.borrow_mut();
        if s.generation != generation || s.conn.is_none() {
            return;
        }
        let was_reconnecting = s.reconnecting;
        s.ws = None;
        s.reconnecting = true;
        s.voice_generation = s.voice_generation.wrapping_add(1);
        let media = s.media.clone();
        drop(s);
        if let Some(m) = media {
            m.leave();
        }
        if !was_reconnecting {
            preferences::sound(&a, "disconnect");
        }
        render_sidebar(&a);
        render_header(&a);
        render_composer(&a);
        let weak = Rc::downgrade(&a);
        let retry = Closure::once_into_js(move || {
            if let Some(a) = weak.upgrade() {
                if a.borrow().generation == generation && a.borrow().conn.is_some() {
                    open_socket(&a);
                }
            }
        });
        window()
            .set_timeout_with_callback_and_timeout_and_arguments_0(retry.unchecked_ref(), 2000)
            .ok();
    });
    ws.set_onclose(Some(close.as_ref().unchecked_ref()));
    close.forget();
    let weak = Rc::downgrade(app);
    let failure = Closure::<dyn FnMut(Event)>::new(move |_| {
        if let Some(a) = weak.upgrade() {
            if a.borrow().generation == generation {
                a.borrow_mut().reconnecting = true;
                render_sidebar(&a);
                render_header(&a);
            }
        }
    });
    ws.set_onerror(Some(failure.as_ref().unchecked_ref()));
    failure.forget();
}

fn server_event(app: &AppRef, event: ServerEvent) {
    let peer_cue = {
        let s = app.borrow();
        match &event {
            ServerEvent::PeerJoined { peer }
                if s.profile.as_ref().is_some_and(|p| p.id != peer.id) =>
            {
                Some("connect")
            }
            ServerEvent::PeerLeft { peer_id } if s.peers.iter().any(|p| &p.id == peer_id) => {
                Some("disconnect")
            }
            ServerEvent::Presence { peer }
                if s.profile.as_ref().is_some_and(|p| p.id != peer.id) =>
            {
                let old = s
                    .peers
                    .iter()
                    .find(|p| p.id == peer.id)
                    .and_then(|p| p.voice_channel.as_ref());
                let current = s.media_state.channel_id.as_ref();
                if current.is_some() && old != peer.voice_channel.as_ref() {
                    if peer.voice_channel.as_ref() == current {
                        Some("connect")
                    } else if old == current {
                        Some("disconnect")
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    };
    if let Some(cue) = peer_cue {
        preferences::sound(app, cue);
    }
    match event {
        ServerEvent::Welcome {
            self_id,
            server_name,
            channels,
            peers,
            messages,
        } => {
            let mut s = app.borrow_mut();
            s.reconnecting = false;
            s.channels = channels;
            s.peers = peers;
            s.messages = messages;
            s.history_busy = false;
            if let Some(c) = &mut s.conn {
                c.server_name = server_name;
            }
            if let Some(p) = &mut s.profile {
                p.id = self_id.clone();
            }
            if !s
                .channels
                .iter()
                .any(|c| c.id == s.selected && c.kind == "text")
            {
                s.selected = s
                    .channels
                    .iter()
                    .find(|c| c.kind == "text")
                    .map(|c| c.id.clone())
                    .unwrap_or_default();
            }
            s.voice_generation = s.voice_generation.wrapping_add(1);
            let previous = s.media.take();
            drop(s);
            if let Some(m) = previous {
                m.leave();
            }
            preferences::sound(app, "connect");
            install_media(app, self_id);
            render(app);
        }
        ServerEvent::PeerJoined { peer } | ServerEvent::Presence { peer } => {
            let mut s = app.borrow_mut();
            if let Some(old) = s.peers.iter_mut().find(|p| p.id == peer.id) {
                *old = peer;
            } else {
                s.peers.push(peer);
            }
            let peers = s.peers.clone();
            let m = s.media.clone();
            drop(s);
            if let Some(m) = m {
                m.sync_peers(peers);
            }
            render_sidebar(app);
            render_members(app);
            render_voice(app);
        }
        ServerEvent::PeerLeft { peer_id } => {
            let mut s = app.borrow_mut();
            s.peers.retain(|p| p.id != peer_id);
            s.connections.remove(&peer_id);
            s.streams.retain(|(p, _), _| p != &peer_id);
            let peers = s.peers.clone();
            let m = s.media.clone();
            drop(s);
            if let Some(m) = m {
                m.sync_peers(peers);
            }
            render_sidebar(app);
            render_members(app);
            render_voice(app);
        }
        ServerEvent::ChannelCreated { channel } => {
            let mut s = app.borrow_mut();
            if !s.channels.iter().any(|c| c.id == channel.id) {
                s.channels.push(channel);
            }
            drop(s);
            render_sidebar(app);
        }
        ServerEvent::Chat { message } => {
            let mut s = app.borrow_mut();
            let selected = message.channel_id == s.selected;
            let notify = s
                .profile
                .as_ref()
                .is_some_and(|p| p.id != message.sender_id)
                && !s.messages.iter().any(|m| m.id == message.id);
            if !s.messages.iter().any(|m| m.id == message.id) {
                s.messages.push(message);
            }
            drop(s);
            if notify {
                preferences::sound(app, "notification");
            }
            if selected {
                render_messages(app, true);
            }
        }
        ServerEvent::History { messages, .. } => {
            let mut s = app.borrow_mut();
            s.history_busy = false;
            if messages.is_empty() {
                s.notice = Some("Более ранних сообщений нет.".into());
            }
            for m in messages {
                if !s.messages.iter().any(|old| old.id == m.id) {
                    s.messages.push(m);
                }
            }
            s.messages.sort_by_key(|m| m.created_at);
            drop(s);
            render_messages(app, false);
            render_overlay(app);
        }
        ServerEvent::Signal { from, payload } => {
            let m = app.borrow().media.clone();
            if let Some(m) = m {
                m.receive_signal(from, payload);
            }
        }
        ServerEvent::Invite {
            from,
            channel_id,
            video,
        } => {
            preferences::sound(app, "invite");
            app.borrow_mut().invite = Some(Invite {
                from,
                channel_id,
                video,
            });
            render_overlay(app);
        }
        ServerEvent::Error { message, .. } => {
            let history_busy = app.borrow().history_busy;
            app.borrow_mut().history_busy = false;
            if history_busy {
                render_messages(app, false);
            }
            error(app, message);
        }
        ServerEvent::Disconnected { reason } => {
            error(app, format!("Соединение прервано: {reason}"));
            let ws = app.borrow().ws.clone();
            if let Some(ws) = ws {
                ws.close().ok();
            }
        }
    }
}

fn install_media(app: &AppRef, self_id: String) {
    let weak = Rc::downgrade(app);
    let on_signal = Rc::new(move |to: String, payload: Value| {
        if let Some(a) = weak.upgrade() {
            send(&a, ClientCommand::Signal { to, payload });
        }
    });
    let weak = Rc::downgrade(app);
    let on_state = Rc::new(move |state: MediaState| {
        if let Some(a) = weak.upgrade() {
            let cue = {
                let s = a.borrow();
                let old = &s.media_state;
                if old.channel_id != state.channel_id {
                    Some(if state.channel_id.is_some() {
                        "connect"
                    } else {
                        "disconnect"
                    })
                } else if old.deafened != state.deafened {
                    Some(if state.deafened {
                        "sound-off"
                    } else {
                        "sound-on"
                    })
                } else if old.muted != state.muted {
                    Some(if state.muted { "mic-off" } else { "mic-on" })
                } else {
                    None
                }
            };
            let close_viewer = {
                let mut s = a.borrow_mut();
                s.media_state = state.clone();
                let close = state.channel_id.is_none() && s.modal.as_deref() == Some("viewer");
                if close {
                    s.expanded_stream = None;
                    s.modal = None;
                }
                close
            };
            if let Some(cue) = cue {
                preferences::sound(&a, cue);
            }
            updates::report_activity(&a);
            if close_viewer && document().fullscreen_element().is_some() {
                document().exit_fullscreen();
            }
            let connected = {
                let s = a.borrow();
                s.conn.is_some()
                    && s.ws
                        .as_ref()
                        .is_some_and(|w| w.ready_state() == WebSocket::OPEN)
            };
            if connected {
                send(
                    &a,
                    ClientCommand::MediaState {
                        muted: state.muted,
                        camera: state.camera,
                        screen: state.screen,
                    },
                );
            }
            render_sidebar(&a);
            render_voice(&a);
            if close_viewer {
                render_overlay(&a);
            }
        }
    });
    let weak = Rc::downgrade(app);
    let on_stream = Rc::new(
        move |peer: String, kind: String, stream: Option<MediaStream>| {
            if let Some(a) = weak.upgrade() {
                let mut s = a.borrow_mut();
                let key = (peer, kind);
                let mut viewer_changed = s.modal.as_deref() == Some("viewer")
                    && s.expanded_stream.as_ref() == Some(&key)
                    && s.streams.get(&key).map(MediaStream::id)
                        != stream.as_ref().map(MediaStream::id);
                if let Some(stream) = stream {
                    s.streams.insert(key, stream);
                } else {
                    s.streams.remove(&key);
                    if s.expanded_stream.as_ref() == Some(&key) {
                        viewer_changed = true;
                        s.expanded_stream = None;
                        if s.modal.as_deref() == Some("viewer") {
                            s.modal = None;
                        }
                    }
                }
                drop(s);
                render_voice(&a);
                if viewer_changed {
                    render_overlay(&a);
                }
            }
        },
    );
    let weak = Rc::downgrade(app);
    let on_connection = Rc::new(move |peer: String, state: String| {
        if let Some(a) = weak.upgrade() {
            a.borrow_mut().connections.insert(peer, state);
            render_voice(&a);
        }
    });
    let weak = Rc::downgrade(app);
    let on_error = Rc::new(move |message: String| {
        if let Some(a) = weak.upgrade() {
            error(&a, message);
        }
    });
    let manager = MediaManager::new(
        self_id,
        MediaCallbacks {
            on_signal,
            on_state,
            on_stream,
            on_error,
            on_connection,
        },
    );
    app.borrow_mut().media = Some(manager);
}
fn join_voice(app: &AppRef, channel_id: String, video: bool) {
    if app.borrow().update_installing {
        return;
    }
    let s = app.borrow();
    let Some(manager) = s.media.clone() else {
        drop(s);
        error(app, "Дождитесь подключения к комнате.");
        return;
    };
    if s.media_state.channel_id.as_deref() == Some(channel_id.as_str()) {
        return;
    }
    let peers = s.peers.clone();
    drop(s);
    let generation = {
        let mut s = app.borrow_mut();
        s.voice_generation = s.voice_generation.wrapping_add(1);
        s.voice_generation
    };
    if !send(
        app,
        ClientCommand::JoinVoice {
            channel_id: channel_id.clone(),
        },
    ) {
        return;
    }
    let a = app.clone();
    spawn_local(async move {
        if let Err(e) = manager.join(channel_id, peers).await {
            if a.borrow().voice_generation == generation {
                send(&a, ClientCommand::LeaveVoice);
                error(&a, e);
            }
            return;
        }
        if a.borrow().voice_generation != generation {
            return;
        }
        if video {
            if let Err(e) = manager.toggle_camera().await {
                error(&a, e);
            }
        }
    });
}

fn desktop() -> Option<JsValue> {
    Reflect::get(&window(), &JsValue::from_str("sparkDesktop"))
        .ok()
        .filter(|v| !v.is_null() && !v.is_undefined())
}
async fn desktop_call(method: &str, args: &[JsValue]) -> Result<JsValue, String> {
    let obj = desktop().ok_or("Функция доступна в приложении Spark")?;
    let f: Function = Reflect::get(&obj, &JsValue::from_str(method))
        .map_err(err_text)?
        .dyn_into()
        .map_err(err_text)?;
    let array = js_sys::Array::new();
    for arg in args {
        array.push(arg);
    }
    let result = f.apply(&obj, &array).map_err(err_text)?;
    JsFuture::from(Promise::resolve(&result))
        .await
        .map_err(err_text)
}
fn screen_picker(app: &AppRef) {
    let s = app.borrow();
    let m = s.media.clone();
    let sharing = s.media_state.screen;
    drop(s);
    let Some(m) = m else { return };
    let a = app.clone();
    if sharing || desktop().is_none() {
        spawn_local(async move {
            if let Err(e) = m.toggle_screen().await {
                error(&a, e);
            }
        });
        return;
    }
    spawn_local(async move {
        match desktop_call("sources", &[]).await {
            Ok(sources) => {
                let text = js_sys::JSON::stringify(&sources)
                    .ok()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                match serde_json::from_str::<Vec<Source>>(&text) {
                    Ok(list) if !list.is_empty() => {
                        let mut s = a.borrow_mut();
                        s.sources = list;
                        s.modal = Some("sources".into());
                        drop(s);
                        render_overlay(&a);
                    }
                    _ => error(&a, "Не удалось получить экраны и окна для демонстрации."),
                }
            }
            Err(e) => error(&a, e),
        }
    });
}
fn select_source(app: &AppRef, id: String) {
    let m = app.borrow().media.clone();
    let a = app.clone();
    app.borrow_mut().modal = None;
    render_overlay(app);
    if let Some(m) = m {
        spawn_local(async move {
            match desktop_call("selectSource", &[JsValue::from_str(&id)]).await {
                Ok(_) => {
                    if let Err(e) = m.toggle_screen().await {
                        error(&a, e)
                    }
                }
                Err(e) => error(&a, e),
            }
        });
    }
}
fn download(app: &AppRef, id: String, name: String) {
    if desktop().is_some() {
        let a = app.clone();
        spawn_local(async move {
            match desktop_call(
                "saveFile",
                &[JsValue::from_str(&id), JsValue::from_str(&name)],
            )
            .await
            {
                Ok(result) => {
                    if !result.is_null() && !result.is_undefined() {
                        a.borrow_mut().notice =
                            Some("Файл сохранён. Размер и SHA256 проверены.".into());
                        render_overlay(&a);
                    }
                }
                Err(e) => error(&a, e),
            }
        });
    } else {
        let token = app.borrow().token.clone();
        if let Ok(link) = document().create_element("a") {
            link.set_attribute(
                "href",
                &format!("/api/files/{}?token={}", encode(&id), encode(&token)),
            )
            .ok();
            link.set_attribute("download", &name).ok();
            if let Ok(link) = link.dyn_into::<web_sys::HtmlElement>() {
                if let Some(body) = document().body() {
                    body.append_child(&link).ok();
                    link.click();
                    link.remove();
                }
            }
        }
    }
}

fn start_upload(app: &AppRef, file: web_sys::File) {
    if app.borrow().update_installing {
        return;
    }
    let s = app.borrow();
    if s.upload.is_some() {
        drop(s);
        error(app, "Дождитесь завершения текущей передачи.");
        return;
    }
    if file.size() > MAX_FILE_BYTES as f64 {
        drop(s);
        error(app, "Максимальный размер файла — 2 ГБ.");
        return;
    }
    let channel = s.selected.clone();
    let token = s.token.clone();
    drop(s);
    if channel.is_empty() {
        error(app, "Выберите текстовый канал.");
        return;
    }
    let xhr = match web_sys::XmlHttpRequest::new() {
        Ok(x) => x,
        Err(e) => {
            error(app, err_text(e));
            return;
        }
    };
    if let Err(e) = xhr.open(
        "POST",
        &format!(
            "/api/upload?channel_id={}&name={}",
            encode(&channel),
            encode(&file.name())
        ),
    ) {
        error(app, err_text(e));
        return;
    }
    if let Err(e) = xhr.set_request_header("x-spark-token", &token) {
        error(app, err_text(e));
        return;
    }
    let mime = file.type_();
    let _ = xhr.set_request_header(
        "content-type",
        if mime.is_empty() {
            "application/octet-stream"
        } else {
            &mime
        },
    );
    let weak = Rc::downgrade(app);
    let progress =
        Closure::<dyn FnMut(web_sys::ProgressEvent)>::new(move |event: web_sys::ProgressEvent| {
            if let Some(a) = weak.upgrade() {
                let mut s = a.borrow_mut();
                if let Some(upload) = &mut s.upload {
                    if event.length_computable() && event.total() > 0. {
                        upload.percent = ((event.loaded() / event.total()) * 100.).min(99.) as u32;
                    }
                    if let Some(el) = element("upload-label") {
                        el.set_text_content(Some(&format!(
                            "↑ {} · {}%",
                            upload.name, upload.percent
                        )));
                    }
                    if let Some(el) = element("upload-meter") {
                        el.set_attribute("value", &upload.percent.to_string()).ok();
                    }
                }
            }
        });
    if let Ok(upload) = xhr.upload() {
        upload.set_onprogress(Some(progress.as_ref().unchecked_ref()));
    }
    let weak = Rc::downgrade(app);
    let done = Closure::<dyn FnMut(Event)>::new(move |ev: Event| {
        let Some(a) = weak.upgrade() else { return };
        let Some(x) = ev
            .target()
            .and_then(|t| t.dyn_into::<web_sys::XmlHttpRequest>().ok())
        else {
            return;
        };
        let status = x.status().unwrap_or(0);
        let text = x.response_text().ok().flatten().unwrap_or_default();
        a.borrow_mut().upload = None;
        if (200..300).contains(&status) {
            match serde_json::from_str::<ChatMessage>(&text) {
                Ok(message) => server_event(&a, ServerEvent::Chat { message }),
                Err(_) => error(&a, "Не удалось подтвердить передачу файла."),
            }
        } else {
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| {
                    if status == 0 {
                        "Передача отменена или соединение потеряно.".into()
                    } else {
                        format!("Ошибка передачи файла ({status})")
                    }
                });
            error(&a, message);
        }
        render_composer(&a);
    });
    xhr.set_onloadend(Some(done.as_ref().unchecked_ref()));
    app.borrow_mut().upload = Some(Upload {
        name: file.name(),
        percent: 0,
    });
    app.borrow_mut().upload_request = Some(UploadRequest {
        xhr: xhr.clone(),
        _progress: progress,
        _done: done,
    });
    render_composer(app);
    // File/Blob is handed directly to Chromium. Its bytes never pass through WASM memory.
    if let Err(e) = xhr.send_with_opt_blob(Some(file.unchecked_ref())) {
        app.borrow_mut().upload = None;
        error(app, err_text(e));
        render_composer(app);
    }
}

fn save_settings(app: &AppRef) {
    let s = app.borrow();
    let name = s.form.settings_name.trim().to_owned();
    let token = s.token.clone();
    drop(s);
    if name.is_empty() {
        error(app, "Имя не может быть пустым.");
        return;
    }
    let a = app.clone();
    spawn_local(async move {
        match api::<Profile>(&token, "PUT", "/api/profile", Some(json!({"name":name}))).await {
            Ok(profile) => {
                let mut s = a.borrow_mut();
                s.form.name = profile.name.clone();
                s.profile = Some(profile);
                s.modal = None;
                s.notice = Some("Имя сохранено.".into());
                let ws = s.ws.clone();
                drop(s);
                if let Some(input) =
                    element("display-name").and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
                {
                    input.set_value(&name);
                }
                render_sidebar(&a);
                render_overlay(&a);
                if let Some(ws) = ws {
                    ws.close().ok();
                }
            }
            Err(e) => error(&a, e),
        }
    });
}
fn disconnect(app: &AppRef) {
    let token = app.borrow().token.clone();
    let a = app.clone();
    spawn_local(async move {
        match api::<Value>(&token, "POST", "/api/disconnect", Some(json!({}))).await {
            Ok(_) => {
                preferences::sound(&a, "disconnect");
                let mut s = a.borrow_mut();
                s.generation += 1;
                s.voice_generation = s.voice_generation.wrapping_add(1);
                s.conn = None;
                s.modal = None;
                s.invite = None;
                s.error = None;
                s.notice = None;
                s.reconnecting = false;
                let ws = s.ws.take();
                let media = s.media.take();
                let upload_request = s.upload_request.take();
                s.upload = None;
                s.streams.clear();
                s.connections.clear();
                s.channels.clear();
                s.peers.clear();
                s.messages.clear();
                s.draft.clear();
                drop(s);
                if let Some(request) = upload_request {
                    request.xhr.set_onloadend(None);
                    if let Ok(upload) = request.xhr.upload() {
                        upload.set_onprogress(None);
                    }
                    let _ = request.xhr.abort();
                }
                if let Some(ws) = ws {
                    ws.set_onclose(None);
                    ws.set_onmessage(None);
                    ws.close().ok();
                }
                if let Some(m) = media {
                    m.leave();
                }
                a.borrow_mut().media_state = MediaState::default();
                render(&a);
                discovery(&a);
            }
            Err(e) => error(&a, e),
        }
    });
}
