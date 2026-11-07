use crate::shortcuts::{Action, Binding, MicMute, SharedShortcuts, Shortcuts, key_name};
use directories::ProjectDirs;
use glib::ControlFlow::{Break, Continue};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub(super) struct ShortcutSettings {
    pub state: SharedShortcuts,
    path: PathBuf,
    window: RefCell<Option<gtk::Window>>,
    persistence_error: RefCell<Option<String>>,
}

impl ShortcutSettings {
    pub fn new(mute: MicMute) -> Rc<Self> {
        let path = ProjectDirs::from("", "Comms", "Comms")
            .unwrap()
            .data_dir()
            .join("shortcuts.json");
        let mut error = None;
        let bindings = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<Vec<Binding>>(&bytes) {
                Ok(bindings) => bindings,
                Err(e) => {
                    error = Some(format!("Could not load shortcuts: {e}"));
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                error = Some(format!("Could not load shortcuts: {e}"));
                Vec::new()
            }
        };
        let state = Arc::new(Mutex::new(Shortcuts::new(bindings, mute)));
        Shortcuts::start(state.clone());
        Rc::new(Self {
            state,
            path,
            window: RefCell::new(None),
            persistence_error: RefCell::new(error),
        })
    }

    fn save(&self) {
        let bindings = self.state.lock().unwrap().bindings.clone();
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            std::fs::create_dir_all(self.path.parent().unwrap())?;
            std::fs::write(&self.path, serde_json::to_vec_pretty(&bindings)?)?;
            Ok(())
        })();
        *self.persistence_error.borrow_mut() = result
            .err()
            .map(|e| format!("Shortcuts work for this session, but could not be saved: {e}"));
    }

    pub fn add_user_controls(self: &Rc<Self>, row: &gtk::Box, parent: &gtk::ApplicationWindow) {
        let mute_button = gtk::Button::with_label("Mute");
        let status = gtk::Label::new(None);
        let configure = gtk::Button::with_label("Configure shortcuts");
        row.insert_child_after(&configure, row.first_child().as_ref());
        row.insert_child_after(&status, Some(&configure));
        row.insert_child_after(&mute_button, Some(&status));
        {
            let state = self.state.clone();
            mute_button.connect_clicked(move |_| state.lock().unwrap().toggle());
        }
        {
            let settings = self.clone();
            let parent = parent.clone();
            configure.connect_clicked(move |_| settings.open(&parent));
        }
        let state = self.state.clone();
        let weak_button = mute_button.downgrade();
        let weak_status = status.downgrade();
        let update = move || {
            let (Some(button), Some(status)) = (weak_button.upgrade(), weak_status.upgrade())
            else {
                return Break;
            };
            let (muted, holding) = {
                let state = state.lock().unwrap();
                (state.mute.is_muted(), state.hold_muted())
            };
            button.set_sensitive(!holding);
            button.set_tooltip_text(Some(if holding {
                "Release all Hold mute keys/buttons to use this button."
            } else {
                "Toggle your microphone mute."
            }));
            button.set_label(if muted { "Unmute" } else { "Mute" });
            status.set_label(if muted {
                "Microphone muted"
            } else {
                "Microphone on"
            });
            if muted {
                status.add_css_class("error");
            } else {
                status.remove_css_class("error");
            }
            Continue
        };
        update();
        glib::timeout_add_local(Duration::from_millis(30), update);
    }

    fn open(self: &Rc<Self>, parent: &gtk::ApplicationWindow) {
        if let Some(window) = self.window.borrow().as_ref() {
            window.present();
            return;
        }
        let window = gtk::Window::builder()
            .title("Configure shortcuts")
            .transient_for(parent)
            .modal(true)
            .hide_on_close(true)
            .default_width(560)
            .default_height(320)
            .build();
        // Space/Enter must be recordable without activating the focused
        // recording button a second time. Only this settings window consumes
        // these GTK events; global hooks still forward input to other apps.
        let capture_keys = gtk::EventControllerKey::new();
        capture_keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let state = self.state.clone();
            capture_keys.connect_key_pressed(move |_, _, _, _| {
                if state.lock().unwrap().capturing_input() {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(capture_keys);
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(18);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let help = gtk::Label::new(Some(
            "Mute stops your microphone transmission. You can still hear others.\nClick a binding, then press a key or mouse button. Escape cancels.\nShortcuts work outside Comms. Normal key/button behavior is unchanged.",
        ));
        help.set_wrap(true);
        help.set_xalign(0.0);
        content.append(&help);
        let list = gtk::Box::new(gtk::Orientation::Vertical, 8);
        let scroller = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .build();
        content.append(&scroller);
        let error_label = gtk::Label::new(None);
        error_label.set_wrap(true);
        error_label.set_xalign(0.0);
        error_label.add_css_class("error");
        content.append(&error_label);
        let add = gtk::Button::with_label("Add shortcut");
        content.append(&add);
        window.set_child(Some(&content));
        let labels = Rc::new(RefCell::new(Vec::<gtk::Button>::new()));
        self.rebuild(&list, &labels);
        {
            let settings = self.clone();
            let list = list.clone();
            let labels = labels.clone();
            add.connect_clicked(move |_| {
                {
                    let mut state = settings.state.lock().unwrap();
                    state.cancel_recording();
                    state.bindings.push(Binding::default());
                }
                settings.save();
                settings.rebuild(&list, &labels);
            });
        }
        {
            let state = self.state.clone();
            window.connect_close_request(move |_| {
                state.lock().unwrap().cancel_recording();
                glib::Propagation::Proceed
            });
        }
        // Capturing and audio gating happen in the Windows hook thread. The UI
        // only reflects state, so a busy GTK thread cannot delay microphone mute.
        let settings = self.clone();
        let weak_window = window.downgrade();
        let was_recording = Cell::new(false);
        let last_bindings =
            RefCell::new(serde_json::to_vec(&self.state.lock().unwrap().bindings).unwrap());
        glib::timeout_add_local(Duration::from_millis(30), move || {
            let Some(window) = weak_window.upgrade() else {
                return Break;
            };
            let state = settings.state.lock().unwrap();
            let recording = state.recording();
            let bindings = state.bindings.clone();
            let hook_error = state.error.clone();
            drop(state);
            let serialized = serde_json::to_vec(&bindings).unwrap();
            if was_recording.replace(recording.is_some()) && recording.is_none()
                || *last_bindings.borrow() != serialized
            {
                settings.save();
                *last_bindings.borrow_mut() = serialized;
            }
            if !window.is_visible() {
                return Continue;
            }
            for (index, button) in labels.borrow().iter().enumerate() {
                button.set_label(&if recording == Some(index) {
                    "Press a key or mouse button…".into()
                } else {
                    key_name(bindings[index].key)
                });
            }
            let error = hook_error.or_else(|| settings.persistence_error.borrow().clone());
            error_label.set_label(error.as_deref().unwrap_or(""));
            error_label.set_visible(error.is_some());
            Continue
        });
        window.present();
        *self.window.borrow_mut() = Some(window);
    }

    fn rebuild(self: &Rc<Self>, list: &gtk::Box, labels: &Rc<RefCell<Vec<gtk::Button>>>) {
        while let Some(child) = list.first_child() {
            list.remove(&child);
        }
        labels.borrow_mut().clear();
        let bindings = self.state.lock().unwrap().bindings.clone();
        for (index, binding) in bindings.into_iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            let action = gtk::DropDown::from_strings(&["Toggle mute", "Hold mute"]);
            action.set_selected(if binding.action == Action::ToggleMute {
                0
            } else {
                1
            });
            action.set_hexpand(true);
            row.append(&action);
            let key = gtk::Button::with_label(&key_name(binding.key));
            key.set_hexpand(true);
            row.append(&key);
            labels.borrow_mut().push(key.clone());
            let remove = gtk::Button::with_label("Remove");
            row.append(&remove);
            list.append(&row);
            {
                let settings = self.clone();
                action.connect_selected_notify(move |dropdown| {
                    {
                        let mut state = settings.state.lock().unwrap();
                        state.cancel_recording();
                        state.bindings[index].action = if dropdown.selected() == 0 {
                            Action::ToggleMute
                        } else {
                            Action::HoldMute
                        };
                        state.refresh();
                    }
                    settings.save();
                });
            }
            {
                let state = self.state.clone();
                key.connect_clicked(move |_| state.lock().unwrap().record(index));
            }
            {
                let settings = self.clone();
                let list = list.clone();
                let labels = labels.clone();
                remove.connect_clicked(move |_| {
                    settings.state.lock().unwrap().remove(index);
                    settings.save();
                    settings.rebuild(&list, &labels);
                });
            }
        }
    }
}
