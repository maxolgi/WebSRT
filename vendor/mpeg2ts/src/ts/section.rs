use crate::Result;
use crate::ts::payload::Bytes;
use crate::util::WriteBytesExt;
use std::io::Write;

/// Payload for Section Stream packets.
#[allow(missing_docs)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Section {
    pub pointer_field: u8,
    pub data: Bytes,
}

impl Section {
    pub(super) fn write_to<W: Write>(&self, mut writer: W) -> Result<()> {
        writer.write_u8(self.pointer_field)?;
        self.data.write_to(writer)?;
        Ok(())
    }
}
