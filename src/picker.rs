//! The menu above a meter that picks the device it records: "System default"
//! (what `parec` calls `@DEFAULT_SOURCE@` or `@DEFAULT_MONITOR@`), then the
//! microphones or outputs there are now. A picked device that is unplugged
//! stays in the menu, marked and with a warning under the meter, so it is
//! recorded again once it is back.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;

use crate::audio::{Device, Devices, Role, Source};
use crate::settings;

pub struct Picker {
    pub dropdown: gtk::DropDown,
    /// Under the meter while the picked device is unplugged.
    pub warning: gtk::Label,
    source: Source,
    role: Role,
    /// What the menu shows now.
    shown: RefCell<Menu>,
    /// What the system default is called, under "System default" in the list.
    default_label: Rc<RefCell<Option<String>>>,
    /// Set while the menu is rebuilt, so that is not taken for a choice.
    refreshing: Cell<bool>,
    /// Called after a choice, to show it in the other windows too.
    on_choose: Box<dyn Fn()>,
}

impl Picker {
    pub fn new(source: &Source, role: Role, on_choose: impl Fn() + 'static) -> Rc<Self> {
        let default_label: Rc<RefCell<Option<String>>> = Rc::default();
        let dropdown = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&["System default"]))
            .factory(&labels(false, default_label.clone()))
            .list_factory(&labels(true, default_label.clone()))
            .css_classes(["picker"])
            .valign(gtk::Align::Center)
            .tooltip_text("The device to record, can be changed during the call")
            .build();
        dropdown.update_property(&[gtk::accessible::Property::Label(match role {
            Role::Mic => "Microphone to record",
            Role::System => "Output to record",
        })]);
        let warning = gtk::Label::builder()
            .label("Not connected: this track stays silent until it is back")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "warning"])
            .visible(false)
            .build();
        let picker = Rc::new(Picker {
            dropdown,
            warning,
            source: source.clone(),
            role,
            shown: RefCell::default(),
            default_label,
            refreshing: Cell::new(false),
            on_choose: Box::new(on_choose),
        });
        // Until the first listing, the saved device is all there is.
        picker.update(None);
        let weak = Rc::downgrade(&picker);
        picker.dropdown.connect_selected_notify(move |dropdown| {
            if let Some(p) = weak.upgrade()
                && !p.refreshing.get()
            {
                p.choose(dropdown.selected());
            }
        });
        picker
    }

    /// Shows `devices` with the one being recorded selected, `None` before
    /// they are first listed.
    pub fn update(&self, devices: Option<&Devices>) {
        let current = self.source.device();
        let saved = settings::load_device(self.role).filter(|d| d.name == current);
        let (devices, default_label) = devices.map(|d| d.of(self.role)).unzip();
        let menu = Menu::new(
            devices,
            &current,
            self.role.default_device(),
            saved.as_ref().map(|d| d.label.as_str()),
        );
        self.warning.set_visible(menu.unplugged);
        let default_label = default_label.flatten().map(str::to_owned);
        self.refreshing.set(true);
        if self.shown.borrow().labels() != menu.labels()
            || *self.default_label.borrow() != default_label
        {
            *self.default_label.borrow_mut() = default_label;
            let labels = menu.labels();
            let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
            self.dropdown
                .set_model(Some(&gtk::StringList::new(&labels)));
        }
        if self.dropdown.selected() != menu.selected {
            self.dropdown.set_selected(menu.selected);
        }
        *self.shown.borrow_mut() = menu;
        self.refreshing.set(false);
    }

    fn choose(&self, index: u32) {
        let device = self.shown.borrow().device(index);
        self.source.set_device(
            device
                .as_ref()
                .map_or(self.role.default_device(), |d| &d.name),
        );
        settings::save_device(self.role, device.as_ref());
        (self.on_choose)();
    }
}

/// The rows of a picker after "System default".
#[derive(Default)]
struct Menu {
    /// As the sound server names them, or as they were saved.
    devices: Vec<Device>,
    selected: u32,
    /// Whether the selected device is unplugged; it is then the last one.
    unplugged: bool,
}

impl Menu {
    /// `devices`, and `current` too when it is unplugged, `current` selected.
    /// `devices` is `None` before they are first listed, when `current` is
    /// simply shown. `saved_label` is what `current` was called when it was
    /// picked.
    fn new(
        devices: Option<&[Device]>,
        current: &str,
        default: &str,
        saved_label: Option<&str>,
    ) -> Self {
        let listed = devices.is_some();
        let mut devices = devices.unwrap_or_default().to_vec();
        let missing = current != default && !devices.iter().any(|d| d.name == current);
        if missing {
            devices.push(Device {
                name: current.to_owned(),
                label: saved_label.unwrap_or(current).to_owned(),
            });
        }
        let selected = devices
            .iter()
            .position(|d| d.name == current)
            .map_or(0, |i| i + 1) as u32;
        Menu {
            devices,
            selected,
            unplugged: listed && missing,
        }
    }

    /// The text of each row, "System default" first.
    fn labels(&self) -> Vec<String> {
        let last = self.devices.len().saturating_sub(1);
        std::iter::once("System default".to_owned())
            .chain(self.devices.iter().enumerate().map(|(i, d)| {
                if self.unplugged && i == last {
                    format!("{} (not connected)", d.label)
                } else {
                    d.label.clone()
                }
            }))
            .collect()
    }

    /// The device of row `index`, `None` for "System default".
    fn device(&self, index: u32) -> Option<Device> {
        let i = index.checked_sub(1)?;
        self.devices.get(i as usize).cloned()
    }
}

/// Labels for a picker: on the button cut short, in the list in full, with
/// what the system default is called under "System default".
fn labels(in_list: bool, default_label: Rc<RefCell<Option<String>>>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let label = gtk::Label::builder().xalign(0.0).build();
        let below = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .visible(false)
            .build();
        if !in_list {
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(24);
        }
        row.append(&label);
        row.append(&below);
        item.downcast_ref::<gtk::ListItem>()
            .expect("a list item")
            .set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let item = item.downcast_ref::<gtk::ListItem>().expect("a list item");
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string());
        let Some(row) = item.child() else {
            return;
        };
        let (Some(label), Some(below)) = (
            row.first_child().and_downcast::<gtk::Label>(),
            row.last_child().and_downcast::<gtk::Label>(),
        ) else {
            return;
        };
        label.set_label(text.as_deref().unwrap_or_default());
        let default = default_label
            .borrow()
            .clone()
            .filter(|_| in_list && item.position() == 0);
        below.set_visible(default.is_some());
        below.set_label(default.as_deref().unwrap_or_default());
    });
    factory
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{DEFAULT_MIC, DEFAULT_SYSTEM};

    fn device(name: &str, label: &str) -> Device {
        Device {
            name: name.to_owned(),
            label: label.to_owned(),
        }
    }

    #[test]
    fn on_the_default() {
        let devices = [device("a", "Headset"), device("b", "Speakers")];
        let menu = Menu::new(Some(&devices), DEFAULT_SYSTEM, DEFAULT_SYSTEM, None);
        assert_eq!(menu.labels(), ["System default", "Headset", "Speakers"]);
        assert_eq!((menu.selected, menu.unplugged), (0, false));
        assert_eq!(menu.device(0), None);
    }

    #[test]
    fn on_a_connected_device() {
        let devices = [device("a", "Headset"), device("b", "Speakers")];
        let menu = Menu::new(Some(&devices), "b", DEFAULT_MIC, None);
        assert_eq!((menu.selected, menu.unplugged), (2, false));
        assert_eq!(menu.device(2), Some(device("b", "Speakers")));
    }

    #[test]
    fn keeps_an_unplugged_device() {
        let devices = [device("b", "Speakers")];
        let menu = Menu::new(Some(&devices), "a", DEFAULT_MIC, Some("Headset"));
        assert_eq!(
            menu.labels(),
            ["System default", "Speakers", "Headset (not connected)"]
        );
        assert_eq!((menu.selected, menu.unplugged), (2, true));
        // Choosing it again saves its own name, not the row's text.
        assert_eq!(menu.device(2), Some(device("a", "Headset")));
        // Never saved: its PulseAudio name is all there is.
        let menu = Menu::new(Some(&[]), "a", DEFAULT_MIC, None);
        assert_eq!(menu.labels(), ["System default", "a (not connected)"]);
    }

    #[test]
    fn unplugging_the_last_device_changes_the_rows() {
        // Where it was is where it stays, so only the text tells it apart.
        let plugged = Menu::new(Some(&[device("a", "Headset")]), "a", DEFAULT_MIC, None);
        let unplugged = Menu::new(Some(&[]), "a", DEFAULT_MIC, Some("Headset"));
        assert_eq!(plugged.devices, unplugged.devices);
        assert_ne!(plugged.labels(), unplugged.labels());
    }

    #[test]
    fn before_the_first_listing() {
        // The saved device, without a warning it may not deserve.
        let menu = Menu::new(None, "a", DEFAULT_MIC, Some("Headset"));
        assert_eq!(menu.labels(), ["System default", "Headset"]);
        assert_eq!((menu.selected, menu.unplugged), (1, false));
        let menu = Menu::new(None, DEFAULT_MIC, DEFAULT_MIC, None);
        assert_eq!(menu.labels(), ["System default"]);
        assert_eq!(menu.selected, 0);
    }
}
