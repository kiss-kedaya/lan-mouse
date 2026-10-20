mod authorization_window;
mod client_object;
mod client_row;
mod fingerprint_window;
pub mod i18n;
mod key_object;
mod key_row;
#[cfg(target_os = "macos")]
mod macos_privacy;
#[cfg(target_os = "macos")]
mod macos_status_item;
mod pairing_window;
pub mod ui_i18n;
mod window;

use std::path::Path;
use std::{env, process, str, sync::OnceLock};

use gtk::CssProvider;
use window::Window;

/// Local build's commit hash, set once by [`run`] before the GTK
/// main loop starts. Read by per-row UI to compare against each
/// peer's [`lan_mouse_ipc::ClientState::peer_commit`] for the
/// soft-warn version-mismatch indicator.
pub(crate) static LOCAL_COMMIT: OnceLock<[u8; 8]> = OnceLock::new();

/// Convenience: returns the local commit as an 8-char ASCII string,
/// or a placeholder if unset (which would indicate a programmer
/// error since [`run`] always sets it).
pub(crate) fn local_commit_str() -> String {
    LOCAL_COMMIT
        .get()
        .and_then(|c| std::str::from_utf8(c).ok())
        .unwrap_or("????????")
        .to_string()
}

use lan_mouse_ipc::FrontendEvent;

use adw::Application;
use gtk::{IconTheme, gdk::Display, glib::clone, prelude::*};
use gtk::{gio, glib, prelude::ApplicationExt};

use self::client_object::ClientObject;
use self::key_object::KeyObject;

use thiserror::Error;

#[derive(Error, Debug)]
pub enum GtkError {
    #[error("gtk frontend exited with non zero exit code: {0}")]
    NonZeroExitCode(i32),
}

pub fn run(local_commit: [u8; 8]) -> Result<(), GtkError> {
    log::debug!("running gtk frontend");
    LOCAL_COMMIT
        .set(local_commit)
        .expect("local_commit set once");

    #[cfg(windows)]
    let ret = std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024) // https://gitlab.gnome.org/GNOME/gtk/-/commit/52dbb3f372b2c3ea339e879689c1de535ba2c2c3 -> caused crash on windows
        .name("gtk".into())
        .spawn(gtk_entry)
        .unwrap()
        .join()
        .unwrap();
    #[cfg(not(windows))]
    let ret = gtk_entry();

    match ret {
        glib::ExitCode::SUCCESS => Ok(()),
        e => Err(GtkError::NonZeroExitCode(e.value())),
    }
}

/// the language of the user interface, as found by `lan_mouse::i18n`. The
/// window templates are translated at compile time, one copy per language.
static LANGUAGE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// selects the language of the window templates, called before `run`
pub fn set_language(language: Option<String>) {
    let _ = LANGUAGE.set(language);
}

/// the binary that starts the GUI installs its translation function here
/// before `run` is called, so the GUI texts follow the selected language
#[cfg(feature = "i18n")]
pub fn set_translator(translator: fn(&str) -> String) {
    crate::i18n::set_translator(translator);
}

/// the resources have to be registered before the templates are opened
fn gtk_entry() -> glib::ExitCode {
    gio::resources_register_include!("lan-mouse.gresource").expect("Failed to register resources.");
    crate::ui_i18n::register(LANGUAGE.get().and_then(|l| l.as_deref()));
    gtk_main()
}

fn gtk_main() -> glib::ExitCode {
    configure_portable_environment();
    #[cfg(windows)]
    install_theme_log_filter();
    #[cfg(target_os = "macos")]
    {
        configure_macos_bundle_environment();
        install_macos_gtk_log_filter();
    }

    let app = Application::builder()
        .application_id("de.feschber.LanMouse")
        .build();

    app.connect_startup(|app| {
        load_css();
        load_icons();
        setup_actions(app);
        setup_menu(app);
    });
    app.connect_activate(build_ui);

    let args: Vec<&'static str> = vec![];
    app.run_with_args(&args)
}

/// the checkboxes and switches of the bundled theme make GTK print a theme
/// parser warning for every single one of them; they are expected on Windows
#[cfg(windows)]
fn install_theme_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && fields.iter().any(is_theme_parser_warning) {
            return glib::LogWriterOutput::Handled;
        }
        glib::log_writer_default(level, fields)
    });
}

#[cfg(windows)]
fn is_theme_parser_warning(field: &glib::LogField<'_>) -> bool {
    field.key() == "MESSAGE"
        && field
            .value_str()
            .is_some_and(|message| message.starts_with("Theme parser warning:"))
}

#[cfg(target_os = "macos")]
fn install_macos_gtk_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        if level == glib::LogLevel::Warning && is_gtk_theme_parser_warning(fields) {
            return glib::LogWriterOutput::Handled;
        }

        glib::log_writer_default(level, fields)
    });
}

#[cfg(target_os = "macos")]
fn is_gtk_theme_parser_warning(fields: &[glib::LogField<'_>]) -> bool {
    let mut domain = None;
    let mut message = None;

    for field in fields {
        match field.key() {
            "GLIB_DOMAIN" => domain = field.value_str(),
            "MESSAGE" => message = field.value_str(),
            _ => {}
        }
    }

    domain == Some("Gtk")
        && message.is_some_and(|message| message.starts_with("Theme parser warning: gtk.css:"))
}

#[cfg(target_os = "macos")]
fn configure_macos_bundle_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(contents) = exe
        .parent()
        .and_then(|dir| dir.parent())
        .map(std::path::Path::to_owned)
    else {
        return;
    };

    let share = contents.join("Resources").join("share");
    if !share.exists() {
        return;
    }

    let schemas = share.join("glib-2.0").join("schemas");
    if schemas.exists() {
        env::set_var("GSETTINGS_SCHEMA_DIR", schemas);
    }

    env::set_var("XDG_DATA_DIRS", &share);
    env::set_var(
        "GTK_DATA_PREFIX",
        contents.join("Resources").to_string_lossy().as_ref(),
    );
}

/// Use the resources that ship next to the executable (portable build): GTK
/// caches the icon, pixbuf loader and settings paths at startup, so they are
/// set before the application is built. Missing directories are ignored and
/// the system defaults apply.
fn configure_portable_environment() {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let Some(dir) = exe.parent() else {
        return;
    };

    // the package has everything next to the exe: share/, etc/, lib/ and the
    // GTK DLLs, so the exe is started by a double click like any other program
    set_if_unset("XDG_DATA_DIRS", &dir.join("share"));
    set_if_unset("GTK_DATA_PREFIX", dir);

    // image loaders, e.g. the SVG loader needed for symbolic icons
    let pixbuf = dir.join("lib").join("gdk-pixbuf-2.0").join("2.10.0");
    if pixbuf.join("loaders.cache").is_file() {
        set_if_unset("GDK_PIXBUF_MODULE_FILE", &pixbuf.join("loaders.cache"));
    }

    let schemas = dir.join("share").join("glib-2.0").join("schemas");
    if schemas.is_dir() {
        set_if_unset("GSETTINGS_SCHEMA_DIR", &schemas);
    }

    let fonts = dir.join("etc").join("fonts");
    if fonts.is_dir() {
        set_if_unset("FONTCONFIG_PATH", &fonts);
    }

    // old packages kept the DLLs in bin, that directory is searched first
    #[cfg(windows)]
    {
        let bin = dir.join("bin");
        if bin.is_dir() {
            if let Some(path) = env::var_os("PATH") {
                let mut paths = vec![bin];
                paths.extend(env::split_paths(&path));
                if let Ok(joined) = env::join_paths(paths) {
                    env::set_var("PATH", joined);
                }
            }
        }
    }
}

/// sets `name` unless the user configured it already
fn set_if_unset(name: &str, value: &Path) {
    if env::var_os(name).is_none() {
        env::set_var(name, value);
    }
}

fn load_css() {
    let provider = CssProvider::default();
    provider.load_from_resource("de/feschber/LanMouse/style.css");
    gtk::style_context_add_provider_for_display(
        &Display::default().expect("Could not connect to a display"),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn load_icons() {
    let display = &Display::default().expect("Could not connect to a display.");
    let icon_theme = IconTheme::for_display(display);
    icon_theme.add_resource_path("/de/feschber/LanMouse/icons");
}

// Add application actions
fn setup_actions(app: &adw::Application) {
    // Quit action
    // This is important on macOS, where users expect a File->Quit action with a Cmd+Q shortcut.
    let quit_action = gio::SimpleAction::new("quit", None);
    quit_action.connect_activate({
        let app = app.clone();
        move |_, _| {
            app.quit();
        }
    });
    app.add_action(&quit_action);
}

// Set up a global menu
//
// Currently this is used only on macOS
fn setup_menu(app: &adw::Application) {
    let menu = gio::Menu::new();

    let file_menu = gio::Menu::new();
    file_menu.append(Some(&crate::i18n::gettext("Quit")), Some("app.quit"));
    menu.append_submenu(Some(&crate::i18n::gettext("_File")), &file_menu);

    app.set_menubar(Some(&menu))
}

fn build_ui(app: &Application) {
    // If a window already exists (re-activation), just present it
    if let Some(window) = app.windows().first() {
        window.present();
        return;
    }

    log::debug!("connecting to lan-mouse-socket");
    let (mut frontend_rx, frontend_tx) = match lan_mouse_ipc::try_connect() {
        Ok(conn) => conn,
        Err(e) => {
            log::warn!("could not connect to daemon ({e}), spawning a new one");
            if let Err(spawn_err) = process::Command::new(
                env::current_exe().expect("could not determine executable path"),
            )
            .args(env::args().skip(1))
            .arg("daemon")
            .spawn()
            {
                log::error!("failed to spawn daemon: {spawn_err}");
                process::exit(1);
            }
            match lan_mouse_ipc::connect() {
                Ok(conn) => conn,
                Err(e) => {
                    log::error!("{e}");
                    process::exit(1);
                }
            }
        }
    };
    log::debug!("connected to lan-mouse-socket");

    let (sender, receiver) = async_channel::bounded(10);

    gio::spawn_blocking(move || {
        while let Some(e) = frontend_rx.next_event() {
            match e {
                Ok(e) => sender.send_blocking(e).unwrap(),
                Err(e) => {
                    log::error!("{e}");
                    break;
                }
            }
        }
    });

    let window = Window::new(app, frontend_tx);
    #[cfg(target_os = "macos")]
    {
        window.connect_close_request(|window| {
            window.set_visible(false);
            glib::Propagation::Stop
        });
        macos_status_item::setup(app, &window);
        // First-launch TCC prompts. No-op when already granted.
        macos_privacy::fire_initial_prompts();
        // Watch the Accessibility grant continuously for the lifetime
        // of the process. On a grant, swap the warning row into its
        // "relaunch required" state (the daemon subprocess already
        // bailed and can't recover without a restart). On a REVOKE,
        // quit immediately — an active CGEventTap at
        // HeadInsertEventTap can wedge system input if the process
        // lingers after losing AX, and forcing the process to exit is
        // the only bulletproof way to guarantee the kernel tears the
        // tap down.
        let window_weak = window.downgrade();
        let app_weak = app.downgrade();
        macos_privacy::watch_accessibility_state(move |change| match change {
            macos_privacy::AccessibilityChange::Granted => {
                if let Some(window) = window_weak.upgrade() {
                    window.present();
                    window.refresh_capture_emulation_status();
                }
            }
            macos_privacy::AccessibilityChange::Revoked => {
                log::warn!("Accessibility revoked — quitting to avoid wedging system input");
                if let Some(app) = app_weak.upgrade() {
                    app.quit();
                }
            }
        });
    }

    glib::spawn_future_local(clone!(
        #[weak]
        window,
        async move {
            loop {
                let notify = receiver.recv().await.unwrap_or_else(|_| process::exit(1));
                match notify {
                    FrontendEvent::Created(handle, client, state) => {
                        window.new_client(handle, client, state)
                    }
                    FrontendEvent::Deleted(client) => window.delete_client(client),
                    FrontendEvent::State(handle, config, state) => {
                        window.update_client_config(handle, config);
                        window.update_client_state(handle, state);
                    }
                    FrontendEvent::NoSuchClient(_) => {}
                    FrontendEvent::Error(e) => window.show_toast(e.as_str()),
                    FrontendEvent::Enumerate(clients) => window.update_client_list(clients),
                    FrontendEvent::PortChanged(port, msg) => window.update_port(port, msg),
                    FrontendEvent::CaptureStatus(s) => window.set_capture(s.into()),
                    FrontendEvent::EmulationStatus(s) => window.set_emulation(s.into()),
                    FrontendEvent::AuthorizedUpdated(keys) => window.set_authorized_keys(keys),
                    FrontendEvent::PublicKeyFingerprint(fp) => window.set_pk_fp(&fp),
                    FrontendEvent::ConnectionAttempt { fingerprint } => {
                        window.request_authorization(&fingerprint);
                    }
                    FrontendEvent::DeviceConnected {
                        fingerprint: _,
                        addr,
                    } => {
                        let msg = crate::i18n::gettext("device connected: {addr}")
                            .replace("{addr}", &addr.to_string());
                        window.show_toast(&msg);
                    }
                    FrontendEvent::DeviceEntered {
                        fingerprint: _,
                        addr,
                        pos,
                    } => {
                        let msg = crate::i18n::gettext("device entered: {addr} ({pos})")
                            .replace("{addr}", &addr.to_string())
                            .replace("{pos}", &pos.localized());
                        window.show_toast(&msg);
                    }
                    FrontendEvent::IncomingDisconnected(addr) => {
                        let msg = crate::i18n::gettext("{addr} disconnected")
                            .replace("{addr}", &addr.to_string());
                        window.show_toast(&msg);
                    }
                    FrontendEvent::PairingCode(code) => window.set_pairing_code(&code),
                    FrontendEvent::PairingFinished { hostname, error } => {
                        window.notify_pairing_finished(&hostname, error.as_deref())
                    }
                    FrontendEvent::Discovered {
                        addr,
                        hostname,
                        fingerprint,
                    } => {
                        let mut devices = window.discovered();
                        let addr = addr.to_string();
                        if let Some(device) = devices.iter_mut().find(|(a, ..)| *a == addr) {
                            device.1 = hostname;
                            device.2 = fingerprint;
                        } else {
                            devices.push((addr, hostname, fingerprint));
                        }
                        devices.sort_by(|a, b| a.1.cmp(&b.1));
                        window.set_discovered(devices);
                        window.refresh_discovered();
                    }
                    FrontendEvent::DiscoveredGone { addr } => {
                        let addr = addr.to_string();
                        let mut devices = window.discovered();
                        devices.retain(|(a, ..)| *a != addr);
                        window.set_discovered(devices);
                        window.refresh_discovered();
                    }
                }
            }
        }
    ));

    #[cfg(not(target_os = "macos"))]
    window.present();

    // On macOS, default to presenting the main window on every launch
    // so the user gets a visible confirmation that the app is running
    // — including the post-grant relaunch and normal Dock/Finder/`open`
    // launches. Opt out by setting `LAN_MOUSE_HIDDEN=1` in the
    // environment (useful for a LaunchAgent / login-item configuration
    // where the user wants the app to come up quietly into the menu
    // bar only, with no window on boot).
    #[cfg(target_os = "macos")]
    if env::var_os("LAN_MOUSE_HIDDEN").is_none() {
        window.present();
    }
}

#[cfg(test)]
mod ui_translation {
    /// every text of the templates is translated, none stays English
    #[test]
    #[cfg(feature = "i18n")]
    fn templates_are_translated() {
        crate::ui_i18n::register(Some("zh_CN"));
        for name in [
            "window",
            "pairing_window",
            "client_row",
            "authorization_window",
            "fingerprint_window",
            "key_row",
        ] {
            let path = format!("/de/feschber/LanMouse/{name}.ui");
            let data = gtk::gio::resources_lookup_data(&path, gtk::gio::ResourceLookupFlags::NONE)
                .expect("template registered");
            let text = String::from_utf8_lossy(&data);
            // commented out properties are no texts
            let uncommented = {
                let mut out = String::new();
                let mut rest = text.as_ref();
                while let Some(start) = rest.find("<!--") {
                    out.push_str(&rest[..start]);
                    match rest[start..].find("-->") {
                        Some(end) => rest = &rest[start + end + 3..],
                        None => {
                            rest = "";
                            break;
                        }
                    }
                }
                out.push_str(rest);
                out
            };
            let texts: Vec<&str> = uncommented
                .split("translatable=\"yes\">")
                .skip(1)
                .map(|rest| rest.split('<').next().unwrap_or(""))
                .filter(|text| !text.trim().is_empty())
                .collect();
            let english: Vec<&&str> = texts
                .iter()
                .filter(|text| text.chars().all(|c| c.is_ascii()))
                .collect();
            assert!(english.is_empty(), "{name}.ui: untranslated {english:?}");
        }
    }
}
