use async_std::task::sleep;
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::DerefMut;
use std::rc::Rc;
use std::time::Duration;

use directories::ProjectDirs;
use futures::channel::mpsc::Sender;
use futures::stream::StreamExt;
use futures_timer::Delay;
use gio::prelude::*;
use gio::ApplicationFlags;
use glib::ControlFlow::{Break, Continue};
use glib::{timeout_add_local, SignalHandlerId};
use gtk::gdk::Display;
use gtk::pango::EllipsizeMode;
use gtk::prelude::*;
use gtk::{
    gdk, DialogFlags, Orientation, PositionType, ResponseType, RevealerTransitionType, StyleContext,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use comms_lib::network::CommsNetworkError;
use comms_lib::packet;
use comms_lib::packet::{
    ClientId, ErrorResponse, NetworkUser, Packet, PacketBody, UserId, UserJoined, UserLeft,
};

use crate::audio::ipc::{AudioCommand, AudioHandle, EventLoopCreationError};
use crate::network::future::{attempt_p2p, keep_alive, P2pDestination};
use crate::network::ipc::{ConnectError, NetworkCommand, NetworkEvent, NetworkHandle};
use crate::ui::ipc::{AudioIPCInitStatus, UICommand};
use crate::{network, RESOURCES_DIR};

#[cfg(windows)]
use crate::ui::update::check_for_updates;

pub mod ipc;
#[cfg(windows)]
mod update;

#[derive(Serialize, Deserialize)]
struct LocalStorage {
    username: String,
    server: String,
    user_id: UserId,
    volumes: HashMap<UserId, u16>,
}

pub fn comms_version() -> Cow<'static, str> {
    option_env!("CI_COMMIT_SHA")
        .map(|sha| Cow::from(format!("commit-{sha}")))
        .unwrap_or("local-dev".into())
}

const BUILD_TIMESTAMP: Option<&str> = option_env!("CI_JOB_STARTED_AT");

fn finish_connect(
    audio_handle: AudioHandle,
    network_handle: NetworkHandle,
    network_event_sender: Sender<NetworkEvent>,
    ui: Rc<RefCell<UiModel>>,
    client_id: ClientId,
    existing_users: Vec<NetworkUser>,
    round_trip_latency: Duration,
    username: String,
    server: String,
    user_id: UserId,
    saved_volumes: HashMap<UserId, u16>,
) {
    let glib_context = glib::MainContext::default();

    glib_context.spawn_local(keep_alive(
        network_handle.clone(),
        network_event_sender.clone(),
    ));

    audio_handle.send(AudioCommand::ServerConnect).unwrap();

    for user in existing_users.iter() {
        audio_handle
            .send(AudioCommand::NewClient(user.client_id))
            .unwrap();

        glib_context.spawn_local(attempt_p2p(
            network_handle.clone(),
            network_event_sender.clone(),
            client_id,
            P2pDestination {
                client_id: user.client_id,
                local_udp_addr: user.local_udp_addr,
                public_udp_addr: user.public_udp_addr,
            },
            true,
        ));
    }

    let user_list: gtk::ListBox = {
        let ui = ui.borrow_mut();
        ui.builder.object("user_list").unwrap()
    };

    let (row_widget, latency_widget) = UiUser::create_widgets(&username, round_trip_latency);
    user_list.append(&row_widget);

    let user = Rc::new(RefCell::new(UiUser {
        client_id,
        user_id,
        volume: *saved_volumes.get(&user_id).unwrap_or(&100u16),
        is_current_user: true,
        network_latency: round_trip_latency,
        is_peer_to_peer: false,
        root_widget: row_widget,
        latency_widget,
        is_speaking: false,
        speaking_duration: Duration::ZERO,
    }));
    UiUser::attach_signal_handlers(user.clone(), audio_handle.clone(), ui.clone());
    let mut users = HashMap::from([(client_id, user)]);

    for user in existing_users.into_iter() {
        let one_way_latency = user.round_trip_latency.saturating_add(round_trip_latency) / 2;

        let volume = if let Some(volume) = saved_volumes.get(&user.user_id) {
            audio_handle
                .send(AudioCommand::ChangeClientVolume(user.client_id, *volume))
                .unwrap();
            *volume
        } else {
            100u16
        };

        let (row_widget, latency_widget) = UiUser::create_widgets(&user.name, round_trip_latency);
        user_list.append(&row_widget);

        let user_rc = Rc::new(RefCell::new(UiUser {
            client_id: user.client_id,
            user_id: user.user_id,
            volume,
            is_current_user: false,
            network_latency: one_way_latency,
            is_peer_to_peer: false,
            root_widget: row_widget,
            latency_widget,
            is_speaking: false,
            speaking_duration: Duration::ZERO,
        }));
        UiUser::attach_signal_handlers(user_rc.clone(), audio_handle.clone(), ui.clone());
        users.insert(user.client_id, user_rc);
    }

    let mut ui = ui.borrow_mut();
    ui.pane_stack
        .set_transition_type(gtk::StackTransitionType::SlideUp);
    ui.pane_stack.set_visible_child_name("user_list");
    ui.disconnect_button_revealer.set_reveal_child(true);
    ui.set_state(UiState::Main {
        users,
        client_id,
        round_trip_latency,
        user_id,
        saved_volumes,
        user_list,
        username,
        server,
    });
}

async fn try_connect(
    audio_handle: AudioHandle,
    network_handle: NetworkHandle,
    network_event_sender: Sender<NetworkEvent>,
    ui: Rc<RefCell<UiModel>>,
) {
    let (user_id, saved_volumes) = match ui.borrow_mut().use_state(|state| {
        if let UiState::ChooseServer {
            user_id,
            saved_volumes,
            ..
        } = state
        {
            (UiState::Connecting, Some((user_id, saved_volumes)))
        } else {
            (state, None)
        }
    }) {
        Some(m) => m,
        None => return,
    };

    let builder = ui.borrow().builder.clone();
    let username_entry: adw::EntryRow = builder.object("username_entry").unwrap();
    let server_entry: adw::EntryRow = builder.object("server_entry").unwrap();
    let connect_button: gtk::Button = builder.object("connect_button").unwrap();
    let connect_progress: gtk::ProgressBar = builder.object("connect_progress").unwrap();
    let connect_failure_status: gtk::Label = builder.object("connect_failure_status").unwrap();

    let username: String = username_entry.text().into();
    let server: String = server_entry.text().into();
    if username.is_empty() || server.is_empty() {
        connect_failure_status.set_label("Set both username and server");
        ui.borrow_mut().set_state(UiState::ChooseServer {
            user_id,
            saved_volumes,
        });
        return;
    } else {
        connect_failure_status.set_label(" ");
    }

    connect_button.set_sensitive(false);
    connect_progress.set_opacity(1f64);

    let finished = Rc::new(Cell::new(false));
    {
        let finished = finished.clone();
        let connect_progress = connect_progress.clone();
        timeout_add_local(Duration::from_millis(80), move || {
            if !finished.get() {
                connect_progress.pulse();
                Continue
            } else {
                Break
            }
        });
    }

    if let Some((project_dirs, storage)) =
        ProjectDirs::from("", "Comms", "Comms").and_then(|project_dirs| {
            let local_storage = LocalStorage {
                username: username.clone(),
                server: server.clone(),
                user_id,
                volumes: saved_volumes.clone(),
            };
            serde_json::to_string(&local_storage)
                .ok()
                .map(|storage| (project_dirs, storage))
        })
    {
        let _ = async_std::fs::create_dir_all(project_dirs.data_dir()).await;
        let _ = async_std::fs::write(project_dirs.data_dir().join("local_storage"), storage).await;
    }

    let connect_result =
        network::future::connect(&network_handle, server.clone(), username.clone(), user_id).await;

    finished.replace(true);
    connect_progress.set_opacity(0f64);
    connect_button.set_sensitive(true);

    match connect_result {
        Ok((client_id, existing_users, round_trip_latency)) => {
            finish_connect(
                audio_handle,
                network_handle,
                network_event_sender,
                ui,
                client_id,
                existing_users,
                round_trip_latency,
                username,
                server,
                user_id,
                saved_volumes,
            );
        }
        Err(e) => {
            let error_message = match e {
                ConnectError::DestinationUnusable => {
                    "Could not find the server provided".to_string()
                }
                ConnectError::NetworkError => "Could not connect to the server".to_string(),
                // AlreadyConnected shouldn't be possible, but it's useful for end-to-end testing network futures right now.
                ConnectError::AlreadyConnected => {
                    "Can't connect because you're already connected to a server".to_string()
                }
                ConnectError::RequestError(e) => format!("Request failed: {e:?}"),
                ConnectError::IncompatibleServerProtocolVersion(server_version) => {
                    if server_version > packet::PROTOCOL_VERSION {
                        format!(
                            "App is out-of-date compared to server ({} < {}), please update Comms",
                            packet::PROTOCOL_VERSION,
                            server_version
                        )
                    } else {
                        format!(
                            "App is newer than out-of-date server ({} > {})",
                            packet::PROTOCOL_VERSION,
                            server_version
                        )
                    }
                }
            };
            connect_failure_status.set_label(&error_message);
            ui.borrow_mut().set_state(UiState::ChooseServer {
                user_id,
                saved_volumes,
            });
        }
    }
}

fn finish_cancel_reconnect(
    ui: &mut UiModel,
    connect_progress: &gtk::ProgressBar,
    connect_button: &gtk::Button,
    connect_button_label: &gtk::Label,
    click_handler_id: &Cell<Option<SignalHandlerId>>,
    user_id: UserId,
    saved_volumes: HashMap<UserId, u16>,
) {
    connect_progress.set_opacity(0f64);
    connect_button_label.set_label("Connect");
    if let Some(handler_id) = click_handler_id.replace(None) {
        connect_button.disconnect(handler_id)
    }
    ui.set_state(UiState::ChooseServer {
        user_id,
        saved_volumes,
    });
}

async fn try_reconnect(
    network_handle: NetworkHandle,
    audio_handle: AudioHandle,
    network_event_sender: Sender<NetworkEvent>,
    ui: Rc<RefCell<UiModel>>,
    user_id: UserId,
    saved_volumes: HashMap<UserId, u16>,
    username: String,
    server: String,
) {
    let builder = {
        let mut ui = ui.borrow_mut();
        ui.set_state(UiState::Reconnecting);
        ui.builder.clone()
    };

    let connect_button: gtk::Button = builder.object("connect_button").unwrap();
    let connect_button_label: gtk::Label = builder.object("connect_button_label").unwrap();
    let connect_progress: gtk::ProgressBar = builder.object("connect_progress").unwrap();
    let connect_failure_status: gtk::Label = builder.object("connect_failure_status").unwrap();

    connect_button_label.set_label("Cancel reconnecting...");

    connect_progress.set_opacity(1f64);
    let completion_params = Rc::new(RefCell::new(Some((user_id, saved_volumes))));
    {
        let completion_params = completion_params.clone();
        let connect_progress = connect_progress.clone();
        timeout_add_local(Duration::from_millis(80), move || {
            if completion_params.borrow().is_some() {
                connect_progress.pulse();
                Continue
            } else {
                Break
            }
        });
    };

    let click_handler_id = Rc::new(Cell::new(None));
    (*click_handler_id).set(Some({
        let ui = ui.clone();
        let completion_params = completion_params.clone();
        let click_handler_id = click_handler_id.clone();
        let connect_progress = connect_progress.clone();
        let connect_button_label = connect_button_label.clone();
        connect_button.connect_clicked(move |connect_button| {
            let Some((user_id, saved_volumes)) = completion_params.replace(None) else {
                return;
            };

            finish_cancel_reconnect(
                ui.borrow_mut().deref_mut(),
                &connect_progress,
                connect_button,
                &connect_button_label,
                &click_handler_id,
                user_id,
                saved_volumes,
            )
        })
    }));

    loop {
        sleep(Duration::from_secs(1)).await;

        if completion_params.borrow().is_none() {
            return;
        }

        let connect_result =
            network::future::connect(&network_handle, server.clone(), username.clone(), user_id)
                .await;

        match connect_result {
            Ok((client_id, existing_users, round_trip_latency)) => {
                let Some((user_id, saved_volumes)) = completion_params.replace(None) else {
                    return;
                };

                finish_connect(
                    audio_handle,
                    network_handle,
                    network_event_sender,
                    ui,
                    client_id,
                    existing_users,
                    round_trip_latency,
                    username,
                    server,
                    user_id,
                    saved_volumes,
                );
                connect_progress.set_opacity(0f64);
                connect_button_label.set_label("Connect");
                if let Some(handler_id) = (*click_handler_id).replace(None) {
                    connect_button.disconnect(handler_id)
                }
                return;
            }
            Err(e) => {
                if let ConnectError::IncompatibleServerProtocolVersion(server_version) = e {
                    let error_message = if server_version > packet::PROTOCOL_VERSION {
                        format!(
                            "App is out-of-date compared to server ({} < {}), please update Comms",
                            packet::PROTOCOL_VERSION,
                            server_version
                        )
                    } else {
                        format!(
                            "App is newer than out-of-date server ({} > {})",
                            packet::PROTOCOL_VERSION,
                            server_version
                        )
                    };

                    let Some((user_id, saved_volumes)) = completion_params.replace(None) else {
                        return;
                    };

                    finish_cancel_reconnect(
                        ui.borrow_mut().deref_mut(),
                        &connect_progress,
                        &connect_button,
                        &connect_button_label,
                        &click_handler_id,
                        user_id,
                        saved_volumes,
                    );
                    connect_failure_status.set_label(&error_message);
                    return;
                };
            }
        }
    }
}

async fn disconnect_from_server(
    users: HashMap<ClientId, Rc<RefCell<UiUser>>>,
    user_list: gtk::ListBox,
    ui: Rc<RefCell<UiModel>>,
    handle: NetworkHandle,
    audio: AudioHandle,
) {
    network::future::disconnect(&handle, None).await.unwrap();
    {
        let pane_stack = &ui.borrow().pane_stack;
        pane_stack.set_transition_type(gtk::StackTransitionType::SlideDown);
        pane_stack.set_visible_child_name("connect");
        let ui = ui.borrow();
        let connect_button: gtk::Button = ui.builder.object("connect_button").unwrap();
        ui.window.set_default_widget(Some(&connect_button));
        ui.disconnect_button_revealer.set_reveal_child(false);
    }
    for user in users.values() {
        // ListBox implicitly creates a "ListBoxRow" parent for each appended widget
        user_list.remove(&user.borrow().root_widget.parent().unwrap());
    }
    audio.send(AudioCommand::ServerDisconnect).unwrap();
}

struct UiUser {
    client_id: ClientId,
    user_id: UserId,
    volume: u16,
    is_current_user: bool,
    network_latency: Duration,
    is_peer_to_peer: bool,
    root_widget: gtk::Box,
    latency_widget: gtk::Label,
    is_speaking: bool,
    speaking_duration: Duration,
}

impl UiUser {
    fn create_widgets(username: &str, network_latency: Duration) -> (gtk::Box, gtk::Label) {
        let row_container = gtk::Box::new(Orientation::Horizontal, 0);
        row_container.append(
            &gtk::Label::builder()
                .label(username)
                .xalign(0.0)
                .hexpand(true)
                .ellipsize(EllipsizeMode::End)
                .build(),
        );
        let latency_label = gtk::Label::builder()
            .label(Self::format_latency_text(network_latency, false))
            .xalign(1.0)
            .build();
        row_container.append(&latency_label);
        (row_container, latency_label)
    }

    fn format_latency_text(network_latency: Duration, show_link_emoji: bool) -> String {
        format!(
            "{:.1}ms {}",
            (network_latency.as_micros() as f64) / 1000.0 / 2.0,
            if show_link_emoji { "🔗" } else { "" }
        )
    }

    fn update_latency_text(&self) {
        self.latency_widget.set_text(&Self::format_latency_text(
            self.network_latency,
            self.is_peer_to_peer,
        ));
    }

    fn attach_signal_handlers(
        this: Rc<RefCell<Self>>,
        audio: AudioHandle,
        ui: Rc<RefCell<UiModel>>,
    ) {
        let user = this.borrow();
        let click_gesture = gtk::GestureClick::new();
        click_gesture.set_button(3);
        user.root_widget.add_controller(click_gesture.clone());
        {
            let this = this.clone();
            click_gesture.connect_pressed(move |_, _, x, y| {
                let x = x.round() as i32;
                let y = y.round() as i32;

                let user = this.borrow_mut();
                let popover = gtk::Popover::new();
                let popover_box = gtk::Box::new(Orientation::Vertical, 0);

                let label_text = match user.is_current_user {
                    true => "Headphone Volume",
                    false => "Volume",
                };

                let adjustment =
                    gtk::Adjustment::new(user.volume as f64, 0.0, 400.0, 2.0, 10.0, 0.0);
                let scale = gtk::Scale::builder()
                    .adjustment(&adjustment)
                    .value_pos(PositionType::Left)
                    .orientation(Orientation::Horizontal)
                    .digits(0)
                    .draw_value(true)
                    .build();

                scale.add_mark(100.0, gtk::PositionType::Bottom, None);
                scale.add_mark(200.0, gtk::PositionType::Bottom, None);
                scale.add_mark(300.0, gtk::PositionType::Bottom, None);
                scale.set_width_request(200);

                popover_box.insert_child_after(&scale, None::<&gtk::Widget>);
                popover_box.insert_child_after(
                    &gtk::Label::builder().label(label_text).build(),
                    None::<&gtk::Widget>,
                );

                let this = this.clone();
                let audio = audio.clone();
                let ui = ui.clone();
                scale.connect_value_changed(move |scale| {
                    let mut ui = ui.borrow_mut();
                    let saved_volumes = if let UiState::Main { saved_volumes, .. } = ui.state_mut()
                    {
                        saved_volumes
                    } else {
                        return;
                    };

                    let mut user = this.borrow_mut();
                    user.volume = scale.adjustment().value() as u16;
                    audio
                        .send(if user.is_current_user {
                            AudioCommand::ChangeMainVolume(user.volume)
                        } else {
                            AudioCommand::ChangeClientVolume(user.client_id, user.volume)
                        })
                        .unwrap();

                    saved_volumes.insert(user.user_id, user.volume);
                    let glib_context = glib::MainContext::default();
                    glib_context.spawn_local(update_saved_volumes(saved_volumes.clone()));
                });

                let rectangle = gdk::Rectangle::new(x, y, 0, 0);
                popover.set_pointing_to(Some(&rectangle));
                user.root_widget.prepend(&popover);
                popover.set_child(Some(&popover_box));
                popover.popup();

                popover.connect_closed(|popover| {
                    popover.unparent();
                });
            });
        }

        user.latency_widget.set_property("has-tooltip", true);
        {
            let this = this.clone();
            user.latency_widget
                .connect_query_tooltip(move |_, _, _, _, tooltip| {
                    let user = this.borrow();
                    let latency_text = Self::format_latency_text(user.network_latency, false);
                    let text = match (user.is_peer_to_peer, user.is_current_user) {
                        (_, true) => {
                            format!("Latency to server is {}", &latency_text)
                        }
                        (true, _) => {
                            format!("Latency to other client (direct) is {}", &latency_text)
                        }
                        (false, _) => {
                            format!("Latency to other client (via server) is {}", &latency_text)
                        }
                    };
                    tooltip.set_text(Some(&text));
                    true
                });
        }
    }
}

enum UiState {
    ChooseServer {
        user_id: UserId,
        saved_volumes: HashMap<UserId, u16>,
    },
    Main {
        users: HashMap<ClientId, Rc<RefCell<UiUser>>>,
        user_id: UserId,
        saved_volumes: HashMap<UserId, u16>,
        user_list: gtk::ListBox,
        client_id: ClientId,
        round_trip_latency: Duration,
        username: String,
        server: String,
    },
    Connecting,
    Disconnecting,
    Reconnecting,
}

struct UiModel {
    state: Option<UiState>,
    pane_stack: gtk::Stack,
    builder: gtk::Builder,
    app: adw::Application,
    window: gtk::ApplicationWindow,
    disconnect_button_revealer: gtk::Revealer,
}

impl UiModel {
    fn state(&self) -> &UiState {
        self.state.as_ref().unwrap()
    }

    fn state_mut(&mut self) -> &mut UiState {
        self.state.as_mut().unwrap()
    }

    fn set_state(&mut self, state: UiState) {
        self.state = Some(state);
    }

    fn use_state<T>(&mut self, f: impl FnOnce(UiState) -> (UiState, T)) -> T {
        let old = self.state.take().unwrap();
        let (new, return_value) = f(old);
        self.state = Some(new);
        return_value
    }
}

async fn handle_network_events(
    mut receiver: futures::channel::mpsc::Receiver<NetworkEvent>,
    network_event_sender: Sender<NetworkEvent>,
    audio: AudioHandle,
    network: NetworkHandle,
    ui: Rc<RefCell<UiModel>>,
) {
    while let Some(event) = receiver.next().await {
        match event {
            NetworkEvent::SocketClosed => {
                let (users, user_list, user_id, saved_volumes, username, server) =
                    match ui.borrow_mut().use_state(|state| {
                        if let UiState::Main {
                            users,
                            user_list,
                            user_id,
                            saved_volumes,
                            username,
                            server,
                            ..
                        } = state
                        {
                            (
                                UiState::Disconnecting,
                                Some((users, user_list, user_id, saved_volumes, username, server)),
                            )
                        } else {
                            (state, None)
                        }
                    }) {
                        Some(m) => m,
                        None => continue,
                    };

                disconnect_from_server(
                    users,
                    user_list,
                    ui.clone(),
                    network.clone(),
                    audio.clone(),
                )
                .await;

                {
                    let builder = &ui.borrow().builder;
                    let connect_failure_status: gtk::Label =
                        builder.object("connect_failure_status").unwrap();
                    connect_failure_status.set_text("Disconnected: the socket closed");
                }

                try_reconnect(
                    network.clone(),
                    audio.clone(),
                    network_event_sender.clone(),
                    ui.clone(),
                    user_id,
                    saved_volumes,
                    username,
                    server,
                )
                .await;
            }
            NetworkEvent::NetworkError => {
                let (users, user_list, user_id, saved_volumes, username, server) =
                    match ui.borrow_mut().use_state(|state| {
                        if let UiState::Main {
                            users,
                            user_list,
                            user_id,
                            saved_volumes,
                            username,
                            server,
                            ..
                        } = state
                        {
                            (
                                UiState::Disconnecting,
                                Some((users, user_list, user_id, saved_volumes, username, server)),
                            )
                        } else {
                            (state, None)
                        }
                    }) {
                        Some(m) => m,
                        None => continue,
                    };

                disconnect_from_server(
                    users,
                    user_list,
                    ui.clone(),
                    network.clone(),
                    audio.clone(),
                )
                .await;

                {
                    let builder = &ui.borrow().builder;
                    let connect_failure_status: gtk::Label =
                        builder.object("connect_failure_status").unwrap();
                    connect_failure_status.set_text("Disconnected: encountered network issue");
                }

                try_reconnect(
                    network.clone(),
                    audio.clone(),
                    network_event_sender.clone(),
                    ui.clone(),
                    user_id,
                    saved_volumes,
                    username,
                    server,
                )
                .await;
            }
            NetworkEvent::P2pEstablished {
                round_trip_latency,
                client_id,
                is_from_lead_client,
            } => {
                if let UiState::Main { users, .. } = ui.borrow_mut().state_mut() {
                    if let Some(user) = users.get_mut(&client_id) {
                        let mut user = user.borrow_mut();
                        if !user.is_peer_to_peer || is_from_lead_client {
                            user.is_peer_to_peer = true;
                            user.network_latency = round_trip_latency;
                            user.update_latency_text();
                        }
                    }
                }
            }
            NetworkEvent::Packet(packet) => {
                let result = async {
                    match packet.kind() {
                        UserJoined::KIND_ID => {
                            let mut ui_ref = ui.borrow_mut();
                            let UiState::Main {
                                users,
                                user_list,
                                client_id,
                                round_trip_latency,
                                saved_volumes,
                                ..
                            } = ui_ref.state_mut()
                            else {
                                return Ok(());
                            };
                            let packet: Packet<UserJoined> = packet.finish_parse()?;
                            audio
                                .send(AudioCommand::NewClient(packet.body.user.client_id))
                                .unwrap();
                            let glib_context = glib::MainContext::default();
                            glib_context.spawn_local(attempt_p2p(
                                network.clone(),
                                network_event_sender.clone(),
                                *client_id,
                                P2pDestination {
                                    client_id: packet.body.user.client_id,
                                    local_udp_addr: packet.body.user.local_udp_addr,
                                    public_udp_addr: packet.body.user.public_udp_addr,
                                },
                                false,
                            ));

                            let one_way_latency = packet
                                .body
                                .user
                                .round_trip_latency
                                .saturating_add(*round_trip_latency)
                                / 2;

                            let volume = if let Some(volume) =
                                saved_volumes.get(&packet.body.user.user_id)
                            {
                                audio
                                    .send({
                                        AudioCommand::ChangeClientVolume(
                                            packet.body.user.client_id,
                                            *volume,
                                        )
                                    })
                                    .unwrap();
                                *volume
                            } else {
                                100u16
                            };

                            let (row_widget, latency_widget) =
                                UiUser::create_widgets(&packet.body.user.name, *round_trip_latency);
                            let user = UiUser {
                                client_id: packet.body.user.client_id,
                                user_id: packet.body.user.user_id,
                                volume,
                                is_current_user: false,
                                network_latency: one_way_latency,
                                is_peer_to_peer: false,
                                root_widget: row_widget,
                                latency_widget,
                                is_speaking: false,
                                speaking_duration: Duration::ZERO,
                            };
                            user_list.append(&user.root_widget);
                            let user = Rc::new(RefCell::new(user));
                            UiUser::attach_signal_handlers(user.clone(), audio.clone(), ui.clone());
                            users.insert(packet.body.user.client_id, user);
                            audio.send(AudioCommand::UserJoined).unwrap();
                        }
                        UserLeft::KIND_ID => {
                            let mut ui_ref = ui.borrow_mut();
                            let UiState::Main {
                                users, user_list, ..
                            } = ui_ref.state_mut()
                            else {
                                return Ok(());
                            };
                            let packet: Packet<UserLeft> = packet.finish_parse()?;
                            audio
                                .send(AudioCommand::DropClient(packet.body.client_id))
                                .unwrap();

                            network.command(NetworkCommand::RemoveP2pClient {
                                client_id: packet.body.client_id,
                            });

                            let user = users.remove(&packet.body.client_id).unwrap();
                            user_list.remove(&user.borrow().root_widget.parent().unwrap());
                            audio.send(AudioCommand::UserLeft).unwrap();
                        }
                        ErrorResponse::KIND_ID => {
                            let packet: Packet<ErrorResponse> = packet.finish_parse()?;
                            let Some((users, user_list, user_id, saved_volumes, username, server)) =
                                ui.borrow_mut().use_state(|state| {
                                    if let UiState::Main {
                                        users,
                                        user_list,
                                        user_id,
                                        saved_volumes,
                                        username,
                                        server,
                                        ..
                                    } = state
                                    {
                                        (
                                            UiState::Disconnecting,
                                            Some((
                                                users,
                                                user_list,
                                                user_id,
                                                saved_volumes,
                                                username,
                                                server,
                                            )),
                                        )
                                    } else {
                                        (state, None)
                                    }
                                })
                            else {
                                return Ok(());
                            };

                            disconnect_from_server(
                                users,
                                user_list,
                                ui.clone(),
                                network.clone(),
                                audio.clone(),
                            )
                            .await;

                            {
                                let builder = &ui.borrow().builder;
                                let connect_failure_status: gtk::Label =
                                    builder.object("connect_failure_status").unwrap();
                                connect_failure_status.set_text(&format!(
                                    "Disconnected: an error occurred: {:?}",
                                    packet.body
                                ));
                            }

                            try_reconnect(
                                network.clone(),
                                audio.clone(),
                                network_event_sender.clone(),
                                ui.clone(),
                                user_id,
                                saved_volumes,
                                username,
                                server,
                            )
                            .await;
                        }
                        _ => (),
                    }

                    Ok::<(), CommsNetworkError>(())
                }
                .await;
                if result.is_err() {
                    let builder = &ui.borrow().builder;
                    let connect_failure_status: gtk::Label =
                        builder.object("connect_failure_status").unwrap();
                    connect_failure_status.set_text("Disconnected: encountered network issue");
                }
            }
        }
    }
}

async fn handle_ui_commands(
    receiver: async_channel::Receiver<UICommand>,
    ui: Rc<RefCell<UiModel>>,
) {
    futures::pin_mut!(receiver);
    while let Some(event) = receiver.next().await {
        match event {
            UICommand::UserIsSpeaking {
                client_id,
                duration,
            } => {
                if let UiState::Main { users, .. } = ui.borrow().state() {
                    if let Some(user_rc) = users.get(&client_id) {
                        let mut user = user_rc.borrow_mut();
                        if !user.is_speaking {
                            user.is_speaking = true;
                            user.speaking_duration = duration;
                            if let Some(p) = user.root_widget.parent() {
                                p.add_css_class("active")
                            }
                            let glib_context = glib::MainContext::default();
                            let user_rc = user_rc.clone();
                            glib_context.spawn_local(async move {
                                loop {
                                    let timeout = {
                                        let mut user = user_rc.borrow_mut();
                                        let timeout = user.speaking_duration;
                                        user.speaking_duration = Duration::ZERO;
                                        timeout
                                    };
                                    Delay::new(timeout).await;

                                    if timeout == Duration::ZERO {
                                        let mut user = user_rc.borrow_mut();
                                        user.is_speaking = false;
                                        if let Some(p) = user.root_widget.parent() {
                                            p.remove_css_class("active")
                                        }
                                        break;
                                    }
                                }
                            });
                        } else {
                            user.speaking_duration += duration;
                        }
                    }
                }
            }
            UICommand::CurrentUserStartedSpeaking => {
                if let UiState::Main {
                    client_id, users, ..
                } = ui.borrow().state()
                {
                    if let Some(user_rc) = users.get(client_id) {
                        let mut user = user_rc.borrow_mut();
                        user.is_speaking = true;
                        if let Some(p) = user.root_widget.parent() {
                            p.add_css_class("active")
                        }
                    }
                }
            }
            UICommand::CurrentUserStoppedSpeaking => {
                if let UiState::Main {
                    client_id, users, ..
                } = ui.borrow().state()
                {
                    if let Some(user_rc) = users.get(client_id) {
                        let mut user = user_rc.borrow_mut();
                        user.is_speaking = false;
                        if let Some(p) = user.root_widget.parent() {
                            p.remove_css_class("active")
                        }
                    }
                }
            }
            UICommand::AudioIPCInitStatus(audio_status) => {
                if let AudioIPCInitStatus::Error(e) = audio_status {
                    let error_message = match e {
                        EventLoopCreationError::InputDeviceError(m) => {
                            format!("Failed to set up microphone: {m}")
                        }
                        EventLoopCreationError::OutputDeviceError(m) => {
                            format!("Failed to set up headphones/speakers: {m}")
                        }
                    };

                    let app = ui.borrow().app.clone();
                    let window = ui.borrow().window.clone();
                    let dialog = gtk::Dialog::with_buttons(
                        Some(&error_message),
                        Some(&window),
                        DialogFlags::USE_HEADER_BAR.union(DialogFlags::DESTROY_WITH_PARENT),
                        &[("_Close", ResponseType::Close)],
                    );
                    dialog.connect_response(move |_, _| {
                        app.quit();
                    });
                    dialog.show();
                    return;
                }
            }
            UICommand::DevicesFound { .. } => (),
        }
    }
}

fn gtk_start(f: impl FnOnce(&adw::Application) + 'static) {
    let application =
        adw::Application::new(Some("ca.mitchhentges.comms"), ApplicationFlags::NON_UNIQUE);

    let o = RefCell::new(Some(f));
    application.connect_startup(move |a| {
        let mut o = o.borrow_mut();
        if let Some(f) = o.take() {
            f(a);
        }
    });
    application.connect_activate(|_| {});
    application.run();
}

pub fn run(
    network: NetworkHandle,
    network_event_receiver: futures::channel::mpsc::Receiver<NetworkEvent>,
    network_event_sender: Sender<NetworkEvent>,
    ui_receiver: async_channel::Receiver<UICommand>,
    audio: AudioHandle,
) {
    let project_dirs = ProjectDirs::from("", "Comms", "Comms").unwrap();
    let initial_local_storage: Option<LocalStorage> =
        std::fs::read_to_string(project_dirs.data_dir().join("local_storage"))
            .ok()
            .and_then(|storage_string| serde_json::from_str(&storage_string).ok());

    let initial_local_storage = initial_local_storage.unwrap_or(LocalStorage {
        username: std::env::var_os(if cfg!(windows) { "USERNAME" } else { "USER" })
            .map(|v| v.to_string_lossy().to_string())
            .unwrap_or_default(),
        server: "getcomms.app".to_string(),
        user_id: Uuid::new_v4(),
        volumes: HashMap::new(),
    });

    let server_parameter = std::env::args()
        .nth(1)
        .map(|p| p.replace("comms:", "").replace('/', ""));

    if let Some(initial_main_volume) = initial_local_storage
        .volumes
        .get(&initial_local_storage.user_id)
    {
        audio
            .send(AudioCommand::ChangeMainVolume(*initial_main_volume))
            .unwrap();
    }

    gtk_start(move |app| {
        let css_provider = gtk::CssProvider::new();
        css_provider.load_from_data(
            RESOURCES_DIR
                .get_file("theme.css")
                .unwrap()
                .contents_utf8()
                .unwrap(),
        );

        gtk::style_context_add_provider_for_display(
            &Display::default().unwrap(),
            &css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let window = gtk::ApplicationWindow::new(app);
        let disconnect_button = gtk::Button::builder().label("Disconnect").build();

        let disconnect_button_revealer = gtk::Revealer::builder()
            .transition_type(RevealerTransitionType::Crossfade)
            .transition_duration(100)
            .child(&disconnect_button)
            .reveal_child(false)
            .build();

        let update_button_progress_box = gtk::Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(0)
            .build();
        let update_button_label = gtk::Label::new(Some("🎉"));
        update_button_progress_box.append(&update_button_label);

        let update_progress_css_provider = gtk::CssProvider::new();
        StyleContext::add_provider(
            &update_button_progress_box.style_context(),
            &update_progress_css_provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );

        let header_bar = adw::HeaderBar::new();
        header_bar.pack_start(&disconnect_button_revealer);
        window.set_titlebar(Some(&header_bar));

        // Audio thread is expecting this to be provided.
        audio.send(AudioCommand::ListDevices).unwrap();

        let stack = gtk::Stack::new();
        stack.set_visible(true);
        let raw_app_ui = RESOURCES_DIR
            .get_file("app.ui")
            .unwrap()
            .contents_utf8()
            .unwrap();
        let builder = gtk::Builder::from_string(raw_app_ui);
        let ui = Rc::new(RefCell::new(UiModel {
            state: Some(UiState::ChooseServer {
                user_id: initial_local_storage.user_id,
                saved_volumes: initial_local_storage.volumes,
            }),
            pane_stack: stack.clone(),
            disconnect_button_revealer,
            app: app.clone(),
            window: window.clone(),
            builder: builder.clone(),
        }));

        let glib_context = glib::MainContext::default();
        glib_context.spawn_local(handle_network_events(
            network_event_receiver,
            network_event_sender.clone(),
            audio.clone(),
            network.clone(),
            ui.clone(),
        ));
        glib_context.spawn_local(handle_ui_commands(ui_receiver, ui.clone()));
        #[cfg(windows)]
        {
            let update_button = gtk::Button::builder()
                .css_classes(vec![
                    "update_button".to_string(),
                    "suggested-action".to_string(),
                ])
                .has_tooltip(true)
                .tooltip_text("Update is available")
                .child(&update_button_progress_box)
                .overflow(gtk::Overflow::Hidden)
                .build();
            let update_button_revealer = gtk::Revealer::builder()
                .transition_type(RevealerTransitionType::Crossfade)
                .transition_duration(100)
                .child(&update_button)
                .reveal_child(false)
                .build();
            glib_context.spawn_local(check_for_updates(
                project_dirs,
                header_bar,
                update_button_revealer,
                update_button,
                update_button_label,
                update_progress_css_provider,
            ));
        }

        window.set_title(Some("Comms"));
        stack.set_transition_duration(100);

        // --- Setup for connect-to-server state

        let connect_progress: gtk::ProgressBar = builder.object("connect_progress").unwrap();
        connect_progress.add_css_class("osd");
        connect_progress.add_css_class("horizontal");

        let server_entry: adw::EntryRow = builder.object("server_entry").unwrap();
        server_entry.set_text(&server_parameter.unwrap_or(initial_local_storage.server));

        let username_entry: adw::EntryRow = builder.object("username_entry").unwrap();
        username_entry.set_text(&initial_local_storage.username);

        let submit_connection_form = {
            let glib_context = glib_context.clone();
            let network = network.clone();
            let audio = audio.clone();
            let ui = ui.clone();
            Rc::new(RefCell::new(move || {
                let Some(UiState::ChooseServer { .. }) = ui.borrow().state else {
                    return;
                };

                glib_context.spawn_local(try_connect(
                    audio.clone(),
                    network.clone(),
                    network_event_sender.clone(),
                    ui.clone(),
                ));
            }))
        };

        let connect_button: gtk::Button = builder.object("connect_button").unwrap();
        connect_button.set_overflow(gtk::Overflow::Hidden);
        window.set_default_widget(Some(&connect_button));
        {
            let submit_connection_form = submit_connection_form.clone();
            connect_button.connect_clicked(move |_| {
                let submit_fn = &mut *submit_connection_form.borrow_mut();
                submit_fn();
            });
        }
        if let Some(timestamp) = BUILD_TIMESTAMP {
            connect_button
                .set_tooltip_text(Some(&format!("Build {timestamp} ({})", comms_version())));
            connect_button.set_has_tooltip(true);
        }

        let username_entry: adw::EntryRow = builder.object("username_entry").unwrap();
        {
            let submit_connection_form = submit_connection_form.clone();
            username_entry.connect_activate(move |_| {
                let submit_fn = &mut *submit_connection_form.borrow_mut();
                submit_fn();
            });
        }

        let server_entry: adw::EntryRow = builder.object("server_entry").unwrap();
        server_entry.connect_activate(move |_| {
            let submit_fn = &mut *submit_connection_form.borrow_mut();
            submit_fn();
        });
        let connect_form: gtk::Box = builder.object("connect_form").unwrap();
        stack.add_named(&connect_form, Some("connect"));

        // --- Setup for main state
        disconnect_button.connect_clicked(move |_| {
            let ui = ui.clone();
            let network = network.clone();
            let audio = audio.clone();
            glib_context.spawn_local(async move {
                let (users, user_list, user_id, saved_volumes) =
                    match ui.borrow_mut().use_state(|state| {
                        if let UiState::Main {
                            users,
                            user_list,
                            user_id,
                            saved_volumes,
                            ..
                        } = state
                        {
                            (
                                UiState::Disconnecting,
                                Some((users, user_list, user_id, saved_volumes)),
                            )
                        } else {
                            (state, None)
                        }
                    }) {
                        Some(m) => m,
                        None => return,
                    };

                disconnect_from_server(users, user_list, ui.clone(), network, audio).await;

                ui.borrow_mut().set_state(UiState::ChooseServer {
                    user_id,
                    saved_volumes,
                });
            });
        });

        let user_list_root: gtk::ScrolledWindow = builder.object("user_list_root").unwrap();
        stack.add_named(&user_list_root, Some("user_list"));

        window.set_child(Some(&stack));
        #[cfg(not(windows))]
        window.show();

        #[cfg(windows)]
        if let Some(warp_window) = std::env::var_os("WARP_WINDOW_HANDLE")
            .as_ref()
            .and_then(|h| h.to_str())
        {
            use gdk4_win32::Win32Surface;
            use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
            use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
            use windows::Win32::UI::WindowsAndMessaging::WM_CLOSE;
            use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWMINNOACTIVE};

            let warp_window = warp_window.parse().unwrap();
            let handler_id = Rc::new(RefCell::new(None::<SignalHandlerId>));
            *handler_id.borrow_mut() = {
                let handler_id = handler_id.clone();
                Some(window.connect_realize(move |window| {
                    if let Some(signal_id) = handler_id.take() {
                        window.disconnect(signal_id);
                    }

                    let native = window.native().unwrap();
                    let surface = native.surface();
                    let surface: Win32Surface = surface.downcast().unwrap();
                    let app_window: HWND = HWND(surface.handle().0);
                    unsafe { ShowWindow(app_window, SW_SHOWMINNOACTIVE) };

                    unsafe {
                        SendMessageW(HWND(warp_window), WM_CLOSE, WPARAM(0), LPARAM(0));
                    };

                    window.show();
                }))
            };
            WidgetExt::realize(&window);
        } else {
            window.show();
        }
    });
}

async fn update_local_storage(f: impl FnOnce(&mut LocalStorage)) {
    let local_storage_dir = if let Some(dirs) =
        ProjectDirs::from("", "Comms", "Comms").map(|dirs| dirs.data_dir().join("local_storage"))
    {
        dirs
    } else {
        return;
    };

    let mut storage: LocalStorage = if let Some(storage) =
        std::fs::read_to_string(&local_storage_dir)
            .ok()
            .and_then(|storage_string| serde_json::from_str(&storage_string).ok())
    {
        storage
    } else {
        return;
    };

    f(&mut storage);

    if let Ok(raw_storage) = serde_json::to_string(&storage) {
        let _ = async_std::fs::write(&local_storage_dir, raw_storage).await;
    }
}

async fn update_saved_volumes(volumes: HashMap<UserId, u16>) {
    update_local_storage(|storage| {
        storage.volumes = volumes;
    })
    .await;
}
