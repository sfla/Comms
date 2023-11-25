use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::sync::mpsc::{Receiver, RecvError, SendError};
use std::time::{Duration, Instant};

use mio::net::TcpStream;
use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};

use comms_lib::network::{
    ClientUdpPacket, ClientUdpReadIter, CommsNetworkError, TcpConnection, UdpConnection,
};
use comms_lib::packet::{
    AudioFromClient, AudioToClient, ClientId, Hello, KeepAlive, P2pAcknowledge, P2pAcknowledgeBack,
    P2pSynchronize, Packet, PacketBody,
};
use comms_lib::serialization::{IncomingPacketFrame, IncomingPacketFrameRef, OutgoingPacketFrame};

use crate::audio::ipc::{AudioCommand, AudioHandle};
use crate::network::future::SendResponse;
use crate::network::ipc::{
    CommandQueue, ConnectError, ConnectSuccess, NetworkCommand, NetworkEvent, Protocol,
};
use crate::NetworkHandle;
use futures::channel::mpsc::Sender;

pub mod future;
pub mod ipc;

type P2pReplyTo = (
    Instant,
    SendResponse<Result<(Duration, Packet<P2pAcknowledge>), CommsNetworkError>>,
);

pub struct Connection {
    udp: UdpConnection<ClientUdpPacket>,
    udp_token: Token,
    tcp: TcpConnection,
    tcp_token: Token,
    local_addr: SocketAddr,
    server_addr: SocketAddr,
    known_peers: HashMap<ClientId, SocketAddr>,
    known_sockets: HashMap<SocketAddr, ClientId>,
    next_packet_id: NonZeroU64,
    client_id: Option<ClientId>,
    reply_map: HashMap<
        NonZeroU64,
        (
            Option<SendResponse<IncomingPacketFrame>>,
            Option<P2pReplyTo>,
        ),
    >,
    connect_response: Option<(
        SocketAddr,
        SendResponse<Result<ConnectSuccess, ConnectError>>,
    )>,
}

impl Connection {
    fn new(
        socket: UdpSocket,
        udp_token: Token,
        tcp: TcpConnection,
        tcp_token: Token,
        server_addr: SocketAddr,
        local_addr: SocketAddr,
    ) -> Self {
        Connection {
            udp: UdpConnection::new(socket),
            udp_token,
            tcp,
            tcp_token,
            local_addr,
            server_addr,
            known_peers: HashMap::new(),
            known_sockets: HashMap::new(),
            next_packet_id: NonZeroU64::new(1).unwrap(),
            client_id: None,
            reply_map: HashMap::new(),
            connect_response: None,
        }
    }

    fn next_packet_id(&mut self) -> NonZeroU64 {
        let packet_id = self.next_packet_id;
        self.next_packet_id = NonZeroU64::new(packet_id.get() + 1).unwrap();
        packet_id
    }
}

pub struct WorkerInit {
    audio_handle: AudioHandle,
    network_handle: NetworkHandle,
    event_sender: Sender<NetworkEvent>,
    command_receiver: Receiver<NetworkCommand>,
    poll: Poll,
    command_token: Token,
    next_poll_token: NextPollToken,
}

struct NextPollToken(usize);

impl NextPollToken {
    fn new() -> NextPollToken {
        NextPollToken(0)
    }

    fn create(&mut self) -> Token {
        let next_id = self.0;
        self.0 += 1;
        Token(next_id)
    }
}

pub struct Worker {
    connection: Option<Connection>,
}

fn handle_udp_packet(
    parsed_packet: IncomingPacketFrame,
    addr: SocketAddr,
    reply_map: &mut HashMap<
        NonZeroU64,
        (
            Option<SendResponse<IncomingPacketFrame>>,
            Option<P2pReplyTo>,
        ),
    >,
    audio_handle: &mut AudioHandle,
    event_sender: &mut Sender<NetworkEvent>,
    network_handle: &NetworkHandle,
) -> Result<(), CommsNetworkError> {
    if let Some(reply_to) = parsed_packet.header.reply_to {
        let send_response_tuple = reply_map.remove(&reply_to);
        if let Some((Some(sender), None)) = send_response_tuple {
            sender.done(parsed_packet);
            return Ok(());
        }
        if let Some((None, Some((start, sender)))) = send_response_tuple {
            println!("Found reply_to for P2P sync");
            match parsed_packet.finish_parse() {
                Ok(p) => sender.done(Ok((start.elapsed(), p))),
                Err(e) => sender.done(Err(e.into())),
            }
            return Ok(());
        }
    }

    let kind = parsed_packet.header.kind;
    match kind {
        AudioToClient::KIND_ID => {
            let audio_to_client: Packet<AudioToClient> = parsed_packet.finish_parse()?;
            audio_handle.send_audio(
                audio_to_client.body.index,
                audio_to_client.body.author,
                audio_to_client.body.bytes,
                audio_to_client.body.from_client_peer_to_peer,
            );
        }
        P2pSynchronize::KIND_ID => network_handle.command(NetworkCommand::Packet {
            protocol: Protocol::UdpP2p(addr),
            packet: OutgoingPacketFrame::from_packet(
                Some(parsed_packet.header.id),
                &P2pAcknowledge(),
            )?,
            send_response: None,
        }),
        P2pAcknowledgeBack::KIND_ID => {
            let ack_back: Packet<P2pAcknowledgeBack> = parsed_packet.finish_parse()?;
            println!(
                "P2P connection with {} started successfully ({:.1}ms RT) via ack-back",
                addr,
                (ack_back.body.round_trip_latency.as_micros() as f64) / 1000.0,
            );
            network_handle.command(NetworkCommand::RegisterP2pClient {
                client_id: ack_back.body.client_id,
                udp_addr: addr,
            });
            event_sender
                .try_send(NetworkEvent::P2pEstablished {
                    client_id: ack_back.body.client_id,
                    round_trip_latency: ack_back.body.round_trip_latency,
                    is_from_lead_client: ack_back.body.is_from_lead_client,
                })
                .unwrap();
        }
        _ => {
            event_sender
                .try_send(NetworkEvent::Packet(parsed_packet))
                .unwrap();
        }
    }
    Ok(())
}

fn handle_tcp_packet(
    stream_packet: IncomingPacketFrameRef,
    reply_map: &mut HashMap<
        NonZeroU64,
        (
            Option<SendResponse<IncomingPacketFrame>>,
            Option<P2pReplyTo>,
        ),
    >,
    client_id: &mut Option<ClientId>,
    await_hello: &mut Option<(
        SocketAddr,
        SendResponse<Result<ConnectSuccess, ConnectError>>,
    )>,
    event_sender: &mut Sender<NetworkEvent>,
) -> Result<(), CommsNetworkError> {
    if let Some(reply_to) = stream_packet.header.reply_to {
        if let Some((Some(sender), None)) = reply_map.remove(&reply_to) {
            sender.done(stream_packet.into());
            return Ok(());
        }
    }

    let kind = stream_packet.header.kind;
    if kind == Hello::KIND_ID && await_hello.is_some() {
        if let Some((udp_local_addr, send_response)) = await_hello.take() {
            let hello: Hello = stream_packet.finish_parse()?.body;
            *client_id = Some(hello.client_id);
            send_response.done(Ok(ConnectSuccess {
                hello,
                udp_local_addr,
            }));
        }
    } else {
        event_sender
            .try_send(NetworkEvent::Packet(stream_packet.into()))
            .unwrap();
    }
    Ok(())
}

fn connect(
    poll: &Poll,
    destination: SocketAddr,
    next_poll_token: &mut NextPollToken,
) -> Result<Connection, ConnectError> {
    let mut tcp = TcpStream::connect(destination)?;
    let mut udp = UdpSocket::bind("0.0.0.0:0".parse().unwrap())?;

    let local_addr = {
        let resolve_local_addr_udp = UdpSocket::bind("0.0.0.0:0".parse().unwrap())?;
        resolve_local_addr_udp.connect(destination)?;
        let mut local_addr = resolve_local_addr_udp.local_addr()?;
        local_addr.set_port(udp.local_addr()?.port());
        local_addr
    };

    let udp_token = next_poll_token.create();
    let tcp_token = next_poll_token.create();
    poll.registry()
        .register(&mut udp, udp_token, Interest::READABLE | Interest::WRITABLE)
        .unwrap();
    poll.registry()
        .register(&mut tcp, tcp_token, Interest::READABLE | Interest::WRITABLE)
        .unwrap();

    let (stream, error) = TcpConnection::new(tcp);
    if let Some(e) = error {
        panic!("Expected to be able to set TCP timeout {:?}", e);
    }
    Ok(Connection::new(
        udp,
        udp_token,
        stream,
        tcp_token,
        destination,
        local_addr,
    ))
}

impl Worker {
    pub fn new() -> io::Result<Self> {
        Ok(Worker { connection: None })
    }

    pub fn run(mut self, worker_init: WorkerInit) -> NetworkError {
        let command_queue = CommandQueue::new(worker_init.command_receiver);
        let mut audio_handle = worker_init.audio_handle;
        let network_handle = worker_init.network_handle;
        let mut event_sender = worker_init.event_sender;

        let mut next_poll_token = worker_init.next_poll_token;
        let mut poll = worker_init.poll;
        let command_token = worker_init.command_token;

        let mut events = Events::with_capacity(1024);

        loop {
            if let Err(e) = poll.poll(&mut events, None) {
                return e.into();
            }
            for event in events.iter() {
                let token = event.token();
                if token == command_token && event.is_readable() {
                    while let Some(command) = command_queue.next() {
                        match command {
                            NetworkCommand::Packet {
                                protocol,
                                packet: outgoing,
                                send_response,
                            } => {
                                if let Some(connection) = &mut self.connection {
                                    let id = connection.next_packet_id();

                                    if let Ok(raw) = outgoing.into_raw(id) {
                                        if let Some(send_response) = send_response {
                                            connection
                                                .reply_map
                                                .insert(id, (Some(send_response), None));
                                        }

                                        match protocol {
                                            Protocol::Tcp => {
                                                if let Err(e) = connection.tcp.push_outgoing(&raw) {
                                                    println!("Failed to send TCP data: {e:?}");
                                                    if let Some((_, send_response)) =
                                                        connection.connect_response.take()
                                                    {
                                                        send_response
                                                            .done(Err(ConnectError::NetworkError))
                                                    } else {
                                                        event_sender
                                                            .try_send(NetworkEvent::NetworkError)
                                                            .unwrap();
                                                    }
                                                }
                                            }
                                            Protocol::Udp => {
                                                match connection.udp.push_outgoing(
                                                    ClientUdpPacket::new(
                                                        raw,
                                                        connection.server_addr,
                                                    ),
                                                ) {
                                                    Ok(_) => (),
                                                    Err(e)
                                                        if e.kind()
                                                            == io::ErrorKind::ConnectionRefused =>
                                                    {
                                                        if let Some((_, send_response)) =
                                                            connection.connect_response.take()
                                                        {
                                                            send_response.done(Err(
                                                                ConnectError::NetworkError,
                                                            ));
                                                        }
                                                    }
                                                    Err(e) => return e.into(),
                                                }
                                            }
                                            Protocol::UdpP2p(addr) => {
                                                match connection
                                                    .udp
                                                    .push_outgoing(ClientUdpPacket::new(raw, addr))
                                                {
                                                    Ok(_) => (),
                                                    // UDP P2P connections aren't mandatory. If
                                                    // the connection is refused, ignore it.
                                                    // TODO maybe disconnect the P2P connection?
                                                    Err(e)
                                                        if e.kind()
                                                            == io::ErrorKind::ConnectionRefused => {
                                                    }
                                                    Err(e) => return e.into(),
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            NetworkCommand::SendAudio(chunk) => {
                                if let Some(connection) = &mut self.connection {
                                    let mut packets = vec![(
                                        connection.server_addr,
                                        OutgoingPacketFrame::from_packet(
                                            None,
                                            &AudioFromClient {
                                                index: chunk.index,
                                                bytes: chunk.bytes.clone(),
                                            },
                                        )
                                        .unwrap(),
                                    )];
                                    if let Some(client_id) = connection.client_id {
                                        for peer in connection.known_peers.values() {
                                            packets.push((
                                                *peer,
                                                OutgoingPacketFrame::from_packet(
                                                    None,
                                                    &AudioToClient {
                                                        index: chunk.index,
                                                        bytes: chunk.bytes.clone(),
                                                        author: client_id,
                                                        from_client_peer_to_peer: true,
                                                    },
                                                )
                                                .unwrap(),
                                            ));
                                        }
                                    }

                                    for (destination, packet) in packets {
                                        let id = connection.next_packet_id();
                                        if let Ok(raw) = packet.into_raw(id) {
                                            match connection.udp.push_outgoing(
                                                ClientUdpPacket::new(raw, destination),
                                            ) {
                                                Ok(_) => (),
                                                Err(e)
                                                    if e.kind()
                                                        == io::ErrorKind::ConnectionRefused =>
                                                {
                                                    if let Some((_, send_response)) =
                                                        connection.connect_response.take()
                                                    {
                                                        send_response
                                                            .done(Err(ConnectError::NetworkError));
                                                    }
                                                }
                                                Err(e) => return e.into(),
                                            }
                                        }
                                    }
                                }
                            }
                            NetworkCommand::Connect {
                                destination,
                                send_response,
                            } => {
                                if self.connection.is_some() {
                                    send_response.done(Err(ConnectError::AlreadyConnected));
                                    continue;
                                }

                                match connect(&poll, destination, &mut next_poll_token) {
                                    Ok(mut c) => {
                                        c.connect_response = Some((c.local_addr, send_response));
                                        self.connection = Some(c)
                                    }
                                    Err(e) => send_response.done(Err(e)),
                                }
                            }
                            NetworkCommand::Disconnect { send_response } => {
                                self.connection.take();
                                send_response.done(());
                            }
                            NetworkCommand::AttemptP2pSync {
                                udp_addr,
                                send_response,
                            } => {
                                let packet = P2pSynchronize();
                                match OutgoingPacketFrame::from_packet(None, &packet) {
                                    Ok(outgoing) => {
                                        if let Some(connection) = &mut self.connection {
                                            let id = connection.next_packet_id();

                                            if let Ok(raw) = outgoing.into_raw(id) {
                                                connection.reply_map.insert(
                                                    id,
                                                    (None, Some((Instant::now(), send_response))),
                                                );

                                                match connection.udp.push_outgoing(
                                                    ClientUdpPacket::new(raw, udp_addr),
                                                ) {
                                                    Ok(_) => (),
                                                    Err(e)
                                                        if e.kind()
                                                            == io::ErrorKind::ConnectionRefused =>
                                                    {
                                                        // TODO: disconnect the P2P connection
                                                    }
                                                    Err(e) => return e.into(),
                                                }
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        send_response.done(Err(e.into()));
                                    }
                                }
                            }
                            NetworkCommand::KeepAliveP2pClients => {
                                if let Some(connection) = &mut self.connection {
                                    let id = connection.next_packet_id();
                                    for udp_addr in connection.known_sockets.keys() {
                                        let packet = KeepAlive;
                                        let Ok(outgoing) =
                                            OutgoingPacketFrame::from_packet(None, &packet)
                                        else {
                                            continue;
                                        };

                                        let Ok(raw) = outgoing.into_raw(id) else {
                                            continue;
                                        };

                                        let _ = connection
                                            .udp
                                            .push_outgoing(ClientUdpPacket::new(raw, *udp_addr));
                                    }
                                }
                            }
                            NetworkCommand::RegisterP2pClient {
                                client_id,
                                udp_addr,
                            } => {
                                if let Some(connection) = &mut self.connection {
                                    connection.known_peers.insert(client_id, udp_addr);
                                    connection.known_sockets.insert(udp_addr, client_id);
                                }
                            }
                            NetworkCommand::RemoveP2pClient { client_id } => {
                                if let Some(connection) = &mut self.connection {
                                    connection.known_peers.remove(&client_id);
                                    connection.known_sockets.retain(|_, v| *v != client_id);
                                }
                            }
                        }
                    }
                } else if let Some(connection) = &mut self.connection {
                    if token == connection.udp_token {
                        if event.is_readable() {
                            let reply_map = &mut connection.reply_map;
                            match connection
                                .udp
                                .read_new_packets::<'_, ClientUdpReadIter>()
                                .try_process(
                                    &connection.server_addr,
                                    &connection.known_sockets,
                                    |packet, addr| {
                                        handle_udp_packet(
                                            packet,
                                            addr,
                                            reply_map,
                                            &mut audio_handle,
                                            &mut event_sender,
                                            &network_handle,
                                        )
                                    },
                                ) {
                                Err(CommsNetworkError::Closed) => {
                                    event_sender.try_send(NetworkEvent::SocketClosed).unwrap();
                                }
                                Err(_) => {
                                    event_sender.try_send(NetworkEvent::NetworkError).unwrap();
                                }
                                Ok(_) => (),
                            }
                        }
                        if event.is_writable() && connection.udp.notify_writable().is_err() {
                            event_sender.try_send(NetworkEvent::NetworkError).unwrap();
                        }
                    } else if token == connection.tcp_token {
                        if event.is_readable() {
                            let reply_map = &mut connection.reply_map;
                            let connect_response = &mut connection.connect_response;
                            let client_id = &mut connection.client_id;

                            match connection.tcp.read_new_packets().try_process(|packet| {
                                handle_tcp_packet(
                                    packet,
                                    reply_map,
                                    client_id,
                                    connect_response,
                                    &mut event_sender,
                                )
                            }) {
                                Err(CommsNetworkError::Closed) => {
                                    event_sender.try_send(NetworkEvent::SocketClosed).unwrap();
                                }
                                Err(_) => {
                                    if let Some((_, send_response)) =
                                        connection.connect_response.take()
                                    {
                                        send_response.done(Err(ConnectError::NetworkError));
                                    } else {
                                        event_sender.try_send(NetworkEvent::NetworkError).unwrap();
                                    }
                                }
                                _ => {}
                            }
                        }
                        if event.is_writable() && connection.tcp.notify_writable().is_err() {
                            event_sender.try_send(NetworkEvent::NetworkError).unwrap();
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug)]
pub enum NetworkError {
    Io(Option<io::Error>),
    SendCommandToAudioThread,
    ReceiveFromAudioThread(RecvError),
}

impl Display for NetworkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkError::Io(None) => f.write_str("Io error (unclear which, though)"),
            NetworkError::Io(Some(e)) => e.fmt(f),
            NetworkError::SendCommandToAudioThread => {
                f.write_str("Failed to send command to audio thread")
            }
            NetworkError::ReceiveFromAudioThread(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for NetworkError {}

impl From<io::Error> for NetworkError {
    fn from(e: io::Error) -> Self {
        NetworkError::Io(Some(e))
    }
}

impl From<SendError<AudioCommand>> for NetworkError {
    fn from(_: SendError<AudioCommand>) -> Self {
        NetworkError::SendCommandToAudioThread
    }
}

impl From<RecvError> for NetworkError {
    fn from(e: RecvError) -> Self {
        NetworkError::ReceiveFromAudioThread(e)
    }
}

#[derive(Debug)]
pub enum SendToNetworkThreadError {
    Network(CommsNetworkError),
    ThreadSend,
    SetReadiness(io::Error),
}

impl Display for SendToNetworkThreadError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SendToNetworkThreadError::Network(e) => e.fmt(f),
            SendToNetworkThreadError::ThreadSend => f.write_str("Failed to send between threads"),
            SendToNetworkThreadError::SetReadiness(e) => e.fmt(f),
        }
    }
}

impl From<CommsNetworkError> for SendToNetworkThreadError {
    fn from(e: CommsNetworkError) -> Self {
        SendToNetworkThreadError::Network(e)
    }
}

impl From<SendError<NetworkCommand>> for SendToNetworkThreadError {
    fn from(_: SendError<NetworkCommand>) -> Self {
        SendToNetworkThreadError::ThreadSend
    }
}

impl From<io::Error> for SendToNetworkThreadError {
    fn from(e: io::Error) -> Self {
        SendToNetworkThreadError::SetReadiness(e)
    }
}
