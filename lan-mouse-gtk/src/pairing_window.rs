mod imp;

use glib::Object;
use adw::prelude::*;
use gtk::{gio, glib, subclass::prelude::ObjectSubclassIsExt};

glib::wrapper! {
    pub struct PairingWindow(ObjectSubclass<imp::PairingWindow>)
    @extends adw::Window, gtk::Window, gtk::Widget,
    @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
                gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl PairingWindow {
    pub(crate) fn new(devices: &[String]) -> Self {
        let window: Self = Object::builder().build();
        window.set_devices(devices);
        window
    }

    /// replace the list of discovered devices
    pub(crate) fn set_devices(&self, devices: &[String]) {
        let model = self.imp().device_list.get();
        let selected = self.imp().device_row.selected();
        let device_refs: Vec<&str> = devices.iter().map(|d| d.as_str()).collect();
        model.splice(0, model.n_items(), &device_refs);
        if devices.is_empty() {
            self.imp().device_row.set_visible(false);
        } else {
            self.imp().device_row.set_visible(true);
            self.imp()
                .device_row
                .set_selected(selected.min(devices.len() as u32 - 1));
        }
    }
}
