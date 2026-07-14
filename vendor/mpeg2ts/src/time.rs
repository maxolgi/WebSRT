//! Time-related constituent elements.
use crate::util::{ReadBytesExt, WriteBytesExt};
use crate::{Error, Result};
use std::io::{Read, Write};

/// Timestamp type for PTS/DTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(u64);
impl Timestamp {
    /// 90 kHz.
    pub const RESOLUTION: u64 = 90_000;

    /// Maximum timestamp value.
    pub const MAX: u64 = (1 << 33) - 1;

    /// Makes a new `Timestamp` instance.
    ///
    /// # Errors
    ///
    /// If `n` exceeds `Timestamp::MAX`, it will return an `ErrorKind::InvalidInput` error.
    pub fn new(n: u64) -> Result<Self> {
        if n > Self::MAX {
            return Err(Error::invalid_input(format!("Too large value: {n}")));
        }
        Ok(Timestamp(n))
    }

    /// Returns the value of the timestamp.
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub(crate) fn from_u64(n: u64) -> Result<Self> {
        if (n & 1) == 0 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }
        if ((n >> 16) & 1) == 0 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }
        if ((n >> 32) & 1) == 0 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }

        let n0 = n >> (32 + 1) & ((1 << 3) - 1);
        let n1 = n >> (16 + 1) & ((1 << 15) - 1);
        let n2 = n >> 1 & ((1 << 15) - 1);
        Ok(Timestamp((n0 << 30) | (n1 << 15) | n2))
    }

    pub(crate) fn read_from<R: Read>(mut reader: R, check_bits: u8) -> Result<Self> {
        let n = reader.read_uint::<5>()?;
        if (n >> 36) as u8 != check_bits {
            return Err(Error::invalid_input(format!(
                "Expected check_bits: {check_bits}, got: {}",
                (n >> 36) as u8
            )));
        }
        Self::from_u64(n)
    }

    pub(crate) fn write_to<W: Write>(&self, mut writer: W, check_bits: u8) -> Result<()> {
        let n0 = u64::from(check_bits);
        let n1 = self.0 >> 30;
        let n2 = (self.0 >> 15) & ((1 << 15) - 1);
        let n3 = self.0 & ((1 << 15) - 1);

        let n = (n0 << 36) | (n1 << 33) | (1 << 32) | (n2 << 17) | (1 << 16) | (n3 << 1) | 1;
        writer.write_uint::<5>(n)?;
        Ok(())
    }
}
impl From<u32> for Timestamp {
    fn from(n: u32) -> Self {
        Timestamp(u64::from(n))
    }
}

/// Timestamp type for PCR/OPCR/ESCR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClockReference(u64);
impl ClockReference {
    /// 27MHz.
    pub const RESOLUTION: u64 = 27_000_000;

    /// Maximum PCR value.
    pub const MAX: u64 = ((1 << 33) - 1) * 300 + 0b1_1111_1111;

    /// Makes a new `ClockReference` instance.
    ///
    /// # Errors
    ///
    /// If `n` exceeds `ClockReference::MAX`, it will return an `ErrorKind::InvalidInput` error.
    pub fn new(n: u64) -> Result<Self> {
        if n > Self::MAX {
            return Err(Error::invalid_input(format!("Too large value: {n}")));
        }
        Ok(ClockReference(n))
    }

    /// Returns the value of the PCR.
    pub fn as_u64(&self) -> u64 {
        self.0
    }

    pub(crate) fn read_pcr_from<R: Read>(mut reader: R) -> Result<Self> {
        let n = reader.read_uint::<6>()?;
        let base = n >> 15;
        let extension = n & 0b1_1111_1111;
        Ok(ClockReference(base * 300 + extension))
    }

    pub(crate) fn write_pcr_to<W: Write>(&self, mut writer: W) -> Result<()> {
        let base = self.0 / 300;
        let extension = self.0 % 300;

        let n = (base << 15) | extension;
        writer.write_uint::<6>(n)?;
        Ok(())
    }

    pub(crate) fn read_escr_from<R: Read>(mut reader: R) -> Result<Self> {
        let n = reader.read_uint::<6>()?;
        if n >> 46 != 0 {
            return Err(Error::invalid_input("Expected zero in reserved bits"));
        }

        if (n & 1) != 1 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }
        let extension = (n >> 1) & 0b1_1111_1111;

        let n = n >> 10;
        if (n & 1) != 1 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }
        if ((n >> 16) & 1) != 1 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }
        if ((n >> 32) & 1) != 1 {
            return Err(Error::invalid_input("Unexpected marker bit"));
        }

        let n0 = (n >> 1) & ((1 << 15) - 1);
        let n1 = (n >> 17) & ((1 << 15) - 1);
        let n2 = (n >> 33) & ((1 << 3) - 1);
        let base = n0 | (n1 << 15) | (n2 << 30);
        Ok(ClockReference(base * 300 + extension))
    }

    pub(crate) fn write_escr_to<W: Write>(&self, mut writer: W) -> Result<()> {
        let base = self.0 / 300;
        let extension = self.0 % 300;

        let marker = 1;
        let base0 = base & ((1 << 15) - 1);
        let base1 = (base >> 15) & ((1 << 15) - 1);
        let base2 = base >> 30;

        let n = marker
            | (extension << 1)
            | (marker << 10)
            | (base0 << 11)
            | (marker << 26)
            | (base1 << 27)
            | (marker << 42)
            | (base2 << 43);
        writer.write_uint::<6>(n)?;
        Ok(())
    }
}
impl From<u32> for ClockReference {
    fn from(n: u32) -> Self {
        ClockReference(u64::from(n))
    }
}
impl From<Timestamp> for ClockReference {
    fn from(f: Timestamp) -> Self {
        ClockReference(f.0 * 300)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn pcr_conversion() {
        let cr = ClockReference::new(10000).unwrap();
        let mut buf = Vec::new();
        cr.write_pcr_to(&mut buf).unwrap();
        let new_cr = ClockReference::read_pcr_from(&buf[..]).unwrap();
        assert_eq!(cr, new_cr);
    }

    #[test]
    fn escr_conversion() {
        let cr = ClockReference::new(10000).unwrap();
        let mut buf = Vec::new();
        cr.write_escr_to(&mut buf).unwrap();
        let new_cr = ClockReference::read_escr_from(&buf[..]).unwrap();
        assert_eq!(cr, new_cr);
    }
}
