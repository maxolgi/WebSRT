use crate::{Error, Result};

/// Stream identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StreamId(u8);
impl StreamId {
    /// `program_stream_map`
    pub const PROGRAM_STREAM_MAP: u8 = 0xBC;

    /// `private_stream_1`
    pub const PRIVATE_STREAM_1: u8 = 0xBD;

    /// `padding_stream`
    pub const PADDING_STREAM: u8 = 0xBE;

    /// `private_stream_2`
    pub const PRIVATE_STREAM_2: u8 = 0xBF;

    /// Minimum value of the identifiers for audio streams.
    pub const AUDIO_MIN: u8 = 0xC0;

    /// Maximum value of the identifiers for audio streams.
    pub const AUDIO_MAX: u8 = 0xDF;

    /// Minimum value of the identifiers for video streams.
    pub const VIDEO_MIN: u8 = 0xE0;

    /// Maximum value of the identifiers for video streams.
    pub const VIDEO_MAX: u8 = 0xEF;

    /// `ECM_stream`
    pub const ECM_STREAM: u8 = 0xF0;

    /// `EMM_stream`
    pub const EMM_STREAM: u8 = 0xF1;

    /// Rec. ITU-T H.222.0 | ISO/IEC 13818-1 Annex B or ISO/IEC 13818-6_DSMCC_stream
    pub const DSM_CC: u8 = 0xF2;

    /// ISO/IEC_13522_stream
    pub const ISO_13522_STREAM: u8 = 0xF3;

    /// Rec. ITU-T H.222.1 type A
    pub const H222_1_TYPE_A: u8 = 0xF4;

    /// Rec. ITU-T H.222.1 type B
    pub const H222_1_TYPE_B: u8 = 0xF5;

    /// Rec. ITU-T H.222.1 type C
    pub const H222_1_TYPE_C: u8 = 0xF6;

    /// Rec. ITU-T H.222.1 type D
    pub const H222_1_TYPE_D: u8 = 0xF7;

    /// Rec. ITU-T H.222.1 type E
    pub const H222_1_TYPE_E: u8 = 0xF8;

    /// `ancillary_stream`
    pub const ANCILLARY_STREAM: u8 = 0xF9;

    /// ISO/IEC 14496-1_SL-packetized_stream
    pub const SL_PACKETIZED_STREAM: u8 = 0xFA;

    /// ISO/IEC 14496-1_FlexMux_stream
    pub const FLEX_MUX_STREAM: u8 = 0xFB;

    /// metadata stream
    pub const METADATA_STREAM: u8 = 0xFC;

    /// `extended_stream_id`
    pub const EXTENDED_STREAM_ID: u8 = 0xFD;

    /// reserved data stream
    pub const RESERVED_DATA_STREAM: u8 = 0xFE;

    /// `program_stream_directory`
    pub const PROGRAM_STREAM_DIRECTORY: u8 = 0xFF;

    /// Makes a new `StreamId` instance.
    pub fn new(id: u8) -> Self {
        StreamId(id)
    }

    /// Makes a new `StreamId` instance for audio stream.
    ///
    /// # Errors
    ///
    /// If `id` is not between `AUDIO_MIN` and `AUDIO_MAX`, it will return an `ErrorKind::InvalidInput` error.
    pub fn new_audio(id: u8) -> Result<Self> {
        if !((Self::AUDIO_MIN..=Self::AUDIO_MAX).contains(&id)) {
            return Err(Error::invalid_input(format!("Not an audio ID: {}", id)));
        }
        Ok(StreamId(id))
    }

    /// Makes a new `StreamId` instance for video stream.
    ///
    /// # Errors
    ///
    /// If `id` is not between `VIDEO_MIN` and `VIDEO_MAX`, it will return an `ErrorKind::InvalidInput` error.
    pub fn new_video(id: u8) -> Result<Self> {
        if !((Self::VIDEO_MIN..=Self::VIDEO_MAX).contains(&id)) {
            return Err(Error::invalid_input(format!("Not a video ID: {}", id)));
        }
        Ok(StreamId(id))
    }

    /// Returns the value of the identifier.
    pub fn as_u8(&self) -> u8 {
        self.0
    }

    /// Returns `true` if it is an audio identifier, otherwise `false`.
    pub fn is_audio(&self) -> bool {
        0xC0 <= self.0 && self.0 <= 0xDF
    }

    /// Returns `true` if it is a video identifier, otherwise `false`.
    pub fn is_video(&self) -> bool {
        0xE0 <= self.0 && self.0 <= 0xEF
    }
}
