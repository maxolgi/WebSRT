use crate::es::StreamId;
use crate::time::{ClockReference, Timestamp};
use crate::util::{ReadBytesExt, WriteBytesExt};
use crate::{Error, Result};
use std::io::{Read, Write};

const PACKET_START_CODE_PREFIX: u64 = 0x00_0001;

/// PES packet.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct PesPacket<B> {
    pub header: PesHeader,
    pub data: B,
}

/// PES packet header.
///
/// Note that `PesHeader` contains the fields that belong to the optional PES header.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PesHeader {
    pub stream_id: StreamId,
    pub priority: bool,

    /// `true` indicates that the PES packet header is immediately followed by
    /// the video start code or audio syncword.
    pub data_alignment_indicator: bool,

    /// `true` implies copyrighted.
    pub copyright: bool,

    /// `true` implies original.
    pub original_or_copy: bool,

    pub pts: Option<Timestamp>,
    pub dts: Option<Timestamp>,

    /// Elementary stream clock reference.
    pub escr: Option<ClockReference>,
}
impl PesHeader {
    pub(super) fn optional_header_len(&self) -> u16 {
        3 + self.pts.map_or(0, |_| 5) + self.dts.map_or(0, |_| 5) + self.escr.map_or(0, |_| 6)
    }

    pub(crate) fn read_from<R: Read>(mut reader: R) -> Result<(Self, u16)> {
        let packet_start_code_prefix = reader.read_uint::<3>()?;
        if packet_start_code_prefix != PACKET_START_CODE_PREFIX {
            return Err(Error::invalid_input(format!(
                "Expected packet start code prefix 0x{:06x}, got 0x{:06x}",
                PACKET_START_CODE_PREFIX, packet_start_code_prefix
            )));
        }

        let stream_id = StreamId::new(reader.read_u8()?);
        let packet_len = reader.read_u16()?;

        if stream_id.as_u8() == StreamId::PROGRAM_STREAM_MAP
            || stream_id.as_u8() == StreamId::PADDING_STREAM
            || stream_id.as_u8() == StreamId::PRIVATE_STREAM_2
            || stream_id.as_u8() == StreamId::ECM_STREAM
            || stream_id.as_u8() == StreamId::EMM_STREAM
            || stream_id.as_u8() == StreamId::PROGRAM_STREAM_DIRECTORY
            || stream_id.as_u8() == StreamId::DSM_CC
            || stream_id.as_u8() == StreamId::H222_1_TYPE_E
        {
            let header = PesHeader {
                stream_id,
                priority: false,
                data_alignment_indicator: false,
                copyright: false,
                original_or_copy: false,
                pts: None,
                dts: None,
                escr: None,
            };
            return Ok((header, packet_len));
        }

        let b = reader.read_u8()?;
        if (b & 0b1100_0000) != 0b1000_0000 {
            return Err(Error::invalid_input("Unexpected marker bits"));
        }
        let scrambling_control = (b & 0b0011_0000) >> 4;
        let priority = (b & 0b0000_1000) != 0;
        let data_alignment_indicator = (b & 0b0000_0100) != 0;
        let copyright = (b & 0b0000_0010) != 0;
        let original_or_copy = (b & 0b0000_0001) != 0;
        if scrambling_control != 0 {
            return Err(Error::unsupported("Scrambling control is not supported"));
        }

        let b = reader.read_u8()?;
        let pts_flag = (b & 0b1000_0000) != 0;
        let dts_flag = (b & 0b0100_0000) != 0;
        if !pts_flag && dts_flag {
            return Err(Error::invalid_input("DTS cannot be present without PTS"));
        }

        let escr_flag = (b & 0b0010_0000) != 0;
        let es_rate_flag = (b & 0b0001_0000) != 0;
        let dsm_trick_mode_flag = (b & 0b0000_1000) != 0;
        let additional_copy_info_flag = (b & 0b0000_0100) != 0;
        let crc_flag = (b & 0b0000_0010) != 0;
        let extension_flag = (b & 0b0000_0001) != 0;

        if es_rate_flag {
            return Err(Error::unsupported("ES rate flag is not supported"));
        }
        if dsm_trick_mode_flag {
            return Err(Error::unsupported("DSM trick mode flag is not supported"));
        }
        if additional_copy_info_flag {
            return Err(Error::unsupported(
                "Additional copy info flag is not supported",
            ));
        }
        if crc_flag {
            return Err(Error::unsupported("CRC flag is not supported"));
        }
        if extension_flag {
            return Err(Error::unsupported("Extension flag is not supported"));
        }

        let pes_header_len = reader.read_u8()?;

        let mut reader = reader.take(u64::from(pes_header_len));
        let pts = if pts_flag {
            let check_bits = if dts_flag { 3 } else { 2 };
            Some(Timestamp::read_from(&mut reader, check_bits)?)
        } else {
            None
        };
        let dts = if dts_flag {
            let check_bits = 1;
            Some(Timestamp::read_from(&mut reader, check_bits)?)
        } else {
            None
        };
        let escr = if escr_flag {
            Some(ClockReference::read_escr_from(&mut reader)?)
        } else {
            None
        };
        crate::util::consume_stuffing_bytes(reader)?;

        let header = PesHeader {
            stream_id,
            priority,
            data_alignment_indicator,
            copyright,
            original_or_copy,
            pts,
            dts,
            escr,
        };
        Ok((header, packet_len))
    }

    pub(crate) fn write_to<W: Write>(&self, mut writer: W, pes_header_len: u16) -> Result<()> {
        writer.write_uint::<3>(PACKET_START_CODE_PREFIX)?;
        writer.write_u8(self.stream_id.as_u8())?;
        writer.write_u16(pes_header_len)?;

        if self.stream_id.as_u8() == StreamId::PROGRAM_STREAM_MAP
            || self.stream_id.as_u8() == StreamId::PADDING_STREAM
            || self.stream_id.as_u8() == StreamId::PRIVATE_STREAM_2
            || self.stream_id.as_u8() == StreamId::ECM_STREAM
            || self.stream_id.as_u8() == StreamId::EMM_STREAM
            || self.stream_id.as_u8() == StreamId::PROGRAM_STREAM_DIRECTORY
            || self.stream_id.as_u8() == StreamId::DSM_CC
            || self.stream_id.as_u8() == StreamId::H222_1_TYPE_E
        {
            return Ok(());
        }

        let n = 0b1000_0000
            | ((self.priority as u8) << 3)
            | ((self.data_alignment_indicator as u8) << 2)
            | ((self.copyright as u8) << 1)
            | self.original_or_copy as u8;
        writer.write_u8(n)?;

        if self.dts.is_some() && self.pts.is_none() {
            return Err(Error::invalid_input("DTS cannot be present without PTS"));
        }
        let n = ((self.pts.is_some() as u8) << 7)
            | ((self.dts.is_some() as u8) << 6)
            | ((self.escr.is_some() as u8) << 5);
        writer.write_u8(n)?;

        let pes_header_len = self.optional_header_len() as u8 - 3;
        writer.write_u8(pes_header_len)?;
        if let Some(x) = self.pts {
            let check_bits = if self.dts.is_some() { 3 } else { 2 };
            x.write_to(&mut writer, check_bits)?;
        }
        if let Some(x) = self.dts {
            let check_bits = 1;
            x.write_to(&mut writer, check_bits)?;
        }
        if let Some(x) = self.escr {
            x.write_escr_to(&mut writer)?;
        }

        Ok(())
    }
}
