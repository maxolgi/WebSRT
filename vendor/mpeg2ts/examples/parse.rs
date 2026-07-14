use mpeg2ts::pes::{PesPacketReader, ReadPesPacket};
use mpeg2ts::ts::{ReadTsPacket, TsPacketReader, TsPacketWriter, WriteTsPacket};
use std::io::Write;

fn main() -> noargs::Result<()> {
    let mut args = noargs::raw_args();
    noargs::HELP_FLAG.take_help(&mut args);

    let output_type: String = noargs::opt("output_type")
        .default("ts-packet")
        .take(&mut args)
        .then(|o| o.value().parse())?;

    if let Some(help) = args.finish()? {
        print!("{help}");
        return Ok(());
    }

    match output_type.as_str() {
        "ts" => {
            let mut writer = TsPacketWriter::new(std::io::stdout());
            let mut reader = TsPacketReader::new(std::io::stdin());
            while let Some(packet) = reader.read_ts_packet()? {
                writer.write_ts_packet(&packet)?;
            }
        }
        "ts-packet" => {
            let mut reader = TsPacketReader::new(std::io::stdin());
            while let Some(packet) = reader.read_ts_packet()? {
                println!("{:?}", packet);
            }
        }
        "pes-packet" => {
            let mut reader = PesPacketReader::new(TsPacketReader::new(std::io::stdin()));
            while let Some(packet) = reader.read_pes_packet()? {
                println!("{:?} {} bytes", packet.header, packet.data.len());
            }
        }
        "es-audio" => {
            let mut reader = PesPacketReader::new(TsPacketReader::new(std::io::stdin()));
            while let Some(packet) = reader.read_pes_packet()? {
                if !packet.header.stream_id.is_audio() {
                    continue;
                }
                std::io::stdout().write_all(&packet.data)?;
            }
        }
        "es-video" => {
            let mut reader = PesPacketReader::new(TsPacketReader::new(std::io::stdin()));
            while let Some(packet) = reader.read_pes_packet()? {
                if !packet.header.stream_id.is_video() {
                    continue;
                }
                std::io::stdout().write_all(&packet.data)?;
            }
        }
        _ => {
            eprintln!("Error: Invalid output type '{}'", output_type);
            eprintln!("Valid options are: ts, ts-packet, pes-packet, es-audio, es-video");
            std::process::exit(1);
        }
    }

    Ok(())
}
