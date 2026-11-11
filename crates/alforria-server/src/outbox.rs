//! The webhook outbox (alforria extension, `hook.webhook`).
//!
//! `permission.asked`, `question.asked`, `session.idle` and their like are
//! bus events: they reach whoever is connected at that moment and nobody
//! else. A control plane that must learn an agent is waiting — with no
//! client attached, across its own restarts and the agent's — needs them
//! delivered. So the selected events are written to a table (`outbox`,
//! alforria's own migration) and a worker delivers each one until the
//! receiver says 2xx:
//!
//! * **at least once** — a delivery is retried with exponential backoff
//!   until it succeeds or is `max_age` old, and the rows survive a restart;
//! * **deduplicable** — the row id is the delivery id, the same on every
//!   attempt, sent as `X-Alforria-Delivery` and as `id` in the body;
//! * **signed** — over `"<timestamp>.<body>"`, with HMAC-SHA256 (`secret`)
//!   or Ed25519 (`signing_key`);
//! * **small** — ids, the session and its title, the kind of event, times.
//!   Never message text, tool input or output.
//! * **bounded** — delivered and abandoned rows are pruned after
//!   `retention`; undelivered ones are capped at `max_pending`.
//!
//! Capture is synchronous and cheap (a channel send from the bus listener);
//! a writer thread turns captures into rows, so nothing touches the
//! database from inside an event publish.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use alforria_core::config::host::{HookInfo, WebhookInfo};
use alforria_core::event::definition::Payload;
use alforria_core::shell_env::glob_match;
use alforria_core::{Clock, Storage};
use base64::Engine;
use serde_json::{json, Map, Value};

/// How a hook signs its deliveries.
enum Signer {
    Hmac(ring::hmac::Key),
    Ed25519(ring::signature::Ed25519KeyPair),
}

struct Hook {
    /// What deliveries are tied to: the configured name, else the URL.
    key: String,
    url: String,
    events: Vec<String>,
    signer: Signer,
}

/// Retry and retention settings (`hook.outbox`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboxSettings {
    pub retention: Duration,
    pub max_age: Duration,
    pub retry_initial: Duration,
    pub retry_max: Duration,
    pub timeout: Duration,
    pub max_pending: u64,
}

impl Default for OutboxSettings {
    fn default() -> Self {
        OutboxSettings {
            retention: Duration::from_secs(24 * 3600),
            max_age: Duration::from_secs(72 * 3600),
            retry_initial: Duration::from_secs(5),
            retry_max: Duration::from_secs(300),
            timeout: Duration::from_secs(10),
            max_pending: 10_000,
        }
    }
}

enum Message {
    Event(Box<Payload>),
    /// Reply once everything sent before this has been written.
    Flush(mpsc::Sender<()>),
}

/// One row, as [`Outbox::deliveries`] reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: String,
    pub hook: String,
    pub event_type: String,
    pub body: String,
    pub attempts: u32,
    pub time_created: u64,
    pub time_next: u64,
    pub delivered: bool,
    pub failed: bool,
    pub last_error: Option<String>,
}

/// The outbox of one server.
pub struct Outbox {
    storage: Arc<Storage>,
    hooks: Vec<Hook>,
    settings: OutboxSettings,
    clock: Arc<dyn Clock>,
    capture: Mutex<Option<mpsc::Sender<Message>>>,
    /// Bus subscriptions, kept alive for as long as the outbox is.
    subscriptions: Mutex<Vec<alforria_core::Subscription>>,
    /// Wakes the delivery loop when a row is written.
    wake: Arc<tokio::sync::Notify>,
    stopped: AtomicBool,
    http: reqwest::Client,
}

fn decode_seed(text: &str) -> Option<[u8; 32]> {
    let text = text.trim();
    let bytes = if text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(text).ok()?
    } else {
        let text = text.trim_end_matches('=');
        base64::engine::general_purpose::STANDARD_NO_PAD
            .decode(text)
            .or_else(|_| base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text))
            .ok()?
    };
    bytes.try_into().ok()
}

fn hook_from_config(index: usize, info: &WebhookInfo) -> Result<Hook, String> {
    let at = |message: &str| format!("hook.webhook[{index}]: {message}");
    let scheme_ok = info.url.starts_with("http://") || info.url.starts_with("https://");
    if !scheme_ok || reqwest::Url::parse(&info.url).is_err() {
        return Err(at("url must be an http(s) URL"));
    }
    if info.events.iter().all(|event| event.trim().is_empty()) {
        return Err(at("events must name at least one event type or pattern"));
    }
    let signer = match (info.secret.as_deref(), info.signing_key.as_deref()) {
        (Some(secret), None) if !secret.is_empty() => Signer::Hmac(ring::hmac::Key::new(
            ring::hmac::HMAC_SHA256,
            secret.as_bytes(),
        )),
        (None, Some(key)) => {
            let seed = decode_seed(key)
                .ok_or_else(|| at("signing_key must be base64 or hex of a 32-byte Ed25519 seed"))?;
            Signer::Ed25519(
                ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed)
                    .map_err(|_| at("signing_key is not a usable Ed25519 seed"))?,
            )
        }
        (Some(_), Some(_)) => return Err(at("set secret or signing_key, not both")),
        _ => {
            return Err(at(
                "a secret or a signing_key is required: deliveries are always signed",
            ))
        }
    };
    Ok(Hook {
        key: info
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| info.url.clone()),
        url: info.url.clone(),
        events: info
            .events
            .iter()
            .map(|event| event.trim().to_string())
            .filter(|event| !event.is_empty())
            .collect(),
        signer,
    })
}

/// The properties of an event that may leave the machine: identifiers and
/// short labels. Everything else — message text, tool input and output,
/// patterns, metadata — stays.
fn small_properties(data: &Value) -> Map<String, Value> {
    const LABELS: &[&str] = &["permission", "kind", "name", "status", "reason", "trigger"];
    let mut out = Map::new();
    let Some(fields) = data.as_object() else {
        return out;
    };
    for (key, value) in fields {
        let is_id = key == "id" || key.ends_with("ID") || key.ends_with("Id");
        match value {
            Value::String(text)
                if (is_id || LABELS.contains(&key.as_str())) && text.len() <= 200 =>
            {
                out.insert(key.clone(), value.clone());
            }
            Value::Bool(_) | Value::Number(_) => {
                out.insert(key.clone(), value.clone());
            }
            _ => {}
        }
    }
    // `session.error` carries `{ error: { name, data: { message } } }`: the
    // kind of error is a label, its message is not ours to forward.
    if let Some(name) = data
        .get("error")
        .and_then(|error| error.get("name"))
        .and_then(Value::as_str)
    {
        out.insert("error".to_string(), json!(name));
    }
    out
}

impl Outbox {
    /// Build the outbox for a `hook` config. With no hooks it captures and
    /// delivers nothing.
    pub fn new(storage: Arc<Storage>, config: Option<&HookInfo>) -> Result<Arc<Outbox>, String> {
        Outbox::with_clock(
            storage,
            config,
            Arc::new(alforria_core::catalog::SystemClock),
        )
    }

    /// [`Outbox::new`] with the clock injected (tests).
    pub fn with_clock(
        storage: Arc<Storage>,
        config: Option<&HookInfo>,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Outbox>, String> {
        let mut hooks = Vec::new();
        for (index, info) in config
            .and_then(|config| config.webhook.as_ref())
            .into_iter()
            .flatten()
            .enumerate()
        {
            let hook = hook_from_config(index, info)?;
            if hooks.iter().any(|other: &Hook| other.key == hook.key) {
                return Err(format!(
                    "hook.webhook[{index}]: another hook is already called {:?}; give one a name",
                    hook.key
                ));
            }
            hooks.push(hook);
        }
        let mut settings = OutboxSettings::default();
        if let Some(info) = config.and_then(|config| config.outbox.as_ref()) {
            let set =
                |target: &mut Duration,
                 value: Option<alforria_core::config::host::DurationSetting>| {
                    if let Some(value) = value {
                        *target = value.get();
                    }
                };
            set(&mut settings.retention, info.retention);
            set(&mut settings.max_age, info.max_age);
            set(&mut settings.retry_initial, info.retry_initial);
            set(&mut settings.retry_max, info.retry_max);
            set(&mut settings.timeout, info.timeout);
            if let Some(max_pending) = info.max_pending {
                settings.max_pending = max_pending.max(1);
            }
        }
        let http = reqwest::Client::builder()
            .timeout(settings.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("alforria/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| format!("webhook client: {err}"))?;
        let outbox = Arc::new(Outbox {
            storage,
            hooks,
            settings,
            clock,
            capture: Mutex::new(None),
            subscriptions: Mutex::new(Vec::new()),
            wake: Arc::new(tokio::sync::Notify::new()),
            stopped: AtomicBool::new(false),
            http,
        });
        if outbox.enabled() {
            let (tx, rx) = mpsc::channel::<Message>();
            *outbox.capture.lock().unwrap_or_else(|p| p.into_inner()) = Some(tx);
            // The writer holds a weak handle: it ends when the outbox does.
            let writer = Arc::downgrade(&outbox);
            std::thread::Builder::new()
                .name("alforria-outbox".to_string())
                .spawn(move || {
                    while let Ok(message) = rx.recv() {
                        let Some(outbox) = writer.upgrade() else {
                            break;
                        };
                        match message {
                            Message::Event(payload) => outbox.record(&payload),
                            Message::Flush(done) => {
                                let _ = done.send(());
                            }
                        }
                    }
                })
                .map_err(|err| format!("outbox writer: {err}"))?;
        }
        Ok(outbox)
    }

    /// Whether any hook is configured.
    pub fn enabled(&self) -> bool {
        !self.hooks.is_empty()
    }

    pub fn settings(&self) -> OutboxSettings {
        self.settings
    }

    /// Capture the events of a project instance from now on.
    pub fn attach(self: &Arc<Self>, events: &alforria_core::EventBus) {
        if !self.enabled() {
            return;
        }
        let outbox = Arc::downgrade(self);
        let subscription = events.listen(Arc::new(move |payload: &Payload| {
            if let Some(outbox) = outbox.upgrade() {
                outbox.capture(payload);
            }
        }));
        self.subscriptions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(subscription);
    }

    fn wants(&self, event_type: &str) -> bool {
        self.hooks.iter().any(|hook| {
            hook.events
                .iter()
                .any(|pattern| glob_match(event_type, pattern))
        })
    }

    /// Hand an event to the writer if any hook selects it. Called from
    /// inside the bus publish, so it only filters and sends.
    pub fn capture(&self, payload: &Payload) {
        if !self.wants(&payload.r#type) {
            return;
        }
        if let Some(tx) = self
            .capture
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            let _ = tx.send(Message::Event(Box::new(payload.clone())));
        }
    }

    /// Wait until every event captured so far is a row.
    pub fn flush(&self) {
        let tx = self
            .capture
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        if let Some(tx) = tx {
            let (done, wait) = mpsc::channel();
            if tx.send(Message::Flush(done)).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(5));
            }
        }
    }

    /// What is sent for an event: small, and the same on every attempt.
    fn body(&self, delivery_id: &str, payload: &Payload, now: u64) -> Value {
        let session_id = payload
            .data
            .get("sessionID")
            .and_then(Value::as_str)
            .map(str::to_string);
        let title = session_id.as_deref().and_then(|session_id| {
            self.storage.with_connection(|conn| {
                conn.query_row(
                    "SELECT title FROM session WHERE id = ?1",
                    [session_id],
                    |row| row.get::<_, String>(0),
                )
                .ok()
            })
        });
        let kind = match payload.r#type.as_str() {
            "permission.asked" => Some("permission"),
            "question.asked" => Some("question"),
            _ => None,
        };
        json!({
            "id": delivery_id,
            "type": payload.r#type,
            "event": payload.id,
            "time": now,
            "directory": payload.location.as_ref().map(|location| location.directory.clone()),
            "sessionID": session_id,
            "title": title,
            "kind": kind,
            "properties": small_properties(&payload.data),
        })
    }

    /// Write one row per hook that selects the event.
    fn record(&self, payload: &Payload) {
        let now = self.clock.now_ms();
        let mut wrote = false;
        for hook in &self.hooks {
            if !hook
                .events
                .iter()
                .any(|pattern| glob_match(&payload.r#type, pattern))
            {
                continue;
            }
            let id = format!("whd_{}", ulid::Ulid::new());
            let body = self.body(&id, payload, now).to_string();
            let inserted = self.storage.with_connection(|conn| {
                conn.execute(
                    "INSERT INTO outbox (id, hook, event_type, body, time_created, attempts, time_next) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 0, ?5)",
                    rusqlite::params![id, hook.key, payload.r#type, body, now as i64],
                )
            });
            match inserted {
                Ok(_) => wrote = true,
                Err(err) => tracing::warn!("outbox: could not record {}: {err}", payload.r#type),
            }
        }
        if wrote {
            self.prune();
            self.wake.notify_one();
        }
    }

    /// Bounded retention: finished rows older than `retention` go, and the
    /// oldest undelivered ones beyond `max_pending`.
    pub fn prune(&self) {
        let now = self.clock.now_ms() as i64;
        let cutoff = now - self.settings.retention.as_millis() as i64;
        let result = self.storage.with_connection(|conn| {
            conn.execute(
                "DELETE FROM outbox WHERE COALESCE(time_delivered, time_failed) IS NOT NULL \
                 AND COALESCE(time_delivered, time_failed) <= ?1",
                [cutoff],
            )?;
            conn.execute(
                "DELETE FROM outbox WHERE time_delivered IS NULL AND time_failed IS NULL AND id NOT IN \
                 (SELECT id FROM outbox WHERE time_delivered IS NULL AND time_failed IS NULL \
                  ORDER BY rowid DESC LIMIT ?1)",
                [self.settings.max_pending as i64],
            )
        });
        if let Err(err) = result {
            tracing::warn!("outbox: prune failed: {err}");
        }
    }

    /// Every row, oldest first (tests, diagnostics).
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.storage
            .with_connection(|conn| -> rusqlite::Result<Vec<Delivery>> {
                let mut statement = conn.prepare(
                    "SELECT id, hook, event_type, body, attempts, time_created, time_next, \
                     time_delivered, time_failed, last_error FROM outbox ORDER BY rowid",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok(Delivery {
                        id: row.get(0)?,
                        hook: row.get(1)?,
                        event_type: row.get(2)?,
                        body: row.get(3)?,
                        attempts: row.get::<_, i64>(4)? as u32,
                        time_created: row.get::<_, i64>(5)? as u64,
                        time_next: row.get::<_, i64>(6)? as u64,
                        delivered: row.get::<_, Option<i64>>(7)?.is_some(),
                        failed: row.get::<_, Option<i64>>(8)?.is_some(),
                        last_error: row.get(9)?,
                    })
                })?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    fn backoff(&self, attempts: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempts.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.settings
            .retry_initial
            .checked_mul(factor)
            .unwrap_or(self.settings.retry_max)
            .min(self.settings.retry_max)
    }

    fn signature(hook: &Hook, timestamp: u64, body: &str) -> String {
        let signed = format!("{timestamp}.{body}");
        match &hook.signer {
            Signer::Hmac(key) => format!(
                "t={timestamp},v1={}",
                hex::encode(ring::hmac::sign(key, signed.as_bytes()).as_ref())
            ),
            Signer::Ed25519(pair) => format!(
                "t={timestamp},ed25519={}",
                base64::engine::general_purpose::STANDARD
                    .encode(pair.sign(signed.as_bytes()).as_ref())
            ),
        }
    }

    fn finish(&self, id: &str, column: &str, error: Option<&str>) {
        let now = self.clock.now_ms() as i64;
        let _ = self.storage.with_connection(|conn| {
            conn.execute(
                &format!("UPDATE outbox SET {column} = ?2, last_error = ?3 WHERE id = ?1"),
                rusqlite::params![id, now, error],
            )
        });
    }

    /// Try every delivery that is due, once. Returns how many attempts
    /// were made.
    pub async fn deliver_due(&self) -> usize {
        let now = self.clock.now_ms();
        let due: Vec<Delivery> = self
            .deliveries()
            .into_iter()
            .filter(|delivery| !delivery.delivered && !delivery.failed && delivery.time_next <= now)
            .collect();
        let mut attempted = 0;
        for delivery in due {
            let Some(hook) = self.hooks.iter().find(|hook| hook.key == delivery.hook) else {
                self.finish(
                    &delivery.id,
                    "time_failed",
                    Some("the hook is no longer configured"),
                );
                continue;
            };
            if now.saturating_sub(delivery.time_created) > self.settings.max_age.as_millis() as u64
            {
                self.finish(
                    &delivery.id,
                    "time_failed",
                    Some(
                        delivery
                            .last_error
                            .as_deref()
                            .unwrap_or("not delivered within max_age"),
                    ),
                );
                continue;
            }
            attempted += 1;
            let attempt = delivery.attempts + 1;
            let timestamp = self.clock.now_ms() / 1000;
            let response = self
                .http
                .post(&hook.url)
                .header("content-type", "application/json")
                .header("x-alforria-delivery", &delivery.id)
                .header("x-alforria-event", &delivery.event_type)
                .header("x-alforria-timestamp", timestamp)
                .header("x-alforria-attempt", attempt)
                .header(
                    "x-alforria-signature",
                    Outbox::signature(hook, timestamp, &delivery.body),
                )
                .body(delivery.body.clone())
                .send()
                .await;
            let failure = match response {
                Ok(response) if response.status().is_success() => None,
                Ok(response) => Some(format!(
                    "the receiver answered {}",
                    response.status().as_u16()
                )),
                // No URL in the message: it may carry credentials.
                Err(err) if err.is_timeout() => {
                    Some("the receiver did not answer in time".to_string())
                }
                Err(err) if err.is_connect() => {
                    Some("could not connect to the receiver".to_string())
                }
                Err(_) => Some("the request failed".to_string()),
            };
            match failure {
                None => {
                    let now = self.clock.now_ms() as i64;
                    let _ = self.storage.with_connection(|conn| {
                        conn.execute(
                            "UPDATE outbox SET attempts = ?2, time_delivered = ?3, last_error = NULL WHERE id = ?1",
                            rusqlite::params![delivery.id, attempt, now],
                        )
                    });
                }
                Some(error) => {
                    let next = self.clock.now_ms() + self.backoff(attempt).as_millis() as u64;
                    let _ = self.storage.with_connection(|conn| {
                        conn.execute(
                            "UPDATE outbox SET attempts = ?2, time_next = ?3, last_error = ?4 WHERE id = ?1",
                            rusqlite::params![delivery.id, attempt, next as i64, error],
                        )
                    });
                }
            }
        }
        attempted
    }

    /// When the next undelivered row is due, if there is one.
    fn next_due(&self) -> Option<u64> {
        self.deliveries()
            .into_iter()
            .filter(|delivery| !delivery.delivered && !delivery.failed)
            .map(|delivery| delivery.time_next)
            .min()
    }

    /// Deliver until [`Outbox::stop`]. Rows written before this starts —
    /// by an earlier process — are picked up on the first pass.
    pub async fn run(self: Arc<Self>) {
        if !self.enabled() {
            return;
        }
        while !self.stopped.load(Ordering::SeqCst) {
            self.deliver_due().await;
            self.prune();
            let now = self.clock.now_ms();
            let wait = match self.next_due() {
                Some(due) => Duration::from_millis(due.saturating_sub(now).clamp(10, 60_000)),
                None => Duration::from_secs(60),
            };
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Stop the delivery loop after its current pass.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_identifiers_and_labels_leave() {
        let properties = small_properties(&json!({
            "id": "per_1",
            "sessionID": "ses_1",
            "permission": "bash",
            "patterns": ["rm -rf /"],
            "metadata": { "command": "cat ~/.ssh/id_ed25519" },
            "text": "the user's message",
            "status": "idle",
            "count": 3,
            "ok": true,
            "name": "x".repeat(500),
            "error": { "name": "ProviderAuthError", "data": { "message": "key sk-123 rejected" } }
        }));
        assert_eq!(
            Value::Object(properties),
            json!({
                "id": "per_1",
                "sessionID": "ses_1",
                "permission": "bash",
                "status": "idle",
                "count": 3,
                "ok": true,
                "error": "ProviderAuthError"
            })
        );
    }

    #[test]
    fn hooks_must_be_signed_and_well_formed() {
        let hook = |value: Value| {
            let info: WebhookInfo = serde_json::from_value(value).unwrap();
            hook_from_config(0, &info).map(|hook| hook.key)
        };
        let base = json!({ "url": "https://cp.example/hook", "events": ["session.*"] });
        let with = |extra: Value| {
            let mut value = base.clone();
            for (key, entry) in extra.as_object().unwrap() {
                value[key] = entry.clone();
            }
            value
        };
        assert_eq!(
            hook(with(json!({ "secret": "s" }))).unwrap(),
            "https://cp.example/hook"
        );
        assert_eq!(
            hook(with(json!({ "secret": "s", "name": "cp" }))).unwrap(),
            "cp"
        );
        assert!(hook(with(json!({ "signing_key": hex::encode([7u8; 32]) }))).is_ok());
        assert!(hook(base.clone()).unwrap_err().contains("always signed"));
        assert!(hook(with(json!({ "secret": "" }))).is_err());
        assert!(hook(with(
            json!({ "secret": "s", "signing_key": hex::encode([7u8; 32]) })
        ))
        .unwrap_err()
        .contains("not both"));
        assert!(hook(with(json!({ "signing_key": "short" }))).is_err());
        assert!(hook(json!({ "url": "ftp://x/y", "events": ["a"], "secret": "s" })).is_err());
        assert!(hook(json!({ "url": "https://x/y", "events": [], "secret": "s" })).is_err());
    }
}
