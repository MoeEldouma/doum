//! Alerts and questions: desktop pop-ups and phone pushes (both with answer buttons for questions), webhooks, and a
//! log of recent alerts for the panel and `doum alerts`.
//!
//! Phone answers: a question's ntfy buttons POST `<question id> <choice> <one-time code>` to a second private topic.
//! Doum subscribes to that topic (an outgoing connection: nothing on this machine is opened to the internet) and hands
//! each answer to the local API, which checks the code. Everything runs on background threads: a broken notifier
//! never blocks scheduling.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::NotifyCfg;
use crate::store::Decision;

/// What an alert is about. It sets the urgency and the phone's icon (an ntfy emoji tag).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Question,
    Paused,
    Resumed,
    Finished,
    Failed,
    Outside,
    Stuck,
    Decided,
}

impl Kind {
    fn tag(self) -> &'static str {
        match self {
            Kind::Question => "question",
            Kind::Paused => "pause_button",
            Kind::Resumed => "arrow_forward",
            Kind::Finished => "white_check_mark",
            Kind::Failed => "x",
            Kind::Outside => "warning",
            Kind::Stuck => "hourglass_flowing_sand",
            Kind::Decided => "ballot_box_with_check",
        }
    }
    fn high(self) -> bool {
        matches!(self, Kind::Question | Kind::Failed | Kind::Outside | Kind::Stuck)
    }
}

/// One alert, as kept in the log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertRec {
    pub t: u64,
    pub kind: Kind,
    pub title: String,
    pub body: String,
}

/// Alerts kept for the panel and `doum alerts` (also saved with the daemon's state).
const KEEP: usize = 100;

#[derive(Clone)]
pub struct Notifier {
    cfg: NotifyCfg,
    log: Arc<Mutex<Vec<AlertRec>>>,
    version: Arc<AtomicU64>,
}

impl Notifier {
    pub fn new(cfg: NotifyCfg, past: Vec<AlertRec>) -> Self {
        Self {
            cfg,
            log: Arc::new(Mutex::new(past)),
            version: Arc::new(AtomicU64::new(0)),
        }
    }

    fn record(&self, kind: Kind, title: &str, body: &str) {
        tracing::info!(%title, %body, ?kind, "alert");
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.push(AlertRec {
            t: crate::job::now(),
            kind,
            title: title.into(),
            body: body.into(),
        });
        let extra = log.len().saturating_sub(KEEP);
        log.drain(..extra);
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Newest first.
    pub fn recent(&self, n: usize) -> Vec<AlertRec> {
        let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        log.iter().rev().take(n).cloned().collect()
    }

    /// Oldest first, for saving.
    pub fn all(&self) -> Vec<AlertRec> {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Changes whenever an alert is recorded (so the daemon knows to save).
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    pub fn alert(&self, kind: Kind, title: &str, body: &str) {
        self.record(kind, title, body);
        let (cfg, title, body) = (self.cfg.clone(), title.to_string(), body.to_string());
        std::thread::spawn(move || {
            if cfg.desktop {
                let mut n = notify_rust::Notification::new();
                n.appname("Doum").summary(&title).body(&body);
                if kind.high() {
                    n.urgency(notify_rust::Urgency::Critical);
                }
                let _ = n.show();
            }
            push(&cfg, &title, &body, kind, None);
        });
    }

    /// Ask a question: desktop buttons answer through the local API, phone buttons through the answers topic.
    pub fn ask(&self, d: &Decision, api: &str, token: &str) {
        let body = question_body(d);
        self.record(Kind::Question, &d.title, &body);
        let (cfg, d, api, token) = (self.cfg.clone(), d.clone(), api.to_string(), token.to_string());
        std::thread::spawn(move || {
            push(&cfg, &d.title, &body, Kind::Question, Some(&d));
            if !cfg.desktop {
                return;
            }
            let mut n = notify_rust::Notification::new();
            n.appname("Doum")
                .summary(&d.title)
                .body(&body)
                .urgency(notify_rust::Urgency::Critical);
            for o in &d.options {
                n.action(o, &button(&d, o));
            }
            // stays up until the question's deadline; then the default applies and a follow-up says what was chosen
            let secs = d.deadline.saturating_sub(crate::job::now()).max(5);
            n.timeout(notify_rust::Timeout::Milliseconds((secs * 1000) as u32));
            if let Ok(h) = n.show() {
                h.wait_for_action(|choice| {
                    if d.options.iter().any(|o| o == choice) {
                        answer_locally(&api, &token, d.id, choice, None, "desktop");
                    }
                });
            }
        });
    }
}

/// The question as shown everywhere: the situation, one line per choice, and what happens without an answer.
pub fn question_body(d: &Decision) -> String {
    let left = d.deadline.saturating_sub(d.created_at);
    format!(
        "{}\n\nNo answer in {}: {}.",
        d.text,
        crate::daemon::fmt_dur(left),
        label(&d.recommended)
    )
}

fn button(d: &Decision, option: &str) -> String {
    if option == d.recommended {
        format!("{} (recommended)", label(option))
    } else {
        label(option).to_string()
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

/// `https://ntfy.sh/topic` -> (`https://ntfy.sh`, `topic`).
fn split_topic(url: &str) -> Option<(&str, &str)> {
    let url = url.trim_end_matches('/');
    let (base, topic) = url.rsplit_once('/')?;
    (base.contains("://") && !topic.is_empty()).then_some((base, topic))
}

/// Where phone answers go: `[notify] ntfy_answers_url`, or the alerts topic with `-answers` added.
pub fn answers_url(cfg: &NotifyCfg) -> Option<String> {
    if !cfg.phone_answers || cfg.ntfy_url.is_empty() {
        return None;
    }
    Some(if cfg.ntfy_answers_url.is_empty() {
        format!("{}-answers", cfg.ntfy_url.trim_end_matches('/'))
    } else {
        cfg.ntfy_answers_url.clone()
    })
}

/// The ntfy message (JSON publishing), with answer buttons for a question.
pub fn ntfy_message(cfg: &NotifyCfg, title: &str, body: &str, kind: Kind, d: Option<&Decision>) -> Option<(String, Value)> {
    let (base, topic) = split_topic(&cfg.ntfy_url)?;
    let mut m = json!({
        "topic": topic,
        "title": title,
        "message": body,
        "priority": if kind.high() { 4 } else { 3 },
        "tags": [kind.tag()],
    });
    if let (Some(d), Some(reply)) = (d, answers_url(cfg)) {
        // ntfy shows at most three buttons; every question has three choices
        let actions: Vec<Value> = d
            .options
            .iter()
            .take(3)
            .map(|o| {
                json!({
                    "action": "http",
                    "label": button(d, o),
                    "url": reply,
                    "method": "POST",
                    "body": format!("{} {} {}", d.id, o, d.code),
                    "clear": true,
                })
            })
            .collect();
        m["actions"] = Value::Array(actions);
    }
    Some((base.to_string(), m))
}

fn push(cfg: &NotifyCfg, title: &str, body: &str, kind: Kind, d: Option<&Decision>) {
    let c = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(8)).build() {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some((base, msg)) = ntfy_message(cfg, title, body, kind, d) {
        let _ = c.post(base).json(&msg).send();
    }
    for url in &cfg.webhooks {
        let _ = c
            .post(url)
            .json(&json!({ "source": "doum", "kind": kind, "title": title, "body": body, "decision": d }))
            .send();
    }
}

fn answer_locally(api: &str, token: &str, id: u64, choice: &str, code: Option<&str>, from: &str) {
    let _ = reqwest::blocking::Client::new()
        .post(format!("{api}/api/decisions/{id}"))
        .header("x-doum-token", token)
        .json(&json!({ "choice": choice, "code": code, "from": from }))
        .timeout(Duration::from_secs(5))
        .send();
}

/// `<question id> <choice> <code>` from a phone button.
pub fn parse_phone_answer(text: &str) -> Option<(u64, String, String)> {
    let mut w = text.split_whitespace();
    let (id, choice, code) = (w.next()?.parse().ok()?, w.next()?.to_string(), w.next()?.to_string());
    w.next().is_none().then_some((id, choice, code))
}

/// Listen for phone answers on the answers topic, for as long as the daemon runs (reconnecting as needed).
pub fn listen_for_answers(cfg: &NotifyCfg, api: String, token: String) {
    let Some(url) = answers_url(cfg) else { return };
    std::thread::spawn(move || {
        let mut since = crate::job::now();
        loop {
            let client = reqwest::blocking::Client::builder().timeout(None::<Duration>).build();
            if let Ok(resp) = client.and_then(|c| c.get(format!("{url}/json?since={since}")).send()) {
                tracing::info!("listening for phone answers");
                for line in BufReader::new(resp).lines() {
                    let Ok(line) = line else { break };
                    let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                    if v["event"] != "message" {
                        continue;
                    }
                    since = v["time"].as_u64().unwrap_or(since).max(since);
                    if let Some((id, choice, code)) = v["message"].as_str().and_then(parse_phone_answer) {
                        answer_locally(&api, &token, id, &choice, Some(&code), "phone");
                    }
                }
            }
            std::thread::sleep(Duration::from_secs(10));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::DecisionKind;

    fn decision() -> Decision {
        Decision {
            id: 12,
            job_id: 3,
            kind: DecisionKind::Fit,
            title: "llm-writer is waiting for the GPU".into(),
            text: "Needs 110 of 110 shards; 50 free.".into(),
            choices: vec![],
            options: vec!["smart".into(), "wait".into(), "pause".into()],
            recommended: "smart".into(),
            code: "c0ffee".into(),
            created_at: 100,
            deadline: 160,
            answer: None,
        }
    }

    #[test]
    fn phone_buttons_post_the_answer_with_its_code() {
        let cfg = NotifyCfg {
            ntfy_url: "https://ntfy.sh/doum-secret".into(),
            ..Default::default()
        };
        let (base, m) = ntfy_message(&cfg, "t", "b", Kind::Question, Some(&decision())).unwrap();
        assert_eq!(base, "https://ntfy.sh");
        assert_eq!(m["topic"], "doum-secret");
        assert_eq!(m["tags"][0], "question");
        let a = &m["actions"];
        assert_eq!(a.as_array().unwrap().len(), 3);
        assert_eq!(a[0]["label"], "Smart (recommended)");
        assert_eq!(a[0]["url"], "https://ntfy.sh/doum-secret-answers");
        assert_eq!(a[2]["body"], "12 pause c0ffee");
        assert_eq!(parse_phone_answer("12 pause c0ffee"), Some((12, "pause".into(), "c0ffee".into())));
        assert_eq!(parse_phone_answer("12 pause"), None);
        assert_eq!(parse_phone_answer("x pause c"), None);
    }

    #[test]
    fn no_buttons_when_phone_answers_are_off() {
        let cfg = NotifyCfg {
            ntfy_url: "https://ntfy.sh/doum-secret".into(),
            phone_answers: false,
            ..Default::default()
        };
        let (_, m) = ntfy_message(&cfg, "t", "b", Kind::Question, Some(&decision())).unwrap();
        assert!(m.get("actions").is_none());
        assert!(answers_url(&cfg).is_none());
    }

    #[test]
    fn the_log_keeps_the_newest() {
        let n = Notifier::new(
            NotifyCfg {
                desktop: false,
                ..Default::default()
            },
            vec![],
        );
        for i in 0..(KEEP + 5) {
            n.record(Kind::Finished, &format!("job {i}"), "");
        }
        assert_eq!(n.all().len(), KEEP);
        assert_eq!(n.recent(1)[0].title, format!("job {}", KEEP + 4));
    }

    #[test]
    fn question_text_says_what_happens_without_an_answer() {
        assert!(question_body(&decision()).ends_with("No answer in 1 min: Smart."));
    }
}
