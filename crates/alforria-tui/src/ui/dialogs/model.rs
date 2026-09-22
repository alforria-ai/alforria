//! `component/dialog-model.tsx`, `dialog-agent.tsx`,
//! `dialog-variant.tsx` and `dialog-provider.tsx` — the model/agent/
//! variant/provider option lists.

use serde_json::Value;

use super::primitives::SelectOption;
use crate::state::local::ModelRef;
use crate::state::route::Route;
use crate::state::{App, Effect};
use crate::ui::theme::Theme;

/// `sortModelOptions` (`dialog-model.tsx:178-197`) — free models sort
/// last, then by release date desc, then title.
fn sort_model_options(options: Vec<SelectOption>, release: Vec<i64>) -> Vec<SelectOption> {
    let mut keyed: Vec<(bool, i64, String, usize, SelectOption)> = options
        .into_iter()
        .enumerate()
        .zip(release)
        .map(|((index, option), release)| {
            let free = option.footer.as_deref() == Some("Free");
            (free, release, option.title.clone(), index, option)
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

/// `DialogModel.options` (`dialog-model.tsx:26-135`).
pub fn model_options(app: &App) -> Vec<SelectOption> {
    let connected = crate::state::connected(app);
    let favorites = app.state.local.model_favorite().to_vec();
    let recents = app.state.local.model_recent().to_vec();
    let current = app
        .state
        .local
        .model_current(&app.state.sync, &app.state.args)
        .map(|model| model.key());

    let mut options: Vec<SelectOption> = Vec::new();
    // Favorites + Recent sections (`toOptions`).
    if connected {
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
                options.push(
                    model_option(provider, &model_id, &info, &favorites, current.as_deref())
                        .with_category(category),
                );
            }
        }
    }

    let mut model_list: Vec<SelectOption> = Vec::new();
    let mut release: Vec<i64> = Vec::new();
    for provider in &app.state.sync.provider {
        let provider_id = provider
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for (model_id, info) in provider_models(provider) {
            if info.get("status").and_then(Value::as_str) == Some("deprecated") {
                continue;
            }
            if provider_id == "opencode" && model_id.contains("-nano") {
                continue;
            }
            if connected {
                let known = favorites
                    .iter()
                    .chain(recents.iter())
                    .any(|item| item.provider_id == provider_id && item.model_id == model_id);
                if known {
                    continue;
                }
            }
            model_list.push(if connected {
                model_option(provider, &model_id, &info, &favorites, current.as_deref())
                    .with_category(
                        provider
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(provider_id)
                            .to_string(),
                    )
            } else {
                model_option(provider, &model_id, &info, &favorites, current.as_deref())
            });
            release.push(
                info.get("release_date")
                    .and_then(Value::as_i64)
                    .or_else(|| {
                        info.get("release_date")
                            .and_then(Value::as_str)
                            .and_then(|v| v.parse().ok())
                    })
                    .unwrap_or(0),
            );
        }
    }
    options.extend(sort_model_options(model_list, release));
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

/// `Select auth method` (`dialog-provider.tsx:166-175`). The
/// oauth/credential submission is a recorded seam gap — the list is
/// informational.
pub fn auth_method_options(app: &App, provider_id: &str) -> Vec<SelectOption> {
    let methods = app
        .state
        .sync
        .provider_auth
        .get(provider_id)
        .cloned()
        .unwrap_or_default();
    if methods.is_empty() {
        return vec![SelectOption::new("API key").with_value("api")];
    }
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
}

/// The route session id (used by the export dialog default filename).
pub fn route_session_id(app: &App) -> Option<&str> {
    match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id),
        _ => None,
    }
}
