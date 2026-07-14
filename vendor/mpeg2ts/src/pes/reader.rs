use crate::pes::PesPacket;
use crate::ts::payload::{Bytes, Pes};
use crate::ts::{Pid, ReadTsPacket, TsPayload};
use crate::{Error, Result};
use std::collections::HashMap;

/// The `ReadPesPacket` trait allows for reading PES packets from a source.
pub trait ReadPesPacket {
    /// Reads a PES packet.
    ///
    /// If the end of the stream is reached, it will return `Ok(None)`.
    fn read_pes_packet(&mut self) -> Result<Option<PesPacket<Vec<u8>>>>;
}

/// PES packet reader.
#[derive(Debug)]
pub struct PesPacketReader<R> {
    ts_packet_reader: R,
    pes_packets: HashMap<Pid, PartialPesPacket>,
    eos: bool,
}
impl<R: ReadTsPacket> PesPacketReader<R> {
    /// Makes a new `PesPacketReader` instance.
    pub fn new(ts_packet_reader: R) -> Self {
        PesPacketReader {
            ts_packet_reader,
            pes_packets: HashMap::new(),
            eos: false,
        }
    }

    /// Returns a reference to the underlaying TS packet reader.
    pub fn ts_packet_reader(&self) -> &R {
        &self.ts_packet_reader
    }

    /// Converts `PesPacketReader` into the underlaying TS packet reader.
    pub fn into_ts_packet_reader(self) -> R {
        self.ts_packet_reader
    }

    fn handle_eos(&mut self) -> Result<Option<PesPacket<Vec<u8>>>> {
        if let Some(key) = self.pes_packets.keys().next().cloned() {
            let partial = self.pes_packets.remove(&key).expect("Never fails");
            if partial.data_len.is_some() && partial.data_len != Some(partial.packet.data.len()) {
                return Err(Error::invalid_input("Unexpected EOS"));
            }
            Ok(Some(partial.packet))
        } else {
            Ok(None)
        }
    }

    fn handle_pes_start_payload(
        &mut self,
        pid: Pid,
        pes: Pes,
    ) -> Result<Option<PesPacket<Vec<u8>>>> {
        let data_len = if pes.pes_packet_len == 0 {
            None
        } else {
            let optional_header_len = pes.header.optional_header_len();
            if pes.pes_packet_len < optional_header_len {
                return Err(Error::invalid_input(format!(
                    "pes.pes_packet_len={}, optional_header_len={}",
                    pes.pes_packet_len, optional_header_len
                )));
            }
            Some((pes.pes_packet_len - optional_header_len) as usize)
        };

        let mut data = Vec::with_capacity(data_len.unwrap_or(pes.data.len()));
        data.extend_from_slice(&pes.data);

        let packet = PesPacket {
            header: pes.header,
            data,
        };
        let partial = PartialPesPacket { packet, data_len };
        if let Some(pred) = self.pes_packets.insert(pid, partial) {
            if let Some(data_len) = pred.data_len
                && data_len != pred.packet.data.len()
            {
                return Err(Error::invalid_input(format!(
                    "Mismatched PES packet data length: actual={}, expected={}",
                    pred.packet.data.len(),
                    data_len,
                )));
            }
            Ok(Some(pred.packet))
        } else {
            Ok(None)
        }
    }

    fn handle_pes_continuation_payload(
        &mut self,
        pid: Pid,
        data: &Bytes,
    ) -> Result<Option<PesPacket<Vec<u8>>>> {
        let mut partial = self
            .pes_packets
            .remove(&pid)
            .ok_or_else(|| Error::invalid_input("PES packet not found for PID"))?;
        partial.packet.data.extend_from_slice(data);
        if Some(partial.packet.data.len()) == partial.data_len {
            Ok(Some(partial.packet))
        } else {
            if let Some(expected) = partial.data_len
                && partial.packet.data.len() > expected
            {
                return Err(Error::invalid_input(format!(
                    "Too large PES packet data: actual={}, expected={}",
                    partial.packet.data.len(),
                    expected
                )));
            }
            self.pes_packets.insert(pid, partial);
            Ok(None)
        }
    }
}
impl<R: ReadTsPacket> ReadPesPacket for PesPacketReader<R> {
    fn read_pes_packet(&mut self) -> Result<Option<PesPacket<Vec<u8>>>> {
        if self.eos {
            return self.handle_eos();
        }

        while let Some(ts_packet) = self.ts_packet_reader.read_ts_packet()? {
            let pid = ts_packet.header.pid;
            let result = match ts_packet.payload {
                Some(TsPayload::PesStart(payload)) => {
                    self.handle_pes_start_payload(pid, payload)?
                }
                Some(TsPayload::PesContinuation(payload)) => {
                    self.handle_pes_continuation_payload(pid, &payload)?
                }
                _ => None,
            };
            if result.is_some() {
                return Ok(result);
            }
        }

        self.eos = true;
        self.handle_eos()
    }
}

#[derive(Debug)]
struct PartialPesPacket {
    packet: PesPacket<Vec<u8>>,
    data_len: Option<usize>,
}
