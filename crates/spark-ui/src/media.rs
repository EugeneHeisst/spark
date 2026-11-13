//! Browser media lives in Rust. Chromium supplies device capture and the WebRTC
//! implementation; signaling always travels through the authenticated LAN host.
//!
//! Every connection reserves audio, camera, screen and screen-audio transceivers in that order.
//! Only the lexicographically smaller peer creates offers. Replacing a reserved
//! sender's track never creates offer glare, and early ICE is queued until SDP.
use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::{Rc, Weak},
};

use js_sys::{Array, Object, Reflect};
use serde_json::{Value, json};
use spark_protocol::Peer;
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, future_to_promise, spawn_local};
use web_sys::{
    DisplayMediaStreamConstraints, Event, MediaStream, MediaStreamConstraints, MediaStreamTrack,
    RtcConfiguration, RtcIceCandidateInit, RtcPeerConnection, RtcPeerConnectionIceEvent,
    RtcRtpSender, RtcRtpTransceiver, RtcRtpTransceiverDirection, RtcRtpTransceiverInit, RtcSdpType,
    RtcSessionDescriptionInit, RtcSignalingState, RtcStatsReport, RtcTrackEvent,
};

#[derive(Clone, Default, Debug)]
pub struct MediaState {
    pub channel_id: Option<String>,
    pub muted: bool,
    pub deafened: bool,
    pub camera: bool,
    pub screen: bool,
}

pub struct MediaCallbacks {
    pub on_signal: Rc<dyn Fn(String, Value)>,
    pub on_state: Rc<dyn Fn(MediaState)>,
    /// The local preview uses peer ID `local`; kinds are audio/camera/screen.
    pub on_stream: Rc<dyn Fn(String, String, Option<MediaStream>)>,
    pub on_error: Rc<dyn Fn(String)>,
    pub on_connection: Rc<dyn Fn(String, String)>,
    pub on_speaking: Rc<dyn Fn(String, bool)>,
}

#[derive(Clone)]
pub struct MediaManager {
    inner: Rc<Inner>,
}

struct Inner {
    self_id: String,
    callbacks: MediaCallbacks,
    state: RefCell<MediaState>,
    peers: RefCell<HashMap<String, Rc<Connection>>>,
    known: RefCell<HashMap<String, Peer>>,
    microphone: RefCell<Option<MediaStream>>,
    camera: RefCell<Option<MediaStream>>,
    screen: RefCell<Option<MediaStream>>,
    screen_ended: RefCell<Option<JsValue>>,
    camera_ended: RefCell<Option<JsValue>>,
    epoch: Cell<u64>,
    camera_busy: Cell<bool>,
    screen_busy: Cell<bool>,
    noise_suppression: Cell<bool>,
    noise_busy: Cell<bool>,
    share_audio: Cell<bool>,
    activity: super::voice_activity::VoiceActivity,
}

struct Connection {
    pc: RtcPeerConnection,
    senders: RefCell<[Option<RtcRtpSender>; 4]>,
    streams: RefCell<HashMap<String, MediaStream>>,
    remote_description: Cell<bool>,
    remote_ufrag: RefCell<Option<String>>,
    candidates: RefCell<Vec<Value>>,
    operations: RefCell<VecDeque<Operation>>,
    processing: Cell<bool>,
    handlers: RefCell<Vec<JsValue>>,
    recovery_attempts: Cell<u8>,
    recovery_pending: Cell<bool>,
    recovery_offer_needed: Cell<bool>,
    recovery_timer: Cell<Option<i32>>,
    recovery_exhausted: Cell<bool>,
}

enum Operation {
    Attach,
    Offer,
    Signal(Value),
}

impl MediaManager {
    pub fn new(self_id: String, callbacks: MediaCallbacks) -> Self {
        let this = Self {
            inner: Rc::new(Inner {
                activity: super::voice_activity::VoiceActivity::new(callbacks.on_speaking.clone()),
                self_id,
                callbacks,
                state: RefCell::new(MediaState::default()),
                peers: RefCell::new(HashMap::new()),
                known: RefCell::new(HashMap::new()),
                microphone: RefCell::new(None),
                camera: RefCell::new(None),
                screen: RefCell::new(None),
                screen_ended: RefCell::new(None),
                camera_ended: RefCell::new(None),
                epoch: Cell::new(0),
                camera_busy: Cell::new(false),
                screen_busy: Cell::new(false),
                noise_suppression: Cell::new(true),
                noise_busy: Cell::new(false),
                share_audio: Cell::new(false),
            }),
        };
        this.install_debug();
        this
    }

    pub async fn join(&self, channel_id: String, peers: Vec<Peer>) -> Result<(), String> {
        self.leave();
        let epoch = self.inner.epoch.get();
        self.inner.state.borrow_mut().channel_id = Some(channel_id);
        self.emit_state();
        self.sync_peers(peers);
        let result = self.capture(false).await;
        let stream = match result {
            Ok(stream) => stream,
            Err(error) => {
                if self.inner.epoch.get() == epoch {
                    self.leave();
                }
                return Err(error);
            }
        };
        if self.inner.epoch.get() != epoch {
            stop_stream(&stream);
            return Err("Вызов уже завершён".into());
        }
        if let Err(e) = self.inner.activity.add("local", &stream) {
            self.report(format!("Индикатор речи: {}", js_error(e)));
        }
        *self.inner.microphone.borrow_mut() = Some(stream);
        self.apply_audio_enabled();
        // An offer may have arrived while the device permission prompt was open.
        for (id, _) in self.connections() {
            self.enqueue(&id, Operation::Attach);
        }
        self.emit_state();
        Ok(())
    }

    pub fn leave(&self) {
        stop_share_audio();
        self.inner.epoch.set(self.inner.epoch.get().wrapping_add(1));
        let ids: Vec<_> = self.inner.peers.borrow().keys().cloned().collect();
        for id in ids {
            self.remove_peer(&id);
        }
        self.inner.known.borrow_mut().clear();
        self.inner.activity.clear();
        for slot in [
            &self.inner.microphone,
            &self.inner.camera,
            &self.inner.screen,
        ] {
            if let Some(stream) = slot.borrow_mut().take() {
                stop_stream(&stream);
            }
        }
        self.inner.screen_ended.borrow_mut().take();
        self.inner.camera_ended.borrow_mut().take();
        self.inner.camera_busy.set(false);
        self.inner.screen_busy.set(false);
        *self.inner.state.borrow_mut() = MediaState::default();
        for kind in ["camera", "screen"] {
            (self.inner.callbacks.on_stream)("local".into(), kind.into(), None);
        }
        self.emit_state();
    }

    pub fn configure_audio(&self, noise: bool, share: bool) {
        self.inner.noise_suppression.set(noise);
        self.inner.share_audio.set(share);
    }

    pub async fn set_noise_suppression(&self, enabled: bool) -> Result<(), String> {
        if self.inner.noise_busy.replace(true) {
            return Err("Шумоподавление уже переключается".into());
        }
        let previous = self.inner.noise_suppression.replace(enabled);
        let epoch = self.inner.epoch.get();
        let old = self.inner.microphone.borrow().clone();
        let result = async {
            let Some(old) = old else {
                return Ok(());
            };
            // Chromium can resolve applyConstraints without changing its audio
            // processor. Acquire the requested processor and swap the sender.
            let stream = self.capture(false).await?;
            if self.inner.epoch.get() != epoch {
                stop_stream(&stream);
                return Err("Вызов уже завершён".into());
            }
            let track = first_track(&stream, "audio").ok_or("Микрофон не предоставил звук")?;
            let state = self.inner.state.borrow().clone();
            track.set_enabled(!state.muted && !state.deafened);
            if let Err(e) = self.replace_for_all(0, Some(track)).await {
                let _ = self.replace_for_all(0, first_track(&old, "audio")).await;
                stop_stream(&stream);
                return Err(e);
            }
            if self.inner.epoch.get() != epoch {
                stop_stream(&stream);
                return Err("Вызов уже завершён".into());
            }
            if let Err(e) = self.inner.activity.add("local", &stream) {
                self.report(format!("Индикатор речи: {}", js_error(e)));
            }
            *self.inner.microphone.borrow_mut() = Some(stream);
            self.apply_audio_enabled();
            stop_stream(&old);
            Ok(())
        }
        .await;
        self.inner.noise_busy.set(false);
        self.inner
            .noise_suppression
            .set(if result.is_ok() { enabled } else { previous });
        result
    }

    pub fn set_muted(&self, muted: bool) {
        self.inner.state.borrow_mut().muted = muted;
        self.apply_audio_enabled();
        self.emit_state();
    }

    pub fn set_deafened(&self, deafened: bool) {
        self.inner.state.borrow_mut().deafened = deafened;
        self.apply_audio_enabled();
        for (_, connection) in self.connections() {
            for stream in connection.streams.borrow().values() {
                for track in tracks(stream) {
                    if track.kind() == "audio" {
                        track.set_enabled(!deafened);
                    }
                }
            }
        }
        self.emit_state();
    }

    pub async fn toggle_camera(&self) -> Result<(), String> {
        self.require_call()?;
        if self.inner.camera_busy.replace(true) {
            return Err("Камера уже переключается".into());
        }
        let epoch = self.inner.epoch.get();
        let result = self.toggle_camera_inner(epoch).await;
        if self.inner.epoch.get() == epoch {
            self.inner.camera_busy.set(false);
        }
        result
    }

    async fn toggle_camera_inner(&self, epoch: u64) -> Result<(), String> {
        if self.inner.camera.borrow().is_some() {
            return self.stop_camera().await;
        }
        let stream = self.capture(true).await?;
        if self.inner.epoch.get() != epoch {
            stop_stream(&stream);
            return Err("Вызов уже завершён".into());
        }
        let track = first_track(&stream, "video").ok_or("Камера не предоставила видеодорожку")?;
        self.bind_capture_ended(&track, false);
        // MediaStream::clone() is the browser API and clones its tracks. Share
        // the JS stream reference so stop/ended affect the actual sent capture.
        *self.inner.camera.borrow_mut() = Some(Clone::clone(&stream));
        if let Err(error) = self.replace_for_all(1, Some(track)).await {
            let _ = self.stop_camera().await;
            return Err(error);
        }
        if self.inner.epoch.get() != epoch || self.inner.camera.borrow().is_none() {
            stop_stream(&stream);
            return Err("Камера уже остановлена".into());
        }
        self.inner.state.borrow_mut().camera = true;
        (self.inner.callbacks.on_stream)("local".into(), "camera".into(), Some(stream));
        self.emit_state();
        Ok(())
    }

    pub async fn toggle_screen(&self) -> Result<(), String> {
        self.require_call()?;
        if self.inner.screen_busy.replace(true) {
            return Err("Демонстрация уже переключается".into());
        }
        let epoch = self.inner.epoch.get();
        let result = self.toggle_screen_inner(epoch).await;
        // A cancelled capture from an old call must not stop a newer call's source.
        if result.is_err() && self.inner.epoch.get() == epoch {
            stop_share_audio();
        }
        if self.inner.epoch.get() == epoch {
            self.inner.screen_busy.set(false);
        }
        result
    }

    async fn toggle_screen_inner(&self, epoch: u64) -> Result<(), String> {
        if self.inner.screen.borrow().is_some() {
            return self.stop_screen().await;
        }
        // In Electron the UI selects a source via sparkDesktop before this call.
        // In an ordinary browser this is the browser's own permission/source picker.
        let constraints = DisplayMediaStreamConstraints::new();
        constraints.set_video(&JsValue::TRUE);
        let audio = Object::new();
        let _ = Reflect::set(&audio, &"restrictOwnAudio".into(), &JsValue::TRUE);
        for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
            let _ = Reflect::set(&audio, &key.into(), &JsValue::FALSE);
        }
        let source_label = if self.inner.share_audio.get() && super::desktop().is_some() {
            super::desktop_call("prepareShareAudio", &[])
                .await?
                .as_string()
        } else {
            None
        };
        if self.inner.share_audio.get() && source_label.is_none() {
            constraints.set_audio(audio.as_ref());
        } else {
            constraints.set_audio(&JsValue::FALSE);
        }
        let promise = devices()?
            .get_display_media_with_constraints(&constraints)
            .map_err(|e| format!("Не удалось запросить экран: {}", js_error(e)))?;
        let stream: MediaStream = JsFuture::from(promise)
            .await
            .map_err(|e| format!("Демонстрация не начата: {}", js_error(e)))?
            .dyn_into()
            .map_err(|_| "Браузер не вернул поток экрана")?;
        if let Some(label) = source_label {
            let captured = capture_share_audio(&label).await;
            match captured {
                Ok(audio) => {
                    for track in tracks(&audio) {
                        stream.add_track(&track);
                    }
                }
                Err(error) => {
                    stop_stream(&stream);
                    return Err(error);
                }
            }
        }
        if self.inner.epoch.get() != epoch {
            stop_stream(&stream);
            return Err("Вызов уже завершён".into());
        }
        let track = first_track(&stream, "video").ok_or("Экран не предоставил видеодорожку")?;
        self.bind_capture_ended(&track, true);
        *self.inner.screen.borrow_mut() = Some(Clone::clone(&stream));
        if let Err(error) = self.replace_for_all(2, Some(track)).await {
            let _ = self.stop_screen().await;
            return Err(error);
        }
        if self.inner.epoch.get() != epoch || self.inner.screen.borrow().is_none() {
            stop_stream(&stream);
            return Err("Демонстрация уже остановлена".into());
        }
        if let Err(error) = self.replace_for_all(3, first_track(&stream, "audio")).await {
            let _ = self.stop_screen().await;
            return Err(error);
        }
        if self.inner.epoch.get() != epoch {
            stop_stream(&stream);
            return Err("Вызов уже завершён".into());
        }
        if self.inner.share_audio.get() && first_track(&stream, "audio").is_none() {
            self.report("Источник не предоставил звук. Экран передаётся без звука.".into());
        }
        self.inner.state.borrow_mut().screen = true;
        (self.inner.callbacks.on_stream)("local".into(), "screen".into(), Some(stream));
        self.emit_state();
        Ok(())
    }

    pub fn sync_peers(&self, peers: Vec<Peer>) {
        let channel = self.inner.state.borrow().channel_id.clone();
        let active: HashMap<String, Peer> = peers
            .into_iter()
            .filter(|peer| {
                peer.id != self.inner.self_id && channel.is_some() && peer.voice_channel == channel
            })
            .map(|peer| (peer.id.clone(), peer))
            .collect();
        let stale: Vec<_> = self
            .inner
            .peers
            .borrow()
            .keys()
            .filter(|id| !active.contains_key(*id))
            .cloned()
            .collect();
        for id in stale {
            self.remove_peer(&id);
        }
        *self.inner.known.borrow_mut() = active.clone();
        for (id, peer) in active {
            if let Err(error) = self.ensure_peer(&id) {
                self.report(error);
                continue;
            }
            self.publish_remote(&id, "camera", peer.camera);
            self.publish_remote(&id, "screen", peer.screen);
            self.publish_remote(&id, "screen-audio", peer.screen);
        }
    }

    pub fn receive_signal(&self, from: String, payload: Value) {
        if self.inner.state.borrow().channel_id.is_none() || from == self.inner.self_id {
            return;
        }
        if !matches!(
            payload.get("type").and_then(Value::as_str),
            Some("offer" | "answer" | "ice" | "restart")
        ) {
            self.report("Получен неизвестный сигнал WebRTC".into());
            return;
        }
        match self.ensure_peer(&from) {
            Ok(()) => self.enqueue(&from, Operation::Signal(payload)),
            Err(error) => self.report(error),
        }
    }

    fn require_call(&self) -> Result<(), String> {
        if self.inner.state.borrow().channel_id.is_none() {
            Err("Сначала войдите в голосовой канал".into())
        } else {
            Ok(())
        }
    }

    async fn capture(&self, video: bool) -> Result<MediaStream, String> {
        let constraints = MediaStreamConstraints::new();
        if video {
            constraints.set_audio(&JsValue::FALSE);
            constraints.set_video(&JsValue::TRUE);
        } else {
            let audio = Object::new();
            for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
                let enabled = key != "noiseSuppression" || self.inner.noise_suppression.get();
                let constraint = if key == "noiseSuppression" {
                    let exact = Object::new();
                    let _ = Reflect::set(&exact, &"exact".into(), &JsValue::from_bool(enabled));
                    JsValue::from(exact)
                } else {
                    JsValue::from_bool(enabled)
                };
                let _ = Reflect::set(&audio, &key.into(), &constraint);
            }
            constraints.set_audio(&audio);
            constraints.set_video(&JsValue::FALSE);
        }
        let promise = devices()?
            .get_user_media_with_constraints(&constraints)
            .map_err(js_error)?;
        JsFuture::from(promise)
            .await
            .map_err(|e| {
                format!(
                    "{}: {}",
                    if video {
                        "Камера недоступна"
                    } else {
                        "Микрофон недоступен"
                    },
                    js_error(e)
                )
            })?
            .dyn_into()
            .map_err(|_| "Браузер не вернул медиапоток".into())
    }

    fn emit_state(&self) {
        let state = self.inner.state.borrow().clone();
        (self.inner.callbacks.on_state)(state);
    }
    fn report(&self, error: String) {
        (self.inner.callbacks.on_error)(error.chars().take(320).collect());
    }
    fn connections(&self) -> Vec<(String, Rc<Connection>)> {
        self.inner
            .peers
            .borrow()
            .iter()
            .map(|(id, p)| (id.clone(), p.clone()))
            .collect()
    }
    fn is_current(&self, id: &str, connection: &Rc<Connection>) -> bool {
        self.inner
            .peers
            .borrow()
            .get(id)
            .is_some_and(|current| Rc::ptr_eq(current, connection))
    }

    fn apply_audio_enabled(&self) {
        let state = self.inner.state.borrow();
        if let Some(stream) = self.inner.microphone.borrow().as_ref() {
            for track in tracks(stream) {
                track.set_enabled(!state.muted && !state.deafened);
            }
        }
    }

    fn ensure_peer(&self, id: &str) -> Result<(), String> {
        if self.inner.peers.borrow().contains_key(id) {
            return Ok(());
        }
        let config = RtcConfiguration::new();
        config.set_ice_servers(&Array::new()); // LAN host candidates only. No cloud traffic.
        let pc = RtcPeerConnection::new_with_configuration(&config).map_err(js_error)?;
        let init = RtcRtpTransceiverInit::new();
        init.set_direction(RtcRtpTransceiverDirection::Sendrecv);
        // Answerers must adopt the offered transceivers. Creating their own here
        // would leave those unassociated and produce one-way recvonly media.
        let senders = if self.inner.self_id.as_str() < id {
            [
                Some(
                    pc.add_transceiver_with_str_and_init("audio", &init)
                        .sender(),
                ),
                Some(
                    pc.add_transceiver_with_str_and_init("video", &init)
                        .sender(),
                ),
                Some(
                    pc.add_transceiver_with_str_and_init("video", &init)
                        .sender(),
                ),
                Some(
                    pc.add_transceiver_with_str_and_init("audio", &init)
                        .sender(),
                ),
            ]
        } else {
            [None, None, None, None]
        };
        let connection = Rc::new(Connection {
            pc: pc.clone(),
            senders: RefCell::new(senders),
            streams: RefCell::new(HashMap::new()),
            remote_description: Cell::new(false),
            remote_ufrag: RefCell::new(None),
            candidates: RefCell::new(Vec::new()),
            operations: RefCell::new(VecDeque::new()),
            processing: Cell::new(false),
            handlers: RefCell::new(Vec::new()),
            recovery_attempts: Cell::new(0),
            recovery_pending: Cell::new(false),
            recovery_offer_needed: Cell::new(false),
            recovery_timer: Cell::new(None),
            recovery_exhausted: Cell::new(false),
        });
        self.inner
            .peers
            .borrow_mut()
            .insert(id.into(), connection.clone());

        let weak = Rc::downgrade(&self.inner);
        let weak_connection = Rc::downgrade(&connection);
        let peer_id = id.to_string();
        let ice = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(inner) = weak.upgrade() else { return };
            let Some(connection) = weak_connection.upgrade() else {
                return;
            };
            if !inner
                .peers
                .borrow()
                .get(&peer_id)
                .is_some_and(|current| Rc::ptr_eq(current, &connection))
            {
                return;
            }
            let Ok(event) = event.dyn_into::<RtcPeerConnectionIceEvent>() else {
                return;
            };
            if let Some(candidate) = event.candidate() {
                (inner.callbacks.on_signal)(
                    peer_id.clone(),
                    json!({"type":"ice", "candidate": candidate.candidate(),
                    "sdpMid":candidate.sdp_mid(), "sdpMLineIndex":candidate.sdp_m_line_index(),
                    "usernameFragment":property_string(candidate.as_ref(), "usernameFragment")}),
                );
            }
        });
        let ice = ice.into_js_value();
        pc.set_onicecandidate(Some(ice.unchecked_ref()));
        connection.handlers.borrow_mut().push(ice);

        let weak = Rc::downgrade(&self.inner);
        let weak_connection = Rc::downgrade(&connection);
        let peer_id = id.to_string();
        let ontrack = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
            let Some(inner) = weak.upgrade() else { return };
            let Some(connection) = weak_connection.upgrade() else {
                return;
            };
            if !inner
                .peers
                .borrow()
                .get(&peer_id)
                .is_some_and(|current| Rc::ptr_eq(current, &connection))
            {
                return;
            }
            let Ok(event) = event.dyn_into::<RtcTrackEvent>() else {
                return;
            };
            let track = event.track();
            let kind = if event.transceiver().mid().as_deref() == Some("3") {
                "screen-audio"
            } else if track.kind() == "audio" {
                "audio"
            } else if event.transceiver().mid().as_deref() == Some("2") {
                "screen"
            } else {
                "camera"
            };
            let Ok(stream) = MediaStream::new() else {
                return;
            };
            stream.add_track(&track);
            if kind == "audio" || kind == "screen-audio" {
                track.set_enabled(!inner.state.borrow().deafened);
            }
            if kind == "audio" {
                if let Err(e) = inner.activity.add(&peer_id, &stream) {
                    (inner.callbacks.on_error)(format!("Индикатор речи: {}", js_error(e)));
                }
            }
            connection.streams.borrow_mut().insert(kind.into(), stream);
            let active = kind == "audio"
                || inner.known.borrow().get(&peer_id).is_some_and(|peer| {
                    if kind == "screen" || kind == "screen-audio" {
                        peer.screen
                    } else {
                        peer.camera
                    }
                });
            MediaManager { inner }.publish_remote(&peer_id, kind, active);
        });
        let ontrack = ontrack.into_js_value();
        pc.set_ontrack(Some(ontrack.unchecked_ref()));
        connection.handlers.borrow_mut().push(ontrack);

        let weak = Rc::downgrade(&self.inner);
        let weak_connection = Rc::downgrade(&connection);
        let peer_id = id.to_string();
        let state_changed = Closure::<dyn FnMut(Event)>::new(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            let Some(connection) = weak_connection.upgrade() else {
                return;
            };
            MediaManager { inner }.connection_changed(&peer_id, &connection);
        });
        let state_changed = state_changed.into_js_value();
        pc.set_onconnectionstatechange(Some(state_changed.unchecked_ref()));
        pc.set_oniceconnectionstatechange(Some(state_changed.unchecked_ref()));
        connection.handlers.borrow_mut().push(state_changed);
        (self.inner.callbacks.on_connection)(id.into(), "new".into());
        self.enqueue(id, Operation::Attach);
        if self.inner.self_id.as_str() < id {
            self.enqueue(id, Operation::Offer);
        }
        Ok(())
    }

    fn connection_changed(&self, id: &str, connection: &Rc<Connection>) {
        if !self.is_current(id, connection) {
            return;
        }
        let state = property_string(connection.pc.as_ref(), "connectionState");
        let ice = property_string(connection.pc.as_ref(), "iceConnectionState");
        (self.inner.callbacks.on_connection)(id.into(), state.clone());
        if state == "connected" && matches!(ice.as_str(), "connected" | "completed") {
            self.cancel_recovery_timer(connection);
            connection.recovery_pending.set(false);
        } else if (state == "failed" || ice == "failed") && !connection.recovery_pending.get() {
            self.schedule_recovery(id, connection, 0, false);
        } else if (state == "disconnected" || ice == "disconnected")
            && !connection.recovery_pending.get()
            && connection.recovery_timer.get().is_none()
        {
            self.schedule_recovery(id, connection, 4_000, false);
        }
    }

    fn cancel_recovery_timer(&self, connection: &Connection) {
        if let Some(timer) = connection.recovery_timer.take() {
            if let Some(window) = web_sys::window() {
                window.clear_timeout_with_handle(timer);
            }
        }
    }

    fn schedule_recovery(&self, id: &str, connection: &Rc<Connection>, delay: i32, watchdog: bool) {
        self.cancel_recovery_timer(connection);
        let weak = Rc::downgrade(&self.inner);
        let weak_connection = Rc::downgrade(connection);
        let id = id.to_string();
        // Weak captures deliberately keep neither devices nor connections alive.
        let callback = Closure::once_into_js(move || {
            let (Some(inner), Some(connection)) = (weak.upgrade(), weak_connection.upgrade())
            else {
                return;
            };
            let this = MediaManager { inner };
            if !this.is_current(&id, &connection) {
                return;
            }
            connection.recovery_timer.set(None);
            let state = property_string(connection.pc.as_ref(), "connectionState");
            let ice = property_string(connection.pc.as_ref(), "iceConnectionState");
            if state == "closed"
                || (state == "connected" && matches!(ice.as_str(), "connected" | "completed"))
            {
                connection.recovery_pending.set(false);
                return;
            }
            if watchdog {
                connection.recovery_pending.set(false);
            }
            if watchdog
                || matches!(state.as_str(), "failed" | "disconnected")
                || matches!(ice.as_str(), "failed" | "disconnected")
            {
                this.begin_recovery(&id, &connection);
            }
        });
        if let Some(window) = web_sys::window() {
            match window.set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.unchecked_ref(),
                delay,
            ) {
                Ok(timer) => connection.recovery_timer.set(Some(timer)),
                Err(error) => self.report(js_error(error)),
            }
        }
    }

    fn begin_recovery(&self, id: &str, connection: &Rc<Connection>) {
        if !self.is_current(id, connection) || connection.recovery_pending.get() {
            return;
        }
        self.cancel_recovery_timer(connection);
        if connection.recovery_attempts.get() >= 3 {
            if !connection.recovery_exhausted.replace(true) {
                (self.inner.callbacks.on_connection)(id.into(), "failed".into());
                self.report("Не удалось восстановить звонок после 3 попыток. Проверьте сетевой экран и изоляцию Wi-Fi, затем выйдите из голосового канала и войдите снова.".into());
            }
            return;
        }
        connection
            .recovery_attempts
            .set(connection.recovery_attempts.get() + 1);
        connection.recovery_pending.set(true);
        (self.inner.callbacks.on_connection)(id.into(), "connecting".into());
        if self.inner.self_id.as_str() < id {
            connection.recovery_offer_needed.set(true);
            self.enqueue(id, Operation::Offer);
        } else {
            // The answerer requests recovery; it never creates a competing offer.
            (self.inner.callbacks.on_signal)(id.into(), json!({"type":"restart"}));
        }
        self.schedule_recovery(id, connection, 8_000, true);
    }

    fn enqueue(&self, id: &str, operation: Operation) {
        let Some(connection) = self.inner.peers.borrow().get(id).cloned() else {
            return;
        };
        if connection.operations.borrow().len() >= 256 {
            self.report("Слишком много ожидающих сигналов WebRTC".into());
            return;
        }
        connection.operations.borrow_mut().push_back(operation);
        if connection.processing.replace(true) {
            return;
        }
        let this = self.clone();
        let id = id.to_string();
        spawn_local(async move {
            loop {
                if !this.is_current(&id, &connection) {
                    break;
                }
                let operation = connection.operations.borrow_mut().pop_front();
                let Some(operation) = operation else { break };
                if let Err(error) = this.perform(&id, &connection, operation).await {
                    if this.is_current(&id, &connection) {
                        this.report(format!("Соединение: {error}"));
                    }
                }
            }
            connection.processing.set(false);
        });
    }

    async fn perform(
        &self,
        id: &str,
        connection: &Rc<Connection>,
        operation: Operation,
    ) -> Result<(), String> {
        match operation {
            Operation::Attach => {
                self.attach_tracks(connection).await?;
            }
            Operation::Offer => {
                if connection.pc.signaling_state() != RtcSignalingState::Stable {
                    return Ok(());
                }
                if connection.recovery_offer_needed.replace(false) {
                    restart_ice(&connection.pc)?;
                }
                let offer = JsFuture::from(connection.pc.create_offer())
                    .await
                    .map_err(js_error)?;
                let sdp = property_string(&offer, "sdp");
                let description = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
                description.set_sdp(&sdp);
                JsFuture::from(connection.pc.set_local_description(&description))
                    .await
                    .map_err(js_error)?;
                if self.is_current(id, connection) {
                    (self.inner.callbacks.on_signal)(id.into(), json!({"type":"offer","sdp":sdp}));
                }
            }
            Operation::Signal(payload) => match payload["type"].as_str() {
                Some("restart") => {
                    if self.inner.self_id.as_str() < id {
                        self.begin_recovery(id, connection);
                    }
                }
                Some("ice") => {
                    if !connection.remote_description.get()
                        || !candidate_matches(&payload, connection.remote_ufrag.borrow().as_deref())
                    {
                        let mut pending = connection.candidates.borrow_mut();
                        if pending.len() >= 256 {
                            return Err("Слишком много ICE-кандидатов".into());
                        }
                        pending.push(payload);
                    } else {
                        add_candidate(&connection.pc, &payload).await?;
                    }
                }
                Some(kind @ ("offer" | "answer")) => {
                    // A single predetermined offerer removes simultaneous-offer races.
                    if (kind == "offer") != (id < self.inner.self_id.as_str()) {
                        return Err("Неверный отправитель SDP".into());
                    }
                    let sdp = payload["sdp"]
                        .as_str()
                        .filter(|s| s.len() <= 60_000)
                        .ok_or("Некорректный SDP")?;
                    let description = RtcSessionDescriptionInit::new(if kind == "offer" {
                        RtcSdpType::Offer
                    } else {
                        RtcSdpType::Answer
                    });
                    description.set_sdp(sdp);
                    JsFuture::from(connection.pc.set_remote_description(&description))
                        .await
                        .map_err(js_error)?;
                    *connection.remote_ufrag.borrow_mut() = sdp.lines().find_map(|line| {
                        line.strip_prefix("a=ice-ufrag:")
                            .map(|value| value.trim().to_string())
                    });
                    if kind == "offer" {
                        let transceivers: Vec<RtcRtpTransceiver> = connection
                            .pc
                            .get_transceivers()
                            .iter()
                            .filter_map(|value| value.dyn_into().ok())
                            .collect();
                        if !(3..=4).contains(&transceivers.len()) {
                            return Err("Несовместимая схема медиадорожек".into());
                        }
                        for (index, transceiver) in transceivers.into_iter().enumerate() {
                            transceiver.set_direction(RtcRtpTransceiverDirection::Sendrecv);
                            connection.senders.borrow_mut()[index] = Some(transceiver.sender());
                        }
                        self.attach_tracks(connection).await?;
                    }
                    connection.remote_description.set(true);
                    let candidates = std::mem::take(&mut *connection.candidates.borrow_mut());
                    for candidate in candidates {
                        let matches = candidate_matches(
                            &candidate,
                            connection.remote_ufrag.borrow().as_deref(),
                        );
                        if matches {
                            add_candidate(&connection.pc, &candidate).await?;
                        }
                    }
                    if kind == "offer" {
                        let answer = JsFuture::from(connection.pc.create_answer())
                            .await
                            .map_err(js_error)?;
                        let sdp = property_string(&answer, "sdp");
                        let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
                        description.set_sdp(&sdp);
                        JsFuture::from(connection.pc.set_local_description(&description))
                            .await
                            .map_err(js_error)?;
                        if self.is_current(id, connection) {
                            (self.inner.callbacks.on_signal)(
                                id.into(),
                                json!({"type":"answer","sdp":sdp}),
                            );
                        }
                    }
                    if kind == "answer" && connection.recovery_offer_needed.get() {
                        self.enqueue(id, Operation::Offer);
                    }
                }
                _ => return Err("Неизвестный сигнал".into()),
            },
        }
        Ok(())
    }

    async fn replace_for_all(
        &self,
        index: usize,
        track: Option<MediaStreamTrack>,
    ) -> Result<(), String> {
        for (id, connection) in self.connections() {
            let Some(sender) = connection.senders.borrow()[index].clone() else {
                continue;
            };
            let result = JsFuture::from(sender.replace_track(track.as_ref())).await;
            if self.is_current(&id, &connection) {
                result.map_err(js_error)?;
            }
        }
        Ok(())
    }

    async fn attach_tracks(&self, connection: &Connection) -> Result<(), String> {
        let streams = [
            self.inner.microphone.borrow().clone(),
            self.inner.camera.borrow().clone(),
            self.inner.screen.borrow().clone(),
            self.inner.screen.borrow().clone(),
        ];
        for (index, stream) in streams.into_iter().enumerate() {
            let Some(sender) = connection.senders.borrow()[index].clone() else {
                continue;
            };
            let track = stream.as_ref().and_then(|stream| {
                first_track(
                    stream,
                    if index == 0 || index == 3 {
                        "audio"
                    } else {
                        "video"
                    },
                )
            });
            JsFuture::from(sender.replace_track(track.as_ref()))
                .await
                .map_err(js_error)?;
        }
        Ok(())
    }

    fn publish_remote(&self, id: &str, kind: &str, active: bool) {
        let stream = self
            .inner
            .peers
            .borrow()
            .get(id)
            .and_then(|connection| connection.streams.borrow().get(kind).cloned());
        (self.inner.callbacks.on_stream)(
            id.into(),
            kind.into(),
            if active { stream } else { None },
        );
    }

    fn remove_peer(&self, id: &str) {
        self.inner.activity.remove(id);
        let connection = self.inner.peers.borrow_mut().remove(id);
        if let Some(connection) = connection {
            connection.pc.set_onicecandidate(None);
            connection.pc.set_ontrack(None);
            connection.pc.set_onconnectionstatechange(None);
            connection.pc.set_oniceconnectionstatechange(None);
            self.cancel_recovery_timer(&connection);
            connection.pc.close();
            connection.operations.borrow_mut().clear();
            connection.handlers.borrow_mut().clear();
            for stream in connection.streams.borrow().values() {
                stop_stream(stream);
            }
            for kind in ["audio", "camera", "screen", "screen-audio"] {
                (self.inner.callbacks.on_stream)(id.into(), kind.into(), None);
            }
            (self.inner.callbacks.on_connection)(id.into(), "closed".into());
        }
    }

    fn bind_capture_ended(&self, track: &MediaStreamTrack, screen: bool) {
        let weak = Rc::downgrade(&self.inner);
        let epoch = self.inner.epoch.get();
        let track_id = track.id();
        let ended = Closure::<dyn FnMut(Event)>::new(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            if inner.epoch.get() != epoch {
                return;
            }
            let this = MediaManager { inner };
            if !this.is_current_capture(screen, &track_id) {
                return;
            }
            let track_id = track_id.clone();
            spawn_local(async move {
                if this.inner.epoch.get() != epoch || !this.is_current_capture(screen, &track_id) {
                    return;
                }
                let result = if screen {
                    this.stop_screen().await
                } else {
                    this.stop_camera().await
                };
                if let Err(error) = result {
                    this.report(error);
                }
            });
        });
        // A native source can queue an ended event just before stop/leave. JS
        // ownership keeps this function callable until that queued reference is
        // gone; dropping a Rust-owned Closure here would invalidate its wrapper.
        // Weak captures and the epoch guard make a late call harmless.
        let ended = ended.into_js_value();
        track.set_onended(Some(ended.unchecked_ref()));
        if screen {
            *self.inner.screen_ended.borrow_mut() = Some(ended);
        } else {
            *self.inner.camera_ended.borrow_mut() = Some(ended);
        }
    }

    fn is_current_capture(&self, screen: bool, track_id: &str) -> bool {
        let slot = if screen {
            &self.inner.screen
        } else {
            &self.inner.camera
        };
        slot.borrow()
            .as_ref()
            .and_then(|stream| first_track(stream, "video"))
            .is_some_and(|track| track.id() == track_id)
    }

    async fn stop_camera(&self) -> Result<(), String> {
        self.stop_video(false).await
    }
    async fn stop_screen(&self) -> Result<(), String> {
        stop_share_audio();
        self.stop_video(true).await
    }
    async fn stop_video(&self, screen: bool) -> Result<(), String> {
        let (slot, kind, index) = if screen {
            (&self.inner.screen, "screen", 2)
        } else {
            (&self.inner.camera, "camera", 1)
        };
        if let Some(stream) = slot.borrow_mut().take() {
            stop_stream(&stream);
        }
        if screen {
            self.inner.screen_ended.borrow_mut().take();
            self.inner.state.borrow_mut().screen = false;
        } else {
            self.inner.camera_ended.borrow_mut().take();
            self.inner.state.borrow_mut().camera = false;
        }
        (self.inner.callbacks.on_stream)("local".into(), kind.into(), None);
        self.emit_state();
        self.replace_for_all(index, None).await?;
        if screen {
            self.replace_for_all(3, None).await?;
        }
        Ok(())
    }

    fn install_debug(&self) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let debug = Object::new();
        let weak = Rc::downgrade(&self.inner);
        let snapshot = Closure::<dyn FnMut() -> JsValue>::new(move || {
            let Some(inner) = weak.upgrade() else {
                return JsValue::NULL;
            };
            MediaManager { inner }.snapshot()
        });
        let _ = Reflect::set(&debug, &"snapshot".into(), snapshot.as_ref());
        snapshot.forget();
        let weak: Weak<Inner> = Rc::downgrade(&self.inner);
        let stats = Closure::<dyn FnMut() -> js_sys::Promise>::new(move || {
            let weak = weak.clone();
            future_to_promise(async move {
                let Some(inner) = weak.upgrade() else {
                    return Ok(Array::new().into());
                };
                let this = MediaManager { inner };
                let result = Array::new();
                for (id, connection) in this.connections() {
                    let report: RtcStatsReport = JsFuture::from(connection.pc.get_stats())
                        .await?
                        .dyn_into()?;
                    let reports = Array::from(&report.values());
                    let item = Object::new();
                    Reflect::set(&item, &"peerId".into(), &id.into())?;
                    Reflect::set(
                        &item,
                        &"connectionState".into(),
                        &property_string(connection.pc.as_ref(), "connectionState").into(),
                    )?;
                    Reflect::set(&item, &"reports".into(), &reports)?;
                    result.push(&item);
                }
                Ok(result.into())
            })
        });
        let _ = Reflect::set(&debug, &"stats".into(), stats.as_ref());
        stats.forget();
        let weak = Rc::downgrade(&self.inner);
        let restart = Closure::<dyn FnMut(String)>::new(move |id: String| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            let this = MediaManager { inner };
            let connection = this.inner.peers.borrow().get(&id).cloned();
            if let Some(connection) = connection {
                this.begin_recovery(&id, &connection);
            }
        });
        let _ = Reflect::set(&debug, &"restartIce".into(), restart.as_ref());
        restart.forget();
        let _ = Reflect::set(&window, &"sparkMediaDebug".into(), &debug);
    }

    fn snapshot(&self) -> JsValue {
        let state = self.inner.state.borrow().clone();
        let local: Vec<_> = [
            ("audio", &self.inner.microphone),
            ("camera", &self.inner.camera),
            ("screen", &self.inner.screen),
        ]
        .into_iter()
        .flat_map(|(kind, slot)| {
            slot.borrow()
                .as_ref()
                .map(|stream| {
                    tracks(stream)
                        .into_iter()
                        .map(move |track| {
                            track_snapshot(
                                if kind == "screen" && track.kind() == "audio" {
                                    "screen-audio"
                                } else {
                                    kind
                                },
                                &track,
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect();
        let peers: Vec<_> = self.connections().into_iter().map(|(id, connection)| {
            let senders: Vec<_> = connection.senders.borrow().iter().enumerate().map(|(index, sender)| sender.as_ref().and_then(RtcRtpSender::track).map(|track| track_snapshot(["audio", "camera", "screen", "screen-audio"][index], &track)).unwrap_or_else(|| json!({"kind":(["audio","camera","screen","screen-audio"][index]),"track":null}))).collect();
            json!({"id":id,"connectionState":property_string(connection.pc.as_ref(), "connectionState"),
                "signalingState":property_string(connection.pc.as_ref(), "signalingState"),
                "iceConnectionState":property_string(connection.pc.as_ref(), "iceConnectionState"),
                "senders":senders,"queuedIce":connection.candidates.borrow().len(),
                "recoveryAttempts":connection.recovery_attempts.get(),"recoveryPending":connection.recovery_pending.get(),
                "remoteUfrag":connection.remote_ufrag.borrow().clone(),
                "localUfrag":Reflect::get(connection.pc.as_ref(), &"localDescription".into()).ok().and_then(|description| sdp_ufrag(&property_string(&description,"sdp")))})
        }).collect();
        let value = json!({"selfId":self.inner.self_id,"channelId":state.channel_id,"muted":state.muted,
            "deafened":state.deafened,"camera":state.camera,"screen":state.screen,"localTracks":local,"peers":peers});
        js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
    }
}

fn devices() -> Result<web_sys::MediaDevices, String> {
    web_sys::window()
        .ok_or("Окно браузера недоступно")?
        .navigator()
        .media_devices()
        .map_err(|_| {
            "Захват медиа недоступен. Откройте Spark в настольном приложении или через localhost."
                .into()
        })
}
fn tracks(stream: &MediaStream) -> Vec<MediaStreamTrack> {
    stream
        .get_tracks()
        .iter()
        .filter_map(|track| track.dyn_into().ok())
        .collect()
}
fn first_track(stream: &MediaStream, kind: &str) -> Option<MediaStreamTrack> {
    tracks(stream)
        .into_iter()
        .find(|track| track.kind() == kind)
}
fn stop_stream(stream: &MediaStream) {
    for track in tracks(stream) {
        track.set_onended(None);
        track.stop();
    }
}
fn js_error(error: JsValue) -> String {
    let name = property_string(&error, "name");
    let message = property_string(&error, "message");
    if !message.is_empty() {
        format!("{name}: {message}")
    } else {
        error
            .as_string()
            .unwrap_or_else(|| "ошибка браузера".into())
    }
}
fn property_string(value: &JsValue, key: &str) -> String {
    Reflect::get(value, &key.into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_default()
}
fn track_snapshot(kind: &str, track: &MediaStreamTrack) -> Value {
    json!({"kind":kind,"id":track.id(),"enabled":track.enabled(),"readyState":property_string(track.as_ref(), "readyState"),"muted":track.muted(),"settings":serde_json::from_str::<Value>(&js_sys::JSON::stringify(&track.get_settings()).ok().and_then(|v|v.as_string()).unwrap_or_default()).unwrap_or(Value::Null)})
}
async fn add_candidate(pc: &RtcPeerConnection, payload: &Value) -> Result<(), String> {
    let candidate = payload["candidate"]
        .as_str()
        .filter(|candidate| candidate.len() <= 4096)
        .ok_or("Некорректный ICE-кандидат")?;
    let init = RtcIceCandidateInit::new(candidate);
    init.set_sdp_mid(payload["sdpMid"].as_str());
    if let Some(ufrag) = payload["usernameFragment"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        Reflect::set(init.as_ref(), &"usernameFragment".into(), &ufrag.into()).map_err(js_error)?;
    }
    init.set_sdp_m_line_index(
        payload["sdpMLineIndex"]
            .as_u64()
            .and_then(|index| u16::try_from(index).ok()),
    );
    JsFuture::from(pc.add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init)))
        .await
        .map_err(js_error)?;
    Ok(())
}

fn candidate_matches(payload: &Value, ufrag: Option<&str>) -> bool {
    match payload["usernameFragment"]
        .as_str()
        .filter(|value| !value.is_empty())
    {
        Some(candidate_ufrag) => ufrag == Some(candidate_ufrag),
        None => true,
    }
}

fn sdp_ufrag(sdp: &str) -> Option<String> {
    sdp.lines().find_map(|line| {
        line.strip_prefix("a=ice-ufrag:")
            .map(|value| value.trim().to_string())
    })
}

fn restart_ice(pc: &RtcPeerConnection) -> Result<(), String> {
    // web-sys's stable WebIDL omits restartIce, although Chromium implements it.
    // The browser method is called from Rust; signaling/retry logic stays here.
    let method: js_sys::Function = Reflect::get(pc.as_ref(), &"restartIce".into())
        .map_err(js_error)?
        .dyn_into()
        .map_err(|_| "Браузер не поддерживает восстановление ICE".to_string())?;
    method.call0(pc.as_ref()).map_err(js_error)?;
    Ok(())
}

fn stop_share_audio() {
    if super::desktop().is_some() {
        spawn_local(async {
            let _ = super::desktop_call("stopShareAudio", &[]).await;
        });
    }
}
async fn capture_share_audio(label: &str) -> Result<MediaStream, String> {
    let devices = devices()?;
    let list = JsFuture::from(devices.enumerate_devices().map_err(js_error)?)
        .await
        .map_err(js_error)?;
    let id = Array::from(&list)
        .iter()
        .filter_map(|d| d.dyn_into::<web_sys::MediaDeviceInfo>().ok())
        .find(|d| d.label() == label)
        .map(|d| d.device_id())
        .ok_or("Источник звука демонстрации не найден")?;
    let audio = Object::new();
    let exact = Object::new();
    Reflect::set(&exact, &"exact".into(), &id.into()).map_err(js_error)?;
    Reflect::set(&audio, &"deviceId".into(), &exact).map_err(js_error)?;
    for key in ["echoCancellation", "noiseSuppression", "autoGainControl"] {
        Reflect::set(&audio, &key.into(), &JsValue::FALSE).map_err(js_error)?;
    }
    let constraints = MediaStreamConstraints::new();
    constraints.set_audio(&audio);
    constraints.set_video(&JsValue::FALSE);
    JsFuture::from(
        devices
            .get_user_media_with_constraints(&constraints)
            .map_err(js_error)?,
    )
    .await
    .map_err(js_error)?
    .dyn_into()
    .map_err(|_| "Не удалось получить звук демонстрации".into())
}
