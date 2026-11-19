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
//! **Capture is synchronous.** The bus listener writes the row before the
//! publish that carried the event returns, so whatever goes on to treat the
//! event as handled — the turn that ends after its `session.idle`, the
//! process that is killed right after — finds the delivery already in the
//! database. Listeners run after a durable event's own commit and outside
//! the storage lock.
//!
//! **Delivery is per hook.** Each hook's undelivered rows are a queue in
//! the order they were recorded. A pass works the queues of all hooks at
//! the same time and each queue from its head, and leaves a queue at its
//! first failure: a receiver that hangs or answers 500 holds up its own
//! later deliveries (they stay in order behind the one being retried) and
//! nobody else's.
//!
//! **One process delivers.** Of several servers sharing a data directory
//! all record rows and the one holding the outbox role's lock delivers
//! them, so a row is never sent by two processes at once; the others ask
//! for the role on every pass and one has it when the holder exits.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use alforria_core::owner::RoleLock;

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
    /// `trigger_text`: the one piece of content a delivery may carry.
    trigger_text: bool,
}

/// The most of a trigger's final answer a delivery carries (`trigger_text`).
const MAX_TRIGGER_TEXT: usize = 16 * 1024;

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

/// How long the delivery loop sleeps at most: rows another process
/// recorded, and a role its holder gave up, are noticed within this.
const POLL: Duration = Duration::from_secs(5);

/// How many of a hook's deliveries one pass reads; the next pass follows
/// at once while a queue is longer.
const PASS_BATCH: usize = 100;

/// At most one "dropped" warning per this long.
const DROP_LOG_INTERVAL_MS: u64 = 60_000;

/// Whether a receiver's answer other than 2xx is worth another attempt.
/// A 4xx says the request itself is wrong and sending it again will not
/// change that — except the three that speak of timing: 408 (request
/// timeout), 425 (too early) and 429 (too many requests).
pub fn permanent_status(status: u16) -> bool {
    (400..500).contains(&status) && !matches!(status, 408 | 425 | 429)
}

/// What one attempt came to.
enum Outcome {
    Delivered,
    /// Try again after the backoff.
    Retry(String),
    /// Never again: the row is closed as failed.
    Permanent(String),
}

/// The outbox in numbers, for `GET /attention`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutboxStats {
    /// Rows not delivered yet and not given up on.
    pub pending: u64,
    /// Rows given up on (refused for good, too old, hook removed) that
    /// `retention` still keeps.
    pub failed: u64,
    /// Undelivered rows this process deleted because more than
    /// `max_pending` were waiting, since it started.
    pub dropped: u64,
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
    /// Bus subscriptions, kept alive for as long as the outbox is.
    subscriptions: Mutex<Vec<alforria_core::Subscription>>,
    /// Wakes the delivery loop when a row is written.
    wake: Arc<tokio::sync::Notify>,
    /// Cancelled by [`Outbox::stop`]: the loop ends and the attempts in
    /// flight are dropped (their rows stay due).
    cancel: tokio_util::sync::CancellationToken,
    /// The delivery loop [`Outbox::spawn`] started.
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Which of the processes on this data directory delivers
    /// ([`Outbox::deliver_as`]). Without one this process always does.
    role: OnceLock<RoleLock>,
    /// Said once, when another process holds the role.
    follower_told: std::sync::atomic::AtomicBool,
    /// Rows dropped beyond `max_pending` since this process started.
    dropped: AtomicU64,
    /// When the last "dropped" warning was logged, ms (`0`: never).
    drop_logged: AtomicU64,
    http: reqwest::Client,
}

/// The file whose lock says which of the processes sharing a data
/// directory delivers its webhooks.
pub fn outbox_lock(data_dir: &std::path::Path) -> RoleLock {
    RoleLock::new(data_dir.join("lock").join("outbox.lock"))
}

/// An undelivered row nobody has given up on.
const PENDING: &str = "time_delivered IS NULL AND time_failed IS NULL";

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
        trigger_text: info.trigger_text == Some(true),
    })
}

/// The properties of an event that may leave the machine: identifiers and
/// short labels. Everything else — message text, tool input and output,
/// patterns, metadata — stays.
fn small_properties(data: &Value) -> Map<String, Value> {
    // `report` and `action` are the fixed words of `trigger.completed`
    // (`final` / `none`) and `memory.updated` (`created`, `updated`, …).
    const LABELS: &[&str] = &[
        "permission",
        "kind",
        "name",
        "status",
        "reason",
        "trigger",
        "report",
        "action",
    ];
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
            subscriptions: Mutex::new(Vec::new()),
            wake: Arc::new(tokio::sync::Notify::new()),
            cancel: tokio_util::sync::CancellationToken::new(),
            task: Mutex::new(None),
            role: OnceLock::new(),
            follower_told: std::sync::atomic::AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            drop_logged: AtomicU64::new(0),
            http,
        });
        Ok(outbox)
    }

    /// Whether any hook is configured.
    pub fn enabled(&self) -> bool {
        !self.hooks.is_empty()
    }

    pub fn settings(&self) -> OutboxSettings {
        self.settings
    }

    /// Deliver only while holding `role` (see [`outbox_lock`]): of the
    /// processes sharing a data directory, one sends. Set once, before the
    /// loop starts; a later call changes nothing.
    pub fn deliver_as(&self, role: RoleLock) {
        let _ = self.role.set(role);
    }

    /// Whether this process is the one that delivers, taking the place if
    /// it is free.
    fn delivers(&self) -> bool {
        let Some(role) = self.role.get() else {
            return true;
        };
        if role.acquire() {
            return true;
        }
        if !self.follower_told.swap(true, Ordering::SeqCst) {
            tracing::info!(
                "another process delivers this data directory's webhooks: this one records \
                 its events for it and takes over when that process exits"
            );
        }
        false
    }

    /// Pending, failed and dropped deliveries.
    pub fn stats(&self) -> OutboxStats {
        let count = |filter: &str| {
            self.storage
                .with_connection(|conn| {
                    conn.query_row(
                        &format!("SELECT COUNT(*) FROM outbox WHERE {filter}"),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                })
                .unwrap_or(0) as u64
        };
        OutboxStats {
            pending: count(PENDING),
            failed: count("time_failed IS NOT NULL"),
            dropped: self.dropped.load(Ordering::SeqCst),
        }
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

    /// Record an event if any hook selects it. Called from inside the bus
    /// publish, and the rows are written before it returns: nothing that
    /// follows the event can happen without them.
    pub fn capture(&self, payload: &Payload) {
        if self.wants(&payload.r#type) {
            self.record(payload);
        }
    }

    /// What is sent for an event: small, and the same on every attempt.
    fn body(&self, hook: &Hook, delivery_id: &str, payload: &Payload, now: u64) -> Value {
        let mut properties = small_properties(&payload.data);
        // A trigger's final answer is content, and stays here like all
        // content — unless this hook asked for it by name.
        if hook.trigger_text && payload.r#type == "trigger.completed" {
            if let Some(text) = payload.data.get("text").and_then(Value::as_str) {
                let mut end = text.len().min(MAX_TRIGGER_TEXT);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                properties.insert("text".to_string(), json!(&text[..end]));
            }
        }
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
            "properties": properties,
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
            let body = self.body(hook, &id, payload, now).to_string();
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
            self.cap_pending();
            self.wake.notify_one();
        }
    }

    /// Drop the oldest undelivered rows beyond `max_pending`. A drop is a
    /// delivery that will never be made: it is counted
    /// ([`OutboxStats::dropped`]) and logged.
    fn cap_pending(&self) {
        let dropped = self.storage.with_connection(|conn| {
            let pending: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM outbox WHERE {PENDING}"),
                [],
                |row| row.get(0),
            )?;
            let over = pending - self.settings.max_pending as i64;
            if over <= 0 {
                return Ok(0);
            }
            conn.execute(
                &format!(
                    "DELETE FROM outbox WHERE id IN \
                     (SELECT id FROM outbox WHERE {PENDING} ORDER BY rowid LIMIT ?1)"
                ),
                [over],
            )
        });
        match dropped {
            Ok(0) => {}
            Ok(dropped) => {
                let total =
                    self.dropped.fetch_add(dropped as u64, Ordering::SeqCst) + dropped as u64;
                let now = self.clock.now_ms().max(1);
                let last = self.drop_logged.load(Ordering::SeqCst);
                if (last == 0 || now.saturating_sub(last) >= DROP_LOG_INTERVAL_MS)
                    && self
                        .drop_logged
                        .compare_exchange(last, now, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    tracing::warn!(
                        dropped = total,
                        max_pending = self.settings.max_pending,
                        "outbox: more than hook.outbox.max_pending deliveries are waiting; the \
                         oldest are dropped and will never be delivered ({total} since this \
                         server started)"
                    );
                }
            }
            Err(err) => tracing::warn!("outbox: capping the undelivered rows failed: {err}"),
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
            )
        });
        if let Err(err) = result {
            tracing::warn!("outbox: prune failed: {err}");
        }
        self.cap_pending();
    }

    /// Every row, oldest first (tests, diagnostics).
    pub fn deliveries(&self) -> Vec<Delivery> {
        self.rows("?1 = ''", "", usize::MAX)
    }

    /// The head of a hook's queue: its oldest undelivered rows, in order.
    fn queue(&self, hook: &str) -> Vec<Delivery> {
        self.rows(&format!("hook = ?1 AND {PENDING}"), hook, PASS_BATCH)
    }

    fn rows(&self, filter: &str, hook: &str, limit: usize) -> Vec<Delivery> {
        self.storage
            .with_connection(|conn| -> rusqlite::Result<Vec<Delivery>> {
                let mut statement = conn.prepare(&format!(
                    "SELECT id, hook, event_type, body, attempts, time_created, time_next, \
                     time_delivered, time_failed, last_error FROM outbox \
                     WHERE {filter} ORDER BY rowid LIMIT ?2"
                ))?;
                let limit = i64::try_from(limit).unwrap_or(i64::MAX);
                let rows = statement.query_map(rusqlite::params![hook, limit], |row| {
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

    /// Close the undelivered rows of hooks the config no longer has.
    fn abandon_removed_hooks(&self) {
        let now = self.clock.now_ms() as i64;
        let result = self
            .storage
            .with_connection(|conn| -> rusqlite::Result<()> {
                let mut statement =
                    conn.prepare(&format!("SELECT DISTINCT hook FROM outbox WHERE {PENDING}"))?;
                let owed: Vec<String> = statement
                    .query_map([], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?;
                for hook in owed {
                    if self.hooks.iter().any(|known| known.key == hook) {
                        continue;
                    }
                    conn.execute(
                        &format!(
                            "UPDATE outbox SET time_failed = ?2, last_error = ?3 \
                         WHERE hook = ?1 AND {PENDING}"
                        ),
                        rusqlite::params![hook, now, "the hook is no longer configured"],
                    )?;
                }
                Ok(())
            });
        if let Err(err) = result {
            tracing::warn!("outbox: closing a removed hook's deliveries failed: {err}");
        }
    }

    /// One attempt at one delivery, and its result written to the row.
    async fn attempt(&self, hook: &Hook, delivery: &Delivery) -> Outcome {
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
        let outcome = match response {
            Ok(response) if response.status().is_success() => Outcome::Delivered,
            Ok(response) => {
                let status = response.status().as_u16();
                let error = format!("the receiver answered {status}");
                if permanent_status(status) {
                    Outcome::Permanent(error)
                } else {
                    Outcome::Retry(error)
                }
            }
            // No URL in the message: it may carry credentials.
            Err(err) if err.is_timeout() => {
                Outcome::Retry("the receiver did not answer in time".to_string())
            }
            Err(err) if err.is_connect() => {
                Outcome::Retry("could not connect to the receiver".to_string())
            }
            Err(_) => Outcome::Retry("the request failed".to_string()),
        };
        let now = self.clock.now_ms();
        let written = self.storage.with_connection(|conn| match &outcome {
            Outcome::Delivered => conn.execute(
                "UPDATE outbox SET attempts = ?2, time_delivered = ?3, last_error = NULL WHERE id = ?1",
                rusqlite::params![delivery.id, attempt, now as i64],
            ),
            Outcome::Retry(error) => {
                let next = now + self.backoff(attempt).as_millis() as u64;
                conn.execute(
                    "UPDATE outbox SET attempts = ?2, time_next = ?3, last_error = ?4 WHERE id = ?1",
                    rusqlite::params![delivery.id, attempt, next as i64, error],
                )
            }
            Outcome::Permanent(error) => conn.execute(
                "UPDATE outbox SET attempts = ?2, time_failed = ?3, last_error = ?4 WHERE id = ?1",
                rusqlite::params![delivery.id, attempt, now as i64, error],
            ),
        });
        if let Err(err) = written {
            tracing::warn!("outbox: could not record the result of a delivery: {err}");
        }
        if let Outcome::Permanent(error) = &outcome {
            tracing::warn!(
                hook = %hook.key,
                delivery = %delivery.id,
                event = %delivery.event_type,
                "outbox: {error}; the delivery is given up, not retried"
            );
        }
        outcome
    }

    /// One hook's pass: its queue from the head, in order, until a row is
    /// not due yet or an attempt fails. Returns how many attempts it made.
    async fn deliver_hook(&self, hook: &Hook, now: u64) -> usize {
        let mut attempted = 0;
        for delivery in self.queue(&hook.key) {
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
            // The head is backing off: what was recorded after it waits.
            if delivery.time_next > now {
                break;
            }
            attempted += 1;
            match self.attempt(hook, &delivery).await {
                Outcome::Delivered => {}
                Outcome::Retry(_) | Outcome::Permanent(_) => break,
            }
        }
        attempted
    }

    /// Try what is due, once: every hook's queue at the same time, each in
    /// order and up to its first failure. Returns how many attempts were
    /// made — none in a process that is not the one delivering.
    pub async fn deliver_due(&self) -> usize {
        if !self.delivers() {
            return 0;
        }
        self.abandon_removed_hooks();
        let now = self.clock.now_ms();
        let passes = self.hooks.iter().map(|hook| self.deliver_hook(hook, now));
        futures::future::join_all(passes).await.into_iter().sum()
    }

    /// When the next delivery is due, if one is waiting: the earliest of
    /// the hooks' heads.
    fn next_due(&self) -> Option<u64> {
        self.storage
            .with_connection(|conn| -> rusqlite::Result<Option<i64>> {
                // With a bare MIN, SQLite takes the other columns from the
                // row that holds the minimum: each hook's oldest row.
                let mut statement = conn.prepare(&format!(
                    "SELECT MIN(rowid), time_next FROM outbox WHERE {PENDING} GROUP BY hook"
                ))?;
                let heads = statement.query_map([], |row| row.get::<_, i64>(1))?;
                let mut earliest = None;
                for head in heads {
                    let head = head?;
                    earliest = Some(earliest.map_or(head, |known: i64| known.min(head)));
                }
                Ok(earliest)
            })
            .ok()
            .flatten()
            .map(|due| due.max(0) as u64)
    }

    /// Deliver until [`Outbox::stop`]. Rows written before this starts —
    /// by an earlier process — are picked up on the first pass.
    pub async fn run(self: Arc<Self>) {
        if !self.enabled() {
            return;
        }
        loop {
            // A stop drops the attempts in flight: their rows stay due and
            // go out again, with the same delivery id, on the next start.
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = self.deliver_due() => {}
            }
            self.prune();
            let now = self.clock.now_ms();
            let wait = match self.next_due() {
                Some(due) if self.role.get().is_none_or(RoleLock::held) => {
                    Duration::from_millis(due.saturating_sub(now))
                        .clamp(Duration::from_millis(10), POLL)
                }
                _ => POLL,
            };
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(wait) => {}
            }
        }
        // Another process may deliver from here on.
        if let Some(role) = self.role.get() {
            role.release();
        }
    }

    /// Start the delivery loop on the current runtime, once.
    pub fn spawn(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let mut task = self.task.lock().unwrap_or_else(|p| p.into_inner());
        if task.is_none() {
            *task = Some(tokio::spawn(self.clone().run()));
        }
    }

    /// Tell the delivery loop to end. Returns at once.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// [`Outbox::stop`], then wait for the loop [`Outbox::spawn`] started.
    /// It never waits for a receiver: an attempt in flight is dropped.
    pub async fn shutdown(&self) {
        self.stop();
        let task = self.task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(task) = task {
            let _ = task.await;
        }
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
