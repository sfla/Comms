use std::fmt::{Display, Formatter};
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, mpsc};
use std::time::Duration;
use std::{fmt, io};

use mio::{Poll, Waker};

use comms_lib::network::CommsNetworkError;
use comms_lib::serialization::{IncomingPacketFrame, OutgoingPacketFrame};

use crate::audio::ipc::AudioHandle;
use crate::network::future::{RequestError, SendResponse};
use crate::network::{NextPollToken, WorkerInit};
use comms_lib::packet::{ClientId, Hello, P2pAcknowledge, Packet, ProtocolVersion};
use comms_lib::{OpusAudioFrame, RawAudioFrame};

#[derive(Debug)]
pub enum NetworkEvent {
    SocketClosed,
    NetworkError,
    Packet(IncomingPacketFrame),
    P2pEstablished {
        client_id: ClientId,
        round_trip_latency: Duration,
        is_from_lead_client: bool,
    },
}

pub struct ConnectSuccess {
    pub hello: Hello,
    pub udp_local_addr: SocketAddr,
}

#[derive(Debug)]
pub enum ConnectError {
    DestinationUnusable,
    NetworkError,
    AlreadyConnected,
    RequestError(RequestError),
    IncompatibleServerProtocolVersion(ProtocolVersion),
}

impl From<io::Error> for ConnectError {
    fn from(_: io::Error) -> Self {
        ConnectError::NetworkError
    }
}

impl From<RequestError> for ConnectError {
    fn from(e: RequestError) -> Self {
        ConnectError::RequestError(e)
    }
}

#[derive(Debug)]
pub enum Protocol {
    Tcp,
    Udp,
    UdpP2p(SocketAddr),
}

pub enum NetworkCommand {
    RegisterP2pClient {
        client_id: ClientId,
        udp_addr: SocketAddr,
    },
    RemoveP2pClient {
        client_id: ClientId,
    },
    Packet {
        protocol: Protocol,
        packet: OutgoingPacketFrame,
        send_response: Option<SendResponse<IncomingPacketFrame>>,
    },
    SendAudio(OpusAudioFrame),
    SendRawAudio(RawAudioFrame),
    RequestRecordRawOneMinute,
    Connect {
        destination: SocketAddr,
        send_response: SendResponse<Result<ConnectSuccess, ConnectError>>,
    },
    AttemptP2pSync {
        udp_addr: SocketAddr,
        send_response: SendResponse<Result<(Duration, Packet<P2pAcknowledge>), CommsNetworkError>>,
    },
    KeepAliveP2pClients,
    Disconnect {
        send_response: SendResponse<()>,
    },
}

impl Display for NetworkCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::Packet {
                protocol,
                packet: outgoing,
                ..
            } => f.write_str(&format!("{:?} Packet of kind {}", protocol, outgoing.kind)),
            Self::Connect { .. } => f.write_str("Connect"),
            Self::Disconnect { .. } => f.write_str("Disconnect"),
            Self::SendAudio { .. } => f.write_str("SendAudio"),
            Self::RegisterP2pClient { .. } => f.write_str("RegisterClient"),
            Self::RemoveP2pClient { .. } => f.write_str("RemoveP2pClient"),
            Self::AttemptP2pSync { .. } => f.write_str("AttemptP2pSync"),
            Self::KeepAliveP2pClients => f.write_str("KeepAliveP2pClients"),
            Self::SendRawAudio { .. } => f.write_str("SendRawAudio"),
            Self::RequestRecordRawOneMinute { .. } => f.write_str("RequestRecordRawOneMinute"),
        }
    }
}

#[derive(Clone)]
pub struct NetworkHandle {
    sender: Sender<NetworkCommand>,
    waker: Arc<Waker>,
}

impl NetworkHandle {
    pub fn new_handle_and_worker(
        audio_handle: AudioHandle,
    ) -> (
        Self,
        futures::channel::mpsc::Receiver<NetworkEvent>,
        futures::channel::mpsc::Sender<NetworkEvent>,
        WorkerInit,
    ) {
        let (event_sender, network_event_receiver) = futures::channel::mpsc::channel(32);
        let (command_sender, command_receiver) = mpsc::channel();

        let mut next_poll_token = NextPollToken::new();
        let poll = Poll::new().unwrap();

        let command_token = next_poll_token.create();
        let waker = Waker::new(poll.registry(), command_token).unwrap();

        let network_handle = Self {
            sender: command_sender,
            waker: Arc::new(waker),
        };

        (
            network_handle.clone(),
            network_event_receiver,
            event_sender.clone(),
            WorkerInit {
                audio_handle,
                network_handle,
                event_sender,
                command_receiver,
                poll,
                command_token,
                next_poll_token,
            },
        )
    }

    pub fn command(&self, command: NetworkCommand) {
        self.sender.send(command).unwrap();
        self.waker.wake().unwrap();
    }

    pub fn audio(&self, frame: OpusAudioFrame) -> Result<(), CommsNetworkError> {
        self.command(NetworkCommand::SendAudio(frame));
        Ok(())
    }

    pub fn raw_audio(&self, frame: RawAudioFrame) -> Result<(), CommsNetworkError> {
        self.command(NetworkCommand::SendRawAudio(frame));
        Ok(())
    }
}

pub struct CommandQueue {
    command_queue: Receiver<NetworkCommand>,
}

impl CommandQueue {
    pub(crate) fn new(command_queue: Receiver<NetworkCommand>) -> CommandQueue {
        CommandQueue { command_queue }
    }

    pub(crate) fn next(&self) -> Option<NetworkCommand> {
        self.command_queue.try_recv().ok()
    }
}
