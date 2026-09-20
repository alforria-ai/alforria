//! cli/cmd/models.ts port — the `models` command.

use clap::ArgMatches;
use std::cmp::Ordering;

use opencode_core::catalog::{Model, Provider};
use opencode_core::CatalogService;

use crate::error::{core_error, CliError, TypedError};
use crate::ui::{self, Ui};

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let catalog = crate::catalog::catalog_service();
    run_with(
        matches.get_one::<String>("provider").map(String::as_str),
        matches.get_flag("verbose"),
        matches.get_flag("refresh"),
        &catalog,
        ui,
    )
}

pub fn run_with(
    provider: Option<&str>,
    verbose: bool,
    refresh: bool,
    catalog: &CatalogService,
    ui: &mut Ui,
) -> Result<(), TypedError> {
    if refresh {
        // Refresh errors are logged and swallowed (models-dev.ts:237-253).
        catalog.refresh(true);
        ui.println(&format!(
            "{}Models cache refreshed{}",
            ui::style::TEXT_SUCCESS_BOLD,
            ui::style::TEXT_NORMAL
        ));
    }
    let providers = catalog.get().map_err(core_error)?;
    if let Some(id) = provider {
        let Some(found) = providers.get(id) else {
            return Err(TypedError::Cli(CliError::new(format!(
                "Provider not found: {id}"
            ))));
        };
        print_provider(ui, id, found, verbose);
        return Ok(());
    }
    let mut ids: Vec<&String> = providers.keys().collect();
    ids.sort_by(|a, b| compare_provider_ids(a.as_str(), b.as_str()));
    for id in ids {
        print_provider(ui, id, &providers[id], verbose);
    }
    Ok(())
}

/// models.ts:56-62 — `opencode*` providers first, then alphabetical.
fn compare_provider_ids(a: &str, b: &str) -> Ordering {
    let a_opencode = a.starts_with("opencode");
    let b_opencode = b.starts_with("opencode");
    match (a_opencode, b_opencode) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.cmp(b),
    }
}

/// models.ts:36-47 — `{providerID}/{modelID}` per model, sorted by model id;
/// `--verbose` appends the pretty-printed model document.
fn print_provider(ui: &mut Ui, provider_id: &str, provider: &Provider, verbose: bool) {
    let mut models: Vec<(&String, &Model)> = provider.models.iter().collect();
    models.sort_by(|a, b| a.0.cmp(b.0));
    for (model_id, model) in models {
        ui.write_stdout(&format!("{provider_id}/{model_id}\n"));
        if verbose {
            let json = serde_json::to_string_pretty(model).unwrap_or_default();
            ui.write_stdout(&format!("{json}\n"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::Arc;

    use opencode_core::catalog::{FetchError, Fetcher};

    struct FixtureFetcher {
        text: String,
        calls: AtomicUsize,
    }

    impl Fetcher for FixtureFetcher {
        fn get(&self, _url: &str, _user_agent: &str) -> Result<String, FetchError> {
            self.calls.fetch_add(1, AtomicOrdering::SeqCst);
            Ok(self.text.clone())
        }
    }

    fn fixture_catalog(dir: &Path) -> (Arc<CatalogService>, Arc<FixtureFetcher>) {
        let fixture = dir.join("models.json");
        std::fs::write(&fixture, FIXTURE).unwrap();
        let fetcher = Arc::new(FixtureFetcher {
            text: FIXTURE.to_string(),
            calls: AtomicUsize::new(0),
        });
        let cfg = opencode_core::CatalogConfig {
            models_path_override: Some(fixture),
            ..opencode_core::CatalogConfig::from_env()
        };
        let clock = Arc::new(opencode_core::catalog::SystemClock);
        (
            Arc::new(CatalogService::new(
                dir.join("cache"),
                cfg,
                clock,
                fetcher.clone(),
            )),
            fetcher,
        )
    }

    #[test]
    fn lists_all_providers_opencode_first_then_alphabetical() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        run_with(None, false, false, &catalog, &mut ui).unwrap();
        let stdout = captured.stdout();
        assert_eq!(
            stdout,
            "opencode/grok-code\nacme/acme-1\nanthropic/claude-3-7\nanthropic/claude-4\nopenai/gpt-5.1\nopenai/o4-mini\n"
        );
    }

    #[test]
    fn filters_by_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        run_with(Some("openai"), false, false, &catalog, &mut ui).unwrap();
        assert_eq!(captured.stdout(), "openai/gpt-5.1\nopenai/o4-mini\n");
    }

    #[test]
    fn unknown_provider_fails() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(dir.path());
        let (mut ui, _captured) = Ui::capture(false);
        let err = run_with(Some("nope"), false, false, &catalog, &mut ui).unwrap_err();
        let message = crate::error::format_error(&err).unwrap();
        assert_eq!(message, "Provider not found: nope");
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn verbose_appends_model_json() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, _) = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        run_with(Some("acme"), true, false, &catalog, &mut ui).unwrap();
        let stdout = captured.stdout();
        assert!(stdout.starts_with("acme/acme-1\n"), "{stdout}");
        let doc = stdout.split_once("acme/acme-1\n").unwrap().1;
        assert!(doc.contains("\"name\": \"Acme One\""), "{doc}");
        assert!(doc.ends_with("}\n"), "{doc}");
    }

    #[test]
    fn refresh_prints_message_to_stderr_and_fetches() {
        let dir = tempfile::tempdir().unwrap();
        let (catalog, fetcher) = fixture_catalog(dir.path());
        let (mut ui, captured) = Ui::capture(false);
        run_with(None, false, true, &catalog, &mut ui).unwrap();
        assert_eq!(
            captured.stderr(),
            format!(
                "{}Models cache refreshed{}\n",
                ui::style::TEXT_SUCCESS_BOLD,
                ui::style::TEXT_NORMAL
            )
        );
        assert_eq!(fetcher.calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(captured.stdout().split('/').next().unwrap(), "opencode");
    }

    static FIXTURE: &str = r#"{
      "anthropic": {
        "name": "Anthropic",
        "env": ["ANTHROPIC_API_KEY"],
        "id": "anthropic",
        "models": {
          "claude-3-7": {
            "id": "claude-3-7",
            "name": "Claude 3.7",
            "release_date": "2025-02-24",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          },
          "claude-4": {
            "id": "claude-4",
            "name": "Claude 4",
            "release_date": "2025-05-22",
            "attachment": true,
            "reasoning": false,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          }
        }
      },
      "opencode": {
        "name": "opencode",
        "env": ["OPENCODE_API_KEY"],
        "id": "opencode",
        "models": {
          "grok-code": {
            "id": "grok-code",
            "name": "Grok Code",
            "release_date": "2025-08-01",
            "attachment": false,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          }
        }
      },
      "openai": {
        "name": "OpenAI",
        "env": ["OPENAI_API_KEY"],
        "id": "openai",
        "models": {
          "gpt-5.1": {
            "id": "gpt-5.1",
            "name": "GPT-5.1",
            "release_date": "2025-08-07",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 400000, "output": 8192}
          },
          "o4-mini": {
            "id": "o4-mini",
            "name": "o4-mini",
            "release_date": "2025-04-16",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "limit": {"context": 200000, "output": 8192}
          }
        }
      },
      "acme": {
        "name": "Acme",
        "env": ["OCPROVIDERS_TEST_ENV_KEY"],
        "id": "acme",
        "models": {
          "acme-1": {
            "id": "acme-1",
            "name": "Acme One",
            "release_date": "2025-09-01",
            "attachment": false,
            "reasoning": false,
            "temperature": false,
            "tool_call": true,
            "limit": {"context": 100000, "output": 4096}
          }
        }
      }
    }"#;
}
