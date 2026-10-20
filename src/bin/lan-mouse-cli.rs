//! `lan-mouse-cli ...`: talks to the running service through its socket, it
//! does not start anything itself. A platform independent entry point, the
//! `cli` subcommand of the main binary does the same.

use std::process::ExitCode;

fn main() -> ExitCode {
    lan_mouse::logging::init();
    // a just started GUI reports "not connected" while its service comes up,
    // the connection wait defaults to a few seconds then
    if std::env::var_os("LAN_MOUSE_CLI_TIMEOUT").is_none() {
        std::env::set_var("LAN_MOUSE_CLI_TIMEOUT", "5000");
    }
    if let Some(i) = std::env::args().position(|a| a == "--language") {
        if let Some(lang) = std::env::args().nth(i + 1) {
            std::env::set_var("LANGUAGE", lang);
        }
    }
    lan_mouse::i18n::init();
    lan_mouse_cli::init_command();
    let args = lan_mouse_cli::parse();
    run(args)
}

/// run the command on a local runtime, used by the main binary's `cli`
pub fn run(args: lan_mouse_cli::CliArgs) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    match runtime.block_on(local.run_until(lan_mouse_cli::run(args))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
