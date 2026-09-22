//! `alforria libertai usage` — LibertAI account usage: plan tier, rolling
//! 5-hour allowance window, weekly limit, prepaid credits. Ported from the
//! `libertai usage` display logic.

use clap::ArgMatches;

use alforria_core::libertai::auth;

use crate::error::TypedError;
use crate::ui::{style, Ui};

pub fn run(matches: &ArgMatches, ui: &mut Ui) -> Result<(), TypedError> {
    let json = matches.get_flag("json");
    let subscription = auth::with_refreshed_session(auth::subscription).map_err(cli_error)?;
    if json {
        ui.println(&serde_json::to_string_pretty(&subscription).unwrap_or_default());
        return Ok(());
    }
    display(ui, &subscription);
    Ok(())
}

fn cli_error(message: impl Into<String>) -> TypedError {
    crate::error::TypedError::Cli(crate::error::CliError::new(message.into()))
}

fn display(ui: &mut Ui, subscription: &auth::Subscription) {
    ui.empty();
    intro(ui, &format!("LibertAI usage — {} plan", subscription.tier));

    usage_row(
        ui,
        "Current session",
        subscription.window_5h_used,
        subscription.window_5h_limit,
        &subscription.window_5h_resets_at,
        true,
    );
    usage_row(
        ui,
        "Weekly limit",
        subscription.weekly_used,
        subscription.weekly_limit,
        &subscription.weekly_resets_at,
        false,
    );

    if let Some(balance) = subscription.prepaid_balance {
        let plural = if balance == 1.0 { "" } else { "s" };
        ui.println(&format!(
            "  {}{}{} prepaid credit{plural}{}",
            style::TEXT_NORMAL_BOLD,
            money(balance),
            style::TEXT_NORMAL,
            style::TEXT_NORMAL
        ));
    }

    ui.println(style::TEXT_NORMAL);
}

fn usage_row(
    ui: &mut Ui,
    label: &str,
    used: Option<f64>,
    limit: Option<f64>,
    resets_at: &Option<String>,
    relative_reset: bool,
) {
    let (used, limit) = match (used, limit) {
        (Some(used), Some(limit)) if limit > 0.0 => (used, limit),
        _ => return,
    };
    let bar = bar(used, limit);
    let reset = resets_at
        .as_deref()
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|when| {
            if relative_reset {
                format!("Resets in {}", relative_time(when))
            } else {
                format!("Resets {}", absolute_time(when))
            }
        });
    let reset = match reset {
        Some(reset) => format!("{}{reset}", style::TEXT_DIM),
        None => String::new(),
    };
    ui.println(&format!(
        "  {} {}{}{reset}{}",
        label,
        bar,
        style::TEXT_NORMAL,
        style::TEXT_NORMAL
    ));
}

fn bar(used: f64, limit: f64) -> String {
    let fraction = (used / limit).clamp(0.0, 1.0);
    let cells = 16;
    let filled = (fraction * cells as f64).round() as usize;
    let empty = cells - filled.min(cells);
    let color = if fraction >= 0.9 {
        style::TEXT_DANGER
    } else if fraction >= 0.75 {
        style::TEXT_WARNING
    } else {
        style::TEXT_SUCCESS
    };
    format!(
        "{color}{}{}{reset}",
        "█".repeat(filled),
        "░".repeat(empty),
        reset = style::TEXT_NORMAL,
    )
}

fn relative_time(target: chrono::DateTime<chrono::FixedOffset>) -> String {
    let now = chrono::Local::now();
    let seconds = (target.with_timezone(&chrono::Local) - now).num_seconds();
    if seconds <= 60 {
        return "1m".to_string();
    }
    let minutes = seconds / 60;
    match minutes {
        0..=59 => format!("{minutes:2}m"),
        _ => format!("{}h {}m", minutes / 60, minutes % 60),
    }
}

fn absolute_time(target: chrono::DateTime<chrono::FixedOffset>) -> String {
    target
        .with_timezone(&chrono::Local)
        .format("%a %-I:%M %p")
        .to_string()
}

fn money(value: f64) -> String {
    format!("${value:.2}")
}

fn intro(ui: &mut Ui, message: &str) {
    ui.println(&format!(
        "{}{message}{}",
        style::TEXT_NORMAL_BOLD,
        style::TEXT_NORMAL
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_colors_by_usage_fraction() {
        assert!(bar(0.1, 1.0).contains(style::TEXT_SUCCESS));
        assert!(bar(0.8, 1.0).contains(style::TEXT_WARNING));
        assert!(bar(0.95, 1.0).contains(style::TEXT_DANGER));
        assert!(bar(1.0, 1.0).contains(style::TEXT_DANGER));
    }

    #[test]
    fn bar_fills_proportionally() {
        let sixteen_full = bar(1.0, 1.0);
        assert_eq!(sixteen_full.matches("█").count(), 16);
        assert_eq!(sixteen_full.matches("░").count(), 0);
        let half = bar(0.5, 1.0);
        assert_eq!(half.matches("█").count(), 8);
        assert_eq!(half.matches("░").count(), 8);
    }

    #[test]
    fn money_formats_two_decimals() {
        assert_eq!(money(0.5), "$0.50");
        assert_eq!(money(12.0), "$12.00");
    }
}
