use super::*;
use serde::Serialize;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub theme: String,
    pub accent: String,
    pub zoom: u16,
    pub sounds: bool,
    pub volume: u16,
    pub noise_suppression: bool,
    pub share_audio: bool,
    pub peer_volumes: HashMap<String, [u16; 2]>,
    pub close_to_tray: bool,
    pub compact: bool,
    pub auto_updates: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            accent: "#ffb251".into(),
            zoom: 100,
            sounds: true,
            volume: 55,
            noise_suppression: true,
            share_audio: false,
            peer_volumes: HashMap::new(),
            close_to_tray: true,
            compact: false,
            auto_updates: true,
        }
    }
}

impl Preferences {
    pub fn peer_volume(&self, peer: &str, kind: &str) -> u16 {
        self.peer_volumes.get(peer).unwrap_or(&[100, 100])[usize::from(kind == "screen-audio")]
            .min(100)
    }
    pub fn set_peer_volume(&mut self, peer: &str, kind: &str, volume: u16) {
        self.peer_volumes.entry(peer.into()).or_insert([100, 100])
            [usize::from(kind == "screen-audio")] = volume.min(100);
    }
}

pub async fn load() -> Preferences {
    if desktop().is_some() {
        if let Ok(value) = desktop_call("getSettings", &[]).await {
            if let Some(text) = js_sys::JSON::stringify(&value)
                .ok()
                .and_then(|v| v.as_string())
            {
                if let Ok(p) = serde_json::from_str(&text) {
                    return p;
                }
            }
        }
    } else if let Some(text) = window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|s| s.get_item("spark-preferences").ok().flatten())
    {
        if let Ok(p) = serde_json::from_str(&text) {
            return p;
        }
    }
    Preferences::default()
}

pub fn apply(p: &Preferences) {
    if let Some(root) = document().document_element() {
        let _ = root.set_attribute("data-theme", &p.theme);
        let _ = root.set_attribute("data-compact", if p.compact { "true" } else { "false" });
        let style = format!(
            "--accent:{};--accent-rgb:{}",
            p.accent,
            (|| {
                let c = p.accent.trim_start_matches('#');
                Some(format!(
                    "{} {} {}",
                    u8::from_str_radix(c.get(0..2)?, 16).ok()?,
                    u8::from_str_radix(c.get(2..4)?, 16).ok()?,
                    u8::from_str_radix(c.get(4..6)?, 16).ok()?
                ))
            })()
            .unwrap_or_else(|| "255 178 81".into())
        );
        let _ = root.set_attribute("style", &style);
        if desktop().is_none() {
            if let Some(body) = document().body() {
                let _ = body.set_attribute("style", &format!("zoom:{}", f64::from(p.zoom) / 100.));
            }
        }
    }
}

pub fn persist(app: &AppRef) {
    let p = app.borrow().preferences.clone();
    apply(&p);
    let text = serde_json::to_string(&p).unwrap_or_default();
    let a = app.clone();
    spawn_local(async move {
        let result = if desktop().is_some() {
            match js_sys::JSON::parse(&text) {
                Ok(value) => desktop_call("setSettings", &[value]).await.map(|_| ()),
                Err(e) => Err(err_text(e)),
            }
        } else {
            window()
                .local_storage()
                .map_err(err_text)
                .and_then(|s| s.ok_or("Хранилище недоступно".into()))
                .and_then(|s| s.set_item("spark-preferences", &text).map_err(err_text))
        };
        if let Err(e) = result {
            error(&a, format!("Не удалось сохранить настройки: {e}"));
        }
    });
}

pub fn controls(p: &Preferences) -> String {
    let checked = |v| if v { "checked" } else { "" };
    let option = |value: &str, label: &str| {
        format!(
            "<option value='{value}' {}>{label}</option>",
            if p.theme == value { "selected" } else { "" }
        )
    };
    format!(
        "<div class='settings-section'><h3>Персонализация</h3><div class='settings-grid'><label class='field'>Тема<select data-field='pref-theme' data-testid='pref-theme'>{}{}{}</select></label><label class='field'>Акцентный цвет<input type='color' data-field='pref-accent' data-testid='pref-accent' value='{}'/></label></div><label class='field'>Масштаб интерфейса <output id='zoom-label'>{}%</output><input type='range' min='75' max='150' step='5' value='{}' data-field='pref-zoom' data-testid='pref-zoom'/></label><button data-action='reset-zoom' class='subtle'>Сбросить масштаб · Ctrl+0</button><label class='check-field'><input type='checkbox' data-field='pref-compact' data-testid='pref-compact' {} />Компактные сообщения</label></div><div class='settings-section'><h3>Голос и трансляция</h3><label class='check-field'><input type='checkbox' data-field='pref-noise' data-testid='pref-noise' {} />Шумоподавление микрофона</label><small>Убирает постоянный фоновый шум. Эхоподавление остаётся включённым.</small><label class='check-field'><input type='checkbox' data-field='pref-share-audio' data-testid='pref-share-audio' {} />Передавать системный звук со стримом</label><small>Для следующей демонстрации. Передаётся звук других приложений. Звук Spark исключается из демонстрации.</small></div><div class='settings-section'><h3>Звуки событий</h3><label class='check-field'><input type='checkbox' data-field='pref-sounds' data-testid='pref-sounds' {} />Звуковые уведомления</label><label class='field'>Громкость <output id='volume-label'>{}%</output><input type='range' min='0' max='100' step='5' value='{}' data-field='pref-volume' data-testid='pref-volume'/></label><div class='sound-previews'>{}</div><small>Собственные звуки Spark. Настройки сохраняются сразу.</small></div><div class='settings-section'><h3>Окно приложения</h3><label class='check-field'><input type='checkbox' data-field='pref-tray' data-testid='pref-tray' {} />При закрытии сворачивать в трей</label><small>Звонки и демонстрация продолжаются, пока Spark находится в трее.</small><button data-action='quit' data-testid='quit-app' class='danger-text'>Выйти из Spark</button></div>",
        option("dark", "Тёмная"),
        option("midnight", "Полночь"),
        option("light", "Светлая"),
        esc(&p.accent),
        p.zoom,
        p.zoom,
        checked(p.compact),
        checked(p.noise_suppression),
        checked(p.share_audio),
        checked(p.sounds),
        p.volume,
        p.volume,
        [
            ("connect", "Подключение"),
            ("peer-join", "Участник вошёл"),
            ("peer-leave", "Участник вышел"),
            ("disconnect", "Выход"),
            ("notification", "Сообщение"),
            ("invite", "Приглашение"),
            ("mic-on", "Микрофон вкл."),
            ("mic-off", "Микрофон выкл."),
            ("sound-on", "Звук вкл."),
            ("sound-off", "Звук выкл.")
        ]
        .iter()
        .map(|(id, label)| format!(
            "<button data-action='preview-sound' data-id='{id}'>{label} ▷</button>"
        ))
        .collect::<String>(),
        checked(p.close_to_tray)
    )
}

pub fn input(app: &AppRef, field: &str, value: &str, checked: bool) {
    if field == "pref-noise" {
        let manager = app.borrow().media.clone();
        let a = app.clone();
        spawn_local(async move {
            if let Some(manager) = manager {
                if let Err(e) = manager.set_noise_suppression(checked).await {
                    if let Some(input) = document()
                        .query_selector("[data-field='pref-noise']")
                        .ok()
                        .flatten()
                        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
                    {
                        input.set_checked(a.borrow().preferences.noise_suppression);
                    }
                    error(&a, format!("Не удалось изменить шумоподавление: {e}"));
                    return;
                }
            }
            a.borrow_mut().preferences.noise_suppression = checked;
            persist(&a);
            render_voice(&a);
        });
        return;
    }
    {
        let mut s = app.borrow_mut();
        let p = &mut s.preferences;
        match field {
            "pref-theme" if matches!(value, "dark" | "light" | "midnight") => {
                p.theme = value.into()
            }
            "pref-accent"
                if value.len() == 7
                    && value.starts_with('#')
                    && value[1..].chars().all(|c| c.is_ascii_hexdigit()) =>
            {
                p.accent = value.into()
            }
            "pref-zoom" => p.zoom = value.parse::<u16>().unwrap_or(100).clamp(75, 150),
            "pref-volume" => p.volume = value.parse::<u16>().unwrap_or(55).min(100),
            "pref-sounds" => p.sounds = checked,
            "pref-share-audio" => p.share_audio = checked,
            "pref-tray" => p.close_to_tray = checked,
            "pref-compact" => p.compact = checked,
            "pref-updates" => p.auto_updates = checked,
            _ => return,
        }
        html("zoom-label", &format!("{}%", p.zoom));
        html("volume-label", &format!("{}%", p.volume));
    }
    if field == "pref-share-audio" {
        let s = app.borrow();
        if let Some(m) = &s.media {
            m.configure_audio(s.preferences.noise_suppression, s.preferences.share_audio);
        }
    }
    persist(app);
}

pub fn sound(app: &AppRef, name: &str) {
    let (enabled, volume, deafened) = {
        let s = app.borrow();
        (
            s.preferences.sounds,
            s.preferences.volume,
            s.media_state.deafened,
        )
    };
    if !enabled || volume == 0 || (deafened && !matches!(name, "sound-on" | "sound-off")) {
        return;
    }
    if !matches!(
        name,
        "connect"
            | "disconnect"
            | "peer-join"
            | "peer-leave"
            | "notification"
            | "invite"
            | "mic-on"
            | "mic-off"
            | "sound-on"
            | "sound-off"
    ) {
        return;
    }
    let audio = {
        let mut s = app.borrow_mut();
        if !s.cue_audio.contains_key(name) {
            let Ok(audio) = web_sys::HtmlAudioElement::new_with_src(&format!("/sounds/{name}.wav"))
            else {
                return;
            };
            s.cue_audio.insert(name.into(), audio);
        }
        s.cue_audio.get(name).cloned()
    };
    if let Some(audio) = audio {
        audio.set_volume(f64::from(volume) / 100.);
        audio.set_current_time(0.);
        if let Ok(play) = audio.play() {
            spawn_local(async move {
                let _ = JsFuture::from(play).await;
            });
        }
    }
}
