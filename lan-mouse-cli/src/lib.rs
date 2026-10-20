use clap::{Args, Parser, Subcommand};
use futures::StreamExt;

pub mod i18n;

use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use thiserror::Error;

use lan_mouse_ipc::{
    ClientHandle, ConnectionError, FrontendEvent, FrontendRequest, IpcError, Position,
    connect_async,
};

#[derive(Debug, Error)]
pub enum CliError {
    /// is the service running?
    #[error(
        "{}",
        crate::i18n::gettext("could not connect: `{0}` - is the service running?")
            .replace("{0}", .0.to_string().as_str())
    )]
    ServiceNotRunning(#[from] ConnectionError),
    #[error(
        "{}",
        crate::i18n::gettext("error communicating with service: {0}")
            .replace("{0}", .0.to_string().as_str())
    )]
    Ipc(#[from] IpcError),
}

#[derive(Parser, Clone, Debug, PartialEq, Eq)]
#[command(name = "lan-mouse-cli", about = "LanMouse CLI interface")]
pub struct CliArgs {
    #[command(subcommand)]
    pub command: CliSubcommand,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct Client {
    #[arg(long)]
    hostname: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    ips: Option<Vec<IpAddr>>,
    #[arg(long)]
    enter_hook: Option<String>,
    #[arg(long)]
    leave_hook: Option<String>,
}

#[derive(Clone, Subcommand, Debug, PartialEq, Eq)]
pub enum CliSubcommand {
    /// add a new client
    AddClient(Client),
    /// remove an existing client
    RemoveClient { id: ClientHandle },
    /// activate a client
    Activate { id: ClientHandle },
    /// deactivate a client
    Deactivate { id: ClientHandle },
    /// list configured clients
    List,
    /// change hostname
    SetHost {
        id: ClientHandle,
        host: Option<String>,
    },
    /// change port
    SetPort { id: ClientHandle, port: u16 },
    /// set position
    SetPosition { id: ClientHandle, pos: Position },
    /// set ips
    SetIps { id: ClientHandle, ips: Vec<IpAddr> },
    /// re-enable capture
    EnableCapture,
    /// re-enable emulation
    EnableEmulation,
    /// authorize a public key
    AuthorizeKey {
        description: String,
        sha256_fingerprint: String,
    },
    /// deauthorize a public key
    RemoveAuthorizedKey { sha256_fingerprint: String },
    /// save configuration to file
    SaveConfig,
    /// pair with another device on the local network
    Pair {
        /// address of the device (as listed by `devices`)
        #[arg(long)]
        addr: Option<std::net::SocketAddr>,
        /// hostname of the device (discovered automatically)
        #[arg(long)]
        host: Option<String>,
        /// pairing code shown on the other device
        #[arg(long)]
        code: Option<String>,
    },
    /// list devices discovered on the local network
    Devices,
    /// show or change this device's pairing code
    Code {
        /// new code (random when omitted)
        #[arg(long)]
        set: Option<String>,
    },
}

pub async fn run(args: CliArgs) -> Result<(), CliError> {
    execute(args.command).await?;
    Ok(())
}

/// parse the command line with translated help and error texts. Called
/// before anything else; the arguments themselves are unchanged.
pub fn localized_command() -> clap::Command {
    CLAP.with(|c| c.borrow().as_ref().expect("invalidated").clone())
}

#[doc(hidden)]
pub fn localized_command_debug() -> clap::Command {
    CLAP.with(|c| c.borrow().as_ref().expect("invalidated").clone())
}

thread_local! {
    /// the localized command. Building it overwrites name and arguments of all
    /// subcommands, so it is only built once and reused (cloning is cheap).
    static CLAP: std::cell::RefCell<Option<clap::Command>> =
        const { std::cell::RefCell::new(None) };
}

/// the options shared by all entry points (`--config`, `--port`, ...)
#[derive(clap::Parser, Clone, Debug, Default)]
#[command(
    name = "daemon",
    about = "run the lan-mouse service in the foreground",
    long_about = "Runs the lan-mouse service without the GTK frontend, meant for headless machines and scripts."
)]
pub struct MainArgs {
    /// listen port
    #[arg(long, short)]
    pub port: Option<u16>,
    /// non-default config file location
    #[arg(long, short)]
    pub config: Option<std::path::PathBuf>,
}

/// convert the parsed matches of the main command into `MainArgs`
pub fn main_args(matches: &clap::ArgMatches) -> MainArgs {
    MainArgs {
        port: matches.get_one::<u16>("port").copied(),
        config: matches.get_one::<std::path::PathBuf>("config").cloned(),
    }
}

/// parse the command line of `lan-mouse cli`
pub fn parse() -> CliArgs {
    let matches = localized_command().get_matches_from(std::env::args_os());
    <CliArgs as clap::FromArgMatches>::from_arg_matches(&matches).unwrap_or_else(|e| e.exit())
}

/// build the localized command once
pub fn init_command() {
    use clap::CommandFactory;
    let cmd = translate(CliArgs::command());
    CLAP.with(|c| c.borrow_mut().replace(cmd));
}

/// the command with translated texts
pub fn translated_command(cmd: clap::Command) -> clap::Command {
    translate(cmd)
}

/// shows the help of `cmd` translated. `cmd` needs a binary name of its own,
/// clap refuses to print the help of the main command otherwise; its texts
/// are the same and already translated by the caller.
pub fn print_localized_help(cmd: clap::Command, bin_name: &str) -> ! {
    let mut cmd = if cmd.get_bin_name().is_none() {
        let name: &'static str = Box::leak(bin_name.to_owned().into_boxed_str());
        cmd.bin_name(name)
    } else {
        cmd
    };
    cmd.print_help().ok();
    std::process::exit(0);
}

/// translate the texts of clap's fixed labels (Usage:, Commands:, Options:),
/// of the help and about texts of `cmd` and of all its subcommands. Names,
/// ids and structure are left untouched.
fn translate(cmd: clap::Command) -> clap::Command {
    let t = |text: &str| crate::i18n::gettext(text);
    // a subcommand shows its description above its help, the top level command
    // prints it through clap on its own instead
    let has_subcommands = cmd.has_subcommands();
    let bin_name = cmd.get_bin_name().map(|b| b.to_string());
    let mut cmd = cmd;
    if !has_subcommands {
        if let Some(description) = cmd.get_long_about().map(|a| a.to_string()) {
            cmd = cmd.long_about(t(&description));
        }
        if let Some(about) = cmd.get_about().map(|a| a.to_string()) {
            cmd = cmd.about(t(&about));
        }
    }
    if bin_name.is_none() {
        cmd = cmd.bin_name("lan-mouse");
    }
    cmd = cmd.help_template(t("Usage: {usage}\n\n{all-args}"));
    let heading: &'static str = Box::leak(
        t(if has_subcommands {
            "Commands"
        } else {
            "Options"
        })
        .into_boxed_str(),
    );
    cmd = cmd
        .next_help_heading(heading)
        .subcommand_help_heading(heading);
    cmd = cmd.mut_args(|arg| match arg.get_help().map(|h| h.to_string()) {
        Some(help) => arg.help(t(&help)),
        None => arg,
    });
    // subcommands keep their names, only their texts are replaced
    let names: Vec<String> = cmd
        .get_subcommands()
        .map(|s| s.get_name().to_owned())
        .collect();
    for name in names {
        let sub = cmd.find_subcommand(&name).cloned().expect("subcommand");
        let sub = translate(sub);
        if let Some(slot) = cmd.find_subcommand_mut(&name) {
            *slot = sub;
        }
    }
    cmd
}

/// ask the service to pair with another device and report the outcome
async fn execute_pair(
    rx: &mut lan_mouse_ipc::AsyncFrontendEventReader,
    tx: &mut lan_mouse_ipc::AsyncFrontendRequestWriter,
    addr: Option<SocketAddr>,
    host: Option<String>,
    code: Option<String>,
) -> Result<(), CliError> {
    // without an explicit address, use the discovered device with this host name
    let addr = match addr {
        Some(addr) => addr,
        None => {
            let host = host.or_else(|| std::env::args().nth(2)).unwrap_or_default();
            tx.request(FrontendRequest::Sync).await?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            let mut found = None;
            while let Ok(Some(Ok(event))) = tokio::time::timeout_at(deadline, rx.next()).await {
                if let FrontendEvent::Discovered { addr, hostname, .. } = event {
                    if found.is_none() && (host.is_empty() || hostname.eq_ignore_ascii_case(&host))
                    {
                        found = Some(addr);
                    }
                }
            }
            found.ok_or_else(|| {
                CliError::Ipc(lan_mouse_ipc::IpcError::Io(std::io::Error::other(
                    crate::i18n::gettext(
                        "device not found, use --addr or check `lan-mouse cli devices`",
                    ),
                )))
            })?
        }
    };
    tx.request(FrontendRequest::Pair { socket: addr, code })
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while let Ok(Some(Ok(event))) = tokio::time::timeout_at(deadline, rx.next()).await {
        match event {
            FrontendEvent::PairingFinished { hostname, error } => match error {
                None => println!(
                    "{}",
                    crate::i18n::gettext("paired with {hostname}, the device is ready to use")
                        .replace("{hostname}", &hostname)
                ),
                Some(e) => eprintln!(
                    "{}",
                    crate::i18n::gettext("pairing with {hostname} failed: {error}")
                        .replace("{hostname}", &hostname)
                        .replace("{error}", &e)
                ),
            },
            FrontendEvent::Error(msg) => eprintln!("{msg}"),
            _ => {}
        }
    }
    eprintln!("{}", crate::i18n::gettext("pairing did not answer in time"));
    Ok(())
}

async fn execute(cmd: CliSubcommand) -> Result<(), CliError> {
    let timeout = std::env::var("LAN_MOUSE_CLI_TIMEOUT")
        .ok()
        .and_then(|t| t.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_millis(500));
    let (mut rx, mut tx) = connect_async(Some(timeout)).await?;
    match cmd {
        CliSubcommand::AddClient(Client {
            hostname,
            port,
            ips,
            enter_hook,
            leave_hook,
        }) => {
            tx.request(FrontendRequest::Create).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Created(handle, _, _) = e? {
                    if let Some(hostname) = hostname {
                        tx.request(FrontendRequest::UpdateHostname(handle, Some(hostname)))
                            .await?;
                    }
                    if let Some(port) = port {
                        tx.request(FrontendRequest::UpdatePort(handle, port))
                            .await?;
                    }
                    if let Some(ips) = ips {
                        tx.request(FrontendRequest::UpdateFixIps(handle, ips))
                            .await?;
                    }
                    if let Some(enter_hook) = enter_hook {
                        tx.request(FrontendRequest::UpdateEnterHook(handle, Some(enter_hook)))
                            .await?;
                    }
                    if let Some(leave_hook) = leave_hook {
                        tx.request(FrontendRequest::UpdateLeaveHook(handle, Some(leave_hook)))
                            .await?;
                    }
                    break;
                }
            }
        }
        CliSubcommand::RemoveClient { id } => tx.request(FrontendRequest::Delete(id)).await?,
        CliSubcommand::Activate { id } => tx.request(FrontendRequest::Activate(id, true)).await?,
        CliSubcommand::Deactivate { id } => {
            tx.request(FrontendRequest::Activate(id, false)).await?
        }
        CliSubcommand::List => {
            tx.request(FrontendRequest::Enumerate()).await?;
            while let Some(e) = rx.next().await {
                if let FrontendEvent::Enumerate(clients) = e? {
                    for (handle, config, state) in clients {
                        let host = config
                            .hostname
                            .unwrap_or_else(|| crate::i18n::gettext("unknown"));
                        let port = config.port;
                        let pos = config.pos;
                        let active = state.active;
                        let ips = state.ips;
                        println!(
                            "{}",
                            crate::i18n::gettext(
                                "id {id}: {host}:{port} ({pos}) active: {active}, ips: {ips}"
                            )
                            .replace("{id}", &handle.to_string())
                            .replace("{host}", &host)
                            .replace("{port}", &port.to_string())
                            .replace("{pos}", &pos.localized())
                            .replace(
                                "{active}",
                                &crate::i18n::gettext(if active { "yes" } else { "no" })
                            )
                            .replace("{ips}", &format!("{ips:?}"))
                        );
                    }
                    break;
                }
            }
        }
        CliSubcommand::SetHost { id, host } => {
            tx.request(FrontendRequest::UpdateHostname(id, host))
                .await?
        }
        CliSubcommand::SetPort { id, port } => {
            tx.request(FrontendRequest::UpdatePort(id, port)).await?
        }
        CliSubcommand::SetPosition { id, pos } => {
            tx.request(FrontendRequest::UpdatePosition(id, pos)).await?
        }
        CliSubcommand::SetIps { id, ips } => {
            tx.request(FrontendRequest::UpdateFixIps(id, ips)).await?
        }
        CliSubcommand::EnableCapture => tx.request(FrontendRequest::EnableCapture).await?,
        CliSubcommand::EnableEmulation => tx.request(FrontendRequest::EnableEmulation).await?,
        CliSubcommand::AuthorizeKey {
            description,
            sha256_fingerprint,
        } => {
            tx.request(FrontendRequest::AuthorizeKey(
                description,
                sha256_fingerprint,
            ))
            .await?
        }
        CliSubcommand::RemoveAuthorizedKey { sha256_fingerprint } => {
            tx.request(FrontendRequest::RemoveAuthorizedKey(sha256_fingerprint))
                .await?
        }
        CliSubcommand::SaveConfig => tx.request(FrontendRequest::SaveConfiguration).await?,
        CliSubcommand::Pair { addr, host, code } => {
            execute_pair(&mut rx, &mut tx, addr, host, code).await?
        }
        CliSubcommand::Devices => {
            tx.request(FrontendRequest::Sync).await?;
            let mut listed = std::collections::HashSet::new();
            // wait for the device list, the service answers right away
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while let Ok(Some(Ok(event))) = tokio::time::timeout_at(deadline, rx.next()).await {
                match event {
                    FrontendEvent::Discovered {
                        addr,
                        hostname,
                        fingerprint,
                    } => {
                        if listed.insert(addr) {
                            println!(
                                "{hostname} ({addr}) {}",
                                crate::i18n::gettext("fingerprint: {fp}")
                                    .replace("{fp}", &fingerprint)
                            );
                        }
                    }
                    FrontendEvent::DiscoveredGone { addr } => {
                        listed.remove(&addr);
                    }
                    FrontendEvent::NoSuchClient(_)
                    | FrontendEvent::Created(..)
                    | FrontendEvent::State(..)
                    | FrontendEvent::Deleted(_)
                    | FrontendEvent::Enumerate(_)
                    | FrontendEvent::PortChanged(..)
                    | FrontendEvent::Error(_)
                    | FrontendEvent::CaptureStatus(_)
                    | FrontendEvent::EmulationStatus(_)
                    | FrontendEvent::StatusReason { .. }
                    | FrontendEvent::AuthorizedUpdated(_)
                    | FrontendEvent::PublicKeyFingerprint(_)
                    | FrontendEvent::DeviceConnected { .. }
                    | FrontendEvent::DeviceEntered { .. }
                    | FrontendEvent::IncomingDisconnected(_)
                    | FrontendEvent::ConnectionAttempt { .. }
                    | FrontendEvent::PairingCode(_)
                    | FrontendEvent::PairingFinished { .. } => {}
                }
            }
            if listed.is_empty() {
                println!(
                    "{}",
                    crate::i18n::gettext("no devices found on the local network")
                );
            }
        }
        CliSubcommand::Code { set } => {
            match set {
                Some(code) => {
                    tx.request(FrontendRequest::SetPairingCode(code)).await?;
                }
                None => tx.request(FrontendRequest::Sync).await?,
            }
            let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
            while let Ok(Some(Ok(event))) = tokio::time::timeout_at(deadline, rx.next()).await {
                if let FrontendEvent::PairingCode(code) = event {
                    println!(
                        "{}",
                        crate::i18n::gettext("pairing code: {code}").replace("{code}", &code)
                    );
                    break;
                }
            }
        }
    }
    Ok(())
}
