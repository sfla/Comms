use crate::client::{Client, Clients, UdpAssociation};
use crate::keep_alive;
use crate::network::{NetworkHandle, SendPacket, Target};
use crate::task::{FutureResult, Spawner};
use async_std::fs::File;
use async_std::io::WriteExt;
use async_std::task::sleep;
use chrono::{DateTime, Utc};
use comms_lib::network::CommsNetworkError;
use comms_lib::packet::{
    AssociateUdp, AssociateUdpAccepted, AudioFromClient, AudioToClient, ClientId, Disconnect,
    DisconnectReason, ErrorResponse, FinishConnect, FinishConnectAccepted, KeepAlive, NetworkUser,
    P2pEstablished, Packet, PacketBody, PacketId, ProtocolVersion, RawAudio, RecordRawOneMinute,
    UserJoined, UserLeft,
};
use comms_lib::serialization::{IncomingPacketFrame, IncomingPacketFrameRef, SerializeError};
use futures::future;
use std::cell::RefCell;
use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

pub struct Context {
    pub clients: Rc<RefCell<Clients>>,
    pub client_id: ClientId,
    pub network_handle: NetworkHandle,
}

async fn associate_udp(
    udp_handle: NetworkHandle,
    clients: Rc<RefCell<Clients>>,
    public_addr: SocketAddr,
    packet: Packet<AssociateUdp>,
) -> FutureResult {
    let outgoing = {
        let mut clients = clients.borrow_mut();
        let client = match clients.connecting.get_mut(&packet.body.client_id) {
            Some(client) => client,
            None => return Ok(()),
        };
        client.udp_association = Some(UdpAssociation {
            local_udp_addr: packet.body.local_udp_addr,
            public_udp_addr: public_addr,
            udp_accepted_sent_at: Instant::now(),
        });
        SendPacket::udp(
            Target::One(packet.body.client_id),
            Some(packet.id),
            AssociateUdpAccepted,
        )?
    };
    outgoing.send(&udp_handle).await;
    Ok(())
}

async fn audio_from_client(context: Context, packet: Packet<AudioFromClient>) -> FutureResult {
    let client_ids = {
        let clients = context.clients.borrow();
        let mut excluded_client_ids = &Vec::new();
        if let Some(client) = clients.established.get(&context.client_id) {
            excluded_client_ids = &client.p2p_connections
        }
        clients
            .established
            .keys()
            .copied()
            .filter(|id| id != &context.client_id)
            .filter(|id| !excluded_client_ids.contains(id))
            .collect()
    };
    let outgoing = SendPacket::udp(
        Target::Many(client_ids),
        None,
        AudioToClient {
            index: packet.body.index,
            author: context.client_id,
            bytes: packet.body.bytes,
            from_client_peer_to_peer: false,
        },
    )?;
    outgoing.send(&context.network_handle).await;
    Ok(())
}

async fn p2p_established(context: Context, packet: Packet<P2pEstablished>) -> FutureResult {
    let mut clients = context.clients.borrow_mut();
    if let Some(client) = clients.established.get_mut(&packet.body.source_client_id) {
        client.p2p_connections.push(context.client_id);
    }
    Ok(())
}

async fn finish_connect(
    spawner: Rc<RefCell<Spawner>>,
    context: Context,
    packet: Packet<FinishConnect>,
) -> FutureResult {
    let packets = {
        let mut clients = context.clients.borrow_mut();
        let mut client = match clients.connecting.remove(&context.client_id) {
            Some(c) => c,
            None => return Ok(()),
        };

        match client.udp_association.take() {
            None => {
                clients.connecting.insert(context.client_id, client);
                vec![SendPacket::tcp(
                    Target::One(context.client_id),
                    Some(packet.id),
                    ErrorResponse::AssociateUdpFirst,
                )?]
            }
            Some(udp_association) => {
                let round_trip_latency = udp_association.udp_accepted_sent_at.elapsed();
                println!(
                    "[{}] \"{}\" has connected (round trip latency: {:.1}ms, local socket addr: {:?})",
                    &udp_association.public_udp_addr, &packet.body.name, (round_trip_latency.as_micros() as f64) / 1000.0, &udp_association.local_udp_addr
                );
                let other_clients = clients.established.keys().copied().collect();
                clients.established.insert(
                    context.client_id,
                    Client {
                        local_udp_addr: udp_association.local_udp_addr,
                        public_udp_addr: udp_association.public_udp_addr,
                        connection: client.connection,
                        username: packet.body.name.clone(),
                        user_id: packet.body.user_id,
                        round_trip_latency,
                        p2p_connections: Vec::new(),
                    },
                );

                spawner.borrow().spawn(keep_alive(
                    context.network_handle.clone(),
                    context.client_id,
                    context.clients.clone(),
                ));

                vec![
                    SendPacket::tcp(
                        Target::One(context.client_id),
                        Some(packet.id),
                        FinishConnectAccepted {
                            users: clients
                                .established
                                .iter()
                                .filter(|(id, _)| **id != context.client_id)
                                .map(|(id, c)| NetworkUser {
                                    client_id: *id,
                                    user_id: c.user_id,
                                    name: c.username.clone(),
                                    local_udp_addr: c.local_udp_addr,
                                    public_udp_addr: c.public_udp_addr,
                                    round_trip_latency: c.round_trip_latency,
                                })
                                .collect(),
                            round_trip_latency,
                        },
                    )?,
                    SendPacket::tcp(
                        Target::Many(other_clients),
                        None,
                        UserJoined {
                            user: NetworkUser {
                                name: packet.body.name,
                                user_id: packet.body.user_id,
                                client_id: context.client_id,
                                local_udp_addr: udp_association.local_udp_addr,
                                public_udp_addr: udp_association.public_udp_addr,
                                round_trip_latency,
                            },
                        },
                    )?,
                ]
            }
        }
    };

    future::join_all(
        packets
            .into_iter()
            .map(|packet| packet.send(&context.network_handle)),
    )
    .await;
    Ok(())
}

async fn receive_disconnect_packet(context: Context, packet: Packet<Disconnect>) -> FutureResult {
    close_connection(
        context,
        match packet.body.reason {
            DisconnectReason::Leaving => CloseConnectionReason::Leaving,
            DisconnectReason::MalformedData => CloseConnectionReason::MalformedData,
            DisconnectReason::IncompatibleClientVersion(v) => {
                CloseConnectionReason::IncompatibleClientVersion(v)
            }
        },
    )
    .await
}

async fn receive_record_raw_one_minute(
    spawner: Rc<RefCell<Spawner>>,
    context: Context,
) -> FutureResult {
    println!("Recording requested");

    let other_clients: Vec<_> = context
        .clients
        .borrow()
        .established
        .keys()
        .copied()
        .collect();

    {
        let mut clients = context.clients.borrow_mut();
        clients.raw_recordings = Some(
            other_clients
                .iter()
                .filter_map(|id| {
                    clients
                        .established
                        .get(id)
                        .map(|c| (*id, (c.username.clone(), Vec::new())))
                })
                .collect(),
        );
    }

    SendPacket::tcp(Target::Many(other_clients), None, RecordRawOneMinute)?
        .send(&context.network_handle)
        .await;

    spawner.borrow().spawn(async move {
        // Including latency, should be enough for all clients to send their 60 seconds of data.
        sleep(Duration::from_secs(60)).await;

        let Some(recordings) = context.clients.borrow_mut().raw_recordings.take() else {
            return Ok(());
        };

        let now = SystemTime::now();
        let datetime: DateTime<Utc> = now.into();

        future::join_all(recordings.values().map(|(name, samples)| async move {
            let filename = format!("{}-{}.dmp", datetime.format("%Y-%m-%d-%H.%M.%S"), name);
            let bytes: Vec<u8> = samples
                .iter()
                .flatten()
                .flat_map(|s| s.to_le_bytes())
                .collect();
            let path = Path::new(&filename);
            let mut file = File::create(path).await.unwrap();
            file.write_all(&bytes).await.unwrap();
        }))
        .await;
        Ok(())
    });

    Ok(())
}

async fn receive_raw_audio(context: Context, packet: Packet<RawAudio>) -> FutureResult {
    let mut clients = context.clients.borrow_mut();
    let Some(raw_recordings) = clients.raw_recordings.as_mut() else {
        return Ok(());
    };

    raw_recordings
        .entry(context.client_id)
        .and_modify(|(_, samples)| samples.push(packet.body.samples));

    Ok(())
}

async fn sent_wrong_packet(context: Context, packet_id: PacketId) -> FutureResult {
    let outgoing = SendPacket::tcp(
        Target::One(context.client_id),
        Some(packet_id),
        ErrorResponse::UnexpectedPacket(packet_id),
    )?;
    outgoing.send(&context.network_handle).await;
    Ok(())
}

pub async fn close_connection(context: Context, reason: CloseConnectionReason) -> FutureResult {
    if let Some((name, addr)) = {
        let clients = context.clients.borrow_mut();
        clients
            .established
            .get(&context.client_id)
            .map(|c| (c.username.clone(), c.public_udp_addr))
    } {
        println!(
            "[{}] \"{}\" has disconnected due to {:?}",
            &addr, &name, &reason
        );

        let remaining_client_ids = {
            let mut clients = context.clients.borrow_mut();
            clients.remove_client(&context.client_id);
            clients.established.keys().copied().collect()
        };

        let outgoing = SendPacket::tcp(
            Target::Many(remaining_client_ids),
            None,
            UserLeft {
                client_id: context.client_id,
            },
        )?;
        outgoing.send(&context.network_handle).await;
    }

    if reason == CloseConnectionReason::MalformedData {
        let outgoing = SendPacket::tcp(
            Target::One(context.client_id),
            None,
            Disconnect {
                reason: DisconnectReason::MalformedData,
            },
        )?;
        outgoing.send(&context.network_handle).await;
    }

    Ok(())
}

fn tcp_packet_to_endpoint(
    spawner: Rc<RefCell<Spawner>>,
    context: Context,
    packet: IncomingPacketFrameRef,
) -> Result<(), CommsNetworkError> {
    let kind = packet.header.kind;

    match kind {
        FinishConnect::KIND_ID => spawner.borrow().spawn(finish_connect(
            spawner.clone(),
            context,
            packet.finish_parse()?,
        )),
        Disconnect::KIND_ID => spawner
            .borrow()
            .spawn(receive_disconnect_packet(context, packet.finish_parse()?)),
        RecordRawOneMinute::KIND_ID => spawner
            .borrow()
            .spawn(receive_record_raw_one_minute(spawner.clone(), context)),
        RawAudio::KIND_ID => spawner
            .borrow()
            .spawn(receive_raw_audio(context, packet.finish_parse()?)),
        KeepAlive::KIND_ID => (),
        _ => spawner
            .borrow()
            .spawn(sent_wrong_packet(context, packet.header.id)),
    };
    Ok(())
}

fn udp_packet_to_endpoint(
    spawner: &Spawner,
    packet: IncomingPacketFrame,
    context: Context,
) -> Result<(), SerializeError> {
    match packet.kind() {
        AudioFromClient::KIND_ID => {
            spawner.spawn(audio_from_client(context, packet.finish_parse()?))
        }
        P2pEstablished::KIND_ID => spawner.spawn(p2p_established(context, packet.finish_parse()?)),
        KeepAlive::KIND_ID => (),
        _ => spawner.spawn(sent_wrong_packet(context, packet.header.id)),
    };
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub enum CloseConnectionReason {
    Leaving,
    SocketClosed,
    MalformedData,
    IncompatibleClientVersion(ProtocolVersion),
}

pub fn spawn_close_connection(
    client_id: ClientId,
    reason: CloseConnectionReason,
    network_handle: &NetworkHandle,
    clients: Rc<RefCell<Clients>>,
    spawner: &Spawner,
) {
    let context = Context {
        client_id,
        network_handle: network_handle.clone(),
        clients,
    };

    spawner.spawn(close_connection(context, reason));
}

#[derive(Debug)]
pub enum HandlePacketError {
    CommunicationFailure,
}

impl From<CommsNetworkError> for HandlePacketError {
    fn from(_: CommsNetworkError) -> Self {
        HandlePacketError::CommunicationFailure
    }
}

impl From<SerializeError> for HandlePacketError {
    fn from(_: SerializeError) -> Self {
        HandlePacketError::CommunicationFailure
    }
}

impl From<io::Error> for HandlePacketError {
    fn from(_: io::Error) -> Self {
        HandlePacketError::CommunicationFailure
    }
}

pub fn handle_udp_packet(
    addr: SocketAddr,
    packet: IncomingPacketFrame,
    clients: Rc<RefCell<Clients>>,
    spawner: &Spawner,
    network_handle: NetworkHandle,
) -> Result<(), SerializeError> {
    let kind = packet.header.kind;
    if kind == AssociateUdp::KIND_ID {
        // This is the only packet that doesn't need to be sent from a client
        spawner.spawn(associate_udp(
            network_handle,
            clients,
            addr,
            packet.finish_parse()?,
        ));
        return Ok(());
    }

    let client_id = {
        let clients = clients.borrow();
        match clients.id_by_public_socket_addr(&addr) {
            Some(client_id) => client_id,
            None => return Ok(()),
        }
    };

    if let Some(reply_to) = packet.header.reply_to {
        if let Some(connection) = clients.borrow_mut().connection_by_id(&client_id) {
            if let Some(send_response) = connection.reply_map.remove(&reply_to) {
                send_response.done(packet);
                return Ok(());
            }
        }
    }

    udp_packet_to_endpoint(
        spawner,
        packet,
        Context {
            clients,
            client_id,
            network_handle,
        },
    )
}

#[derive(Debug)]
pub enum StreamStatus {
    Normal,
    Closed,
    Nonexistent,
}

pub fn handle_tcp_stream(
    client_id: ClientId,
    clients: Rc<RefCell<Clients>>,
    spawner: Rc<RefCell<Spawner>>,
    network_handle: &NetworkHandle,
) -> Result<StreamStatus, HandlePacketError> {
    let clients_arc = clients.clone();
    let mut clients = clients.borrow_mut();
    let connection = match clients.connection_by_id(&client_id) {
        Some(connection) => connection,
        None => return Ok(StreamStatus::Nonexistent),
    };

    let reply_map = &mut connection.reply_map;
    let handle_packet = |packet: IncomingPacketFrameRef| {
        let context = Context {
            client_id,
            clients: clients_arc.clone(),
            network_handle: network_handle.clone(),
        };

        if let Some(reply_to) = packet.header.reply_to {
            if let Some(send_response) = reply_map.remove(&reply_to) {
                send_response.done(IncomingPacketFrame::from(packet));
                return Ok(());
            }
        }

        tcp_packet_to_endpoint(spawner.clone(), context, packet)
    };

    match connection.tcp.read_new_packets().try_process(handle_packet) {
        Err(CommsNetworkError::Closed) => return Ok(StreamStatus::Closed),
        Err(_) => return Err(HandlePacketError::CommunicationFailure),
        _ => {}
    }

    Ok(StreamStatus::Normal)
}
