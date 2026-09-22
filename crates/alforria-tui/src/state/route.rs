//! `route.tsx` — the `Route` union + carried prompt (M8.2).

use serde_json::{Map, Value};

use crate::state::Args;

/// `PromptInfo` (`prompt/history.tsx:9-26`): input + mode + part-input
/// payloads. Parts stay `Value` — they are the `Omit<…Part, …>` part
/// *input* shapes, which have no M1 DTO (TODO(M8.6)).
#[derive(Debug, Clone, PartialEq)]
pub struct PromptInfo {
    pub input: String,
    pub mode: Option<PromptMode>,
    pub parts: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    Normal,
    Shell,
}

/// `Route` union (`route.tsx:6-23`).
#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Home {
        prompt: Option<PromptInfo>,
    },
    Session {
        session_id: String,
        prompt: Option<PromptInfo>,
    },
    Plugin {
        id: String,
        data: Option<Map<String, Value>>,
    },
}

impl Route {
    /// `initialRoute(value)` (`route.tsx:44-53`): validate a startup
    /// route value, stripping `prompt` and `data`.
    pub fn parse_startup(value: &Value) -> Option<Route> {
        let obj = value.as_object()?;
        match obj.get("type")?.as_str()? {
            "home" => Some(Route::Home { prompt: None }),
            "session" => {
                let session_id = obj.get("sessionID")?.as_str()?;
                Some(Route::Session {
                    session_id: session_id.to_string(),
                    prompt: None,
                })
            }
            "plugin" => {
                let id = obj.get("id")?.as_str()?;
                Some(Route::Plugin {
                    id: id.to_string(),
                    data: None,
                })
            }
            _ => None,
        }
    }
}

/// `RouteProvider` (`route.tsx:25-42`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteStore {
    pub data: Route,
}

impl RouteStore {
    /// `props.initialRoute ?? initialRoute(startup.initialRoute) ??
    /// {type:"home"}` — with the provider's own `initialRoute` prop set
    /// to the `--continue` dummy session (`app.tsx:287-293`).
    pub fn new(args: &Args, startup: Option<Route>) -> RouteStore {
        let data = if args.continue_ {
            Route::Session {
                session_id: "dummy".to_string(),
                prompt: None,
            }
        } else {
            startup.unwrap_or(Route::Home { prompt: None })
        };
        RouteStore { data }
    }

    pub fn navigate(&mut self, route: Route) {
        self.data = route;
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn initial_route_prefers_continue_dummy() {
        let args = Args {
            continue_: true,
            ..Args::default()
        };
        let mut route = RouteStore::new(&args, None);
        assert_eq!(
            route.data,
            Route::Session {
                session_id: "dummy".to_string(),
                prompt: None,
            }
        );
        route.navigate(Route::Home { prompt: None });
        assert_eq!(route.data, Route::Home { prompt: None });
    }

    #[test]
    fn startup_route_is_validated_and_stripped() {
        let session = Route::parse_startup(
            &json!({"type": "session", "sessionID": "ses_1", "prompt": {"input": "x"}}),
        )
        .expect("parses");
        assert_eq!(
            session,
            Route::Session {
                session_id: "ses_1".to_string(),
                prompt: None,
            }
        );
        assert_eq!(
            Route::parse_startup(&json!({"type": "plugin", "id": "diff"})),
            Some(Route::Plugin {
                id: "diff".to_string(),
                data: None,
            })
        );
        assert_eq!(
            Route::parse_startup(&json!({"type": "home"})),
            Some(Route::Home { prompt: None })
        );
        assert_eq!(Route::parse_startup(&json!({"type": "other"})), None);
        assert_eq!(Route::parse_startup(&json!({"type": "session"})), None);
        assert_eq!(Route::parse_startup(&json!("string")), None);
    }

    #[test]
    fn startup_route_used_when_not_continuing() {
        let args = Args::default();
        let route = RouteStore::new(
            &args,
            Some(Route::Session {
                session_id: "ses_9".to_string(),
                prompt: None,
            }),
        );
        assert_eq!(
            route.data,
            Route::Session {
                session_id: "ses_9".to_string(),
                prompt: None,
            }
        );
    }
}
