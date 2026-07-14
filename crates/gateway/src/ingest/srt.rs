//! SrtIngester: srt-tokio listener or caller for OBS.
//!
//! Two modes:
//!   - **Listener** (default, `--srt-mode listener`): binds a UDP port, waits
//!     for OBS to call in with `?mode=caller`.
//!   - **Caller** (`--srt-mode caller`): dials OBS at the given address. Use
//!     this when OBS is configured as `srt://...?mode=listener`.

use super::{Ingester, TsMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use srt_tokio::{SrtListener, SrtSocket};

enum Kind {
    Listener(SrtListener),
    Caller,
}

pub struct SrtIngester {
    kind: Kind,
    socket: SrtSocket,
}

impl SrtIngester {
    /// Listener mode: bind `0.0.0.0:{port}`, wait for OBS to call in.
    pub async fn bind(port: u16) -> Result<Self> {
        Self::bind_with_addr(format!("0.0.0.0:{port}")).await
    }

    pub async fn bind_with_addr(addr: impl AsRef<str>) -> Result<Self> {
        let (listener, mut incoming) = SrtListener::builder()
            .bind(addr.as_ref())
            .await
            .map_err(|e| anyhow!("srt listener bind: {e}"))?;
        tracing::info!("SRT listener bound, awaiting OBS connection…");
        let request = incoming
            .incoming()
            .next()
            .await
            .ok_or_else(|| anyhow!("srt listener closed without a connection"))?;
        let remote = request.remote();
        let stream_id = request.stream_id().map(|s| s.to_string());
        tracing::info!(%remote, ?stream_id, "SRT connection accepted from OBS");
        let socket = request
            .accept(None)
            .await
            .map_err(|e| anyhow!("srt accept: {e}"))?;
        Ok(Self {
            kind: Kind::Listener(listener),
            socket,
        })
    }

    /// Caller mode: dial OBS at `addr` (e.g. "192.168.1.3:1234").
    pub async fn call(addr: impl AsRef<str>) -> Result<Self> {
        let addr_str = addr.as_ref().to_string();
        tracing::info!(addr = %addr_str, "SRT caller: dialing OBS…");
        let socket_addr: srt_protocol::options::SocketAddress = addr_str
            .as_str()
            .try_into()
            .map_err(|e| anyhow!("invalid SRT address {addr_str}: {e:?}"))?;
        let socket = SrtSocket::builder()
            .call(socket_addr, None)
            .await
            .map_err(|e| anyhow!("srt call to {addr_str}: {e}"))?;
        tracing::info!(addr = %addr_str, "SRT caller: connected to OBS");
        Ok(Self {
            kind: Kind::Caller,
            socket,
        })
    }
}

#[async_trait]
impl Ingester for SrtIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        match self.socket.next().await {
            Some(Ok(msg)) => Ok(Some(msg)),
            Some(Err(e)) => Err(anyhow!("srt recv: {e}")),
            None => Ok(None),
        }
    }
}
