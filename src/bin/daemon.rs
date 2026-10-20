//! `lan-mouse daemon` without the GTK frontend: runs the service in the
//! foreground, meant for headless machines and scripts.

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    lan_mouse::logging::init();
    if let Some(i) = std::env::args().position(|a| a == "--language") {
        if let Some(lang) = std::env::args().nth(i + 1) {
            std::env::set_var("LANGUAGE", lang);
        }
    }
    lan_mouse::i18n::init();
    lan_mouse_cli::init_command();

    let args = <lan_mouse_cli::MainArgs as Parser>::parse();
    let config = match lan_mouse::config::Config::from_main_args(&args) {
        Ok(config) => config,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let local = tokio::task::LocalSet::new();
    match runtime.block_on(local.run_until(lan_mouse::service::run_daemon(config))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
