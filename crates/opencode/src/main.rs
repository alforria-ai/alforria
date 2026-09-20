pub mod cmd;
pub mod error;
pub mod ui;

use std::ffi::OsString;
use std::process::ExitCode;

use error::TypedError;
use ui::Ui;

fn main() -> ExitCode {
    let mut ui = Ui::production();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    ExitCode::from(run(&mut ui, &args) as u8)
}

fn run(ui: &mut Ui, args: &[OsString]) -> i32 {
    let matches = match cmd::parse(cmd::cli(), args, ui) {
        Ok(matches) => matches,
        Err(code) => return code,
    };
    cmd::apply_middleware(&matches);
    match cmd::route(&matches) {
        Ok(()) => 0,
        Err(typed) => report(ui, &typed),
    }
}

/// index.ts:128-135 catch: format, render, and return the exit code.
fn report(ui: &mut Ui, typed: &TypedError) -> i32 {
    match error::format_error(typed) {
        Some(message) => {
            if !message.is_empty() {
                ui.error(&message);
            }
        }
        None => {
            ui.error("Unexpected error\n");
            ui.write_stderr(&format!("{}\n", error::format_unknown(typed.raw())));
        }
    }
    typed.exit_code()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    #[test]
    fn stub_command_exits_one_with_error_line() {
        let (mut ui, captured) = Ui::capture(false);
        let code = run(&mut ui, &args(&["serve"]));
        assert_eq!(code, 1);
        let stderr = captured.stderr();
        assert!(stderr.contains("serve is not implemented yet"), "{stderr}");
    }

    #[test]
    fn parse_error_exits_one() {
        let (mut ui, _captured) = Ui::capture(false);
        let code = run(&mut ui, &args(&["--bogus"]));
        assert_eq!(code, 1);
    }

    #[test]
    fn help_exits_zero() {
        let (mut ui, _captured) = Ui::capture(false);
        let code = run(&mut ui, &args(&["--help"]));
        assert_eq!(code, 0);
    }

    #[test]
    fn version_exits_zero() {
        let (mut ui, _captured) = Ui::capture(false);
        let code = run(&mut ui, &args(&["--version"]));
        assert_eq!(code, 0);
    }

    #[test]
    fn report_prints_formatted_message_and_exit_code() {
        let (mut ui, captured) = Ui::capture(false);
        let typed = TypedError::Cli(error::CliError::with_exit_code("boom", 42));
        let code = report(&mut ui, &typed);
        assert_eq!(code, 42);
        let stderr = captured.stderr();
        assert!(stderr.contains("Error: "), "{stderr}");
        assert!(stderr.contains("boom"), "{stderr}");
    }

    #[test]
    fn report_is_silent_for_cancelled() {
        let (mut ui, captured) = Ui::capture(false);
        let code = report(&mut ui, &TypedError::UiCancelled);
        assert_eq!(code, 1);
        assert_eq!(captured.stderr(), "");
    }

    #[test]
    fn report_unexpected_error_prints_raw_message() {
        let (mut ui, captured) = Ui::capture(false);
        let typed = TypedError::Unknown {
            raw: "connection reset".to_string(),
        };
        let code = report(&mut ui, &typed);
        assert_eq!(code, 1);
        let stderr = captured.stderr();
        assert!(stderr.contains("Unexpected error"), "{stderr}");
        assert!(stderr.contains("connection reset"), "{stderr}");
    }
}
