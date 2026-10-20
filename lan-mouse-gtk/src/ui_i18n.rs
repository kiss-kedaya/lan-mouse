//! Registers the `.ui` templates in the selected language.
//!
//! `build.rs` compiles one `GResource` per language with the texts of the
//! templates translated. The bundled `lan-mouse.gresource` (English) is
//! registered first; the selected language is registered on top of it, so the
//! `#[template]` lookups find the translated files under the same paths.

/// the compiled templates of every language of `po/`, from `build.rs`
mod bundles {
    include!(concat!(env!("OUT_DIR"), "/ui_translations.rs"));
}

use bundles::UI_BUNDLES;

/// registers the templates for `language` (`None`: keep the English ones)
#[cfg(feature = "i18n")]
pub fn register(language: Option<&str>) {
    let Some(language) = language else { return };
    let Some(bundle) = bundle_for(language) else {
        log::debug!("no translated templates for {language}, using English");
        return;
    };
    match gtk::gio::Resource::from_data(&gtk::glib::Bytes::from(bundle)) {
        Ok(resource) => gtk::gio::resources_register(&resource),
        Err(e) => log::warn!("registering the translated templates failed: {e}"),
    }
}

#[cfg(not(feature = "i18n"))]
pub fn register(_language: Option<&str>) {}

/// `zh_CN` falls back to `zh` when only a generic translation exists
fn bundle_for(language: &str) -> Option<&'static [u8]> {
    let find = |code: &str| UI_BUNDLES.iter().find(|(l, _)| *l == code).map(|(_, b)| *b);
    find(language).or_else(|| {
        let generic = language.split('_').next().unwrap_or(language);
        (generic != language).then(|| find(generic)).flatten()
    })
}
