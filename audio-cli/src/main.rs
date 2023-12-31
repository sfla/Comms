use audiopus::coder::Encoder;
use audiopus::{Application, Channels, SampleRate};
use comms_lib::packet::{
    AssociateUdp, AssociateUdpAccepted, AudioFromClient, AudioIndex, FinishConnect,
    FinishConnectAccepted, Hello, Packet, PacketBody, PacketId,
};
use comms_lib::serialization::{
    IncomingPacketFrame, IncomingPacketFrameRef, OutgoingPacketFrame, SerializeError,
};
use comms_lib::{PacketHeader, PACKET_HEADER_SIZE};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::num::NonZeroU64;
use std::process::exit;
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use std::{env, io};
use uuid::Uuid;

fn read_next_udp(udp: &UdpSocket) -> Result<IncomingPacketFrame, SerializeError> {
    let mut buffer = [0u8; 65535];
    let size = udp.recv(&mut buffer).unwrap();
    IncomingPacketFrame::from_bytes(&buffer[..size])
}

struct TcpConnection {
    raw: TcpStream,
    buffer: Box<[u8; 48_000]>,
    start: usize,
    end: usize,
    next_header: Option<PacketHeader>,
}

impl TcpConnection {
    fn update_from_socket(&mut self, wait_for_single_read: bool) -> io::Result<()> {
        self.raw.set_nonblocking(!wait_for_single_read)?;
        loop {
            let mut buf = [0; 4096];
            match self.raw.read(&mut buf) {
                Ok(0) => {
                    println!("TCP connection closed");
                    exit(0);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    if !wait_for_single_read && e.kind() == io::ErrorKind::WouldBlock {
                        break;
                    }
                    panic!("{}", e)
                }
                Ok(amount) => {
                    let buf = &buf[..amount];
                    let end_gap = self.buffer.len() - self.end;
                    if end_gap > amount {
                        self.buffer[self.end..self.end + amount].copy_from_slice(buf);
                        self.end = (self.end + amount) % self.buffer.len();
                    } else {
                        let remainder = amount - end_gap;
                        self.buffer[self.end..].copy_from_slice(&buf[..end_gap]);
                        self.buffer[..remainder].copy_from_slice(&buf[end_gap..]);
                        self.end = remainder;
                    }

                    if wait_for_single_read {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn read_next(&mut self) -> io::Result<IncomingPacketFrameRef> {
        self.update_from_socket(true)?;
        self.maybe_read_next()
            .transpose()
            .expect("Expecting packet to be available")
    }

    fn maybe_read_next(&mut self) -> io::Result<Option<IncomingPacketFrameRef>> {
        self.update_from_socket(false)?;

        let header = if let Some(header) = self.next_header.take() {
            header
        } else {
            let buffer_size: usize = if self.end >= self.start {
                self.end - self.start
            } else {
                self.buffer.len() - self.start + self.end
            };

            if buffer_size < PACKET_HEADER_SIZE {
                return Ok(None);
            }

            let header = if self.end > self.start {
                let buffer = &self.buffer[self.start..self.start + PACKET_HEADER_SIZE];
                PacketHeader::from_bytes(buffer)
            } else {
                let slices = &mut [
                    &self.buffer[self.start..],
                    &self.buffer[..PACKET_HEADER_SIZE - (self.buffer.len() - self.start)],
                ];
                PacketHeader::from_multiple_byte_slices(slices)
            }
            .unwrap();

            self.start = (self.start + PACKET_HEADER_SIZE) % self.buffer.len();
            header
        };

        let buffer_size = if self.end >= self.start {
            self.end - self.start
        } else {
            (self.buffer.len() - self.start) + self.end
        };

        if buffer_size < header.body_len as usize {
            self.next_header = Some(header);
            return Ok(None);
        }

        Ok(Some(IncomingPacketFrameRef::new(
            header,
            &*self.buffer,
            &mut self.start,
            self.buffer.len(),
        )))
    }
}

struct BeepPacketSerializer {
    next_packet_id: u64,
}

impl BeepPacketSerializer {
    fn new() -> Self {
        Self { next_packet_id: 1 }
    }

    fn serialize<P>(
        &mut self,
        reply_to: Option<PacketId>,
        packet: &P,
    ) -> Result<Vec<u8>, AudioCliError>
    where
        P: PacketBody,
    {
        let packet_id = self.next_packet_id;
        self.next_packet_id += 1;
        OutgoingPacketFrame::from_packet(reply_to, packet)
            .map_err(|e| e.into())
            .and_then(|p| {
                p.into_raw(NonZeroU64::new(packet_id).unwrap())
                    .map_err(|e| e.into())
            })
    }
}

fn parse_arguments(
    args: impl Iterator<Item = String>,
) -> Result<(Option<String>, Option<String>, bool), AudioCliError> {
    let mut server_address = None;
    let mut client_name = None;
    let mut use_interactive_mode = false;

    for argument in args {
        // Handle optional flags
        if !use_interactive_mode && argument.to_lowercase() == "--interactive" {
            use_interactive_mode = true
        }
        // Handle positional arguments
        else if !argument.starts_with("--") && server_address.is_none() {
            server_address = Some(argument)
        } else if !argument.starts_with("--") && client_name.is_none() {
            client_name = Some(argument)
        } else {
            eprintln!("Usage: audio-cli server_address [client_name] [--interactive]");
            return Err(AudioCliError::Usage);
        }
    }

    Ok((server_address, client_name, use_interactive_mode))
}

fn handle_runtime_tcp_events(
    _serializer: &mut BeepPacketSerializer,
    tcp: &mut TcpConnection,
) -> io::Result<()> {
    loop {
        let packet = tcp.maybe_read_next()?;
        let packet = if let Some(packet) = packet {
            packet
        } else {
            break;
        };

        let _packet = IncomingPacketFrame::from(packet);
    }
    Ok(())
}

fn main() -> Result<(), AudioCliError> {
    let mut serializer = BeepPacketSerializer::new();

    let (server_address, client_name, use_interactive_mode) = parse_arguments(env::args().skip(1))?;

    let server_address = match server_address {
        Some(server_address) => server_address,
        None => {
            eprintln!("Usage: audio-cli server_address [client_name] [--interactive]");
            return Err(AudioCliError::Usage);
        }
    };

    let client_name = client_name.unwrap_or_else(|| "audio-cli".to_string());

    let server_address_with_port = format!("{server_address}:18847");
    println!("Connecting to {server_address_with_port}...");

    let tcp = TcpStream::connect(server_address_with_port.clone())?;
    let mut tcp = TcpConnection {
        raw: tcp,
        buffer: Box::new([0u8; 48_000]),
        start: 0,
        end: 0,
        next_header: None,
    };
    let udp = UdpSocket::bind("0.0.0.0:0")?;
    udp.connect(server_address_with_port)?;

    let parsed_packet = tcp.read_next()?;
    assert_eq!(parsed_packet.header.kind, Hello::KIND_ID);
    let hello: Packet<Hello> = parsed_packet.finish_parse()?;
    dbg!(&hello);

    udp.send(&serializer.serialize(
        None,
        &AssociateUdp {
            client_id: hello.body.client_id,
            local_udp_addr: udp.local_addr().unwrap(),
        },
    )?)?;

    let parsed_packet = read_next_udp(&udp)?;
    assert_eq!(parsed_packet.header.kind, AssociateUdpAccepted::KIND_ID);
    let accepted: Packet<AssociateUdpAccepted> = parsed_packet.finish_parse()?;
    dbg!(&accepted);

    tcp.raw.write_all(&serializer.serialize(
        None,
        &FinishConnect {
            name: client_name,
            user_id: Uuid::default(),
        },
    )?)?;

    let parsed_packet = tcp.read_next()?;
    assert_eq!(parsed_packet.header.kind, FinishConnectAccepted::KIND_ID);
    let finish_connect_accepted: Packet<FinishConnectAccepted> = parsed_packet.finish_parse()?;
    dbg!(&finish_connect_accepted);

    let sample_rate = 48000f32;
    let mut sample_clock = 0f32;

    // Produce a sinusoid
    let mut next_value = || {
        sample_clock = (sample_clock + 1.0) % sample_rate;
        (sample_clock * 440.0 * 2.0 * std::f32::consts::PI / sample_rate).sin() * 0.2
    };

    let start = SystemTime::now();
    let mut sample_count: u128 = 0;

    let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::LowDelay)?;
    let mut audio_id = AudioIndex::new(1).unwrap();

    if use_interactive_mode {
        loop {
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let start = SystemTime::now();
            sample_count = 0;

            // One second is 48000 samples, hence we send 480 * 50 = 24000 samples (500ms) for one "press"
            for _ in 0..50 {
                let now = SystemTime::now();
                let diff = now.duration_since(start).unwrap();
                let ms = diff.as_millis();
                let samples_since_start_count = ms * 48;

                while samples_since_start_count - sample_count >= 120 {
                    sample_count += 120;
                    let mut chunk = [0f32; 120];
                    for sample in chunk.iter_mut() {
                        *sample = next_value()
                    }

                    let mut opus_buffer = [0u8; 2048];
                    let size = encoder.encode_float(&chunk, &mut opus_buffer)?;
                    let opus_bytes = opus_buffer[..size].to_vec();
                    udp.send(&serializer.serialize(
                        None,
                        &AudioFromClient {
                            index: audio_id,
                            bytes: opus_bytes,
                        },
                    )?)?;
                    audio_id = AudioIndex::new(audio_id.get() + 1).unwrap();
                }

                handle_runtime_tcp_events(&mut serializer, &mut tcp)?;
                sleep(Duration::from_millis(3));
            }
        }
    } else {
        loop {
            sleep(Duration::from_millis(3));
            let now = SystemTime::now();
            let diff = now.duration_since(start).unwrap();
            let ms = diff.as_millis();
            let samples_since_start_count = ms * 48;

            while samples_since_start_count - sample_count >= 120 {
                sample_count += 120;
                let mut chunk = [0f32; 120];
                for sample in chunk.iter_mut() {
                    *sample = next_value()
                }

                let mut opus_buffer = [0u8; 2048];
                let size = encoder.encode_float(&chunk, &mut opus_buffer)?;
                let opus_bytes = opus_buffer[..size].to_vec();
                udp.send(&serializer.serialize(
                    None,
                    &AudioFromClient {
                        index: audio_id,
                        bytes: opus_bytes,
                    },
                )?)?;
                audio_id = AudioIndex::new(audio_id.get() + 1).unwrap();

                handle_runtime_tcp_events(&mut serializer, &mut tcp)?;
            }
        }
    }
}

#[derive(Debug)]
enum AudioCliError {
    Usage,
    Io(io::Error),
    Serialize,
    Opus,
}

impl From<io::Error> for AudioCliError {
    fn from(e: io::Error) -> Self {
        AudioCliError::Io(e)
    }
}

impl From<SerializeError> for AudioCliError {
    fn from(_: SerializeError) -> Self {
        AudioCliError::Serialize
    }
}

impl From<audiopus::Error> for AudioCliError {
    fn from(_: audiopus::Error) -> Self {
        AudioCliError::Opus
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ip_only() {
        let test_string = [String::from("127.0.0.1")];
        assert_eq!(
            parse_arguments(test_string.into_iter()).unwrap(),
            (Some("127.0.0.1".to_string()), None, false)
        );
    }

    #[test]
    fn test_parse_interactive_enabled() {
        let test_string = [String::from("127.0.0.1"), String::from("--interactive")];
        assert_eq!(
            parse_arguments(test_string.into_iter()).unwrap(),
            (Some("127.0.0.1".to_string()), None, true)
        );
    }

    #[test]
    fn test_parse_interactive_enabled_badly_spelled() {
        let test_string = [String::from("127.0.0.1"), String::from("---interactive")];
        assert!(parse_arguments(test_string.into_iter()).is_err());
    }

    #[test]
    fn test_parse_client_name_and_interactive_enabled() {
        let test_string = [
            String::from("127.0.0.1"),
            String::from("Bob"),
            String::from("--interactive"),
        ];
        assert_eq!(
            parse_arguments(test_string.into_iter()).unwrap(),
            (Some("127.0.0.1".to_string()), Some("Bob".to_string()), true)
        );
    }

    #[test]
    fn test_parse_double_client_name() {
        let test_string = [
            String::from("127.0.0.1"),
            String::from("Bob"),
            String::from("Mitch"),
        ];
        assert!(parse_arguments(test_string.into_iter()).is_err());
    }

    #[test]
    fn test_parse_bad_optional_argument() {
        let test_string = [
            String::from("127.0.0.1"),
            String::from("--icantspelloptionalargument"),
        ];
        assert!(parse_arguments(test_string.into_iter()).is_err());
    }

    #[test]
    fn test_parse_too_many_args() {
        let test_string = [
            String::from("127.0.0.1"),
            String::from("Bob"),
            String::from("--interactive"),
            String::from("argument"),
        ];
        assert!(parse_arguments(test_string.into_iter()).is_err());
    }

    #[test]
    fn test_parse_dumb_order() {
        let test_string = [String::from("--interactive"), String::from("127.0.0.1")];
        assert_eq!(
            parse_arguments(test_string.into_iter()).unwrap(),
            (Some("127.0.0.1".to_string()), None, true)
        );
    }
}
