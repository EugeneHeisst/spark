//! Native update integration; release validation and installation stay in Electron.
use super::*;

#[derive(Clone, Default, PartialEq, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Status {
    pub state: String,
    pub current_version: String,
    pub version: Option<String>,
    pub percent: Option<f64>,
    pub message: String,
}

pub fn activity(app: &AppRef) -> Value {
    let s = app.borrow();
    json!({"inCall":s.media_state.channel_id.is_some(),"transferring":s.upload.is_some(),"hosting":s.conn.as_ref().is_some_and(|c|c.hosting)})
}

pub fn report_activity(app: &AppRef) {
    if desktop().is_none() {
        return;
    }
    let input = activity(app).to_string();
    spawn_local(async move {
        if let Ok(value) = js_sys::JSON::parse(&input) {
            let _ = desktop_call("setActivity", &[value]).await;
        }
    });
}

pub fn panel(app: &AppRef) -> String {
    let s = app.borrow();
    let u = &s.update;
    let label = match u.state.as_str() {
        "checking" => "Проверяем обновления…".into(),
        "downloading" => format!(
            "Скачиваем Spark {} · {}%",
            u.version.as_deref().unwrap_or(""),
            u.percent.unwrap_or(0.).clamp(0., 100.).round()
        ),
        "ready" => format!(
            "Spark {} готов к установке",
            u.version.as_deref().unwrap_or("")
        ),
        "available" => format!("Доступен Spark {}", u.version.as_deref().unwrap_or("")),
        "installing" => "Устанавливаем обновление…".into(),
        "error" => "Не удалось получить обновление".into(),
        "unsupported" => "Обновление доступно в установленной сборке".into(),
        _ => {
            if u.message.is_empty() {
                format!("Текущая версия: {}", s.version)
            } else {
                u.message.clone()
            }
        }
    };
    let blocked = s.media_state.channel_id.is_some()
        || s.upload.is_some()
        || s.conn.as_ref().is_some_and(|c| c.hosting);
    let busy = matches!(u.state.as_str(), "checking" | "downloading" | "installing");
    let install = if u.state == "ready" {
        format!(
            "<button class='primary' data-action='install-update' data-testid='install-update' {}>Перезапустить и обновить</button>",
            if blocked { "disabled" } else { "" }
        )
    } else {
        String::new()
    };
    format!(
        "<p data-testid='updates-status' role='status'>{}</p>{}<div class='update-actions'><button data-action='check-updates' data-testid='check-updates' {}>Проверить обновления</button>{install}</div><small>{}</small>",
        esc(&label),
        if u.state == "error" {
            format!("<small class='update-error'>{}</small>", esc(&u.message))
        } else {
            String::new()
        },
        if busy || u.state == "unsupported" {
            "disabled"
        } else {
            ""
        },
        if blocked {
            "Перед установкой завершите звонок и передачу файлов. Если вы создатель, остановите комнату."
        } else {
            "Обновление установится после нажатия кнопки и перезапуска Spark. Настройки и история сохраняются."
        }
    )
}

pub fn controls(app: &AppRef) -> String {
    let checked = if app.borrow().preferences.auto_updates {
        "checked"
    } else {
        ""
    };
    format!(
        "<div class='settings-section'><h3>Обновления Spark</h3><label class='check-field'><input type='checkbox' data-field='pref-updates' data-testid='pref-updates' {checked} />Автоматически проверять и скачивать обновления</label><small>Источник: публичные релизы EugeneHeisst/spark на GitHub.</small><div id='updates-panel'>{}</div></div>",
        panel(app)
    )
}

fn poll(app: &AppRef) {
    if app.borrow().update_poll_busy {
        return;
    }
    app.borrow_mut().update_poll_busy = true;
    let a = app.clone();
    spawn_local(async move {
        let result = desktop_call("updateStatus", &[]).await;
        a.borrow_mut().update_poll_busy = false;
        if let Ok(value) = result {
            if let Some(status) = js_sys::JSON::stringify(&value)
                .ok()
                .and_then(|v| v.as_string())
                .and_then(|text| serde_json::from_str::<Status>(&text).ok())
            {
                let (changed, notify, viewer) = {
                    let s = a.borrow();
                    (
                        s.update != status,
                        s.update.state != "ready" && status.state == "ready",
                        s.modal.as_deref() == Some("viewer"),
                    )
                };
                a.borrow_mut().update = status;
                if changed {
                    html("updates-panel", &panel(&a));
                    if !viewer && a.borrow().modal.as_deref() != Some("settings") {
                        render_overlay(&a);
                    }
                }
                if notify {
                    preferences::sound(&a, "notification");
                }
            }
        }
    });
}

pub fn start(app: &AppRef) {
    if desktop().is_none() {
        return;
    }
    report_activity(app);
    spawn_local(async move {
        let _ = desktop_call("ready", &[]).await;
    });
    poll(app);
    let weak = Rc::downgrade(app);
    let timer = Closure::<dyn FnMut()>::new(move || {
        if let Some(a) = weak.upgrade() {
            report_activity(&a);
            poll(&a);
        }
    });
    if window()
        .set_interval_with_callback_and_timeout_and_arguments_0(
            timer.as_ref().unchecked_ref(),
            2000,
        )
        .is_ok()
    {
        timer.forget();
    }
}

pub fn action(app: &AppRef, install: bool) {
    let payload = activity(app);
    if install
        && ["inCall", "transferring", "hosting"]
            .iter()
            .any(|k| payload[k] == true)
    {
        error(
            app,
            "Перед обновлением завершите звонок и передачу файлов и остановите созданную комнату.",
        );
        return;
    }
    let a = app.clone();
    spawn_local(async move {
        let args = if install {
            js_sys::JSON::parse(&payload.to_string())
                .map(|v| vec![v])
                .unwrap_or_default()
        } else {
            vec![]
        };
        if let Err(e) = desktop_call(
            if install {
                "installUpdate"
            } else {
                "checkUpdates"
            },
            &args,
        )
        .await
        {
            error(&a, e);
        }
        poll(&a);
    });
}
