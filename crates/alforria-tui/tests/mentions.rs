//! `@` file mentions against the real server router, in memory: typing
//! `@mai#3-5` asks `GET /api/fs/find`, the hit shows in the popup and
//! Enter attaches it as a `file` part with the line range.

use std::sync::Arc;

use alforria_server::engine::EngineSeams;
use alforria_server::{routes, ListenOptions};
use alforria_tui::state::prompt::PromptPart;
use alforria_tui::state::{self, App, Args, Effect, Msg};
use alforria_tui::transport::api::{HttpClientConfig, HttpServerApi, ServerApi};
use alforria_tui::transport::backend::{ServiceBackend, IN_PROCESS_BASE_URL};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

async fn press(app: &Arc<tokio::sync::Mutex<App>>, api: &Arc<dyn ServerApi>, code: KeyCode) {
    let effects = state::update(
        &mut *app.lock().await,
        Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)),
    );
    for effect in effects {
        if matches!(effect, Effect::FindFiles { .. }) {
            alforria_tui::execute_effect(app, Arc::clone(api), effect).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_mention_finds_a_file_and_attaches_a_line_range() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();

    let paths = alforria_core::GlobalPaths::resolve(dir.path().to_path_buf());
    let mut ctx = alforria_server::production_context(
        &ListenOptions::default(),
        paths,
        EngineSeams::default(),
    )
    .expect("server context");
    Arc::get_mut(&mut ctx).expect("sole owner").auth =
        alforria_server::state::AuthConfig::new("alforria", None);
    let router = routes::build_router(ctx);
    let api: Arc<dyn ServerApi> = Arc::new(HttpServerApi::with_backend(
        HttpClientConfig {
            base_url: IN_PROCESS_BASE_URL.to_string(),
            directory: Some(repo.to_string_lossy().into_owned()),
            headers: Vec::new(),
        },
        Arc::new(ServiceBackend::new(router)),
    ));

    let state_dir = tempfile::tempdir().unwrap();
    let app = Arc::new(tokio::sync::Mutex::new(App::new(
        alforria_tui::config::TuiConfig::default(),
        Args::default(),
        Some(state_dir.path()),
    )));
    app.lock().await.ui.prompt_focused = true;

    for char in "@mai#3-5".chars() {
        press(&app, &api, KeyCode::Char(char)).await;
    }
    {
        let app = app.lock().await;
        let displays: Vec<&str> = app
            .ui
            .prompt
            .autocomplete
            .options
            .iter()
            .map(|option| option.display.as_str())
            .collect();
        assert!(
            displays.contains(&"src/main.rs#3-5"),
            "the server's hit, with the range: {displays:?}"
        );
    }
    // Select the file row.
    {
        let mut app = app.lock().await;
        let index = app
            .ui
            .prompt
            .autocomplete
            .options
            .iter()
            .position(|option| option.display == "src/main.rs#3-5")
            .unwrap();
        app.ui.prompt.autocomplete.selected = index;
    }
    press(&app, &api, KeyCode::Enter).await;
    let app = app.lock().await;
    assert_eq!(app.ui.prompt.input(), "@src/main.rs#3-5 ");
    let PromptPart::File {
        url,
        filename,
        source,
        mime,
    } = &app.ui.prompt.parts[0]
    else {
        panic!("a file part: {:?}", app.ui.prompt.parts);
    };
    assert_eq!(mime, "text/plain");
    assert_eq!(filename.as_deref(), Some("src/main.rs#3-5"));
    assert!(
        url.starts_with("file://") && url.ends_with("/repo/src/main.rs?start=3&end=5"),
        "{url}"
    );
    let source = source.as_ref().unwrap();
    assert_eq!(source.path.as_deref(), Some("src/main.rs"));
    let text = source.text.as_ref().unwrap();
    assert_eq!((text.start, text.end), (0, 16));
    assert_eq!(text.value, "@src/main.rs#3-5");
}
