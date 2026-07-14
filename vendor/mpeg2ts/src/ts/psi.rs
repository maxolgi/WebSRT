use crate::ts::VersionNumber;
use crate::util::{self, ReadBytesExt, WithCrc32, WriteBytesExt};
use crate::{Error, Result};
use std::io::{Read, Write};

const MAX_SYNTAX_SECTION_LEN: usize = 1021;

/// Program-specific information.
#[derive(Debug)]
pub struct Psi {
    pub tables: Vec<PsiTable>,
}
impl Psi {
    pub fn read_from<R: Read>(mut reader: R) -> Result<Self> {
        let pointer_field = reader.read_u8()?;
        if pointer_field != 0 {
            return Err(Error::unsupported("Pointer field must be 0"));
        }

        let mut tables = Vec::new();
        loop {
            let mut peek = [0];
            let eos = reader.read(&mut peek)? == 0;
            if eos {
                break;
            }
            if !tables.is_empty() && peek[0] == 0xFF {
                util::consume_stuffing_bytes(&mut reader)?;
                break;
            }
            let table = PsiTable::read_from(peek.chain(&mut reader))?;
            tables.push(table);
        }
        Ok(Psi { tables })
    }

    pub fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_u8(0)?; // pointer field
        for table in &self.tables {
            table.write_to(&mut writer)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct PsiTable {
    pub header: PsiTableHeader,
    pub syntax: Option<PsiTableSyntax>,
}
impl PsiTable {
    fn read_from<R: Read>(reader: R) -> Result<Self> {
        let mut reader = WithCrc32::new(reader);
        let (header, syntax_section_len) = PsiTableHeader::read_from(&mut reader)?;
        let syntax = if syntax_section_len > 0 {
            let syntax = {
                if syntax_section_len < 4 {
                    return Err(Error::invalid_input(
                        "Syntax section length must be at least 4",
                    ));
                }
                let reader = reader.by_ref().take(u64::from(syntax_section_len - 4));
                PsiTableSyntax::read_from(reader)?
            };
            let crc32 = reader.crc32();
            let expected_crc32 = reader.read_u32()?;
            if crc32 != expected_crc32 {
                return Err(Error::invalid_input("CRC32 mismatch"));
            }
            Some(syntax)
        } else {
            None
        };
        Ok(PsiTable { header, syntax })
    }

    fn write_to<W: Write>(&self, writer: W) -> Result<()> {
        let mut writer = WithCrc32::new(writer);

        let syntax_section_len = self.syntax.as_ref().map_or(0, |s| s.external_size());
        self.header.write_to(&mut writer, syntax_section_len)?;
        if let Some(ref x) = self.syntax {
            x.write_to(&mut writer)?;

            let crc32 = writer.crc32();
            writer.write_u32(crc32)?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct PsiTableHeader {
    pub table_id: u8,
    pub private_bit: bool,
}
impl PsiTableHeader {
    fn read_from<R: Read>(mut reader: R) -> Result<(Self, u16)> {
        let table_id = reader.read_u8()?;

        let n = reader.read_u16()?;
        let syntax_section_indicator = (n & 0b1000_0000_0000_0000) != 0;
        let private_bit = (n & 0b0100_0000_0000_0000) != 0;
        if (n & 0b0011_0000_0000_0000) != 0b0011_0000_0000_0000 {
            return Err(Error::invalid_input("Unexpected reserved bits"));
        }
        if (n & 0b0000_1100_0000_0000) != 0 {
            return Err(Error::invalid_input(
                "Unexpected section length unused bits",
            ));
        }
        let syntax_section_len = n & 0b0000_0011_1111_1111;
        if (syntax_section_len as usize) > MAX_SYNTAX_SECTION_LEN {
            return Err(Error::invalid_input(
                "Syntax section length exceeds maximum",
            ));
        }
        if syntax_section_indicator && syntax_section_len == 0 {
            return Err(Error::invalid_input(
                "Syntax section length cannot be 0 when indicator is set",
            ));
        }

        let header = PsiTableHeader {
            table_id,
            private_bit,
        };
        Ok((header, syntax_section_len))
    }

    fn write_to<W: Write>(&self, mut writer: W, syntax_section_len: usize) -> Result<()> {
        if syntax_section_len > MAX_SYNTAX_SECTION_LEN {
            return Err(Error::invalid_input(
                "Syntax section length exceeds maximum",
            ));
        }

        writer.write_u8(self.table_id)?;

        let n = (((syntax_section_len != 0) as u16) << 15)
            | ((self.private_bit as u16) << 14)
            | 0b0011_0000_0000_0000
            | syntax_section_len as u16;
        writer.write_u16(n)?;

        Ok(())
    }
}

#[derive(Debug)]
pub struct PsiTableSyntax {
    pub table_id_extension: u16,
    pub version_number: VersionNumber,
    pub current_next_indicator: bool,
    pub section_number: u8,
    pub last_section_number: u8,
    pub table_data: Vec<u8>,
}
impl PsiTableSyntax {
    fn external_size(&self) -> usize {
        2 /* table_id_extension */ +
            1 /* version_number and current_next_indicator */ +
            1 /* section_number */ +
            1 /* last_section_number */ +
            self.table_data.len() /* table_data */ +
            4 /* CRC32 */
    }

    fn read_from<R: Read>(mut reader: R) -> Result<Self> {
        let table_id_extension = reader.read_u16()?;

        let b = reader.read_u8()?;
        if (b & 0b1100_0000) != 0b1100_0000 {
            return Err(Error::invalid_input("Unexpected reserved bits"));
        }
        let version_number = VersionNumber::from_u8((b & 0b0011_1110) >> 1)?;
        let current_next_indicator = (b & 0b0000_0001) != 0;

        let section_number = reader.read_u8()?;
        let last_section_number = reader.read_u8()?;

        let mut table_data = Vec::new();
        reader.read_to_end(&mut table_data)?;

        Ok(PsiTableSyntax {
            table_id_extension,
            version_number,
            current_next_indicator,
            section_number,
            last_section_number,
            table_data,
        })
    }

    fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_u16(self.table_id_extension)?;

        let n =
            0b1100_0000 | (self.version_number.as_u8() << 1) | self.current_next_indicator as u8;
        writer.write_u8(n)?;

        writer.write_u8(self.section_number)?;
        writer.write_u8(self.last_section_number)?;
        writer.write_all(&self.table_data)?;

        Ok(())
    }
}
