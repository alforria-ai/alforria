//! cli/cmd/web.ts port — the `web` command.

use std::ffi::OsString;
use std::process::{Command as ProcessCommand, Stdio};

use clap::ArgMatches;

use crate::cmd::serve;
use crate::error::TypedError;
use crate::network::{self, NetworkOptions, ResolvedNetworkOptions};
use crate::ui::style;
use crate::ui::Ui;

/// web.ts:53-55 — the `!  `-prefixed warning variant (UI.println → stderr).
pub const UNSECURED_WARNING: &str = "!  OPENCODE_SERVER_PASSWORD is not set; server is unsecured.";

pub fn warning_line() -> String {
    format!("{}{UNSECURED_WARNING}", style::TEXT_WARNING_BOLD)
}

/// web.ts:14-32 — internal and non-IPv4 interfaces are skipped, as are
/// Docker bridge networks (172.x).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInterface {
    pub internal: bool,
    pub ipv4: bool,
    pub address: String,
}

pub fn external_ipv4_addrs(interfaces: &[NetworkInterface]) -> Vec<String> {
    interfaces
        .iter()
        .filter(|iface| !iface.internal && iface.ipv4 && !iface.address.starts_with("172."))
        .map(|iface| iface.address.clone())
        .collect()
}

#[cfg(unix)]
pub fn network_ips() -> Vec<String> {
    let mut interfaces = Vec::new();
    unsafe {
        let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut addrs) != 0 {
            return Vec::new();
        }
        let mut cursor = addrs;
        while !cursor.is_null() {
            let entry = &*cursor;
            if !entry.ifa_addr.is_null() {
                let sockaddr = &*entry.ifa_addr;
                let ipv4 = u32::from(sockaddr.sa_family) == libc::AF_INET as u32;
                let internal = entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0;
                let address = if ipv4 {
                    let sin = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    let [a, b, c, d] = sin.sin_addr.s_addr.to_ne_bytes();
                    format!("{a}.{b}.{c}.{d}")
                } else {
                    String::new()
                };
                interfaces.push(NetworkInterface {
                    internal,
                    ipv4,
                    address,
                });
            }
            cursor = entry.ifa_next;
        }
        libc::freeifaddrs(addrs);
    }
    external_ipv4_addrs(&interfaces)
}

#[cfg(not(unix))]
pub fn network_ips() -> Vec<String> {
    Vec::new()
}

/// web.ts:47-76 — the local/`0.0.0.0` address listing. Returns the printed
/// lines plus the URL opened in the browser. `display_url` is the listener
/// URL — a WHATWG URL string, i.e. with a trailing `/`.
pub fn address_lines(
    opts: &ResolvedNetworkOptions,
    port: u16,
    display_url: &str,
    ips: &[String],
) -> (Vec<String>, String) {
    if opts.hostname != "0.0.0.0" {
        return (
            vec![info_line("  Web interface:    ", display_url)],
            display_url.to_string(),
        );
    }
    let localhost = format!("http://localhost:{port}");
    let mut lines = vec![info_line("  Local access:      ", &localhost)];
    for ip in ips {
        lines.push(info_line(
            "  Network access:    ",
            &format!("http://{ip}:{port}"),
        ));
    }
    if opts.mdns {
        lines.push(info_line(
            "  mDNS:              ",
            &format!("{}:{port}", opts.mdns_domain),
        ));
    }
    (lines, localhost)
}

/// One `UI.println(label, TEXT_NORMAL, rest)` line — elements are joined
/// with single spaces (ui.ts:33-36).
fn info_line(label: &str, rest: &str) -> String {
    format!(
        "{}{label} {} {}",
        style::TEXT_INFO_BOLD,
        style::TEXT_NORMAL,
        rest
    )
}

/// web.ts:76 — best-effort browser open; failures are ignored.
fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = ProcessCommand::new(program)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

pub fn run(matches: &ArgMatches, ui: &mut Ui, raw: &[OsString]) -> Result<(), TypedError> {
    if !serve::password_set() {
        ui.println(&warning_line());
    }
    let args = NetworkOptions::from_matches(matches);
    let config = network::global_server_config();
    let opts = network::resolve(&args, raw, &config);
    let runtime = super::runtime()?;
    let listener = runtime
        .block_on(alforria_server::listen(&opts.listen_options()))
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?;
    ui.empty();
    ui.println(&ui.logo(Some("  ")));
    ui.empty();
    let (lines, url) = address_lines(
        &opts,
        listener.port,
        &format!("{}/", listener.url),
        &network_ips(),
    );
    for line in lines {
        ui.println(&line);
    }
    open_browser(&url);
    // web.ts:78 `Effect.never` — run until interrupted.
    runtime.block_on(tokio::signal::ctrl_c()).ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_line_is_byte_exact() {
        assert_eq!(
            warning_line(),
            "\u{1b}[93m\u{1b}[1m!  OPENCODE_SERVER_PASSWORD is not set; server is unsecured."
        );
    }

    fn resolved(hostname: &str, mdns: bool, mdns_domain: &str) -> ResolvedNetworkOptions {
        ResolvedNetworkOptions {
            port: 0,
            hostname: hostname.to_string(),
            mdns,
            mdns_domain: mdns_domain.to_string(),
            cors: Vec::new(),
        }
    }

    #[test]
    fn address_lines_local_when_hostname_is_any() {
        let opts = resolved("0.0.0.0", false, "opencode.local");
        let (lines, url) = address_lines(
            &opts,
            4096,
            "http://0.0.0.0:4096/",
            &["192.168.1.5".to_string()],
        );
        assert_eq!(
            lines,
            vec![
                "\u{1b}[94m\u{1b}[1m  Local access:       \u{1b}[0m http://localhost:4096",
                "\u{1b}[94m\u{1b}[1m  Network access:     \u{1b}[0m http://192.168.1.5:4096",
            ]
        );
        assert_eq!(url, "http://localhost:4096");
    }

    #[test]
    fn address_lines_includes_mdns_line_when_enabled() {
        let opts = resolved("0.0.0.0", true, "dev.local");
        let (lines, _url) = address_lines(&opts, 4096, "http://0.0.0.0:4096/", &[]);
        assert_eq!(
            lines,
            vec![
                "\u{1b}[94m\u{1b}[1m  Local access:       \u{1b}[0m http://localhost:4096",
                "\u{1b}[94m\u{1b}[1m  mDNS:               \u{1b}[0m dev.local:4096",
            ]
        );
    }

    #[test]
    fn address_lines_shows_web_interface_otherwise() {
        let opts = resolved("127.0.0.1", false, "opencode.local");
        let (lines, url) = address_lines(&opts, 4096, "http://127.0.0.1:4096/", &[]);
        assert_eq!(
            lines,
            vec!["\u{1b}[94m\u{1b}[1m  Web interface:     \u{1b}[0m http://127.0.0.1:4096/"]
        );
        assert_eq!(url, "http://127.0.0.1:4096/");
    }

    #[test]
    fn external_ipv4_addrs_filters_internal_ipv6_and_docker_bridges() {
        let interfaces = vec![
            NetworkInterface {
                internal: true,
                ipv4: true,
                address: "127.0.0.1".to_string(),
            },
            NetworkInterface {
                internal: false,
                ipv4: false,
                address: "2001:db8::1".to_string(),
            },
            NetworkInterface {
                internal: false,
                ipv4: true,
                address: "172.17.0.1".to_string(),
            },
            NetworkInterface {
                internal: false,
                ipv4: true,
                address: "192.168.1.5".to_string(),
            },
        ];
        assert_eq!(
            external_ipv4_addrs(&interfaces),
            vec!["192.168.1.5".to_string()]
        );
    }
}
