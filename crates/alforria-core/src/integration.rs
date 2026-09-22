//! Integration registry — port of `packages/core/src/integration.ts`.
//!
//! The TS registry is plugin-driven and ships empty; the Rust port keeps
//! the registry as a seam ([`IntegrationService::register`]) with no
//! plugin runtime, so lists return `[]` unless a caller registers
//! integrations in-process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::credential::CredentialStore;
use crate::event::definition::Definition;
use crate::event::EventBus;
use crate::tool::def::BoxFuture;
use crate::{Clock, CoreError};

/// `Integration.AttemptID.create()` — `con_`-prefixed.
pub fn attempt_id() -> String {
    crate::session::ids::generate_id("con_")
}

/// The pending-attempt lifetime and terminal retention
/// (`integration.ts:186-188`).
const ATTEMPT_LIFETIME_MS: u64 = 10 * 60 * 1000;
const TERMINAL_RETENTION_MS: u64 = 60 * 1000;

/// `integration.updated` (`schema/src/integration.ts:75-79`).
pub const UPDATED: Definition = Definition {
    r#type: "integration.updated",
    durable: None,
};

/// `integration.connection.updated` (`schema/src/integration.ts:76-79`).
pub const CONNECTION_UPDATED: Definition = Definition {
    r#type: "integration.connection.updated",
    durable: None,
};

/// `Integration.AuthorizationError` — a failed authorize/refresh callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationError {
    pub cause: String,
}

impl AuthorizationError {
    fn new(cause: impl Into<String>) -> AuthorizationError {
        AuthorizationError {
            cause: cause.into(),
        }
    }
}

/// `Integration.CodeRequiredError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeRequiredError {
    pub attempt_id: String,
}

/// The `OAuthAuthorization` callbacks (`integration.ts:50-59`): `auto`
/// completes itself in the background, `code` completes through
/// [`IntegrationService::attempt_complete`].
pub enum OAuthCallback {
    Auto(BoxFuture<'static, Result<Value, String>>),
    Code(fn(&str) -> BoxFuture<'static, Result<Value, String>>),
}

/// The authorization an implementation's `authorize` returns
/// (`integration.ts:50-59`).
pub struct OAuthAuthorization {
    pub url: String,
    pub instructions: String,
    pub callback: OAuthCallback,
}

/// One integration's registered OAuth implementation — the seam the plugin
/// runtime would provide (`integration.ts:52-59`).
pub trait OAuthImplementation: Send + Sync {
    fn integration_id(&self) -> &str;
    fn method_id(&self) -> &str;
    fn authorize(&self, inputs: &HashMap<String, String>) -> OAuthAuthorization;
    /// The user-facing label for the credential created on completion
    /// (`integration.ts:57-58`).
    fn label(&self, _credential: &Value) -> Option<String> {
        None
    }
}

#[derive(Clone)]
struct Entry {
    name: String,
    methods: Vec<Value>,
    implementations: HashMap<String, Arc<dyn OAuthImplementation>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Complete,
    Failed,
    Expired,
}

/// `AttemptTime` (`integration.ts:196-199`).
fn attempt_time(created: u64) -> Value {
    json!({"created": created, "expires": created + ATTEMPT_LIFETIME_MS})
}

struct Attempt {
    status: Status,
    completing: bool,
    message: Option<String>,
    authorization: Option<OAuthAuthorization>,
    integration_id: String,
    method_id: String,
    label: Option<String>,
    time: Value,
    /// `removeAt` for terminal entries (`integration.ts:189-196`).
    remove_at: Option<u64>,
}

/// The integration registry + connection/attempt lifecycle service
/// (`integration.ts:198-419`).
pub struct IntegrationService {
    credentials: Arc<CredentialStore>,
    events: Arc<EventBus>,
    clock: Arc<dyn Clock>,
    integrations: Mutex<HashMap<String, Entry>>,
    attempts: Mutex<HashMap<String, Attempt>>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl IntegrationService {
    pub fn new(
        credentials: Arc<CredentialStore>,
        events: Arc<EventBus>,
        clock: Arc<dyn Clock>,
    ) -> IntegrationService {
        IntegrationService {
            credentials,
            events,
            clock,
            integrations: Mutex::new(HashMap::new()),
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// `transform` — register one integration's identity, methods and
    /// OAuth implementations. Methods are the raw `Integration.Method`
    /// wire values.
    pub fn register(
        &self,
        id: &str,
        name: &str,
        methods: Vec<Value>,
        implementations: Vec<Arc<dyn OAuthImplementation>>,
    ) {
        let mut integrations = lock(&self.integrations);
        let entry = integrations.entry(id.to_string()).or_insert_with(|| Entry {
            name: id.to_string(),
            methods: Vec::new(),
            implementations: HashMap::new(),
        });
        entry.name = name.to_string();
        for method in methods {
            entry.methods.push(method);
        }
        for implementation in implementations {
            entry
                .implementations
                .insert(implementation.method_id().to_string(), implementation);
        }
    }

    fn now(&self) -> u64 {
        self.clock.now_ms()
    }

    /// `scrub` (`integration.ts:218-235`) — applied lazily on every
    /// attempt read: pending attempts past their expiry become `expired`,
    /// terminal entries past their retention are removed.
    fn scrub(&self, attempts: &mut HashMap<String, Attempt>) {
        let now = self.now();
        attempts.retain(|_, attempt| {
            attempt
                .remove_at
                .map(|remove_at| remove_at > now)
                .unwrap_or(true)
        });
        for (_, attempt) in attempts.iter_mut() {
            if attempt.status == Status::Pending
                && attempt.time["expires"].as_u64().unwrap_or(0) <= now
            {
                attempt.status = Status::Expired;
                attempt.authorization = None;
                attempt.remove_at = Some(now + TERMINAL_RETENTION_MS);
            }
        }
    }

    fn connections(&self, entry: Option<&Entry>, integration_id: &str) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .credentials
            .list(integration_id)
            .map(|credentials| {
                credentials
                    .into_iter()
                    .rev()
                    .map(|credential| {
                        json!({"type": "credential", "id": credential.id, "label": credential.label})
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(entry) = entry {
            for method in &entry.methods {
                if method["type"] == "env" {
                    for name in method["names"].as_array().into_iter().flatten() {
                        if let Some(name) = name.as_str() {
                            if std::env::var(name).is_ok() {
                                out.push(json!({"type": "env", "name": name}));
                            }
                        }
                    }
                }
            }
        }
        out
    }

    /// `list` — every integration with its connections, name-sorted
    /// (`integration.ts:304-311`).
    pub fn list(&self) -> Vec<Value> {
        let integrations = lock(&self.integrations);
        let mut entries: Vec<(&String, &Entry)> = integrations.iter().collect();
        entries.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        entries
            .into_iter()
            .map(|(id, entry)| {
                json!({
                    "id": id,
                    "name": entry.name,
                    "methods": entry.methods,
                    "connections": self.connections(Some(entry), id),
                })
            })
            .collect()
    }

    /// `get` — one integration with its connections.
    pub fn get(&self, id: &str) -> Option<Value> {
        let integrations = lock(&self.integrations);
        let entry = integrations.get(id)?;
        Some(json!({
            "id": id,
            "name": entry.name,
            "methods": entry.methods,
            "connections": self.connections(Some(entry), id),
        }))
    }

    /// `connection.active` — the active connection for one integration.
    pub fn connection_active(&self, id: &str) -> Option<Value> {
        let integrations = lock(&self.integrations);
        let entry = integrations.get(id);
        self.connections(entry, id).into_iter().next()
    }

    /// `connection.resolve` — resolve a connection into credential
    /// material (`integration.ts:264-283`). OAuth values without an
    /// implementation-backed refresh are returned verbatim.
    pub fn connection_resolve(&self, connection: &Value) -> Option<Value> {
        if connection["type"] == "env" {
            let name = connection["name"].as_str()?;
            let key = std::env::var(name).ok()?;
            return Some(json!({"type": "key", "key": key}));
        }
        self.credentials
            .get(connection["id"].as_str().unwrap_or_default())
            .ok()?
            .map(|credential| credential.value)
    }

    /// `connection.key` — run a key method and store the credential
    /// (`integration.ts:248-261`).
    pub fn connection_key(
        &self,
        integration_id: &str,
        key: &str,
        label: Option<&str>,
    ) -> Result<(), AuthorizationError> {
        {
            let integrations = lock(&self.integrations);
            let has_key_method = integrations
                .get(integration_id)
                .is_some_and(|entry| entry.methods.iter().any(|method| method["type"] == "key"));
            if !has_key_method {
                return Err(AuthorizationError::new(format!(
                    "Key method not found: {integration_id}"
                )));
            }
        }
        self.credentials
            .create(integration_id, json!({"type": "key", "key": key}), label)
            .map_err(|err| AuthorizationError::new(err.to_string()))?;
        self.publish_connection_updated(integration_id);
        Ok(())
    }

    /// `connection.oauth` — start a stateful OAuth attempt
    /// (`integration.ts:270-298`). Auto-mode callbacks run detached like
    /// the TS fiber.
    pub fn connection_oauth(
        self: &Arc<Self>,
        integration_id: &str,
        method_id: &str,
        inputs: &HashMap<String, String>,
        label: Option<&str>,
    ) -> Result<Value, AuthorizationError> {
        let implementation = {
            let integrations = lock(&self.integrations);
            let Some(entry) = integrations.get(integration_id) else {
                return Err(AuthorizationError::new(format!(
                    "OAuth method not found: {integration_id}/{method_id}"
                )));
            };
            let Some(implementation) = entry.implementations.get(method_id) else {
                return Err(AuthorizationError::new(format!(
                    "OAuth method not found: {integration_id}/{method_id}"
                )));
            };
            implementation.clone()
        };
        let authorization = implementation.authorize(inputs);
        let is_auto = matches!(authorization.callback, OAuthCallback::Auto(_));
        let url = authorization.url.clone();
        let instructions = authorization.instructions.clone();
        let id = attempt_id();
        let time = attempt_time(self.now());
        let attempt = Attempt {
            status: Status::Pending,
            completing: is_auto,
            message: None,
            authorization: Some(authorization),
            integration_id: integration_id.to_string(),
            method_id: method_id.to_string(),
            label: label.map(String::from),
            time: time.clone(),
            remove_at: None,
        };
        lock(&self.attempts).insert(id.clone(), attempt);
        if is_auto {
            // The auto callback runs detached like the TS fiber
            // (`integration.ts:295-303`); the attempt stays `completing`
            // so a concurrent `complete` rejects it.
            let callback = match lock(&self.attempts)
                .get_mut(&id)
                .and_then(|attempt| attempt.authorization.take())
                .map(|authorization| authorization.callback)
            {
                Some(OAuthCallback::Auto(callback)) => callback,
                _ => unreachable!("just inserted auto attempt"),
            };
            let this = Arc::clone(self);
            let attempt_id = id.clone();
            tokio::spawn(async move {
                let exit = callback.await;
                this.settle(&attempt_id, exit);
            });
        }
        Ok(json!({
            "attemptID": id,
            "url": url,
            "instructions": instructions,
            "mode": if is_auto { "auto" } else { "code" },
            "time": time,
        }))
    }

    /// `attempt.status` — poll the current status of an attempt
    /// (`integration.ts:332-341`).
    pub fn attempt_status(&self, id: &str) -> Result<Value, CoreError> {
        let mut attempts = lock(&self.attempts);
        self.scrub(&mut attempts);
        let Some(attempt) = attempts.get(id) else {
            return Err(CoreError::Storage(format!("OAuth attempt not found: {id}")));
        };
        let time = attempt.time.clone();
        match attempt.status {
            Status::Failed => Ok(json!({
                "status": "failed",
                "message": attempt
                    .message
                    .clone()
                    .unwrap_or_else(|| "Authorization failed".to_string()),
                "time": time,
            })),
            status => Ok(json!({
                "status": match status {
                    Status::Pending => "pending",
                    Status::Complete => "complete",
                    _ => "expired",
                },
                "time": time,
            })),
        }
    }

    /// `attempt.complete` — finish a code-mode attempt and store its
    /// credential (`integration.ts:343-373`).
    pub async fn attempt_complete(
        self: &Arc<Self>,
        id: &str,
        code: Option<&str>,
    ) -> Result<Result<(), CodeRequiredError>, CoreError> {
        let (is_code, completing, pending) = {
            let mut attempts = lock(&self.attempts);
            self.scrub(&mut attempts);
            let Some(attempt) = attempts.get(id) else {
                return Err(CoreError::Storage(format!("OAuth attempt not found: {id}")));
            };
            (
                matches!(
                    attempt.authorization.as_ref().map(|a| &a.callback),
                    Some(OAuthCallback::Code(_))
                ),
                attempt.completing,
                attempt.status == Status::Pending,
            )
        };
        if !pending {
            return Ok(Ok(()));
        }
        if is_code && code.is_none() {
            return Ok(Err(CodeRequiredError {
                attempt_id: id.to_string(),
            }));
        }
        if completing {
            return Err(CoreError::Storage(format!(
                "OAuth attempt already completing: {id}"
            )));
        }
        let callback = {
            let mut attempts = lock(&self.attempts);
            let Some(attempt) = attempts.get_mut(id) else {
                return Err(CoreError::Storage(format!("OAuth attempt not found: {id}")));
            };
            attempt.completing = true;
            match attempts
                .get_mut(id)
                .and_then(|attempt| attempt.authorization.take())
                .map(|authorization| authorization.callback)
            {
                Some(OAuthCallback::Code(callback)) => callback,
                _ => {
                    return Err(CoreError::Storage(format!(
                        "OAuth attempt already completing: {id}"
                    )))
                }
            }
        };
        let result = callback(code.unwrap_or_default()).await;
        self.settle(id, result);
        Ok(Ok(()))
    }

    /// `settle` (`integration.ts:236-261`) — record the attempt outcome
    /// and, on success, store the credential.
    fn settle(&self, id: &str, result: Result<Value, String>) {
        let now = self.now();
        let (integration_id, method_id, attempt_label) = {
            let mut attempts = lock(&self.attempts);
            let Some(attempt) = attempts.get_mut(id) else {
                return;
            };
            if attempt.status != Status::Pending {
                return;
            }
            if result.is_ok() {
                attempt.status = Status::Complete;
            } else {
                attempt.status = Status::Failed;
                attempt.message = result.as_ref().err().cloned();
            }
            attempt.authorization = None;
            attempt.remove_at = Some(now + TERMINAL_RETENTION_MS);
            (
                attempt.integration_id.clone(),
                attempt.method_id.clone(),
                attempt.label.clone(),
            )
        };
        let Ok(value) = result else {
            return;
        };
        // `result.label ?? implementation?.label?.(exit.value)`
        // (`integration.ts:243-251`).
        let label = attempt_label.or_else(|| {
            let integrations = lock(&self.integrations);
            integrations
                .get(&integration_id)?
                .implementations
                .get(&method_id)
                .and_then(|implementation| implementation.label(&value))
        });
        self.credentials
            .create(&integration_id, value, label.as_deref())
            .map_err(|err| {
                tracing::warn!("failed to store integration credential: {err}");
            })
            .ok();
        self.publish_connection_updated(&integration_id);
    }

    /// `attempt.cancel` — cancel an attempt and release its resources
    /// (`integration.ts:375-385`).
    /// `connection.update` (`integration.ts:458-464`) — update a stored
    /// credential and publish the connection-updated events.
    pub fn connection_update(
        &self,
        credential_id: &str,
        label: Option<&str>,
    ) -> Result<(), CoreError> {
        let credential = self.credentials.get(credential_id)?;
        self.credentials.update(credential_id, label, None)?;
        if let Some(credential) = credential {
            self.publish_connection_updated(&credential.integration_id);
        }
        Ok(())
    }

    /// `connection.remove` (`integration.ts:466-471`).
    pub fn connection_remove(&self, credential_id: &str) -> Result<(), CoreError> {
        let credential = self.credentials.get(credential_id)?;
        self.credentials.remove(credential_id)?;
        if let Some(credential) = credential {
            self.publish_connection_updated(&credential.integration_id);
        }
        Ok(())
    }

    /// `attempt.cancel` — cancel a pending attempt and release its
    /// resources (`integration.ts:375-380`).
    pub fn attempt_cancel(&self, id: &str) {
        let mut attempts = lock(&self.attempts);
        self.scrub(&mut attempts);
        if let Some(attempt) = attempts.get(id) {
            if attempt.status == Status::Pending {
                attempts.remove(id);
            }
        }
    }

    fn publish_connection_updated(&self, integration_id: &str) {
        let _ = self.events.publish(
            &CONNECTION_UPDATED,
            json!({"integrationID": integration_id}),
            Default::default(),
        );
        let _ = self.events.publish(&UPDATED, json!({}), Default::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::SystemClock;

    struct FakeOAuth {
        integration: String,
        mode: &'static str,
    }

    impl OAuthImplementation for FakeOAuth {
        fn integration_id(&self) -> &str {
            &self.integration
        }
        fn method_id(&self) -> &str {
            "oauth"
        }
        fn authorize(&self, _inputs: &HashMap<String, String>) -> OAuthAuthorization {
            let mode = self.mode;
            OAuthAuthorization {
                url: "https://example.org/authorize".to_string(),
                instructions: "Visit the URL".to_string(),
                callback: match mode {
                    "auto" => OAuthCallback::Auto(Box::pin(async move {
                        Ok(json!({"type": "oauth", "methodID": "oauth"}))
                    })),
                    _ => OAuthCallback::Code(|code| {
                        let code = code.to_string();
                        Box::pin(async move {
                            if code == "good" {
                                Ok(json!({"type": "oauth", "methodID": "oauth"}))
                            } else {
                                Err("bad code".to_string())
                            }
                        })
                    }),
                },
            }
        }
        fn label(&self, _credential: &Value) -> Option<String> {
            Some("fake-label".to_string())
        }
    }

    fn service() -> Arc<IntegrationService> {
        let storage = Arc::new(crate::storage::Storage::open_in_memory().unwrap());
        let credentials = Arc::new(CredentialStore::new(storage.clone(), Arc::new(SystemClock)));
        let events = Arc::new(EventBus::new_shared(storage, None));
        Arc::new(IntegrationService::new(
            credentials,
            events,
            Arc::new(SystemClock),
        ))
    }

    #[test]
    fn empty_registry_lists_empty() {
        let service = service();
        assert!(service.list().is_empty());
        assert!(service.get("missing").is_none());
    }

    #[test]
    fn key_connection_replaces_credential() {
        let service = service();
        service.register(
            "github",
            "GitHub",
            vec![json!({"type": "key", "label": "API key"})],
            Vec::new(),
        );
        service
            .connection_key("github", "secret", Some("first"))
            .unwrap();
        let info = service.get("github").unwrap();
        assert_eq!(info["connections"][0]["label"], "first");
        service.connection_key("github", "secret2", None).unwrap();
        let info = service.get("github").unwrap();
        assert_eq!(info["connections"].as_array().unwrap().len(), 1);
        assert_eq!(info["connections"][0]["label"], "default");
        assert!(service.connection_key("missing", "x", None).is_err());
    }

    #[tokio::test]
    async fn code_attempt_lifecycle() {
        let service = service();
        service.register(
            "github",
            "GitHub",
            vec![json!({"type": "oauth", "id": "oauth", "label": "OAuth", "prompts": []})],
            vec![Arc::new(FakeOAuth {
                integration: "github".to_string(),
                mode: "code",
            })],
        );
        let attempt = service
            .connection_oauth("github", "oauth", &HashMap::new(), Some("mylabel"))
            .unwrap();
        assert_eq!(attempt["mode"], "code");
        let id = attempt["attemptID"].as_str().unwrap().to_string();

        // Code is required first.
        match service.attempt_complete(&id, None).await.unwrap() {
            Err(err) => assert_eq!(err.attempt_id, id),
            Ok(_) => panic!("expected CodeRequiredError"),
        }
        let status = service.attempt_status(&id).unwrap();
        assert_eq!(status["status"], "pending");

        // A failing code keeps the attempt failed with the message.
        service
            .attempt_complete(&id, Some("bad"))
            .await
            .unwrap()
            .ok();
        let status = service.attempt_status(&id).unwrap();
        assert_eq!(status["status"], "failed");
        assert_eq!(status["message"], "bad code");

        // A fresh attempt completes and stores the credential.
        let attempt = service
            .connection_oauth("github", "oauth", &HashMap::new(), None)
            .unwrap();
        let id = attempt["attemptID"].as_str().unwrap().to_string();
        service
            .attempt_complete(&id, Some("good"))
            .await
            .unwrap()
            .ok();
        let status = service.attempt_status(&id).unwrap();
        assert_eq!(status["status"], "complete");
        let info = service.get("github").unwrap();
        assert_eq!(info["connections"][0]["label"], "fake-label");
        // The settled credential resolved through the registry.
        let resolved = service.connection_resolve(&info["connections"][0]).unwrap();
        assert_eq!(resolved["methodID"], "oauth");
    }

    #[tokio::test]
    async fn auto_attempt_completes_in_background() {
        let service = service();
        service.register(
            "slack",
            "Slack",
            vec![json!({"type": "oauth", "id": "oauth", "label": "OAuth"})],
            vec![Arc::new(FakeOAuth {
                integration: "slack".to_string(),
                mode: "auto",
            })],
        );
        let attempt = service
            .connection_oauth("slack", "oauth", &HashMap::new(), None)
            .unwrap();
        assert_eq!(attempt["mode"], "auto");
        let id = attempt["attemptID"].as_str().unwrap().to_string();
        for _ in 0..100 {
            if service.attempt_status(&id).unwrap()["status"] == "complete" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(service.attempt_status(&id).unwrap()["status"], "complete");
        let active = service.connection_active("slack").unwrap();
        assert_eq!(active["type"], "credential");
        service.attempt_cancel(&id);
    }

    #[test]
    fn missing_method_is_an_authorization_error() {
        let service = service();
        service.register("github", "GitHub", vec![], Vec::new());
        let err = service
            .connection_oauth("github", "oauth", &HashMap::new(), None)
            .unwrap_err();
        assert_eq!(err.cause, "OAuth method not found: github/oauth");
    }
}
