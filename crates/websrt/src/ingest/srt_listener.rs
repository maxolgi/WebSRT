use super::{build_key_settings, Ingester, SrtConnectionIngester};
use crate::stream_registry::StreamRegistry;
use anyhow::{anyhow, Result};
use futures::StreamExt;
use srt_protocol::options::ByteCount;
use srt_protocol::settings::KeySettings;
use srt_tokio::{SrtIncoming, SrtListener};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

const UDP_BUF_SIZE: ByteCount = ByteCount(8_388_608);

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
