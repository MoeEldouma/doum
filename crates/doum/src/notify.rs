//! Alerts and questions: desktop pop-ups (with buttons for questions), ntfy phone push, webhooks.
//! Everything is fire-and-forget on background threads: a broken notifier never blocks scheduling.

use serde_json::json;
use std::time::Duration;

use crate::config::NotifyCfg;
use crate::store::Decision;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Urgent,
}

impl Level {
    fn ntfy_priority(self) -> &'static str {
        match self {
            Level::Info => "default",
            Level::Warn => "high",
            Level::Urgent => "urgent",
        }
    }
}

#[derive(Clone)]
pub struct Notifier {
    cfg: NotifyCfg,
}

impl Notifier {
    pub fn new(cfg: NotifyCfg) -> Self {
        Self { cfg }
    }

    pub fn alert(&self, title: &str, body: &str, level: Level) {
        tracing::info!(%title, %body, ?level, "alert");
        let (cfg, title, body) = (self.cfg.clone(), title.to_string(), body.to_string());
        std::thread::spawn(move || {
            if cfg.desktop {
                let mut n = notify_rust::Notification::new();
                n.appname("doum").summary(&title).body(&body);
                if level == Level::Urgent {
                    n.urgency(notify_rust::Urgency::Critical);
                }
                let _ = n.show();
            }
            push(&cfg, &title, &body, level, None);
        });
    }

    /// Ask a question. Desktop buttons answer it through the local API; phone/webhooks get the text and the
    /// command to answer (`doum answer <id> <choice>`) since a phone cannot reach the local API.
    pub fn ask(&self, d: &Decision, api: &str, token: &str) {
        let (cfg, d, api, token) = (self.cfg.clone(), d.clone(), api.to_string(), token.to_string());
        std::thread::spawn(move || {
            let title = "doum: the GPU is busy".to_string();
            let body = format!("{}\n(no answer by the deadline → {})", d.text, d.recommended);
            push(
                &cfg,
                &title,
                &format!("{body}\nAnswer: doum answer {} <{}>", d.id, d.options.join("|")),
                Level::Warn,
                Some(&d),
            );
            if !cfg.desktop {
                return;
            }
            let mut n = notify_rust::Notification::new();
            n.appname("doum")
                .summary(&title)
                .body(&body)
                .urgency(notify_rust::Urgency::Critical);
            for o in &d.options {
                let label = if *o == d.recommended {
                    format!("{} (recommended)", label(o))
                } else {
                    label(o).to_string()
                };
                n.action(o, &label);
            }
            let secs = d.deadline.saturating_sub(crate::job::now()).max(5);
            n.timeout(notify_rust::Timeout::Milliseconds((secs * 1000) as u32));
            if let Ok(h) = n.show() {
                h.wait_for_action(|choice| {
                    if d.options.iter().any(|o| o == choice) {
                        let _ = reqwest::blocking::Client::new()
                            .post(format!("{api}/api/decisions/{}", d.id))
                            .header("x-doum-token", &token)
                            .json(&json!({ "choice": choice }))
                            .timeout(Duration::from_secs(5))
                            .send();
                    }
                });
            }
        });
    }
}

pub fn label(option: &str) -> &str {
    match option {
        "smart" => "Smart",
        "wait" => "Wait",
        "pause" => "Pause now",
        "pause-now" => "Pause now",
        "keep-waiting" => "Keep waiting",
        "cancel" => "Cancel",
        o => o,
    }
}

fn push(cfg: &NotifyCfg, title: &str, body: &str, level: Level, d: Option<&Decision>) {
    let c = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(c) => c,
        Err(_) => return,
    };
    if !cfg.ntfy_url.is_empty() {
        let _ = c
            .post(&cfg.ntfy_url)
            .header("Title", title)
            .header("Priority", level.ntfy_priority())
            .header("Tags", if d.is_some() { "question" } else { "gpu" })
            .body(body.to_string())
            .send();
    }
    for url in &cfg.webhooks {
        let _ = c
            .post(url)
            .json(&json!({ "source": "doum", "title": title, "body": body, "level": format!("{level:?}"), "decision": d }))
            .send();
    }
}
