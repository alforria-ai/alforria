use std::ffi::OsString;
use std::process::ExitCode;

fn main() -> ExitCode {
    alforria::cmd::debug::mark_startup();
    let mut ui = alforria::ui::Ui::production();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    ExitCode::from(alforria::run(&mut ui, &args) as u8)
}
