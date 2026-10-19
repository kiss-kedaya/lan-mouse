//! Runtime localization.
//!
//! Translations live in `po/<language>.po` and are compiled into a lookup
//! table by `build.rs`, so they work the same on every platform without
//! depending on the system's intl library. The language is taken from
//! `--language`, `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`, or the macOS
//! system preference, in that order.

use std::sync::OnceLock;

/// the language that is used for translations
static LANGUAGE: OnceLock<Option<String>> = OnceLock::new();

/// table generated from `po/*.po` by `build.rs`
mod catalogues {
    include!(concat!(env!("OUT_DIR"), "/catalogues.rs"));
}

/// translate `msgid` into the selected language
#[cfg(feature = "i18n")]
pub fn gettext(msgid: &str) -> String {
    let Some(language) = LANGUAGE.get().and_then(|l| l.as_deref()) else {
        return msgid.to_owned();
    };
    for (code, entries) in catalogues::CATALOGUES {
        if *code == language {
            return entries
                .iter()
                .find(|(id, _)| *id == msgid)
                .map(|(_, translated)| (*translated).to_owned())
                .unwrap_or_else(|| msgid.to_owned());
        }
    }
    // `zh_CN` falls back to `zh` when only a generic translation exists
    let generic = language.split('_').next().unwrap_or(language);
    for (code, entries) in catalogues::CATALOGUES {
        if *code == generic {
            return entries
                .iter()
                .find(|(id, _)| *id == msgid)
                .map(|(_, translated)| (*translated).to_owned())
                .unwrap_or_else(|| msgid.to_owned());
        }
    }
    msgid.to_owned()
}

/// builds without gettext keep the original English texts
#[cfg(not(feature = "i18n"))]
pub fn gettext(msgid: &str) -> String {
    msgid.to_owned()
}

/// select the language of the user interface
#[cfg(feature = "i18n")]
pub fn init() {
    let Some(language) = requested_language() else {
        log::debug!("no language requested, using the original texts");
        return;
    };
    log::debug!("language: {language}");
    let _ = LANGUAGE.set(Some(language));
    // the IPC crate builds some texts itself (connection errors, positions)
    lan_mouse_ipc::set_translator(gettext);
    lan_mouse_cli::i18n::set_translator(gettext);
}

#[cfg(not(feature = "i18n"))]
pub fn init() {
    let _ = LANGUAGE.set(None);
}

/// the language to translate into, `None` when the user did not express one
pub fn requested_language() -> Option<String> {
    let value = std::env::var("LANGUAGE")
        .ok()
        .and_then(|l| l.split(':').next().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned))
        .or_else(|| std::env::var("LC_ALL").ok())
        .or_else(|| std::env::var("LC_MESSAGES").ok())
        .or_else(|| std::env::var("LANG").ok())
        // the system preference, Windows has no locale environment
        .or_else(system_language);
    let value = value?;
    // drop the modifier and codeset: zh_CN.UTF-8@euro -> zh_CN, C/POSIX are no language
    let value = value.split(['.', '@']).next().unwrap_or("").trim().to_owned();
    let value = value.replace('-', "_");
    if value.is_empty() || value == "C" || value == "POSIX" {
        return None;
    }
    Some(value)
}

/// the display language configured for the current Windows user
#[cfg(target_os = "windows")]
fn system_language() -> Option<String> {
    // `GetUserDefaultUILanguage` returns a Win32 LANGID, its low word is the
    // primary language, e.g. 0x0804 for zh-CN. Declared here to keep the
    // crate free of a Windows API dependency.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    let langid = unsafe { GetUserDefaultUILanguage() };
    if langid == 0 {
        return None;
    }
    let primary = langid & 0x3ff;
    Some(
        match primary {
            0x04 => "zh_CN", // simplified Chinese
            0x07 => "de",
            0x09 => "en",
            0x0a => "es",
            0x0c => "fr",
            0x10 => "it",
            0x11 => "ja",
            0x12 => "ko",
            0x19 => "ru",
            0x15 => "pl",
            0x16 => "pt",
            0x1f => "tr",
            0x1d => "sv",
            0x13 => "nl",
            _ => return None,
        }
        .to_owned(),
    )
}

#[cfg(not(target_os = "windows"))]
fn system_language() -> Option<String> {
    macos_language()
}

/// the language preference of the current macOS user.
///
/// `defaults` talks to cfprefsd and can block for a long time when the
/// preferences are busy, so it runs in a thread and startup continues with
/// the environment language after a short wait.
#[cfg(target_os = "macos")]
fn macos_language() -> Option<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let language = std::process::Command::new("/usr/bin/defaults")
            .args(["read", "-g", "AppleLanguages"])
            .output()
            .ok()
            .and_then(|output| {
                let text = String::from_utf8_lossy(&output.stdout).into_owned();
                let first = text.lines().find(|l| l.trim().starts_with('"'))?;
                Some(
                    first
                        .trim()
                        .trim_start_matches(['"', ',', ' '])
                        .trim_end_matches(['"', ','])
                        .to_owned(),
                )
            });
        let _ = tx.send(language);
    });
    match rx.recv_timeout(std::time::Duration::from_millis(300)) {
        Ok(language) => language,
        Err(_) => {
            log::debug!("macOS language preference not available in time");
            None
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn macos_language() -> Option<String> {
    None
}

/// Translate a message. Usage: `gettext!("some English text")`.
#[macro_export]
macro_rules! gettext {
    ($msgid:expr) => {
        $crate::i18n::gettext($msgid)
    };
}
