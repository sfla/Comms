use crate::future::SendResponse;
use comms_lib::network::TcpConnection;
use comms_lib::packet::{ClientId, PacketId, UserId};
use comms_lib::serialization::OutgoingPacketFrame;
use mio::net::TcpStream;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::time::{Duration, Instant};

pub struct NextPacketId(PacketId);

impl NextPacketId {
    pub fn new() -> Self {
        Self(PacketId::new(1).unwrap())
    }

    pub fn next(&mut self) -> PacketId {
        let packet_id = self.0;
        self.0 = PacketId::new(self.0.get() + 1).unwrap();
        packet_id
    }
}

pub struct Connection {
    next_packet_id: NextPacketId,
    pub tcp: TcpConnection,
    pub tcp_writable: bool,
    pub outgoing_queue: VecDeque<(ClientId, OutgoingPacketFrame)>,
    pub partially_sent_packet: Option<Vec<u8>>,
    pub reply_map: HashMap<NonZeroU64, SendResponse>,
}

impl Connection {
    pub fn new(stream: TcpStream) -> Self {
        Connection {
            tcp: TcpConnection::new(stream).0,
            tcp_writable: false,
            next_packet_id: NextPacketId::new(),
            outgoing_queue: VecDeque::new(),
            partially_sent_packet: None,
            reply_map: HashMap::new(),
        }
    }

    pub fn next_packet_id(&mut self) -> PacketId {
        self.next_packet_id.next()
    }
}

pub struct UdpAssociation {
    pub local_udp_addr: SocketAddr,
    pub public_udp_addr: SocketAddr,
    pub udp_accepted_sent_at: Instant,
}

pub struct ConnectingClient {
    pub connection: Connection,
    pub udp_association: Option<UdpAssociation>,
}

pub struct Client {
    pub connection: Connection,
    pub local_udp_addr: SocketAddr,
    pub public_udp_addr: SocketAddr,
    pub username: String,
    pub user_id: UserId,
    pub round_trip_latency: Duration,
    pub p2p_connections: Vec<ClientId>,
}

#[derive(Default)]
pub struct Clients {
    pub established: HashMap<ClientId, Client>,
    pub connecting: HashMap<ClientId, ConnectingClient>,
}

pub enum AnyClientRef<'a> {
    Established(&'a mut Client),
    Connecting(&'a mut ConnectingClient),
}

impl Clients {
    pub fn id_by_public_socket_addr(&self, addr: &SocketAddr) -> Option<ClientId> {
        self.established
            .iter()
            .find(|(_, c)| &c.public_udp_addr == addr)
            .map(|(id, _)| *id)
            .or_else(|| {
                self.connecting
                    .iter()
                    .find(|(_, c)| {
                        c.udp_association.as_ref().map(|a| a.public_udp_addr) == Some(*addr)
                    })
                    .map(|(id, _)| *id)
            })
    }

    pub fn connection_by_id(&mut self, client_id: &ClientId) -> Option<&mut Connection> {
        let established_connection = self
            .established
            .get_mut(client_id)
            .map(|c| &mut c.connection);
        let connecting = self
            .connecting
            .get_mut(client_id)
            .map(|c| &mut c.connection);
        established_connection.or(connecting)
    }

    pub fn client_by_id(&mut self, client_id: &ClientId) -> Option<AnyClientRef> {
        match self.established.get_mut(client_id) {
            Some(c) => Some(AnyClientRef::Established(c)),
            None => self
                .connecting
                .get_mut(client_id)
                .map(AnyClientRef::Connecting),
        }
    }

    pub fn remove_client(&mut self, client_id: &ClientId) {
        let _ = self
            .established
            .remove(client_id)
            .map(|_| ())
            .or_else(|| self.connecting.remove(client_id).map(|_| ()));
        for client in self.established.values_mut() {
            client.p2p_connections.retain(|id| id != client_id);
        }
    }
}
