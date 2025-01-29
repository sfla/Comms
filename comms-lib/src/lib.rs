use crate::packet::{AudioIndex, PacketId, PacketKind};
use std::io::Read;

use crate::serialization::SerializeError;
pub use serde::{Deserialize, Serialize};
use std::mem;

pub mod network;
pub mod packet;
pub mod serialization;

pub struct OpusAudioFrame {
    pub index: AudioIndex,
    pub bytes: Vec<u8>,
}

pub struct RawAudioFrame {
    pub samples: Vec<f32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PacketHeader {
    pub kind: PacketKind,
    pub id: PacketId,
    pub body_len: u16,
    pub reply_to: Option<PacketId>,
}
pub const PACKET_HEADER_SIZE: usize = mem::size_of::<PacketHeader>();

pub struct MultipleByteSlices<'a, 'b>(&'a mut [&'b [u8]]);

impl Read for MultipleByteSlices<'_, '_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let slices = mem::take(&mut self.0);

        match slices {
            [head, ..] => {
                let n_bytes = head.read(buf)?;

                if head.is_empty() {
                    self.0 = &mut slices[1..];
                } else {
                    self.0 = slices;
                }

                Ok(n_bytes)
            }
            _ => Ok(0),
        }
    }
}

impl PacketHeader {
    pub fn from_multiple_byte_slices(bytes: &mut [&[u8]]) -> Result<Self, SerializeError> {
        bincode::deserialize_from(MultipleByteSlices(bytes))
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SerializeError> {
        bincode::deserialize(bytes)
    }
}
