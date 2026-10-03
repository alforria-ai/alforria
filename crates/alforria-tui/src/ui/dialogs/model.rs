//! `component/dialog-model.tsx`, `dialog-agent.tsx`,
//! `dialog-variant.tsx` and `dialog-provider.tsx` — the model/agent/
//! variant/provider option lists.

use serde_json::Value;

use super::primitives::SelectOption;
use crate::state::local::ModelRef;
use crate::state::route::Route;
use crate::state::{App, Effect, PendingDialog, Toast, ToastVariant};
use crate::ui::theme::Theme;

/// `sortModelOptions` (`dialog-model.tsx:186-197`) — `footer !== "Free"`
/// ascending puts the free models first, then release date desc, then
/// title.
fn sort_model_options(options: Vec<SelectOption>, release: Vec<String>) -> Vec<SelectOption> {
    let mut keyed: Vec<(bool, String, String, usize, SelectOption)> = options
        .into_iter()
        .enumerate()
        .zip(release)
        .map(|((index, option), release)| {
            let paid = option.footer.as_deref() != Some("Free");
            (paid, release, option.title.clone(), index, option)
        })
        .collect();
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.cmp(&b.2))
            .then(a.3.cmp(&b.3))
    });
    keyed
        .into_iter()
        .map(|(_, _, _, _, option)| option)
        .collect()
}

fn provider_models(provider: &Value) -> Vec<(String, Value)> {
    provider
        .get("models")
        .and_then(Value::as_object)
        .map(|models| {
            models
                .iter()
                .map(|(id, info)| (id.clone(), info.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

fn model_option(
    provider: &Value,
    model_id: &str,
    info: &Value,
    favorites: &[ModelRef],
    current: Option<&str>,
) -> SelectOption {
    let provider_id = provider
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let value = format!("{provider_id}/{model_id}");
    let option = SelectOption::new(info.get("name").and_then(Value::as_str).unwrap_or(model_id))
        .with_value(value.clone())
        .with_current(current == Some(value.as_str()));
    let option = if favorites
        .iter()
        .any(|favorite| favorite.provider_id == provider_id && favorite.model_id == model_id)
    {
        option.with_description("(Favorite)")
    } else {
        option
    };
    if info
        .get("cost")
        .and_then(|cost| cost.get("input"))
        .and_then(Value::as_f64)
        == Some(0.0)
        && provider_id == "opencode"
    {
        return option.with_footer("Free");
    }
    option
}

/// `DialogModel.options` (`dialog-model.tsx:26-135`): the Favorites and
/// Recent sections (connected, not filtering), then every provider's
/// models — providers `opencode` first then by name, each provider's
/// models sorted on their own, so a provider is one block.
pub fn model_options(app: &App, needle: &str) -> Vec<SelectOption> {
    let connected = crate::state::connected(app);
    let sections = connected && needle.trim().is_empty();
    let favorites = app.state.local.model_favorite().to_vec();
    let recents = app.state.local.model_recent().to_vec();
    let current = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
        .map(|model| model.key());
    let provider_name = |provider: &Value| {
        provider
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| provider.get("id").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string()
    };

    let mut options: Vec<SelectOption> = Vec::new();
    // Favorites + Recent sections (`toOptions`) — described by their
    // provider's name.
    if sections {
        let mut recent_only: Vec<ModelRef> = Vec::new();
        for item in &recents {
            if !favorites.iter().any(|favorite| {
                favorite.model_id == item.model_id && favorite.provider_id == item.provider_id
            }) {
                recent_only.push(item.clone());
            }
        }
        for (category, items) in [("Favorites", favorites.clone()), ("Recent", recent_only)] {
            for item in items {
                let Some(provider) = app.state.sync.provider.iter().find(|provider| {
                    provider.get("id").and_then(Value::as_str) == Some(&item.provider_id)
                }) else {
                    continue;
                };
                let Some((model_id, info)) = provider_models(provider)
                    .into_iter()
                    .find(|(id, _)| *id == item.model_id)
                else {
                    continue;
                };
                let option =
                    model_option(provider, &model_id, &info, &favorites, current.as_deref());
                options.push(SelectOption {
                    description: Some(provider_name(provider)),
                    ..option.with_category(category)
                });
            }
        }
    }

    let mut providers: Vec<&Value> = app.state.sync.provider.iter().collect();
    providers.sort_by_key(|provider| {
        (
            provider.get("id").and_then(Value::as_str) != Some("opencode"),
            provider_name(provider),
        )
    });
    for provider in providers {
        let provider_id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut model_list: Vec<SelectOption> = Vec::new();
        let mut release: Vec<String> = Vec::new();
        for (model_id, info) in provider_models(provider) {
            if info.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            if provider_id == "opencode" && model_id.contains("-nano") {
                continue;
            }
            if sections {
                let known = favorites
                    .iter()
                    .chain(recents.iter())
                    .any(|item| item.provider_id == provider_id && item.model_id == model_id);
                if known {
                    continue;
                }
            }
            let option = model_option(provider, &model_id, &info, &favorites, current.as_deref());
            model_list.push(if connected {
                option.with_category(provider_name(provider))
            } else {
                option
            });
            // `releaseDate` sorts as given — ISO dates as strings, numbers
            // as numbers.
            release.push(match info.get("release_date") {
                Some(Value::Number(number)) => format!("{:>20}", number),
                Some(Value::String(date)) => date.clone(),
                _ => String::new(),
            });
        }
        options.extend(sort_model_options(model_list, release));
    }
    // The `popularProviders` of the disconnected model dialog
    // (`dialog-model.tsx:108-117,129`).
    if !connected {
        let theme = app
            .ui
            .theme
            .resolve(&app.state.kv)
            .expect("builtin theme resolves");
        options.extend(
            provider_options(app, &theme)
                .into_iter()
                .take(6)
                .map(|option| option.with_category("Popular providers")),
        );
    }
    options
}

/// `DialogModel.onSelect` (`dialog-model.tsx:138-153`) — set the model,
/// then follow the variant chain.
pub fn select_model(app: &mut App, value: &str) -> Vec<Effect> {
    let Some((provider_id, model_id)) = value.split_once('/') else {
        return Vec::new();
    };
    let model = ModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    };
    let sync = std::mem::take(&mut app.state.sync);
    let args = app.state.args.clone();
    let current = app.state.local.variant_selected(&sync, &args);
    let list = app.state.local.variant_list(&sync, &args);
    if let Some(toast) = app.state.local.model_set(&sync, model, true) {
        app.show_toast(toast);
    }
    app.state.sync = sync;
    let keep = current.as_deref() == Some("default")
        || current.as_ref().is_some_and(|cur| list.contains(cur));
    if keep {
        crate::ui::dialogs::clear(app);
        return Vec::new();
    }
    if !list.is_empty() {
        return crate::ui::dialogs::open(app, crate::state::PendingDialog::Variant);
    }
    crate::ui::dialogs::clear(app);
    Vec::new()
}

/// The favorite toggle (`dialog-model.tsx:96-99`).
pub fn toggle_favorite(app: &mut App, value: &str) {
    let Some((provider_id, model_id)) = value.split_once('/') else {
        return;
    };
    let model = ModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    };
    let sync = std::mem::take(&mut app.state.sync);
    if let Some(toast) = app.state.local.model_toggle_favorite(&sync, &model) {
        app.show_toast(toast);
    }
    app.state.sync = sync;
}

/// `DialogAgent` (`dialog-agent.tsx`).
pub fn agent_options(app: &App) -> Vec<SelectOption> {
    let current = app
        .state
        .local
        .agent_current(&app.state.sync)
        .and_then(|agent| agent.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    crate::state::local::LocalState::agent_values(&app.state.sync)
        .iter()
        .map(|agent| {
            let name = agent
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            // `item.native ? "native" : item.description`
            // (`dialog-agent.tsx:15`).
            let description = if agent.get("native").and_then(Value::as_bool) == Some(true) {
                Some("native".to_string())
            } else {
                agent
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            };
            let option = SelectOption::new(name.clone())
                .with_value(name.clone())
                .with_current(current.as_deref() == Some(name.as_str()));
            match description {
                Some(description) if !description.is_empty() => {
                    option.with_description(description)
                }
                _ => option,
            }
        })
        .collect()
}

/// `DialogVariant` (`dialog-variant.tsx`).
pub fn variant_options(app: &App) -> Vec<SelectOption> {
    let current = app
        .state
        .local
        .variant_current(&app.state.sync, &app.state.args);
    let mut options = vec![SelectOption::new("Default")
        .with_value("default")
        .with_current(current.as_deref() == Some("default"))];
    let list = app
        .state
        .local
        .variant_list(&app.state.sync, &app.state.args);
    for variant in list {
        options.push(
            SelectOption::new(variant.clone())
                .with_value(variant.clone())
                .with_current(current.as_deref() == Some(variant.as_str())),
        );
    }
    options
}

const PROVIDER_PRIORITY: &[(&str, i32)] = &[
    ("opencode", 0),
    ("opencode-go", 1),
    ("libertai", 2),
    ("openai", 3),
    ("github-copilot", 4),
    ("anthropic", 5),
    ("google", 6),
];

const CUSTOM_PROVIDER_OPTION_VALUE: &str = "__opencode_custom_provider__";

/// `providerOptions` (`dialog-provider.tsx:51-118`) — plus the
/// connected `✓` gutter and the console-managed org footer
/// (`dialog-provider.tsx:135-144`).
pub fn provider_options(app: &App, theme: &Theme) -> Vec<SelectOption> {
    let all = app
        .state
        .sync
        .provider_next
        .get("all")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let connected_ids = app
        .state
        .sync
        .provider_next
        .get("connected")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<&str>>())
        .unwrap_or_default();
    let console_managed = app
        .state
        .sync
        .console_state
        .get("consoleManagedProviders")
        .and_then(Value::as_array)
        .map(|ids| ids.iter().filter_map(Value::as_str).collect::<Vec<&str>>())
        .unwrap_or_default();
    let active_org = app
        .state
        .sync
        .console_state
        .get("activeOrgName")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let onboarded = crate::state::connected(app);
    let mut providers: Vec<Value> = all;
    providers.sort_by(|a, b| {
        let priority = |provider: &Value| {
            let id = provider
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            PROVIDER_PRIORITY
                .iter()
                .find(|(name, _)| *name == id)
                .map(|(_, priority)| *priority)
                .unwrap_or(99)
        };
        priority(a)
            .cmp(&priority(b))
            .then_with(|| {
                a.get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase()
                    .cmp(
                        &b.get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_lowercase(),
                    )
            })
            .then_with(|| {
                a.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(b.get("id").and_then(Value::as_str).unwrap_or_default())
            })
    });
    let mut options = Vec::new();
    for provider in providers {
        let id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let description = match id.as_str() {
            "opencode" => Some("(Recommended)"),
            "anthropic" => Some("(API key)"),
            "openai" => Some("(ChatGPT Plus/Pro or API key)"),
            "opencode-go" => Some("Low cost subscription for everyone"),
            "libertai" => Some("LibertAI decentralized inference"),
            _ => None,
        };
        let option = SelectOption::new(
            provider
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        )
        .with_value(id.clone())
        .with_category(if PROVIDER_PRIORITY.iter().any(|(name, _)| *name == id) {
            "Popular"
        } else {
            "Providers"
        });
        // `gutter: connected && onboarded() ? ✓ : undefined`
        // (`dialog-provider.tsx:136,144`).
        let option = if connected_ids.contains(&id.as_str()) && onboarded {
            option
                .with_gutter(Some("✓".to_string()))
                .with_gutter_fg(Some(theme.success))
        } else {
            option
        };
        let option = if console_managed.contains(&id.as_str()) {
            option.with_footer(active_org)
        } else {
            option
        };
        let option = match description {
            Some(description) => option.with_description(description),
            None => option,
        };
        options.push(option);
    }
    options.push(
        SelectOption::new("Other")
            .with_value(CUSTOM_PROVIDER_OPTION_VALUE)
            .with_description("Custom provider")
            .with_category("Providers"),
    );
    options
}

/// `Select auth method` (`dialog-provider.tsx:166-175`). The option value
/// is the method index; an "oauth" method continues into the
/// `provider.oauth.authorize` flow, the "api" method into the
/// [`PendingDialog::ProviderApiKey`](crate::state::PendingDialog::ProviderApiKey)
/// prompt.
pub fn auth_method_options(app: &App, provider_id: &str) -> Vec<SelectOption> {
    let methods = app
        .state
        .sync
        .provider_auth
        .get(provider_id)
        .cloned()
        .unwrap_or_default();
    let mut options = if methods.is_empty() {
        vec![SelectOption::new("API key").with_value("api")]
    } else {
        methods
            .iter()
            .enumerate()
            .map(|(index, method)| {
                SelectOption::new(
                    method
                        .get("label")
                        .and_then(Value::as_str)
                        .unwrap_or("API key")
                        .to_string(),
                )
                .with_value(index.to_string())
            })
            .collect()
    };
    // No TS counterpart (its TUI can't disconnect): the web client's
    // Sign out / Remove, offered for a credential stored in auth.json —
    // env and config keys aren't ours to drop.
    if stored_credential(app, provider_id) {
        let signs_in = methods.iter().any(|method| method["type"] == "oauth");
        options.push(
            SelectOption::new(if signs_in {
                "Sign out"
            } else {
                "Remove API key"
            })
            .with_value(SIGN_OUT_OPTION_VALUE),
        );
    }
    options
}

/// The auth-method option that drops the stored credential.
pub const SIGN_OUT_OPTION_VALUE: &str = "__alforria_sign_out__";

/// Whether the provider is connected through an auth.json credential
/// (`source: "api"` in `provider.list`).
fn stored_credential(app: &App, provider_id: &str) -> bool {
    let provider_next = &app.state.sync.provider_next;
    let connected = provider_next
        .get("connected")
        .and_then(Value::as_array)
        .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(provider_id)));
    connected
        && provider_next
            .get("all")
            .and_then(Value::as_array)
            .and_then(|all| {
                all.iter().find(|provider| {
                    provider.get("id").and_then(Value::as_str) == Some(provider_id)
                })
            })
            .is_some_and(|provider| provider.get("source").and_then(Value::as_str) == Some("api"))
}

/// `auth.remove` finished: say so, and leave the dialog closed.
pub fn signed_out(app: &mut App, provider_id: &str, result: &anyhow::Result<bool>) {
    let name = provider_name(app, provider_id);
    app.show_toast(match result {
        Ok(_) => Toast {
            title: None,
            variant: ToastVariant::Success,
            message: format!("Signed out of {name}"),
            duration_ms: 5000,
        },
        Err(error) => Toast {
            title: None,
            variant: ToastVariant::Error,
            message: format!("{error:#}"),
            duration_ms: 5000,
        },
    });
}

/// `methods[index]` for a selected auth-method option — `None` for the
/// fallback "api" option of a provider without listed methods.
pub fn auth_method(app: &App, provider_id: &str, value: &str) -> Option<Value> {
    let index = value.parse::<usize>().ok()?;
    app.state
        .sync
        .provider_auth
        .get(provider_id)?
        .get(index)
        .cloned()
}

/// The provider's display name from `provider.list`, else its id.
fn provider_name(app: &App, provider_id: &str) -> String {
    app.state
        .sync
        .provider_next
        .get("all")
        .and_then(Value::as_array)
        .and_then(|all| {
            all.iter()
                .find(|provider| provider.get("id").and_then(Value::as_str) == Some(provider_id))
        })
        .and_then(|provider| provider.get("name").and_then(Value::as_str))
        .unwrap_or(provider_id)
        .to_string()
}

// ------------------------------------------------------------- oauth flow

/// Start an OAuth sign-in: a fresh flow id. Any older flow still in
/// flight becomes stale — its late result is dropped.
pub fn oauth_begin(app: &mut App) -> u64 {
    app.ui.oauth_seq += 1;
    app.ui.oauth_flow = Some(app.ui.oauth_seq);
    app.ui.oauth_seq
}

/// Whether the top dialog is `flow`'s sign-in.
fn oauth_dialog_open(app: &App, flow: u64) -> bool {
    matches!(
        app.ui.dialogs.top_kind(),
        Some(PendingDialog::ProviderOauth { flow: open, .. }) if *open == flow
    )
}

/// `authorize` resolved: show `AutoMethod` / `CodeMethod`
/// (`dialog-provider.tsx:196-205`). Returns the URL to open when the
/// flow should now wait on the no-code callback (`method: "auto"`).
pub fn oauth_authorized(
    app: &mut App,
    provider_id: &str,
    method: usize,
    flow: u64,
    title: &str,
    authorization: &Value,
) -> Option<String> {
    if app.ui.oauth_flow != Some(flow) {
        return None;
    }
    let field = |key: &str| {
        authorization
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let (url, kind) = (field("url"), field("method"));
    if url.is_empty() || (kind != "auto" && kind != "code") {
        // `authorize` resolved without a usable result — nothing to show.
        app.ui.oauth_flow = None;
        return None;
    }
    let auto = kind == "auto";
    super::open(
        app,
        PendingDialog::ProviderOauth {
            provider_id: provider_id.to_string(),
            method,
            flow,
            title: title.to_string(),
            url: url.clone(),
            instructions: field("instructions"),
            auto,
            rejected: false,
        },
    );
    auto.then_some(url)
}

/// The sign-in completed (the loopback redirect or an accepted paste).
/// `None` when `flow` is stale or already settled; otherwise whether its
/// dialog was still open — the caller then offers the model picker
/// (`dialog.replace(DialogModel)`, `dialog-provider.tsx:280`).
pub fn oauth_succeeded(app: &mut App, provider_id: &str, flow: u64) -> Option<bool> {
    if app.ui.oauth_flow != Some(flow) {
        return None;
    }
    app.ui.oauth_flow = None;
    let open = oauth_dialog_open(app, flow);
    if open {
        super::clear(app);
    }
    let name = provider_name(app, provider_id);
    app.show_toast(Toast {
        title: None,
        variant: ToastVariant::Success,
        message: format!("Signed in to {name}"),
        duration_ms: 5000,
    });
    Some(open)
}

/// `authorize` or the no-code callback failed. With the sign-in still on
/// screen (or `authorize` itself failing), show why and go back to the
/// method list — retry, or fall back to an API key. A failure after the
/// user dismissed the dialog (the server-side wait timing out) stays
/// quiet.
pub fn oauth_failed(
    app: &mut App,
    provider_id: &str,
    flow: u64,
    error: &anyhow::Error,
    authorizing: bool,
) {
    if app.ui.oauth_flow != Some(flow) {
        return;
    }
    app.ui.oauth_flow = None;
    if !authorizing && !oauth_dialog_open(app, flow) {
        return;
    }
    let text = format!("{error:#}");
    let message = if text.contains("ProviderAuthOauthCallbackFailed") {
        "Sign-in failed. Try again, or use an API key.".to_string()
    } else {
        text
    };
    app.show_toast(Toast {
        title: None,
        variant: ToastVariant::Error,
        message,
        duration_ms: 5000,
    });
    super::open(
        app,
        PendingDialog::ProviderAuthMethod {
            provider_id: provider_id.to_string(),
        },
    );
}

/// A pasted code was rejected: flag it on the dialog (`Invalid code`,
/// `dialog-provider.tsx:336`). The flow stays pending — the browser
/// redirect or a corrected paste can still finish it.
pub fn oauth_rejected(app: &mut App, flow: u64) {
    if app.ui.oauth_flow != Some(flow) || !oauth_dialog_open(app, flow) {
        return;
    }
    if let Some(PendingDialog::ProviderOauth { rejected, .. }) =
        app.ui.dialogs.top_mut().map(|frame| &mut frame.kind)
    {
        *rejected = true;
    }
}

/// The route session id (used by the export dialog default filename).
pub fn route_session_id(app: &App) -> Option<&str> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id),
        _ => None,
    }
}
