//! Share network sync — port of `packages/opencode/src/share/share-next.ts`
//! (`ShareNext`: share REST client, sync queue/flush, per-instance watchers)
//! and `packages/opencode/src/share/session.ts` (`SessionShare`: config
//! gate, `session.setShare`, auto-share).
//!
//! Every network touch goes through the [`ShareHttp`] seam and the account
//! variant through the [`ShareAccounts`] seam (default: no active account —
//! the account/console machinery is not ported, so Rust always uses the
//! legacy `/api/share` API until then).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use alforria_schema::session_v1::{V1SessionInfo, V1SessionShare};

use crate::event::bus::EventBus;
use crate::session::message::message_id;
use crate::session::store::SessionStore;
use crate::storage::Storage;
use crate::{Listener, Subscription};

/// Legacy API default base URL (`share-next.ts:210`).
pub const DEFAULT_BASE_URL: &str = "https://opncd.ai";

/// `Effect.delay(1000)` on the first queued item (`share-next.ts:141`).
pub const FLUSH_DELAY: Duration = Duration::from_millis(1000);

/// `ShareSchema` (`share-next.ts:38-42`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub id: String,
    pub url: String,
    pub secret: String,
}

/// `OPENCODE_DISABLE_SHARE` (`share-next.ts:23`).
pub fn share_disabled() -> bool {
    matches!(
        std::env::var("OPENCODE_DISABLE_SHARE").as_deref(),
        Ok("true") | Ok("1")
    )
}

// ---------------------------------------------------------------------------
// HTTP seam
// ---------------------------------------------------------------------------

/// One HTTP response from the share REST API.
#[derive(Debug, Clone)]
pub struct ShareHttpResponse {
    pub status: u16,
    pub body: String,
}

/// The share REST transport — a seam so tests never touch the network
/// (spec §6.2). Errors are transport failures; status codes are data.
pub trait ShareHttp: Send + Sync {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<ShareHttpResponse, String>;
    fn delete(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<ShareHttpResponse, String>;
}

/// Production [`ShareHttp`] — real HTTP over the workspace reqwest client
/// (the thread + one-shot runtime pattern of the remote-instruction fetch).
pub struct HttpShareClient;

impl ShareHttp for HttpShareClient {
    fn post(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<ShareHttpResponse, String> {
        http_execute("POST", url, headers, body)
    }

    fn delete(
        &self,
        url: &str,
        headers: &[(String, String)],
        body: &str,
    ) -> Result<ShareHttpResponse, String> {
        http_execute("DELETE", url, headers, body)
    }
}

/// `HttpClientRequest.bodyJson` sets `content-type: application/json`.
fn http_execute(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &str,
) -> Result<ShareHttpResponse, String> {
    let url = url.to_string();
    let headers = headers.to_vec();
    let body = body.to_string();
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|err| err.to_string())?
        .to_owned();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|err| err.to_string())?;
        runtime.block_on(async move {
            let client = reqwest::Client::new();
            let mut request = client.request(method, &url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }
            let response = request
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(|err| err.to_string())?;
            let status = response.status().as_u16();
            let text = response.text().await.map_err(|err| err.to_string())?;
            Ok(ShareHttpResponse { status, body: text })
        })
    })
    .join()
    .map_err(|_| "share request thread panicked".to_string())?
}

// ---------------------------------------------------------------------------
// Account + model seams
// ---------------------------------------------------------------------------

/// `Account.Service` active account — the console-variant inputs
/// (`share-next.ts:206-222`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveAccount {
    pub id: String,
    pub url: String,
    pub active_org_id: Option<String>,
}

/// The account seam. Defaults to no active account (spec §7.4).
pub trait ShareAccounts: Send + Sync {
    fn active(&self) -> Option<ActiveAccount>;
    fn token(&self, id: &str) -> Option<String>;
}

/// The default account seam: no active account → legacy API.
pub struct NoAccount;

impl ShareAccounts for NoAccount {
    fn active(&self) -> Option<ActiveAccount> {
        None
    }

    fn token(&self, _id: &str) -> Option<String> {
        None
    }
}

/// `Provider.Service.getModel` for the `model` sync items
/// (`share-next.ts:190-192`, `:283-289`).
pub trait ShareModels: Send + Sync {
    fn get_model(&self, provider_id: &str, model_id: &str) -> Result<Value, String>;
}

/// Default model seam — the provider runtime is wired in M7.7.
pub struct NoModels;

impl ShareModels for NoModels {
    fn get_model(&self, _provider_id: &str, _model_id: &str) -> Result<Value, String> {
        Err("provider runtime not wired (M7.7)".to_string())
    }
}

// ---------------------------------------------------------------------------
// API + request shapes
// ---------------------------------------------------------------------------

/// `api(resource)` (`share-next.ts:85-95`) — the legacy (`/api/share`) and
/// console (`/api/shares`) endpoint tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Api {
    resource: &'static str,
}

impl Api {
    pub fn legacy() -> Api {
        Api { resource: "share" }
    }

    pub fn console() -> Api {
        Api { resource: "shares" }
    }

    pub fn create(&self) -> String {
        format!("/api/{}", self.resource)
    }

    pub fn sync(&self, share_id: &str) -> String {
        format!("/api/{}/{share_id}/sync", self.resource)
    }

    pub fn remove(&self, share_id: &str) -> String {
        format!("/api/{}/{}", self.resource, share_id)
    }
}

/// `Req` (`share-next.ts:32-36`).
pub struct ShareReq {
    pub headers: Vec<(String, String)>,
    pub api: Api,
    pub base_url: String,
}

/// Insertion-ordered JSON object string (`{ "secret": …, "data": … }` keeps
/// the TS `bodyJson` field order).
fn json_object(fields: Vec<(&str, String)>) -> String {
    let body = fields
        .iter()
        .map(|(name, value)| {
            format!(
                "{}:{}",
                serde_json::to_string(name).unwrap_or_default(),
                value
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{body}}}")
}

fn json_of<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

// ---------------------------------------------------------------------------
// Sync queue items
// ---------------------------------------------------------------------------

/// One queued sync item (`Data`, `share-next.ts:51-70`). The key is the
/// dedup key computed by `key()` (`:97-110`).
#[derive(Debug, Clone, PartialEq)]
pub struct ShareItem {
    kind: &'static str,
    key: String,
    data: Value,
}

impl ShareItem {
    pub fn session(data: Value) -> ShareItem {
        ShareItem {
            kind: "session",
            key: "session".to_string(),
            data,
        }
    }

    pub fn message(id: &str, data: Value) -> ShareItem {
        ShareItem {
            kind: "message",
            key: format!("message/{id}"),
            data,
        }
    }

    pub fn part(message_id: &str, id: &str, data: Value) -> ShareItem {
        ShareItem {
            kind: "part",
            key: format!("part/{message_id}/{id}"),
            data,
        }
    }

    pub fn session_diff(data: Value) -> ShareItem {
        ShareItem {
            kind: "session_diff",
            key: "session_diff".to_string(),
            data,
        }
    }

    pub fn model(models: Vec<Value>) -> ShareItem {
        ShareItem {
            kind: "model",
            key: "model".to_string(),
            data: Value::Array(models),
        }
    }

    /// `{ type, data }` on the wire.
    fn wire(&self) -> String {
        json_object(vec![
            ("type", json_of(&self.kind)),
            ("data", json_of(&self.data)),
        ])
    }
}

// ---------------------------------------------------------------------------
// ShareNext
// ---------------------------------------------------------------------------

/// Per-instance share state (`State`, `share-next.ts:45-49`). The `shared`
/// cache also stores the known-absent marker (`share ?? null`).
struct ShareState {
    queue: HashMap<String, Vec<ShareItem>>,
    shared: HashMap<String, Option<Share>>,
}

/// Everything `ShareNext` is constructed with.
pub struct ShareInput {
    pub storage: Arc<Storage>,
    pub sessions: SessionStore,
    pub events: Arc<EventBus>,
    /// `config.enterprise?.url ?? "https://opncd.ai"`.
    pub base_url: String,
    pub disabled: bool,
    /// The instance context directory — watchers only observe this
    /// directory's events (`share-next.ts:171`).
    pub directory: String,
    pub http: Arc<dyn ShareHttp>,
    pub account: Arc<dyn ShareAccounts>,
    pub models: Arc<dyn ShareModels>,
    pub flush_delay: Duration,
}

/// The `ShareNext` service (`share-next.ts:112-361`).
pub struct ShareNext {
    storage: Arc<Storage>,
    sessions: SessionStore,
    events: Arc<EventBus>,
    base_url: String,
    disabled: bool,
    directory: String,
    http: Arc<dyn ShareHttp>,
    account: Arc<dyn ShareAccounts>,
    models: Arc<dyn ShareModels>,
    flush_delay: Duration,
    state: Mutex<ShareState>,
    subscriptions: Mutex<Vec<Subscription>>,
}

impl ShareNext {
    pub fn new(input: ShareInput) -> Arc<ShareNext> {
        Arc::new(ShareNext {
            storage: input.storage,
            sessions: input.sessions,
            events: input.events,
            base_url: input.base_url,
            disabled: input.disabled,
            directory: input.directory,
            http: input.http,
            account: input.account,
            models: input.models,
            flush_delay: input.flush_delay,
            state: Mutex::new(ShareState {
                queue: HashMap::new(),
                shared: HashMap::new(),
            }),
            subscriptions: Mutex::new(Vec::new()),
        })
    }

    /// `init` (`share-next.ts:301-304` + the watchers at `:166-201`): the
    /// instance's event watchers. Disabled shares register nothing.
    pub fn init(self: &Arc<Self>) {
        if self.disabled {
            return;
        }
        let weak = Arc::downgrade(self);
        let directory = self.directory.clone();
        let listener: Listener = Arc::new(move |event| {
            let Some(share) = weak.upgrade() else {
                return;
            };
            let Some(location) = &event.location else {
                return;
            };
            if location.directory != directory {
                return;
            }
            share.on_event(&event.r#type, &event.data);
        });
        let subscription = self.events.listen(listener);
        self.subscriptions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(subscription);
    }

    /// The five watchers (`share-next.ts:179-200`).
    fn on_event(self: &Arc<Self>, type_: &str, data: &Value) {
        match type_ {
            "session.updated" => {
                let Some(info) = data.get("info") else {
                    return;
                };
                let Some(session_id) = json_str(info, "id") else {
                    return;
                };
                self.sync(session_id, vec![ShareItem::session(info.clone())]);
            }
            "message.updated" => {
                let Some(info) = data.get("info") else {
                    return;
                };
                let Some(session_id) = json_str(info, "sessionID") else {
                    return;
                };
                let (Some(id), Some(role)) = (json_str(info, "id"), json_str(info, "role")) else {
                    return;
                };
                self.sync(session_id, vec![ShareItem::message(id, info.clone())]);
                if role != "user" {
                    return;
                }
                let model = info.get("model").cloned().unwrap_or(Value::Null);
                if let (Some(provider_id), Some(model_id)) =
                    (json_str(&model, "providerID"), json_str(&model, "modelID"))
                {
                    match self.models.get_model(provider_id, model_id) {
                        Ok(model) => {
                            self.sync(session_id, vec![ShareItem::model(vec![model])]);
                        }
                        Err(cause) => {
                            tracing::error!("share subscriber failed: {cause}");
                        }
                    }
                }
            }
            "message.part.updated" => {
                let Some(part) = data.get("part") else {
                    return;
                };
                let (Some(session_id), Some(message_id), Some(part_id)) = (
                    json_str(part, "sessionID"),
                    json_str(part, "messageID"),
                    json_str(part, "id"),
                ) else {
                    return;
                };
                self.sync(
                    session_id,
                    vec![ShareItem::part(message_id, part_id, part.clone())],
                );
            }
            "session.diff" => {
                let (Some(session_id), diff) = (
                    json_str(data, "sessionID"),
                    data.get("diff").cloned().unwrap_or(Value::Null),
                ) else {
                    return;
                };
                self.sync(session_id, vec![ShareItem::session_diff(diff)]);
            }
            "session.deleted" => {
                if let Some(session_id) = json_str(data, "sessionID") {
                    if let Err(cause) = self.remove(session_id) {
                        tracing::error!("share subscriber failed: {cause}");
                    }
                }
            }
            _ => {}
        }
    }

    /// `create` (`share-next.ts:310-336`).
    pub fn create(self: &Arc<Self>, session_id: &str) -> Result<Share, String> {
        if self.disabled {
            return Ok(Share {
                id: String::new(),
                url: String::new(),
                secret: String::new(),
            });
        }
        tracing::info!("creating share, sessionID: {session_id}");
        let req = self.request()?;
        let body = json_object(vec![("sessionID", json_of(&session_id))]);
        let response = self
            .http
            .post(
                &format!("{}{}", req.base_url, req.api.create()),
                &req.headers,
                &body,
            )
            .map_err(|err| err.to_string())?;
        if !(200..300).contains(&response.status) {
            return Err(format!("share create failed: status {}", response.status));
        }
        let share: Share = serde_json::from_str(&response.body).map_err(|err| err.to_string())?;
        upsert_row(&self.storage, session_id, &share).map_err(|err| err.to_string())?;
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .shared
            .insert(session_id.to_string(), Some(share.clone()));
        if let Err(cause) = self.full(session_id) {
            tracing::error!("share full sync failed, sessionID: {session_id}: {cause}");
        }
        Ok(share)
    }

    /// `remove` (`share-next.ts:338-359`).
    pub fn remove(self: &Arc<Self>, session_id: &str) -> Result<(), String> {
        if self.disabled {
            return Ok(());
        }
        tracing::info!("removing share, sessionID: {session_id}");
        let Some(share) = self.get_cached(session_id)? else {
            let mut state = self.lock_state();
            state.shared.remove(session_id);
            state.queue.remove(session_id);
            return Ok(());
        };

        let req = self.request()?;
        let body = json_object(vec![("secret", json_of(&share.secret))]);
        let response = self
            .http
            .delete(
                &format!("{}{}", req.base_url, req.api.remove(&share.id)),
                &req.headers,
                &body,
            )
            .map_err(|err| err.to_string())?;
        if !(200..300).contains(&response.status) {
            return Err(format!("share remove failed: status {}", response.status));
        }
        delete_row(&self.storage, session_id).map_err(|err| err.to_string())?;
        let mut state = self.lock_state();
        state.shared.remove(session_id);
        state.queue.remove(session_id);
        Ok(())
    }

    /// `sync` (`share-next.ts:124-147`): queue the items deduplicated by
    /// key; the first batch per session schedules the flush `flush_delay`
    /// later, later batches merge into the pending queue.
    fn sync(self: &Arc<Self>, session_id: &str, items: Vec<ShareItem>) {
        if self.disabled {
            return;
        }
        match self.get_cached(session_id) {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(cause) => {
                tracing::error!("share sync failed, sessionID: {session_id}: {cause}");
                return;
            }
        }
        let schedule = {
            let mut state = self.lock_state();
            match state.queue.get_mut(session_id) {
                Some(existing) => {
                    for item in items {
                        upsert_key(existing, item);
                    }
                    false
                }
                None => {
                    let mut fresh: Vec<ShareItem> = Vec::new();
                    for item in items {
                        upsert_key(&mut fresh, item);
                    }
                    state.queue.insert(session_id.to_string(), fresh);
                    true
                }
            }
        };
        if schedule {
            self.schedule_flush(session_id.to_string());
        }
    }

    /// `flush` (`share-next.ts:247-272`) — the delayed flush target. Public
    /// so the scheduler (and tests) can drive it directly.
    pub fn flush(self: &Arc<Self>, session_id: &str) {
        if self.disabled {
            return;
        }
        let queued = self.lock_state().queue.remove(session_id);
        let queued = match queued {
            Some(queued) => queued,
            None => return,
        };
        let share = match self.get_cached(session_id) {
            Ok(Some(share)) => share,
            Ok(None) => return,
            Err(cause) => {
                tracing::error!("share flush failed, sessionID: {session_id}: {cause}");
                return;
            }
        };
        let Ok(req) = self.request() else {
            return;
        };
        let data = format!(
            "[{}]",
            queued
                .iter()
                .map(ShareItem::wire)
                .collect::<Vec<_>>()
                .join(",")
        );
        let body = json_object(vec![("secret", json_of(&share.secret)), ("data", data)]);
        match self.http.post(
            &format!("{}{}", req.base_url, req.api.sync(&share.id)),
            &req.headers,
            &body,
        ) {
            Ok(response) if response.status >= 400 => {
                tracing::warn!(
                    "failed to sync share, sessionID: {session_id}, shareID: {}, status: {}",
                    share.id,
                    response.status
                );
            }
            Ok(_) => {}
            Err(cause) => {
                tracing::error!("share flush failed, sessionID: {session_id}: {cause}")
            }
        }
    }

    fn schedule_flush(self: &Arc<Self>, session_id: String) {
        let share = Arc::downgrade(self);
        let delay = self.flush_delay;
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            if let Some(share) = share.upgrade() {
                share.flush(&session_id);
            }
        });
    }

    /// `full` (`share-next.ts:274-299`): session + messages + parts + diff
    /// + the distinct user models, in one sync batch.
    fn full(self: &Arc<Self>, session_id: &str) -> Result<(), String> {
        tracing::info!("full sync, sessionID: {session_id}");
        let info = self
            .sessions
            .get(session_id)
            .map_err(|err| err.to_string())?;
        let diffs = self
            .sessions
            .diff(session_id)
            .map_err(|err| err.to_string())?;
        let messages = self
            .sessions
            .messages(session_id, None)
            .map_err(|err| err.to_string())?;
        // Distinct user models, first-seen order
        // (`new Map(...).values()`, share-next.ts:279-289).
        let mut models: Vec<Value> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for message in &messages {
            if let alforria_schema::session_v1::V1Message::User { model, .. } = &message.info {
                let key = format!("{}/{}", model.provider_id, model.model_id);
                if seen.contains(&key) {
                    continue;
                }
                seen.push(key);
                models.push(self.models.get_model(&model.provider_id, &model.model_id)?);
            }
        }

        let mut items = vec![ShareItem::session(
            serde_json::to_value(&info).map_err(|err| err.to_string())?,
        )];
        for message in &messages {
            items.push(ShareItem::message(
                message_id(&message.info),
                serde_json::to_value(&message.info).map_err(|err| err.to_string())?,
            ));
        }
        for message in &messages {
            for part in &message.parts {
                let part_json = serde_json::to_value(part).map_err(|err| err.to_string())?;
                let (Some(message_id), Some(part_id)) = (
                    json_str(&part_json, "messageID"),
                    json_str(&part_json, "id"),
                ) else {
                    continue;
                };
                let (message_id, part_id) = (message_id.to_string(), part_id.to_string());
                items.push(ShareItem::part(&message_id, &part_id, part_json));
            }
        }
        items.push(ShareItem::session_diff(
            serde_json::to_value(&diffs).map_err(|err| err.to_string())?,
        ));
        items.push(ShareItem::model(models));
        self.sync(session_id, items);
        Ok(())
    }

    /// `getCached` (`share-next.ts:235-245`).
    fn get_cached(&self, session_id: &str) -> Result<Option<Share>, String> {
        {
            let state = self.lock_state();
            if let Some(cached) = state.shared.get(session_id) {
                return Ok(cached.clone());
            }
        }
        let share = get_row(&self.storage, session_id).map_err(|err| err.to_string())?;
        self.lock_state()
            .shared
            .insert(session_id.to_string(), share.clone());
        Ok(share)
    }

    /// `request` (`share-next.ts:206-222`): the console variant when an
    /// account with an active org exists, else the legacy API.
    fn request(&self) -> Result<ShareReq, String> {
        if let Some(active) = self.account.active() {
            if let Some(org_id) = active.active_org_id.clone() {
                let token = self
                    .account
                    .token(&active.id)
                    .ok_or_else(|| "No active account token available for sharing".to_string())?;
                return Ok(ShareReq {
                    headers: vec![
                        ("authorization".to_string(), format!("Bearer {token}")),
                        ("x-org-id".to_string(), org_id),
                    ],
                    api: Api::console(),
                    base_url: active.url,
                });
            }
        }
        Ok(ShareReq {
            headers: Vec::new(),
            api: Api::legacy(),
            base_url: self.base_url.clone(),
        })
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, ShareState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn json_str<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

/// `existing.set(key(item), item)` — replace in place, keep first-seen
/// order (`share-next.ts:133-139`).
fn upsert_key(items: &mut Vec<ShareItem>, item: ShareItem) {
    if let Some(existing) = items.iter_mut().find(|other| other.key == item.key) {
        *existing = item;
    } else {
        items.push(item);
    }
}

// ---------------------------------------------------------------------------
// `session_share` table ops (`core/src/share/sql.ts:5-13`)
// ---------------------------------------------------------------------------

fn get_row(storage: &Storage, session_id: &str) -> Result<Option<Share>, crate::CoreError> {
    storage.with_connection(|conn| {
        let found = conn.query_row(
            "SELECT id, secret, url FROM session_share WHERE session_id = ?1",
            rusqlite::params![session_id],
            |row| {
                Ok(Share {
                    id: row.get(0)?,
                    secret: row.get(1)?,
                    url: row.get(2)?,
                })
            },
        );
        match found {
            Ok(share) => Ok(Some(share)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(err) => Err(err.into()),
        }
    })
}

/// `Timestamps` (`$default`/`$onUpdate` → now) on both insert paths.
fn upsert_row(storage: &Storage, session_id: &str, share: &Share) -> Result<(), crate::CoreError> {
    let now = now_ms() as i64;
    storage.with_connection(|conn| {
        conn.execute(
            "INSERT INTO session_share (session_id, id, secret, url, time_created, time_updated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT (session_id) DO UPDATE
             SET id = ?2, secret = ?3, url = ?4, time_updated = ?5",
            rusqlite::params![session_id, share.id, share.secret, share.url, now],
        )?;
        Ok(())
    })
}

fn delete_row(storage: &Storage, session_id: &str) -> Result<(), crate::CoreError> {
    storage.with_connection(|conn| {
        conn.execute(
            "DELETE FROM session_share WHERE session_id = ?1",
            rusqlite::params![session_id],
        )?;
        Ok(())
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// SessionShare
// ---------------------------------------------------------------------------

/// The `SessionShare` service (`share/session.ts:17-49`) over [`ShareNext`]:
/// the config gate, `session.setShare`, and the auto-share fork.
#[derive(Clone)]
pub struct SessionShare {
    share: Arc<ShareNext>,
    sessions: SessionStore,
    disabled_config: bool,
    auto_share: bool,
}

impl SessionShare {
    pub fn new(
        share: Arc<ShareNext>,
        sessions: SessionStore,
        config: Option<crate::config::schema::Share>,
        auto_share_flag: bool,
    ) -> SessionShare {
        SessionShare {
            share,
            sessions,
            disabled_config: config == Some(crate::config::schema::Share::Disabled),
            auto_share: auto_share_flag || config == Some(crate::config::schema::Share::Auto),
        }
    }

    /// `share` (`share/session.ts:26-32`).
    pub fn share(&self, session_id: &str) -> Result<(), String> {
        if self.disabled_config {
            return Err("Sharing is disabled in configuration".to_string());
        }
        let result = self.share.create(session_id)?;
        self.sessions
            .set_share(session_id, Some(V1SessionShare { url: result.url }))
            .map_err(|err| err.to_string())
    }

    /// `unshare` (`share/session.ts:34-37`).
    pub fn unshare(&self, session_id: &str) -> Result<(), String> {
        self.share.remove(session_id)?;
        self.sessions
            .set_share(session_id, None)
            .map_err(|err| err.to_string())
    }

    /// The auto-share fork of `create` (`share/session.ts:39-46`):
    /// parentless sessions only, `flags.autoShare || config.share == "auto"`,
    /// forked with failures ignored.
    pub fn auto_share(&self, session: &V1SessionInfo) {
        if session.parent_id.is_some() || !self.auto_share {
            return;
        }
        let this = self.clone();
        let session_id = session.id.clone();
        std::thread::spawn(move || {
            if let Err(cause) = this.share(&session_id) {
                tracing::warn!("share auto share failed: {cause}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use alforria_schema::location::LocationRef;
    use serde_json::json;

    use crate::session::event_definitions::{
        MESSAGE_PART_UPDATED, MESSAGE_UPDATED, SESSION_DELETED, SESSION_DIFF, SESSION_UPDATED,
    };
    use crate::session::store::{CreateInput, SessionContext};
    use crate::session::test_support::{FixedClock, NoJobs};
    use crate::storage::test_support::TempDir;
    use crate::{AgentRegistryInput, PublishOptions, SessionServices, Storage};

    use super::*;

    // -------------------------------------------------------------------
    // Stub HTTP + harness
    // -------------------------------------------------------------------

    #[derive(Debug, Clone, PartialEq)]
    struct Recorded {
        method: &'static str,
        url: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    /// Stub [`ShareHttp`] — records every request, answers the create call.
    #[derive(Default)]
    struct StubHttp {
        requests: Mutex<Vec<Recorded>>,
        sync_status: std::sync::atomic::AtomicU16,
        fail_transport: std::sync::atomic::AtomicBool,
    }

    impl StubHttp {
        fn record(
            &self,
            method: &'static str,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) {
            self.requests.lock().unwrap().push(Recorded {
                method,
                url: url.to_string(),
                headers: headers.to_vec(),
                body: body.to_string(),
            });
        }

        fn posts(&self) -> Vec<Recorded> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == "POST")
                .cloned()
                .collect()
        }

        fn deletes(&self) -> Vec<Recorded> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request.method == "DELETE")
                .cloned()
                .collect()
        }

        fn set_sync_status(&self, status: u16) {
            self.sync_status
                .store(status, std::sync::atomic::Ordering::SeqCst);
        }

        fn set_fail_transport(&self, fail: bool) {
            self.fail_transport
                .store(fail, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl ShareHttp for StubHttp {
        fn post(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) -> Result<ShareHttpResponse, String> {
            self.record("POST", url, headers, body);
            if self
                .fail_transport
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err("connection refused".to_string());
            }
            if url.ends_with("/sync") {
                return Ok(ShareHttpResponse {
                    status: self.sync_status.load(std::sync::atomic::Ordering::SeqCst),
                    body: String::new(),
                });
            }
            Ok(ShareHttpResponse {
                status: 200,
                body: r#"{"id":"shr_1","url":"https://shr.test/1","secret":"s3cret"}"#.to_string(),
            })
        }

        fn delete(
            &self,
            url: &str,
            headers: &[(String, String)],
            body: &str,
        ) -> Result<ShareHttpResponse, String> {
            self.record("DELETE", url, headers, body);
            if self
                .fail_transport
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err("connection refused".to_string());
            }
            Ok(ShareHttpResponse {
                status: 200,
                body: String::new(),
            })
        }
    }

    struct FixedModels;

    impl ShareModels for FixedModels {
        fn get_model(&self, provider_id: &str, model_id: &str) -> Result<Value, String> {
            Ok(json!({
                "id": model_id,
                "providerID": provider_id,
                "name": format!("{provider_id}/{model_id}"),
            }))
        }
    }

    struct Harness {
        _dir: TempDir,
        services: Arc<SessionServices>,
        worktree: std::path::PathBuf,
        http: Arc<StubHttp>,
    }

    fn new_harness(name: &str) -> Harness {
        let dir = TempDir::new(name);
        let worktree = dir.path().join("repo");
        std::fs::create_dir_all(&worktree).unwrap();
        let storage = Arc::new(Storage::open(dir.path().join("db.sqlite")).unwrap());
        let agent_input = AgentRegistryInput {
            config: serde_json::from_value(json!({})).unwrap(),
            skill_dirs: Vec::new(),
            reference_dirs: Vec::new(),
            worktree: worktree.clone(),
            data_dir: dir.path().to_path_buf(),
            tmp_dir: dir.path().to_path_buf(),
            home: dir.path().to_path_buf(),
        };
        let services = SessionServices::new(
            storage.clone(),
            Arc::new(NoJobs),
            Arc::new(FixedClock),
            &agent_input,
        );
        services
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO project (id, worktree, sandboxes, time_created, time_updated)
                     VALUES ('global', ?1, '[]', 1, 1)",
                    rusqlite::params![worktree.to_string_lossy().into_owned()],
                )
            })
            .unwrap();
        Harness {
            _dir: dir,
            services: Arc::new(services),
            worktree,
            http: Arc::new(StubHttp::default()),
        }
    }

    impl Harness {
        fn session(&self) -> V1SessionInfo {
            self.services
                .sessions
                .create(
                    &SessionContext {
                        project_id: "global".to_string(),
                        directory: self.worktree.clone(),
                        worktree: self.worktree.clone(),
                        workspace_id: None,
                    },
                    &CreateInput::default(),
                )
                .unwrap()
        }

        /// A share service over the harness instance with the stub HTTP.
        fn share(&self, flush_delay: Duration) -> Arc<ShareNext> {
            let share = ShareNext::new(ShareInput {
                storage: self.services.storage.clone(),
                sessions: self.services.sessions.clone(),
                events: self.services.events.clone(),
                base_url: "https://base.test".to_string(),
                disabled: false,
                directory: self.worktree.to_string_lossy().into_owned(),
                http: self.http.clone(),
                account: Arc::new(NoAccount),
                models: Arc::new(FixedModels),
                flush_delay,
            });
            share.init();
            share
        }
    }

    fn wait_until(condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("condition not met within timeout");
    }

    // -------------------------------------------------------------------
    // create / remove
    // -------------------------------------------------------------------

    #[test]
    fn create_posts_legacy_api_upserts_row_and_full_syncs() {
        let harness = new_harness("share-create");
        let session = harness.session();
        let share = harness.share(Duration::from_millis(20));

        share.create(&session.id).unwrap();

        let posts = harness
            .http
            .posts()
            .into_iter()
            .filter(|request| !request.url.ends_with("/sync"))
            .collect::<Vec<_>>();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].url, "https://base.test/api/share");
        assert_eq!(posts[0].body, json!({"sessionID": session.id}).to_string());

        // The row is upserted (share-next.ts:320-327).
        let row = harness
            .services
            .storage
            .with_connection(|conn| {
                conn.query_row(
                    "SELECT id, secret, url FROM session_share WHERE session_id = ?1",
                    rusqlite::params![session.id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
            })
            .unwrap();
        assert_eq!(
            row,
            (
                "shr_1".to_string(),
                "s3cret".to_string(),
                "https://shr.test/1".to_string()
            )
        );

        // full() queued: the fork fires a sync POST one flush window later.
        wait_until(|| {
            harness
                .http
                .posts()
                .iter()
                .any(|request| request.url == "https://base.test/api/share/shr_1/sync")
        });
        let sync = harness
            .http
            .posts()
            .into_iter()
            .find(|request| request.url.ends_with("/sync"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&sync.body).unwrap();
        assert_eq!(body["secret"], "s3cret");
        let data = body["data"].as_array().unwrap();
        // full() with no messages: session, session_diff, model
        assert_eq!(data.len(), 3);
        assert_eq!(data[0]["type"], "session");
        assert_eq!(data[0]["data"]["id"], session.id.as_str());
        assert_eq!(data[1]["type"], "session_diff");
        assert_eq!(data[1]["data"], json!([]));
        assert_eq!(data[2]["type"], "model");
        assert_eq!(data[2]["data"], json!([]));
    }

    #[test]
    fn full_syncs_messages_parts_and_models() {
        let harness = new_harness("share-full");
        let session = harness.session();
        let message = crate::session::test_support::user_message(&session.id, "msg_1", 1.0);
        harness.services.sessions.update_message(&message).unwrap();
        harness
            .services
            .sessions
            .update_part(&alforria_schema::session_v1::V1Part::Text {
                id: "prt_1".to_string(),
                session_id: session.id.clone(),
                message_id: "msg_1".to_string(),
                text: "hi".to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            })
            .unwrap();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id);

        let sync = harness
            .http
            .posts()
            .into_iter()
            .find(|request| request.url.ends_with("/sync"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&sync.body).unwrap();
        let data = body["data"].as_array().unwrap();
        // session, message, part, session_diff, model
        assert_eq!(data.len(), 5);
        assert_eq!(data[0]["type"], "session");
        assert_eq!(data[1]["type"], "message");
        assert_eq!(data[1]["data"]["id"], "msg_1");
        assert_eq!(data[2]["type"], "part");
        assert_eq!(data[2]["data"]["id"], "prt_1");
        assert_eq!(data[3]["type"], "session_diff");
        assert_eq!(data[4]["type"], "model");
        assert_eq!(data[4]["data"][0]["id"], "claude");
    }

    #[test]
    fn remove_sends_delete_and_clears_the_row() {
        let harness = new_harness("share-remove");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();

        share.remove(&session.id).unwrap();
        let deletes = harness.http.deletes();
        assert_eq!(deletes.len(), 1);
        assert_eq!(deletes[0].url, "https://base.test/api/share/shr_1");
        assert_eq!(deletes[0].body, json!({"secret": "s3cret"}).to_string());
        let row = harness.services.storage.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM session_share WHERE session_id = ?1",
                rusqlite::params![session.id],
                |row| row.get::<_, i64>(0),
            )
        });
        assert_eq!(row.unwrap(), 0);

        // Removing again: known-absent → no second DELETE.
        share.remove(&session.id).unwrap();
        assert_eq!(harness.http.deletes().len(), 1);
    }

    #[test]
    fn disabled_share_never_touches_the_network() {
        let harness = new_harness("share-disabled");
        let session = harness.session();
        let share = ShareNext::new(ShareInput {
            storage: harness.services.storage.clone(),
            sessions: harness.services.sessions.clone(),
            events: harness.services.events.clone(),
            base_url: "https://base.test".to_string(),
            disabled: true,
            directory: harness.worktree.to_string_lossy().into_owned(),
            http: harness.http.clone(),
            account: Arc::new(NoAccount),
            models: Arc::new(FixedModels),
            flush_delay: Duration::from_secs(300),
        });
        share.init();

        let created = share.create(&session.id).unwrap();
        assert_eq!(
            created,
            Share {
                id: String::new(),
                url: String::new(),
                secret: String::new(),
            }
        );
        assert!(harness.http.requests.lock().unwrap().is_empty());
        share.remove(&session.id).unwrap();
        assert!(harness.http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn flush_window_fires_without_manual_flush() {
        let harness = new_harness("share-window");
        let session = harness.session();
        let share = harness.share(Duration::from_millis(20));
        share.create(&session.id).unwrap();
        let before = harness.http.posts().len();
        wait_until(|| harness.http.posts().len() > before);
    }

    // -------------------------------------------------------------------
    // Queue dedup
    // -------------------------------------------------------------------

    fn publish(
        services: &SessionServices,
        worktree: &std::path::Path,
        definition: &crate::event::definition::Definition,
        data: Value,
    ) {
        services
            .events
            .publish(
                definition,
                data,
                PublishOptions {
                    location: Some(LocationRef {
                        directory: worktree.to_string_lossy().into_owned(),
                        workspace_id: None,
                        project: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
    }

    /// The full `session.updated` payload with a tweaked title.
    fn session_payload(harness: &Harness, session_id: &str, title: &str) -> Value {
        let mut info =
            serde_json::to_value(harness.services.sessions.get(session_id).unwrap()).unwrap();
        info["title"] = json!(title);
        json!({"sessionID": session_id, "info": info})
    }

    #[test]
    fn queue_dedups_by_key_and_merges_while_pending() {
        let harness = new_harness("share-dedup");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id); // drain the create-time full() sync

        publish(
            &harness.services,
            &harness.worktree,
            &SESSION_UPDATED,
            session_payload(&harness, &session.id, "v1"),
        );
        publish(
            &harness.services,
            &harness.worktree,
            &SESSION_UPDATED,
            session_payload(&harness, &session.id, "v2"),
        );
        let message = crate::session::test_support::user_message(&session.id, "msg_1", 1.0);
        publish(
            &harness.services,
            &harness.worktree,
            &MESSAGE_UPDATED,
            json!({"sessionID": session.id, "info": serde_json::to_value(&message).unwrap()}),
        );
        share.flush(&session.id);

        let sync = harness
            .http
            .posts()
            .into_iter()
            .rfind(|request| request.url.ends_with("/sync"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&sync.body).unwrap();
        let data = body["data"].as_array().unwrap();
        // session (overwritten, v2), message, model (user message)
        assert_eq!(data.len(), 3);
        assert_eq!(data[0]["type"], "session");
        assert_eq!(data[0]["data"]["title"], "v2");
        assert_eq!(data[1]["type"], "message");
        assert_eq!(data[1]["data"]["id"], "msg_1");
        assert_eq!(data[2]["type"], "model");
    }

    #[test]
    fn watchers_queue_part_diff_and_model_items() {
        let harness = new_harness("share-watch");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id);

        let user = crate::session::test_support::user_message(&session.id, "msg_user", 1.0);
        publish(
            &harness.services,
            &harness.worktree,
            &MESSAGE_UPDATED,
            json!({"sessionID": session.id.clone(), "info": serde_json::to_value(&user).unwrap()}),
        );
        let part = alforria_schema::session_v1::V1Part::Text {
            id: "prt_1".to_string(),
            session_id: session.id.clone(),
            message_id: "msg_user".to_string(),
            text: "hi".to_string(),
            synthetic: None,
            ignored: None,
            time: None,
            metadata: None,
        };
        publish(
            &harness.services,
            &harness.worktree,
            &MESSAGE_PART_UPDATED,
            json!({
                "sessionID": session.id.clone(),
                "part": serde_json::to_value(&part).unwrap(),
                "time": 1.0,
            }),
        );
        publish(
            &harness.services,
            &harness.worktree,
            &SESSION_DIFF,
            json!({"sessionID": session.id.clone(), "diff": []}),
        );
        share.flush(&session.id);

        let sync = harness
            .http
            .posts()
            .into_iter()
            .rfind(|request| request.url.ends_with("/sync"))
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&sync.body).unwrap();
        let data = body["data"].as_array().unwrap();
        // message, model (user message), part, session_diff
        assert_eq!(data.len(), 4);
        assert_eq!(data[0]["type"], "message");
        assert_eq!(data[0]["data"]["id"], "msg_user");
        assert_eq!(data[1]["type"], "model");
        assert_eq!(data[1]["data"][0]["id"], "claude");
        assert_eq!(data[2]["type"], "part");
        assert_eq!(data[2]["data"]["id"], "prt_1");
        assert_eq!(data[3]["type"], "session_diff");
    }

    #[test]
    fn watcher_ignores_other_directories() {
        let harness = new_harness("share-filter");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id);

        harness
            .services
            .events
            .publish(
                &SESSION_UPDATED,
                session_payload(&harness, &session.id, "elsewhere"),
                PublishOptions {
                    location: Some(LocationRef {
                        directory: "/elsewhere".to_string(),
                        workspace_id: None,
                        project: None,
                    }),
                    ..Default::default()
                },
            )
            .unwrap();
        // Events without a location are skipped too
        harness
            .services
            .events
            .publish(
                &SESSION_UPDATED,
                session_payload(&harness, &session.id, "no-location"),
                PublishOptions::default(),
            )
            .unwrap();
        share.flush(&session.id);
        let syncs = harness
            .http
            .posts()
            .into_iter()
            .filter(|request| request.url.ends_with("/sync"))
            .count();
        assert_eq!(syncs, 1, "only the create-time full() sync");
    }

    #[test]
    fn session_deleted_removes_the_share() {
        let harness = new_harness("share-deleted");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id);

        publish(
            &harness.services,
            &harness.worktree,
            &SESSION_DELETED,
            session_payload(&harness, &session.id, "deleted"),
        );
        assert_eq!(harness.http.deletes().len(), 1);
        let row = harness.services.storage.with_connection(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM session_share WHERE session_id = ?1",
                rusqlite::params![session.id],
                |row| row.get::<_, i64>(0),
            )
        });
        assert_eq!(row.unwrap(), 0);
    }

    // -------------------------------------------------------------------
    // SessionShare: config gate + auto-share
    // -------------------------------------------------------------------

    #[test]
    fn config_disabled_gate_blocks_share() {
        let harness = new_harness("share-gate");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        let service = SessionShare::new(
            share,
            harness.services.sessions.clone(),
            Some(crate::config::schema::Share::Disabled),
            false,
        );
        let err = service.share(&session.id).unwrap_err();
        assert_eq!(err, "Sharing is disabled in configuration");
        assert!(harness.http.posts().is_empty());
    }

    #[test]
    fn share_persists_the_share_url_on_the_session() {
        let harness = new_harness("share-persist");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        let service = SessionShare::new(share, harness.services.sessions.clone(), None, false);
        service.share(&session.id).unwrap();
        let updated = harness.services.sessions.get(&session.id).unwrap();
        assert_eq!(
            updated.share,
            Some(V1SessionShare {
                url: "https://shr.test/1".to_string()
            })
        );

        service.unshare(&session.id).unwrap();
        let updated = harness.services.sessions.get(&session.id).unwrap();
        assert_eq!(updated.share, None);
    }

    #[test]
    fn auto_share_fires_for_parentless_sessions_only() {
        let harness = new_harness("share-auto");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        let service = SessionShare::new(share, harness.services.sessions.clone(), None, true);

        service.auto_share(&session);
        wait_until(|| !harness.http.posts().is_empty());
        let share_url_set = harness
            .services
            .sessions
            .get(&session.id)
            .unwrap()
            .share
            .is_some();
        assert!(share_url_set, "auto-share persisted the share url");

        // Parented sessions never auto-share.
        let count = harness.http.posts().len();
        let mut parented = harness.session();
        parented.parent_id = Some(session.id.clone());
        service.auto_share(&parented);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(harness.http.posts().len(), count);
    }

    #[test]
    fn auto_share_swallows_failures() {
        let harness = new_harness("share-auto-fail");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        harness.http.set_fail_transport(true);
        let service = SessionShare::new(share, harness.services.sessions.clone(), None, true);
        service.auto_share(&session);
        std::thread::sleep(Duration::from_millis(50));
    }

    #[test]
    fn auto_share_requires_the_flag_or_config_auto() {
        let harness = new_harness("share-auto-off");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        let service = SessionShare::new(share, harness.services.sessions.clone(), None, false);
        service.auto_share(&session);
        // config.share == "auto" turns it on as well
        let harness2 = new_harness("share-auto-config");
        let session2 = harness2.session();
        let share2 = harness2.share(Duration::from_secs(300));
        let service2 = SessionShare::new(
            share2,
            harness2.services.sessions.clone(),
            Some(crate::config::schema::Share::Auto),
            false,
        );
        service2.auto_share(&session2);
        wait_until(|| !harness2.http.posts().is_empty());
    }

    // -------------------------------------------------------------------
    // Account seam (console variant)
    // -------------------------------------------------------------------

    struct ActiveWithOrg;

    impl ShareAccounts for ActiveWithOrg {
        fn active(&self) -> Option<ActiveAccount> {
            Some(ActiveAccount {
                id: "acc_1".to_string(),
                url: "https://acc.test".to_string(),
                active_org_id: Some("org_1".to_string()),
            })
        }

        fn token(&self, _id: &str) -> Option<String> {
            Some("tok".to_string())
        }
    }

    #[test]
    fn active_account_uses_the_console_api() {
        let harness = new_harness("share-console");
        let session = harness.session();
        let share = ShareNext::new(ShareInput {
            storage: harness.services.storage.clone(),
            sessions: harness.services.sessions.clone(),
            events: harness.services.events.clone(),
            base_url: "https://base.test".to_string(),
            disabled: false,
            directory: harness.worktree.to_string_lossy().into_owned(),
            http: harness.http.clone(),
            account: Arc::new(ActiveWithOrg),
            models: Arc::new(FixedModels),
            flush_delay: Duration::from_secs(300),
        });
        share.create(&session.id).unwrap();
        let posts = harness.http.posts();
        assert_eq!(posts[0].url, "https://acc.test/api/shares");
        assert!(posts[0]
            .headers
            .contains(&("authorization".to_string(), "Bearer tok".to_string())));
        assert!(posts[0]
            .headers
            .contains(&("x-org-id".to_string(), "org_1".to_string())));
    }

    #[test]
    fn missing_account_token_errors() {
        struct NoToken;
        impl ShareAccounts for NoToken {
            fn active(&self) -> Option<ActiveAccount> {
                Some(ActiveAccount {
                    id: "acc_1".to_string(),
                    url: "https://acc.test".to_string(),
                    active_org_id: Some("org_1".to_string()),
                })
            }

            fn token(&self, _id: &str) -> Option<String> {
                None
            }
        }

        let harness = new_harness("share-no-token");
        let session = harness.session();
        let share = ShareNext::new(ShareInput {
            storage: harness.services.storage.clone(),
            sessions: harness.services.sessions.clone(),
            events: harness.services.events.clone(),
            base_url: "https://base.test".to_string(),
            disabled: false,
            directory: harness.worktree.to_string_lossy().into_owned(),
            http: harness.http.clone(),
            account: Arc::new(NoToken),
            models: Arc::new(FixedModels),
            flush_delay: Duration::from_secs(300),
        });
        let err = share.create(&session.id).unwrap_err();
        assert_eq!(err, "No active account token available for sharing");
        assert!(harness.http.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn sync_failure_is_logged_not_propagated() {
        let harness = new_harness("share-sync-status");
        let session = harness.session();
        let share = harness.share(Duration::from_secs(300));
        share.create(&session.id).unwrap();
        share.flush(&session.id);
        let count = harness.http.posts().len();

        harness.http.set_sync_status(500);
        publish(
            &harness.services,
            &harness.worktree,
            &SESSION_UPDATED,
            session_payload(&harness, &session.id, "v1"),
        );
        share.flush(&session.id);
        assert_eq!(
            harness.http.posts().len(),
            count + 1,
            "the 4xx/5xx sync status is data, not an error"
        );
    }
}
