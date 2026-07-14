use super::adaptation_field::AdaptationFieldControl;
use crate::ts::payload::{Bytes, Null, Pat, Pes, Pmt, Section};
use crate::ts::{AdaptationField, ContinuityCounter, Pid, TransportScramblingControl};
use crate::util::{ReadBytesExt, WriteBytesExt};
use crate::{Error, Result};
use std::io::{Cursor, Read, Write};

/// Transport stream packet.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TsPacket {
    pub header: TsHeader,
    pub adaptation_field: Option<AdaptationField>,
    pub payload: Option<TsPayload>,
}
impl TsPacket {
    /// Size of a packet in bytes.
    pub const SIZE: usize = 188;

    /// Synchronization byte.
    ///
    /// Each packet starts with this byte.
    pub const SYNC_BYTE: u8 = 0x47;

    pub(super) fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        let mut payload_buf = [0; TsPacket::SIZE - 4];
        let payload_len = if let Some(ref payload) = self.payload {
            let mut writer = Cursor::new(&mut payload_buf[..]);
            payload.write_to(&mut writer)?;
            writer.position() as usize
        } else {
            0
        };

        let required_len = self
            .adaptation_field
            .as_ref()
            .map_or(0, |a| a.external_size());
        let free_len = TsPacket::SIZE - 4 - payload_len;
        if required_len > free_len {
            return Err(Error::invalid_input(format!(
                "No space for adaptation field: required={}, free={}",
                required_len, free_len,
            )));
        }

        let adaptation_field_control = match (
            self.adaptation_field.is_some() || free_len > 0,
            self.payload.is_some(),
        ) {
            (true, true) => AdaptationFieldControl::AdaptationFieldAndPayload,
            (true, false) => AdaptationFieldControl::AdaptationFieldOnly,
            (false, true) => AdaptationFieldControl::PayloadOnly,
            (false, false) => {
                return Err(Error::invalid_input("Reserved for future use"));
            }
        };
        let payload_unit_start_indicator = !matches!(
            self.payload,
            Some(TsPayload::PesContinuation(_))
                | Some(TsPayload::Raw(_))
                | Some(TsPayload::Null(_))
                | None
        );
        self.header.write_to(
            &mut writer,
            adaptation_field_control,
            payload_unit_start_indicator,
        )?;

        if let Some(ref adaptation_field) = self.adaptation_field {
            let adaptation_field_len = (free_len - 1) as u8;
            adaptation_field.write_to(&mut writer, adaptation_field_len)?;
        } else if free_len > 0 {
            let adaptation_field_len = (free_len - 1) as u8;
            AdaptationField::write_stuffing_bytes(&mut writer, adaptation_field_len)?;
        }
        writer.write_all(&payload_buf[..payload_len])?;
        Ok(())
    }
}

/// TS packet header.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TsHeader {
    pub transport_error_indicator: bool,
    pub transport_priority: bool,
    pub pid: Pid,
    pub transport_scrambling_control: TransportScramblingControl,
    pub continuity_counter: ContinuityCounter,
}
impl TsHeader {
    pub(super) fn read_from<R: Read>(
        mut reader: R,
    ) -> Result<(Self, AdaptationFieldControl, bool)> {
        let sync_byte = reader.read_u8()?;
        if sync_byte != TsPacket::SYNC_BYTE {
            return Err(Error::invalid_input(format!(
                "Expected sync byte 0x{:02x}, got 0x{:02x}",
                TsPacket::SYNC_BYTE,
                sync_byte
            )));
        }

        let n = reader.read_u16()?;
        let transport_error_indicator = (n & 0b1000_0000_0000_0000) != 0;
        let payload_unit_start_indicator = (n & 0b0100_0000_0000_0000) != 0;
        let transport_priority = (n & 0b0010_0000_0000_0000) != 0;
        let pid = Pid::new(n & 0b0001_1111_1111_1111)?;

        let n = reader.read_u8()?;
        let transport_scrambling_control = TransportScramblingControl::from_u8(n >> 6)?;
        let adaptation_field_control = AdaptationFieldControl::from_u8((n >> 4) & 0b11)?;
        let continuity_counter = ContinuityCounter::from_u8(n & 0b1111)?;

        let header = TsHeader {
            transport_error_indicator,
            transport_priority,
            pid,
            transport_scrambling_control,
            continuity_counter,
        };
        Ok((
            header,
            adaptation_field_control,
            payload_unit_start_indicator,
        ))
    }

    fn write_to<W: Write>(
        &self,
        mut writer: W,
        adaptation_field_control: AdaptationFieldControl,
        payload_unit_start_indicator: bool,
    ) -> Result<()> {
        writer.write_u8(TsPacket::SYNC_BYTE)?;

        let n = ((self.transport_error_indicator as u16) << 15)
            | ((payload_unit_start_indicator as u16) << 14)
            | ((self.transport_priority as u16) << 13)
            | self.pid.as_u16();
        writer.write_u16(n)?;

        let n = ((self.transport_scrambling_control as u8) << 6)
            | ((adaptation_field_control as u8) << 4)
            | self.continuity_counter.as_u8();
        writer.write_u8(n)?;

        Ok(())
    }
}

/// TS packet payload.
#[allow(missing_docs, clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TsPayload {
    Pat(Pat),
    Pmt(Pmt),
    PesStart(Pes),
    PesContinuation(Bytes),
    Section(Section),
    Null(Null),
    Raw(Bytes),
}
impl TsPayload {
    fn write_to<W: Write>(&self, writer: W) -> Result<()> {
        match *self {
            TsPayload::Pat(ref x) => x.write_to(writer),
            TsPayload::Pmt(ref x) => x.write_to(writer),
            TsPayload::PesStart(ref x) => x.write_to(writer),
            TsPayload::PesContinuation(ref x) => x.write_to(writer),
            TsPayload::Section(ref x) => x.write_to(writer),
            TsPayload::Null(_) => Ok(()),
            TsPayload::Raw(ref x) => x.write_to(writer),
        }
    }
}
