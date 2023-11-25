use std::fmt::{Debug, Error, Formatter};
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_std::net::ToSocketAddrs;
use async_std::task::sleep;
use futures::channel::mpsc::Sender;
use futures::channel::oneshot;
use futures::future::Either;
use futures::pin_mut;
use futures::{future, SinkExt};
use futures_timer::Delay;
use serde::de::DeserializeOwned;
use serde::Serialize;

use comms_lib::network::CommsNetworkError;
use comms_lib::packet;
use comms_lib::packet::{
    AssociateUdp, AssociateUdpAccepted, ClientId, FinishConnect, FinishConnectAccepted, KeepAlive,
    NetworkUser, P2pAcknowledgeBack, Packet, PacketBody, UserId,
};
use comms_lib::serialization::{IncomingPacketFrame, OutgoingPacketFrame};

use crate::network::ipc::{ConnectError, NetworkCommand, NetworkEvent, NetworkHandle, Protocol};

pub async fn connect(
    handle: &NetworkHandle,
    destination: String,
    username: String,
    user_id: UserId,
) -> Result<(ClientId, Vec<NetworkUser>, Duration), ConnectError> {
    let result = async {
        let destination_with_port = format!("{destination}:18847");
        let destination = match destination_with_port.to_socket_addrs().await {
            Ok(mut addresses) => {
                if let Some(address) = addresses.next() {
                    address
                } else {
                    return Err(ConnectError::DestinationUnusable);
                }
            }
            Err(_) => {
                return Err(ConnectError::DestinationUnusable);
            }
        };

        let connect_success = NetworkFuture::new(handle, |s| NetworkCommand::Connect {
            destination,
            send_response: s,
        })
        .await?;

        if connect_success.hello.protocol_version != packet::PROTOCOL_VERSION {
            return Err(ConnectError::IncompatibleServerProtocolVersion(
                connect_success.hello.protocol_version,
            ));
        }

        let _: Packet<AssociateUdpAccepted> = request_expect(
            handle,
            Protocol::Udp,
            None,
            AssociateUdp {
                client_id: connect_success.hello.client_id,
                local_udp_addr: connect_success.udp_local_addr,
            },
        )
        .await?;

        let connect_accepted: Packet<FinishConnectAccepted> = request_expect(
            handle,
            Protocol::Tcp,
            None,
            FinishConnect {
                name: username,
                user_id,
            },
        )
        .await?;
        Ok((
            connect_success.hello.client_id,
            connect_accepted.body.users,
            connect_accepted.body.round_trip_latency,
        ))
    }
    .await;
    if result.is_err() {
        let reason = match result {
            Err(ConnectError::IncompatibleServerProtocolVersion(_)) => Some(
                packet::DisconnectReason::IncompatibleClientVersion(packet::PROTOCOL_VERSION),
            ),
            _ => None,
        };
        disconnect(handle, reason).await.unwrap();
    }
    result
}

#[derive(Debug)]
pub struct P2pDestination {
    pub client_id: ClientId,
    pub local_udp_addr: SocketAddr,
    pub public_udp_addr: SocketAddr,
}

pub async fn attempt_p2p_without_timeout(
    handle: NetworkHandle,
    local_client_id: ClientId,
    destination: P2pDestination,
    is_lead_client: bool,
) -> Result<(Duration, SocketAddr), ()> {
    println!("Attempting P2P connection with {destination:?}!");
    let timeout = Delay::new(Duration::from_secs(3));

    let acknowledge_local = NetworkFuture::new(&handle, |s| NetworkCommand::AttemptP2pSync {
        udp_addr: destination.local_udp_addr,
        send_response: s,
    });
    let acknowledge_public = NetworkFuture::new(&handle, |s| NetworkCommand::AttemptP2pSync {
        udp_addr: destination.public_udp_addr,
        send_response: s,
    });

    pin_mut!(acknowledge_local);
    pin_mut!(acknowledge_public);
    let (result, addr) = match future::select(
        timeout,
        future::select(acknowledge_local, acknowledge_public),
    )
    .await
    {
        Either::Left(_) => {
            eprintln!("P2P connection attempt timed out");
            return Err(());
        }
        Either::Right((Either::Left((packet_result, _)), _)) => {
            (packet_result, destination.local_udp_addr)
        }
        Either::Right((Either::Right((packet_result, _)), _)) => {
            (packet_result, destination.public_udp_addr)
        }
    };
    let (round_trip_latency, packet) = match result {
        Ok(o) => o,
        Err(e) => {
            eprintln!("Failed to parse P2P packet: {e:?}");
            return Err(());
        }
    };
    if let Err(e) = send(
        &handle,
        Protocol::UdpP2p(addr),
        Some(packet.id),
        P2pAcknowledgeBack {
            round_trip_latency,
            is_from_lead_client: is_lead_client,
            client_id: local_client_id,
        },
    ) {
        eprintln!("Failed to send P2P packet: {e:?}");
        return Err(());
    }

    Ok((round_trip_latency, addr))
}

pub async fn attempt_p2p(
    network_handle: NetworkHandle,
    mut network_event_sender: Sender<NetworkEvent>,
    local_client_id: ClientId,
    destination: P2pDestination,
    is_lead_client: bool,
) {
    let destination_client_id = destination.client_id;
    let public_udp_addr = destination.public_udp_addr;
    match attempt_p2p_without_timeout(
        network_handle.clone(),
        local_client_id,
        destination,
        is_lead_client,
    )
    .await
    {
        Ok((latency, addr)) => {
            println!(
                "P2P connection with {} started successfully ({:.1}ms RT)",
                addr,
                (latency.as_micros() as f64) / 1000.0
            );
            network_handle.command(NetworkCommand::RegisterP2pClient {
                client_id: destination_client_id,
                udp_addr: addr,
            });
            network_event_sender
                .try_send(NetworkEvent::P2pEstablished {
                    client_id: destination_client_id,
                    round_trip_latency: latency,
                    is_from_lead_client: is_lead_client,
                })
                .unwrap();
        }
        Err(()) => {
            eprintln!("Failed to start P2P connection with {public_udp_addr}");
        }
    }
}

pub async fn keep_alive(handle: NetworkHandle, mut network_event_sender: Sender<NetworkEvent>) {
    loop {
        sleep(Duration::from_secs(10)).await;

        match send(&handle, Protocol::Tcp, None, KeepAlive) {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{e:?}");
                network_event_sender
                    .send(NetworkEvent::NetworkError)
                    .await
                    .unwrap();
                return;
            }
        };
        if let Err(e) = send(&handle, Protocol::Udp, None, KeepAlive) {
            eprintln!("{e:?}");
            network_event_sender
                .send(NetworkEvent::NetworkError)
                .await
                .unwrap();
            return;
        }
        handle.command(NetworkCommand::KeepAliveP2pClients);
    }
}

pub async fn disconnect(
    handle: &NetworkHandle,
    reason: Option<packet::DisconnectReason>,
) -> Result<(), CommsNetworkError> {
    if let Some(reason) = reason {
        send(handle, Protocol::Tcp, None, packet::Disconnect { reason })?;
        // Give time to send packet before closing TCP stream.
        sleep(Duration::from_millis(10)).await;
    }
    NetworkFuture::new(handle, |s| NetworkCommand::Disconnect { send_response: s }).await;
    Ok(())
}

pub async fn request_any<Req>(
    handle: &NetworkHandle,
    protocol: Protocol,
    reply_to: Option<NonZeroU64>,
    request: Req,
) -> Result<IncomingPacketFrame, CommsNetworkError>
where
    Req: PacketBody,
{
    let packet = OutgoingPacketFrame::from_packet(reply_to, &request)?;
    Ok(NetworkFuture::new(handle, |s| NetworkCommand::Packet {
        protocol,
        packet,
        send_response: Some(s),
    })
    .await)
}

pub async fn request_expect<Req, Res>(
    handle: &NetworkHandle,
    packet_type: Protocol,
    reply_to: Option<NonZeroU64>,
    request: Req,
) -> Result<Packet<Res>, RequestError>
where
    Req: PacketBody + Serialize,
    Res: PacketBody + DeserializeOwned,
{
    let parsed_packet = request_any(handle, packet_type, reply_to, request)
        .await
        .map_err(|_| RequestError::SerializeError)?;
    if parsed_packet.kind() != Res::KIND_ID {
        return Err(RequestError::InvalidResponsePacket);
    }
    parsed_packet
        .finish_parse()
        .map_err(|_| RequestError::DeserializeError)
}

pub fn send<Req>(
    handle: &NetworkHandle,
    packet_type: Protocol,
    reply_to: Option<NonZeroU64>,
    request: Req,
) -> Result<(), CommsNetworkError>
where
    Req: PacketBody + Serialize,
{
    let packet = OutgoingPacketFrame::from_packet(reply_to, &request)?;
    handle.command(NetworkCommand::Packet {
        protocol: packet_type,
        packet,
        send_response: None,
    });
    Ok(())
}

#[derive(Debug)]
pub enum RequestError {
    SerializeError,
    DeserializeError,
    InvalidResponsePacket,
}

pub struct SendResponse<T> {
    sender: oneshot::Sender<T>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl<T: Debug> Debug for SendResponse<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        f.write_str("<->")
    }
}

impl<T> SendResponse<T> {
    pub fn done(self, response: T) {
        let _ = self.sender.send(response); // Timeouts can cause the result of a future to become irrelevant
        let mut waker = self.waker.lock().unwrap();
        if let Some(waker) = waker.take() {
            waker.wake()
        }
    }
}

struct NetworkFuture<T> {
    receiver: Arc<Mutex<oneshot::Receiver<T>>>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl<T> Future for NetworkFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut receiver = self.receiver.lock().unwrap();
        match receiver.try_recv() {
            Ok(Some(response)) => Poll::Ready(response),
            Ok(None) => {
                let mut waker = self.waker.lock().unwrap();
                *waker = Some(cx.waker().clone());
                Poll::Pending
            }
            Err(_) => {
                panic!("NetworkFuture was cancelled (other side didn't send response?)");
            }
        }
    }
}

impl<T> NetworkFuture<T> {
    pub fn new(
        handle: &NetworkHandle,
        create_command: impl FnOnce(SendResponse<T>) -> NetworkCommand,
    ) -> Self {
        let (sender, receiver) = oneshot::channel();
        let waker = Arc::new(Mutex::new(None));

        handle.command(create_command(SendResponse {
            sender,
            waker: waker.clone(),
        }));

        NetworkFuture {
            receiver: Arc::new(Mutex::new(receiver)),
            waker,
        }
    }
}
