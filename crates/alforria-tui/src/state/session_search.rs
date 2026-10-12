//! The session list's server-side search (`dialog-session-list.tsx`):
//! typing in the list asks `GET /session?roots=true&limit=30&search=…`
//! after a 150 ms debounce, so sessions older than the synced 30-day
//! window are found too. The dialog renders [`sessions`]; the query is
//! scheduled by [`observe`] after every update.

use alforria_schema::session_v1::V1SessionInfo;

use crate::state::route::Route;
use crate::state::{App, Effect, PendingDialog};

/// `createDebouncedSignal("", 150)`.
pub const DEBOUNCE_MS: u64 = 150;

#[derive(Debug, Default)]
pub struct SessionSearch {
    /// The trimmed filter text last seen in the open session list.
    pub query: String,
    /// When the query becomes due (debounce deadline, `tick_ms` clock).
    pub due_at: Option<u64>,
    /// The query the results answer, and the results.
    pub results: Option<(String, Vec<V1SessionInfo>)>,
}

/// `createDialogSessionListQuery({ search, filter })`.
pub fn list_query(
    search: &str,
    filter: crate::transport::api::SessionListQuery,
) -> crate::transport::api::SessionListQuery {
    crate::transport::api::SessionListQuery {
        roots: Some(true),
        limit: Some(if search.is_empty() { 100 } else { 30 }),
        search: (!search.is_empty()).then(|| search.to_string()),
        ..filter
    }
}

/// The open session list's filter text, if it is the top dialog.
fn open_filter(app: &App) -> Option<String> {
    let frame = app.ui.dialogs.top()?;
    matches!(frame.kind, PendingDialog::SessionList).then(|| frame.select.filter.trim().to_string())
}

/// Track the list's filter: a change re-arms the debounce; a due query
/// becomes an [`Effect::SessionSearch`]. Closing the list forgets it all.
pub fn observe(app: &mut App) -> Option<Effect> {
    let now = app.ui.tick_ms;
    let Some(filter) = open_filter(app) else {
        app.ui.session_search = SessionSearch::default();
        return None;
    };
    let search = &mut app.ui.session_search;
    if filter != search.query {
        search.query = filter;
        search.due_at = (!search.query.is_empty()).then_some(now + DEBOUNCE_MS);
        return None;
    }
    let due = search.due_at.is_some_and(|due| now >= due);
    if !due {
        return None;
    }
    search.due_at = None;
    Some(Effect::SessionSearch {
        query: search.query.clone(),
    })
}

/// A search answer: kept only while it is still the query typed.
pub fn found(app: &mut App, query: &str, sessions: Vec<V1SessionInfo>) {
    if app.ui.session_search.query == query {
        app.ui.session_search.results = Some((query.to_string(), sessions));
    }
}

/// The sessions the list shows (`sessions()`, `dialog-session-list.tsx:
/// 79-95`): the search results for the current query when they are in,
/// else the synced list; synced copies win (fresher titles and status),
/// and the current and pinned sessions stay listed.
pub fn sessions<'a>(app: &'a App, filter: &str) -> Vec<&'a V1SessionInfo> {
    let synced = &app.state.sync.session;
    let query = filter.trim();
    let results = app
        .ui
        .session_search
        .results
        .as_ref()
        .filter(|(answered, _)| !query.is_empty() && answered == query)
        .map(|(_, results)| results);
    let Some(results) = results else {
        return synced.iter().collect();
    };
    let mut out: Vec<&V1SessionInfo> = results
        .iter()
        .map(|session| {
            synced
                .iter()
                .find(|synced| synced.id == session.id)
                .unwrap_or(session)
        })
        .collect();
    let current = match &app.state.route.data {
        Route::Session { session_id, .. } => Some(session_id.clone()),
        _ => None,
    };
    for id in current
        .into_iter()
        .chain(app.state.local.session_pinned().iter().cloned())
    {
        if out.iter().any(|session| session.id == id) {
            continue;
        }
        if let Some(session) = synced.iter().find(|session| session.id == id) {
            out.push(session);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{update, Args, Msg};

    fn session(id: &str, title: &str, updated: u64) -> V1SessionInfo {
        V1SessionInfo {
            id: id.into(),
            slug: "x".into(),
            project_id: "prj".into(),
            workspace_id: None,
            directory: "/x".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: None,
            tokens: None,
            share: None,
            title: title.into(),
            agent: None,
            model: None,
            version: "1".into(),
            metadata: None,
            time: alforria_schema::session_v1::V1SessionTime {
                created: updated,
                updated,
                compacting: None,
                archived: None,
            },
            permission: None,
            revert: None,
        }
    }

    #[test]
    fn typing_in_the_list_searches_the_server_after_the_debounce() {
        let mut app = App::new(crate::config::TuiConfig::default(), Args::default(), None);
        app.state.sync.session = vec![session("ses_new", "Recent work", 10)];
        let _ = crate::ui::dialogs::open(&mut app, PendingDialog::SessionList);
        app.ui.tick_ms = 1000;
        for char in "old".chars() {
            let effects = update(
                &mut app,
                Msg::Key(crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(char),
                    crossterm::event::KeyModifiers::NONE,
                )),
            );
            assert!(
                !effects
                    .iter()
                    .any(|effect| matches!(effect, Effect::SessionSearch { .. })),
                "debounced"
            );
        }
        assert_eq!(app.ui.session_search.query, "old");
        let effects = update(&mut app, Msg::Tick(std::time::Duration::from_millis(1200)));
        assert!(
            effects.contains(&Effect::SessionSearch {
                query: "old".into()
            }),
            "{effects:?}"
        );
        // The server finds a session outside the synced window.
        found(
            &mut app,
            "old",
            vec![session("ses_old", "An old thread", 1)],
        );
        let listed: Vec<&str> = sessions(&app, "old")
            .iter()
            .map(|session| session.id.as_str())
            .collect();
        assert_eq!(listed, vec!["ses_old"]);
        let rows =
            crate::ui::dialogs::sessions::session_list_options(&app, app.ui.dialogs.top().unwrap());
        assert!(
            rows.iter().any(|row| row.title == "An old thread"),
            "the dialog shows the server hit"
        );
        // A stale answer is dropped.
        found(&mut app, "ol", vec![]);
        assert_eq!(sessions(&app, "old").len(), 1);
    }

    #[test]
    fn the_search_query_matches_ts() {
        let query = list_query("old", crate::transport::api::SessionListQuery::default());
        assert_eq!(query.roots, Some(true));
        assert_eq!(query.limit, Some(30));
        assert_eq!(query.search.as_deref(), Some("old"));
    }
}
