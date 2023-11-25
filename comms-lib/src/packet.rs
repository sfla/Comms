use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::time::Duration;
use uuid::Uuid;

pub const PROTOCOL_VERSION: ProtocolVersion = 2;

pub type ProtocolVersion = u32;
pub type UserId = Uuid;
pub type ClientId = u16;
pub type PacketKind = u8;
pub type PacketId = NonZeroU64;
pub type AudioIndex = NonZeroU64;

#[derive(Debug, Serialize, Deserialize)]
pub struct Packet<T: PacketBody> {
    pub id: PacketId,
    pub reply_to: Option<PacketId>,
    pub body: T,
}

impl<T: PacketBody> Packet<T> {
    pub fn new(id: PacketId, reply_to: Option<PacketId>, body: T) -> Self {
        Packet { id, reply_to, body }
    }
}

pub trait PacketBody: Serialize {
    const KIND_ID: PacketKind;
    fn kind_id() -> PacketKind {
        Self::KIND_ID
    }
}

#[derive(Serialize, Deserialize)]
pub struct AssociateUdp {
    pub client_id: ClientId,
    pub local_udp_addr: SocketAddr,
}

impl PacketBody for AssociateUdp {
    const KIND_ID: PacketKind = 0u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AssociateUdpAccepted;

impl PacketBody for AssociateUdpAccepted {
    const KIND_ID: PacketKind = 1u8;
}

#[derive(Serialize, Deserialize)]
pub struct FinishConnect {
    pub name: String,
    pub user_id: UserId,
}

impl PacketBody for FinishConnect {
    const KIND_ID: PacketKind = 2u8;
}

#[derive(Serialize, Deserialize, Debug)]
pub struct AudioFromClient {
    pub index: AudioIndex,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
}

impl PacketBody for AudioFromClient {
    const KIND_ID: PacketKind = 3u8;
}

#[derive(Serialize, Deserialize)]
pub struct KeepAlive;

impl PacketBody for KeepAlive {
    const KIND_ID: PacketKind = 4u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AudioToClient {
    pub index: AudioIndex,
    pub author: ClientId,
    #[serde(with = "serde_bytes")]
    pub bytes: Vec<u8>,
    pub from_client_peer_to_peer: bool,
}

impl PacketBody for AudioToClient {
    const KIND_ID: PacketKind = 5u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct NetworkUser {
    pub client_id: ClientId,
    pub user_id: UserId,
    pub name: String,
    pub local_udp_addr: SocketAddr,
    pub public_udp_addr: SocketAddr,
    pub round_trip_latency: Duration,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FinishConnectAccepted {
    pub users: Vec<NetworkUser>,
    pub round_trip_latency: Duration,
}

impl PacketBody for FinishConnectAccepted {
    const KIND_ID: PacketKind = 6u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct P2pSynchronize();

impl PacketBody for P2pSynchronize {
    const KIND_ID: PacketKind = 12u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct P2pAcknowledge();

impl PacketBody for P2pAcknowledge {
    const KIND_ID: PacketKind = 13u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct P2pAcknowledgeBack {
    pub client_id: ClientId,
    pub round_trip_latency: Duration,
    pub is_from_lead_client: bool,
}

impl PacketBody for P2pAcknowledgeBack {
    const KIND_ID: PacketKind = 14u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct P2pEstablished {
    pub source_client_id: ClientId,
}

impl PacketBody for P2pEstablished {
    const KIND_ID: PacketKind = 15u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ErrorResponse {
    AssociateUdpFirst,
    UnexpectedPacket(PacketId),
}

impl PacketBody for ErrorResponse {
    const KIND_ID: PacketKind = 7u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub client_id: ClientId,
    pub protocol_version: ProtocolVersion,
}

impl PacketBody for Hello {
    const KIND_ID: PacketKind = 8u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserJoined {
    pub user: NetworkUser,
}

impl PacketBody for UserJoined {
    const KIND_ID: PacketKind = 9u8;
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UserLeft {
    pub client_id: ClientId,
}

impl PacketBody for UserLeft {
    const KIND_ID: PacketKind = 10u8;
}

#[derive(Debug, Serialize, Deserialize, Eq, PartialEq)]
pub enum DisconnectReason {
    Leaving,
    MalformedData,
    IncompatibleClientVersion(ProtocolVersion),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Disconnect {
    pub reason: DisconnectReason,
}

impl PacketBody for Disconnect {
    const KIND_ID: PacketKind = 11u8;
}
