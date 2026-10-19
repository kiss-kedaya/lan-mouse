//! Translations of the texts of this crate. The language is chosen when the
//! binary starts (`lan_mouse::i18n::init()`), which installs the translation
//! function used here.

use std::sync::OnceLock;

/// translation function installed by the binary at startup
static TRANSLATOR: OnceLock<fn(&str) -> String> = OnceLock::new();

/// install the translation function (does nothing the second time)
pub fn set_translator(translator: fn(&str) -> String) {
    let _ = TRANSLATOR.set(translator);
}

/// translate `msgid`
pub fn gettext(msgid: &str) -> String {
    match TRANSLATOR.get() {
        Some(translator) => translator(msgid),
        None => msgid.to_owned(),
    }
}
