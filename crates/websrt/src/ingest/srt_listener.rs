use super::{build_key_settings, Ingester, SrtConnectionIngester};
use crate::stream_registry::StreamRegistry;
use anyhow::{anyhow, Result};
use futures::StreamExt;
use srt_protocol::options::{ByteCount, PacketCount};
use srt_protocol::settings::KeySettings;
use srt_tokio::{SrtIncoming, SrtListener};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const UDP_BUF_SIZE: ByteCount = ByteCount(8_388_608);

/// Receiver buffer depth in packets, sized from TSBPD latency. TSBPD holds
/// `latency × packet_rate` packets in the receiver buffer simultaneously; the
/// srt-protocol default (8192×1500 B ≈ 8.3k packets) overflows at high
/// channel counts — e.g. 128ch s302m ≈ 20k pkt/s needs 20k packets per
/// second of latency — silently capping ingest throughput (~150 Mbps
/// observed) and stalling publishers on flow control. Mirrors the browser
/// side's formula from 3560a46: `latency_ms × 20000/1000 × 2`, clamped to
/// [8192, 320000].
fn recv_buffer_packets(latency: Duration) -> u64 {
    let ms = latency.as_millis() as u64;
    (ms * 20_000 / 1000 * 2).clamp(8192, 320_000)
}

pub struct SrtListenerService {
    #[allow(dead_code)]
    listener: SrtListener,
    incoming: SrtIncoming,
    key_settings: Option<KeySettings>,
}

impl SrtListenerService {
    pub async fn bind(
        addr: impl AsRef<str>,
        latency: Duration,
        passphrase: Option<String>,
    ) -> Result<Self> {
        let key_settings = build_key_settings(&passphrase)?;

        let (listener, incoming) = SrtListener::builder()
            .latency(latency)
            .set(|o| {
                o.connect.udp_recv_buffer_size = UDP_BUF_SIZE;
                o.connect.udp_send_buffer_size = UDP_BUF_SIZE;
                // Latency-scaled SRT receiver buffer (in bytes; srt-protocol
                // divides by MSS to get packets). See recv_buffer_packets.
                // Flow-control window must be raised in lockstep: SRT requires
                // RCVBUF <= FC * MSS (set FC first, then RCVBUF).
                let pkts = PacketCount(recv_buffer_packets(latency));
                o.sender.flow_control_window_size = pkts;
                o.receiver.buffer_size = ByteCount(recv_buffer_packets(latency) * (1500 - 28) as u64);
            })
            .bind(addr.as_ref())
            .await
            .map_err(|e| anyhow!("srt listener bind: {e}"))?;
        tracing::info!(
            encrypted = key_settings.is_some(),
            "SRT multi-publisher listener bound"
        );
        Ok(Self {
            listener,
            incoming,
            key_settings,
        })
    }

    pub async fn serve<I, F>(self, registry: Arc<StreamRegistry>, shutdown: Arc<Notify>, wrap: F)
    where
        F: Fn(&str, SrtConnectionIngester) -> I + Send + 'static,
        I: Ingester + Send + 'static,
    {
        let Self {
            listener: _listener,
            mut incoming,
            key_settings,
        } = self;

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => {
                    tracing::info!("SRT listener shutting down");
                    break;
                }
                req = incoming.incoming().next() => {
                    let Some(request) = req else { break; };
                    let remote = request.remote();
                    let stream_id = request.stream_id().map(|s| s.to_string());
                    let Some(name) = stream_id.filter(|s| !s.is_empty()) else {
                        tracing::warn!(%remote, "SRT connection rejected: no streamid");
                        continue;
                    };
                    let socket = match request.accept(key_settings.clone()).await {
                        Ok(s) => s,
                        Err(e) => {
                            tracing::warn!(?e, %remote, "SRT accept failed");
                            continue;
                        }
                    };
                    tracing::info!(%remote, stream = %name, "SRT publisher connected");
                    let conn = SrtConnectionIngester::new(socket);
                    let wrapped = wrap(&name, conn);
                    if !registry.try_publish_ingester(&name, wrapped) {
                        tracing::warn!(stream = %name, %remote, "SRT publisher rejected: stream already live");
                    }
                }
            }
        }
    }
}
