use crate::packet::ClientId;
use crate::serialization::{IncomingPacketFrame, IncomingPacketFrameRef, SerializeError};
use crate::{PacketHeader, PACKET_HEADER_SIZE};
use mio::net::{TcpStream, UdpSocket};
use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::{Read, Write};
use std::net::SocketAddr;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Protocol {
    Tcp,
    Udp,
}

const TCP_STREAM_BUFFER_SIZE: usize = 48_000;

pub struct TcpConnection {
    stream: TcpStream,
    is_writable: bool,
    incoming_stream: Box<[u8; TCP_STREAM_BUFFER_SIZE]>,
    incoming_stream_start: usize,
    incoming_stream_end: usize,
    outgoing_stream: Box<[u8; TCP_STREAM_BUFFER_SIZE]>,
    outgoing_stream_start: usize,
    outgoing_stream_end: usize,
    next_incoming_header: Option<PacketHeader>,
}

impl TcpConnection {
    pub fn new(stream: TcpStream) -> (Self, Option<io::Error>) {
        #[cfg(all(target_os = "linux", not(debug_assertions)))]
        let error = {
            use std::os::fd::AsRawFd;
            use std::os::fd::BorrowedFd;
            use std::time::Duration;

            let fd = unsafe { BorrowedFd::borrow_raw(stream.as_raw_fd()) };
            let raw_socket = socket2::SockRef::from(&fd);
            raw_socket
                .set_tcp_user_timeout(Some(Duration::from_secs(6)))
                .err()
        };
        #[cfg(any(not(target_os = "linux"), debug_assertions))]
        let error: Option<io::Error> = None;
        (
            Self {
                stream,
                is_writable: false,
                incoming_stream: Box::new([0; TCP_STREAM_BUFFER_SIZE]),
                incoming_stream_start: 0,
                incoming_stream_end: 0,
                outgoing_stream: Box::new([0; TCP_STREAM_BUFFER_SIZE]),
                outgoing_stream_start: 0,
                outgoing_stream_end: 0,
                next_incoming_header: None,
            },
            error,
        )
    }

    pub fn write_until_blocked(&mut self) -> Result<(), io::Error> {
        if !self.is_writable {
            return Ok(());
        }

        while self.outgoing_stream_start != self.outgoing_stream_end {
            let buffer = if self.outgoing_stream_end > self.outgoing_stream_start {
                &self.outgoing_stream[self.outgoing_stream_start..self.outgoing_stream_end]
            } else {
                &self.outgoing_stream[self.outgoing_stream_start..TCP_STREAM_BUFFER_SIZE]
            };
            match self.stream.write(buffer) {
                Ok(count) => {
                    self.outgoing_stream_start =
                        (self.outgoing_stream_start + count) % TCP_STREAM_BUFFER_SIZE;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.is_writable = false;
                    return Ok(());
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn push_outgoing(&mut self, bytes: &[u8]) -> Result<(), io::Error> {
        self.push_outgoing_without_send(bytes);
        self.write_until_blocked()
    }

    pub fn push_outgoing_without_send(&mut self, bytes: &[u8]) {
        let end_gap = TCP_STREAM_BUFFER_SIZE - self.outgoing_stream_end;
        if end_gap > bytes.len() {
            self.outgoing_stream[self.outgoing_stream_end..self.outgoing_stream_end + bytes.len()]
                .copy_from_slice(bytes);
            self.outgoing_stream_end =
                (self.outgoing_stream_end + bytes.len()) % TCP_STREAM_BUFFER_SIZE;
        } else {
            let remainder = bytes.len() - end_gap;
            self.outgoing_stream[self.outgoing_stream_end..].copy_from_slice(&bytes[..end_gap]);
            self.outgoing_stream[..remainder].copy_from_slice(&bytes[end_gap..]);
            self.outgoing_stream_end = remainder;
        }
    }

    pub fn notify_writable(&mut self) -> Result<(), io::Error> {
        self.is_writable = true;
        self.write_until_blocked()
    }

    fn read_from_network(&mut self) -> Result<(), CommsNetworkError> {
        loop {
            let mut buf = [0; 4096];
            match self.stream.read(&mut buf) {
                Ok(0) => return Err(CommsNetworkError::Closed),
                Ok(amount) => {
                    let buf = &buf[..amount];
                    let end_gap = TCP_STREAM_BUFFER_SIZE - self.incoming_stream_end;
                    if end_gap > amount {
                        self.incoming_stream
                            [self.incoming_stream_end..self.incoming_stream_end + amount]
                            .copy_from_slice(buf);
                        self.incoming_stream_end =
                            (self.incoming_stream_end + amount) % TCP_STREAM_BUFFER_SIZE;
                    } else {
                        let remainder = amount - end_gap;
                        self.incoming_stream[self.incoming_stream_end..]
                            .copy_from_slice(&buf[..end_gap]);
                        self.incoming_stream[..remainder].copy_from_slice(&buf[end_gap..]);
                        self.incoming_stream_end = remainder;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    return Err(CommsNetworkError::Closed)
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn read_new_packets(&mut self) -> TcpReadIter {
        let read_result = self.read_from_network();
        TcpReadIter {
            read_error: read_result.err(),
            incoming_stream: &*self.incoming_stream,
            incoming_stream_start: &mut self.incoming_stream_start,
            incoming_stream_end: &mut self.incoming_stream_end,
            next_incoming_header: &mut self.next_incoming_header,
        }
    }

    pub fn peer_addr(&self) -> std::io::Result<SocketAddr> {
        self.stream.peer_addr()
    }
}

pub trait UdpPacket {
    type IncomingPacket;

    fn send(&self, socket: &mut UdpSocket) -> io::Result<usize>;
    fn from_recv(socket: &mut UdpSocket) -> io::Result<Option<Self>>
    where
        Self: Sized;
    fn into_incoming_packet(self) -> (Result<Self::IncomingPacket, SerializeError>, SocketAddr);
}

pub struct ServerUdpPacket {
    client_addr: SocketAddr,
    buffer: Vec<u8>,
}

impl ServerUdpPacket {
    pub fn new(client_addr: SocketAddr, buffer: Vec<u8>) -> Self {
        Self {
            client_addr,
            buffer,
        }
    }
}

impl UdpPacket for ServerUdpPacket {
    type IncomingPacket = IncomingPacketFrame;

    fn send(&self, socket: &mut UdpSocket) -> io::Result<usize> {
        socket.send_to(&self.buffer, self.client_addr)
    }

    fn from_recv(socket: &mut UdpSocket) -> io::Result<Option<Self>> {
        let mut buffer = [0u8; 65536];
        let (size, address) = socket.recv_from(&mut buffer)?;
        if size == 0 {
            return Ok(None);
        }

        Ok(Some(Self {
            client_addr: address,
            buffer: buffer[..size].to_vec(),
        }))
    }

    fn into_incoming_packet(self) -> (Result<Self::IncomingPacket, SerializeError>, SocketAddr) {
        let incoming_packet_result = IncomingPacketFrame::from_bytes(&self.buffer);
        (incoming_packet_result, self.client_addr)
    }
}

pub struct ClientUdpPacket {
    client_addr: SocketAddr,
    buffer: Vec<u8>,
}

impl ClientUdpPacket {
    pub fn new(buffer: Vec<u8>, client_addr: SocketAddr) -> Self {
        Self {
            buffer,
            client_addr,
        }
    }
}

impl UdpPacket for ClientUdpPacket {
    type IncomingPacket = IncomingPacketFrame;

    fn send(&self, socket: &mut UdpSocket) -> io::Result<usize> {
        socket.send_to(&self.buffer, self.client_addr)
    }

    fn from_recv(socket: &mut UdpSocket) -> io::Result<Option<Self>> {
        let mut buffer = [0u8; 65536];
        let (size, address) = socket.recv_from(&mut buffer)?;

        if size == 0 {
            return Ok(None);
        }

        Ok(Some(Self {
            client_addr: address,
            buffer: buffer[..size].to_vec(),
        }))
    }

    fn into_incoming_packet(self) -> (Result<Self::IncomingPacket, SerializeError>, SocketAddr) {
        (
            IncomingPacketFrame::from_bytes(&self.buffer),
            self.client_addr,
        )
    }
}

pub struct UdpConnection<U> {
    socket: UdpSocket,
    is_writable: bool,
    incoming_packets: VecDeque<U>,
    outgoing_packets: VecDeque<U>,
}

impl<U: UdpPacket> UdpConnection<U> {
    pub fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            is_writable: false,
            incoming_packets: VecDeque::with_capacity(256),
            outgoing_packets: VecDeque::with_capacity(256),
        }
    }

    pub fn write_until_blocked(&mut self) -> Result<(), io::Error> {
        if !self.is_writable {
            return Ok(());
        }

        while let Some(packet) = self.outgoing_packets.pop_front() {
            match packet.send(&mut self.socket) {
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.is_writable = false;
                    self.outgoing_packets.push_front(packet);
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn push_outgoing(&mut self, packet: U) -> Result<(), io::Error> {
        self.outgoing_packets.push_back(packet);
        self.write_until_blocked()
    }

    pub fn push_outgoing_without_send(&mut self, packet: U) {
        self.outgoing_packets.push_back(packet);
    }

    pub fn notify_writable(&mut self) -> Result<(), io::Error> {
        self.is_writable = true;
        self.write_until_blocked()
    }

    fn read_from_network(&mut self) -> Result<(), CommsNetworkError> {
        loop {
            match U::from_recv(&mut self.socket) {
                Ok(Some(packet)) => self.incoming_packets.push_back(packet),
                Ok(None) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("Encountered generic error during UDP reading: {e:?}");
                    continue;
                }
            }
        }
    }

    pub fn read_new_packets<'a, I: UdpReadIter<'a, U>>(&'a mut self) -> I {
        let read_result = self.read_from_network();
        I::from(read_result.err(), &mut self.incoming_packets)
    }
}

pub struct TcpReadIter<'a> {
    read_error: Option<CommsNetworkError>,
    incoming_stream: &'a [u8],
    incoming_stream_start: &'a mut usize,
    incoming_stream_end: &'a mut usize,
    next_incoming_header: &'a mut Option<PacketHeader>,
}

impl<'a> TcpReadIter<'a> {
    pub fn try_process(
        &'a mut self,
        mut f: impl FnMut(IncomingPacketFrameRef) -> Result<(), CommsNetworkError>,
    ) -> Result<(), CommsNetworkError> {
        if let Some(e) = self.read_error.take() {
            return Err(e);
        }

        while let Some(result) = self.next_frame() {
            result.map_err(|e| e.into()).and_then(&mut f)?;
        }
        Ok(())
    }

    fn next_frame(&mut self) -> Option<Result<IncomingPacketFrameRef, SerializeError>> {
        let header = if let Some(header) = self.next_incoming_header.take() {
            header
        } else {
            let buffer_size: usize = if self.incoming_stream_end >= self.incoming_stream_start {
                *self.incoming_stream_end - *self.incoming_stream_start
            } else {
                TCP_STREAM_BUFFER_SIZE - *self.incoming_stream_start + *self.incoming_stream_end
            };

            if buffer_size < PACKET_HEADER_SIZE {
                return None;
            }

            let header_result = if self.incoming_stream_end > self.incoming_stream_start {
                let buffer = &self.incoming_stream
                    [*self.incoming_stream_start..*self.incoming_stream_start + PACKET_HEADER_SIZE];
                PacketHeader::from_bytes(buffer)
            } else {
                let slices = &mut [
                    &self.incoming_stream[*self.incoming_stream_start..],
                    &self.incoming_stream[..PACKET_HEADER_SIZE
                        - (TCP_STREAM_BUFFER_SIZE - *self.incoming_stream_start)],
                ];
                PacketHeader::from_multiple_byte_slices(slices)
            };

            let header = match header_result {
                Ok(header) => header,
                Err(e) => return Some(Err(e)),
            };
            *self.incoming_stream_start =
                (*self.incoming_stream_start + PACKET_HEADER_SIZE) % TCP_STREAM_BUFFER_SIZE;
            header
        };

        let buffer_size = if self.incoming_stream_end >= self.incoming_stream_start {
            *self.incoming_stream_end - *self.incoming_stream_start
        } else {
            (TCP_STREAM_BUFFER_SIZE - *self.incoming_stream_start) + *self.incoming_stream_end
        };

        if buffer_size < header.body_len as usize {
            *self.next_incoming_header = Some(header);
            return None;
        }

        Some(Ok(IncomingPacketFrameRef::new(
            header,
            self.incoming_stream,
            self.incoming_stream_start,
            TCP_STREAM_BUFFER_SIZE,
        )))
    }
}

pub trait UdpReadIter<'a, U: UdpPacket> {
    fn from(read_error: Option<CommsNetworkError>, incoming_packets: &'a mut VecDeque<U>) -> Self;
}

pub struct ClientUdpReadIter<'a> {
    read_error: Option<CommsNetworkError>,
    incoming_packets: &'a mut VecDeque<ClientUdpPacket>,
}

impl ClientUdpReadIter<'_> {
    pub fn try_process(
        &mut self,
        server_addr: &SocketAddr,
        known_sockets: &HashMap<SocketAddr, ClientId>,
        mut f: impl FnMut(
            <ClientUdpPacket as UdpPacket>::IncomingPacket,
            SocketAddr,
        ) -> Result<(), CommsNetworkError>,
    ) -> Result<(), CommsNetworkError> {
        if let Some(e) = self.read_error.take() {
            return Err(e);
        }
        for (result, addr) in self {
            if let Err(e) = result.map_err(|e| e.into()).and_then(|p| f(p, addr)) {
                if *server_addr == addr {
                    return Err(e);
                } else if let Some(client_id) = known_sockets.get(&addr) {
                    eprintln!(
                        "Encountered error while communicating with client {client_id}: {e:?}"
                    );
                } else {
                    eprintln!(
                        "Encountered error while communicating with unknown client {addr}: {e:?}"
                    );
                };
            }
        }
        Ok(())
    }
}

impl<'a> UdpReadIter<'a, ClientUdpPacket> for ClientUdpReadIter<'a> {
    fn from(
        read_error: Option<CommsNetworkError>,
        incoming_packets: &'a mut VecDeque<ClientUdpPacket>,
    ) -> Self {
        Self {
            read_error,
            incoming_packets,
        }
    }
}

impl Iterator for ClientUdpReadIter<'_> {
    type Item = (
        Result<<ClientUdpPacket as UdpPacket>::IncomingPacket, SerializeError>,
        SocketAddr,
    );

    fn next(&mut self) -> Option<Self::Item> {
        self.incoming_packets
            .pop_front()
            .map(|udp_packet| udp_packet.into_incoming_packet())
    }
}

pub struct ServerUdpReadIter<'a> {
    read_error: Option<CommsNetworkError>,
    incoming_packets: &'a mut VecDeque<ServerUdpPacket>,
}

impl ServerUdpReadIter<'_> {
    pub fn try_process(
        &mut self,
        mut f: impl FnMut(
            <ServerUdpPacket as UdpPacket>::IncomingPacket,
            SocketAddr,
        ) -> Result<(), CommsNetworkError>,
    ) -> Result<(), CommsNetworkError> {
        if let Some(e) = self.read_error.take() {
            return Err(e);
        }
        for (result, addr) in self {
            result.map_err(|e| e.into()).and_then(|p| f(p, addr))?
        }
        Ok(())
    }
}

impl<'a> UdpReadIter<'a, ServerUdpPacket> for ServerUdpReadIter<'a> {
    fn from(
        read_error: Option<CommsNetworkError>,
        incoming_packets: &'a mut VecDeque<ServerUdpPacket>,
    ) -> Self {
        Self {
            read_error,
            incoming_packets,
        }
    }
}

impl<'a> Iterator for ServerUdpReadIter<'a> {
    type Item = (
        Result<<ServerUdpPacket as UdpPacket>::IncomingPacket, SerializeError>,
        SocketAddr,
    );

    fn next(&mut self) -> Option<Self::Item> {
        self.incoming_packets
            .pop_front()
            .map(|udp_packet| udp_packet.into_incoming_packet())
    }
}

pub enum UpdateFromStream {
    Success,
    ConnectionClosed,
}

#[derive(thiserror::Error, Debug)]
pub enum CommsNetworkError {
    #[error("Connection is now closed")]
    Closed,
    #[error("IO error")]
    Io(#[from] io::Error),
    #[error("(De)serializeError")]
    Deserialize(#[from] bincode::Error),
}
