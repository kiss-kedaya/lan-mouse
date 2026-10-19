use std::sync::OnceLock;

use adw::prelude::*;
use adw::subclass::prelude::*;
use glib::subclass::InitializingObject;
use gtk::{
    Button, CompositeTemplate, Entry,
    glib::{self, subclass::Signal},
    template_callbacks,
};

#[derive(CompositeTemplate, Default)]
#[template(resource = "/de/feschber/LanMouse/pairing_window.ui")]
pub struct PairingWindow {
    #[template_child]
    pub device_row: TemplateChild<adw::ComboRow>,
    #[template_child]
    pub device_list: TemplateChild<gtk::StringList>,
    #[template_child]
    pub addr_entry: TemplateChild<Entry>,
    #[template_child]
    pub code_entry: TemplateChild<Entry>,
    #[template_child]
    pub connect_button: TemplateChild<Button>,
}

#[glib::object_subclass]
impl ObjectSubclass for PairingWindow {
    const NAME: &'static str = "PairingWindow";
    const ABSTRACT: bool = false;

    type Type = super::PairingWindow;
    type ParentType = adw::Window;

    fn class_init(klass: &mut Self::Class) {
        klass.bind_template();
        klass.bind_template_callbacks();
    }

    fn instance_init(obj: &InitializingObject<Self>) {
        obj.init_template();
    }
}

#[template_callbacks]
impl PairingWindow {
    #[template_callback]
    fn handle_connect(&self, _button: Button) {
        let addr = self.addr_entry.text().as_str().trim().to_owned();
        let code = self.code_entry.text().as_str().trim().to_owned();
        self.obj().emit_by_name::<()>(
            "connect-clicked",
            &[&addr, &code, &self.device_row.selected()],
        )
    }
}

impl ObjectImpl for PairingWindow {
    fn signals() -> &'static [Signal] {
        static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
        SIGNALS.get_or_init(|| {
            vec![
                // address, code, index of the selected discovered device
                Signal::builder("connect-clicked")
                    .param_types([
                        String::static_type(),
                        String::static_type(),
                        u32::static_type(),
                    ])
                    .build(),
            ]
        })
    }
}

impl WidgetImpl for PairingWindow {}
impl WindowImpl for PairingWindow {}
impl ApplicationWindowImpl for PairingWindow {}
impl AdwWindowImpl for PairingWindow {}
