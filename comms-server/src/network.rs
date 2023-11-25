use crate::client::{AnyClientRef, Clients};
use crate::future::SendResponse;
use crate::task::FutureError;
use comms_lib::network::{Protocol, ServerUdpPacket, UdpConnection};
use comms_lib::packet::{ClientId, PacketBody, PacketId};
use comms_lib::serialization::OutgoingPacketFrame;
use mio::Waker;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Arc;

pub struct OutgoingQueue {
    receiver: Receiver<(
        Protocol,
        ClientId,
        OutgoingPacketFrame,
        Option<SendResponse>,
    )>,
}

impl OutgoingQueue {
    pub fn sort_from_handle(
        &mut self,
        clients: &mut Clients,
        udp: &mut UdpConnection<ServerUdpPacket>,
    ) {
        // Sort the incoming queue of packets into their respective socket buckets
        for (protocol, dest, packet, send_response) in self.receiver.try_iter() {
            let connection = match clients.client_by_id(&dest) {
                Some(AnyClientRef::Established(c)) => &mut c.connection,
                Some(AnyClientRef::Connecting(c)) => &mut c.connection,
                _ => continue,
            };
            let packet_id = connection.next_packet_id();
            if let Some(send_response) = send_response {
                connection.reply_map.insert(packet_id, send_response);
            }

            match protocol {
                Protocol::Udp => {
                    let  udp_addr = match clients.client_by_id(&dest) {
                        Some(AnyClientRef::Established(c)) => {
                            c.public_udp_addr
                        },
                        Some(AnyClientRef::Connecting(c)) => {
                            c.udp_association.as_ref().unwrap_or_else(|| panic!("Attempting to send UDP packet of kind \"{}\" to client that hasn't associated yet", packet.kind)).public_udp_addr
                        },
                        _ => continue,
                    };

                    udp.push_outgoing_without_send(ServerUdpPacket::new(
                        udp_addr,
                        packet.into_raw(packet_id).unwrap(),
                    ));
                }
                Protocol::Tcp => {
                    let bytes = packet.into_raw(packet_id).unwrap();
                    connection.tcp.push_outgoing_without_send(&bytes);
                }
            }
        }
    }

    pub fn attempt_write_all(
        &mut self,
        udp: &mut UdpConnection<ServerUdpPacket>,
        clients: &mut Clients,
    ) -> Vec<ClientId> {
        udp.write_until_blocked().unwrap();

        let mut failures = Vec::new();
        for (client_id, connection) in clients
            .established
            .iter_mut()
            .map(|(client_id, client)| (client_id, &mut client.connection))
            .chain(
                clients
                    .connecting
                    .iter_mut()
                    .map(|(client_id, client)| (client_id, &mut client.connection)),
            )
        {
            if connection.tcp.write_until_blocked().is_err() {
                failures.push(*client_id);
            }
        }

        failures
    }
}

#[derive(Clone)]
pub struct NetworkHandle {
    sender: SyncSender<(
        Protocol,
        ClientId,
        OutgoingPacketFrame,
        Option<SendResponse>,
    )>,
    waker: Arc<Waker>,
}

impl NetworkHandle {
    pub fn send(&self, protocol: Protocol, dest: ClientId, packet: OutgoingPacketFrame) {
        self.sender.send((protocol, dest, packet, None)).unwrap();
        self.waker.wake().unwrap();
    }

    pub fn send_expect(
        &self,
        protocol: Protocol,
        dest: ClientId,
        packet: OutgoingPacketFrame,
        send_response: SendResponse,
    ) {
        self.sender
            .send((protocol, dest, packet, Some(send_response)))
            .unwrap();
        self.waker.wake().unwrap();
    }
}

pub fn queue_and_handle(waker: Arc<Waker>) -> (NetworkHandle, OutgoingQueue) {
    let (sender, receiver) = sync_channel(10_000);
    (NetworkHandle { sender, waker }, OutgoingQueue { receiver })
}

pub enum Target {
    One(ClientId),
    Many(Vec<ClientId>),
}

pub struct SendPacket {
    destination: Target,
    protocol: Protocol,
    packet: OutgoingPacketFrame,
}

impl SendPacket {
    fn new<B>(
        protocol: Protocol,
        destination: Target,
        reply_to: Option<PacketId>,
        packet: B,
    ) -> Result<Self, FutureError>
    where
        B: PacketBody,
    {
        let packet = OutgoingPacketFrame::from_packet(reply_to, &packet)
            .map_err(|_| FutureError::SerializeFailed { kind: B::KIND_ID })?;
        Ok(Self {
            destination,
            protocol,
            packet,
        })
    }

    pub fn udp<B>(
        destination: Target,
        reply_to: Option<PacketId>,
        packet: B,
    ) -> Result<Self, FutureError>
    where
        B: PacketBody,
    {
        Self::new(Protocol::Udp, destination, reply_to, packet)
    }

    pub fn tcp<B>(
        destination: Target,
        reply_to: Option<PacketId>,
        packet: B,
    ) -> Result<Self, FutureError>
    where
        B: PacketBody,
    {
        Self::new(Protocol::Tcp, destination, reply_to, packet)
    }

    pub async fn send(self, handle: &NetworkHandle) {
        let protocol = self.protocol;
        match self.destination {
            Target::One(dest) => handle.send(protocol, dest, self.packet),
            Target::Many(clients) => {
                for client in clients {
                    handle.send(protocol.clone(), client, self.packet.clone())
                }
            }
        }
    }
}
