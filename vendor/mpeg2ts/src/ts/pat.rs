use crate::ts::psi::{Psi, PsiTable, PsiTableHeader, PsiTableSyntax};
use crate::ts::{Pid, VersionNumber};
use crate::util::{ReadBytesExt, WriteBytesExt};
use crate::{Error, Result};
use std::io::{Read, Write};

/// Payload for PAT(Program Association Table) packets.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pat {
    pub transport_stream_id: u16,
    pub version_number: VersionNumber,
    pub table: Vec<ProgramAssociation>,
}
impl Pat {
    const TABLE_ID: u8 = 0;

    pub(super) fn read_from<R: Read>(reader: R) -> Result<Self> {
        let mut psi = Psi::read_from(reader)?;
        if psi.tables.len() != 1 {
            return Err(Error::invalid_input("Expected exactly one PSI table"));
        }

        let table = psi.tables.pop().expect("Never fails");
        let header = table.header;
        if header.table_id != Self::TABLE_ID {
            return Err(Error::invalid_input(format!(
                "Expected table_id {}, got {}",
                Self::TABLE_ID,
                header.table_id
            )));
        }
        if header.private_bit {
            return Err(Error::invalid_input("Unexpected private_bit"));
        }

        let syntax = table
            .syntax
            .as_ref()
            .ok_or_else(|| Error::invalid_input("Expected table syntax"))?;
        if syntax.section_number != 0 {
            return Err(Error::invalid_input("Expected section_number 0"));
        }
        if syntax.last_section_number != 0 {
            return Err(Error::invalid_input("Expected last_section_number 0"));
        }
        if !syntax.current_next_indicator {
            return Err(Error::invalid_input(
                "Expected current_next_indicator to be true",
            ));
        }

        let mut reader = &syntax.table_data[..];
        let mut table = Vec::new();
        while !reader.is_empty() {
            table.push(ProgramAssociation::read_from(&mut reader)?);
        }
        Ok(Pat {
            transport_stream_id: syntax.table_id_extension,
            version_number: syntax.version_number,
            table,
        })
    }

    pub(super) fn write_to<W: Write>(&self, writer: W) -> Result<()> {
        self.to_psi()?.write_to(writer)
    }

    fn to_psi(&self) -> Result<Psi> {
        let mut table_data = Vec::new();
        for pa in &self.table {
            pa.write_to(&mut table_data)?;
        }

        let header = PsiTableHeader {
            table_id: Self::TABLE_ID,
            private_bit: false,
        };
        let syntax = Some(PsiTableSyntax {
            table_id_extension: self.transport_stream_id,
            version_number: self.version_number,
            current_next_indicator: true,
            section_number: 0,
            last_section_number: 0,
            table_data,
        });
        let tables = vec![PsiTable { header, syntax }];
        Ok(Psi { tables })
    }
}

/// An entry of a program association table.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProgramAssociation {
    pub program_num: u16,

    /// The packet identifier that contains the associated PMT.
    pub program_map_pid: Pid,
}
impl ProgramAssociation {
    fn read_from<R: Read>(mut reader: R) -> Result<Self> {
        let program_num = reader.read_u16()?;
        let program_map_pid = Pid::read_from(reader)?;
        Ok(ProgramAssociation {
            program_num,
            program_map_pid,
        })
    }

    fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_u16(self.program_num)?;
        self.program_map_pid.write_to(writer)?;
        Ok(())
    }
}
