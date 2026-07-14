use crate::crc::Crc32;
use crate::ts::TsPacket;
use crate::{Error, Result};
use std::io::{self, Read, Write};

#[track_caller]
pub fn consume_stuffing_bytes<R: Read>(mut reader: R) -> Result<()> {
    let mut buf = [0];
    while 1 == reader.read(&mut buf)? {
        if buf[0] != 0xFF {
            return Err(Error::invalid_input("Expected stuffing byte 0xFF"));
        }
    }
    Ok(())
}

#[track_caller]
pub fn write_stuffing_bytes<W: Write>(mut writer: W, size: usize) -> Result<()> {
    let buf = [0xFF; TsPacket::SIZE];
    writer.write_all(&buf[..size])?;
    Ok(())
}

#[derive(Debug)]
pub struct WithCrc32<T> {
    stream: T,
    crc32: Crc32,
}
impl<T> WithCrc32<T> {
    pub fn new(stream: T) -> Self {
        WithCrc32 {
            stream,
            crc32: Crc32::new(),
        }
    }
    pub fn crc32(&self) -> u32 {
        self.crc32.value()
    }
}
impl<T: Read> Read for WithCrc32<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.stream.read(buf)?;
        self.crc32.update(&buf[..size]);
        Ok(size)
    }
}
impl<T: Write> Write for WithCrc32<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let size = self.stream.write(buf)?;
        self.crc32.update(&buf[..size]);
        Ok(size)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

pub trait WriteBytesExt {
    fn write_u8(&mut self, n: u8) -> std::io::Result<()>;
    fn write_u16(&mut self, n: u16) -> std::io::Result<()>;
    fn write_u32(&mut self, n: u32) -> std::io::Result<()>;
    fn write_uint<const SIZE: usize>(&mut self, n: u64) -> std::io::Result<()>;
    fn write_i8(&mut self, n: i8) -> std::io::Result<()>;
}

impl<W: Write> WriteBytesExt for W {
    fn write_u8(&mut self, n: u8) -> std::io::Result<()> {
        self.write_all(&[n])
    }
    fn write_u16(&mut self, n: u16) -> std::io::Result<()> {
        self.write_all(&n.to_be_bytes())
    }
    fn write_u32(&mut self, n: u32) -> std::io::Result<()> {
        self.write_all(&n.to_be_bytes())
    }
    fn write_uint<const SIZE: usize>(&mut self, n: u64) -> std::io::Result<()> {
        let bytes = n.to_be_bytes();
        self.write_all(&bytes[8 - SIZE..])
    }
    fn write_i8(&mut self, n: i8) -> std::io::Result<()> {
        self.write_all(&[n as u8])
    }
}

pub trait ReadBytesExt {
    fn read_u8(&mut self) -> std::io::Result<u8>;
    fn read_u16(&mut self) -> std::io::Result<u16>;
    fn read_u32(&mut self) -> std::io::Result<u32>;
    fn read_uint<const SIZE: usize>(&mut self) -> std::io::Result<u64>;
    fn read_i8(&mut self) -> std::io::Result<i8>;
}

impl<R: Read> ReadBytesExt for R {
    fn read_u8(&mut self) -> std::io::Result<u8> {
        let mut buf = [0; 1];
        Read::read_exact(self, &mut buf)?;
        Ok(buf[0])
    }
    fn read_u16(&mut self) -> std::io::Result<u16> {
        let mut buf = [0; 2];
        Read::read_exact(self, &mut buf)?;
        Ok(u16::from_be_bytes(buf))
    }
    fn read_u32(&mut self) -> std::io::Result<u32> {
        let mut buf = [0; 4];
        Read::read_exact(self, &mut buf)?;
        Ok(u32::from_be_bytes(buf))
    }
    fn read_uint<const SIZE: usize>(&mut self) -> std::io::Result<u64> {
        let mut buf = [0; 8];
        Read::read_exact(self, &mut buf[8 - SIZE..])?;
        Ok(u64::from_be_bytes(buf))
    }
    fn read_i8(&mut self) -> std::io::Result<i8> {
        let mut buf = [0; 1];
        Read::read_exact(self, &mut buf)?;
        Ok(buf[0] as i8)
    }
}
