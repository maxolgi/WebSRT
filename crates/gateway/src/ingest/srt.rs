//! SrtIngester: srt-tokio listener or caller for OBS.
//!
//! Two modes:
//!   - **Listener** (default, `--srt-mode listener`): binds a UDP port, waits
//!     for OBS to call in with `?mode=caller`.
//!   - **Caller** (`--srt-mode caller`): dials OBS at the given address. Use
//!     this when OBS is configured as `srt://...?mode=listener`.
//!
//! Supports automatic reconnection: when the SRT socket closes (OBS
//! disconnect/restart), the ingester waits for a new connection rather than
//! signalling end-of-stream.

use super::{Ingester, TsMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures::StreamExt;
use srt_tokio::{SrtIncoming, SrtListener, SrtSocket};
use std::time::Duration;

enum Kind {
    Listener(SrtListener, SrtIncoming),
    Caller(String),
}

pub struct SrtIngester {
    kind: Kind,
    socket: Option<SrtSocket>,
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
        let socket = Self::accept_one(&mut incoming).await?;
        Ok(Self {
            kind: Kind::Listener(listener, incoming),
            socket: Some(socket),
        })
    }

    async fn accept_one(incoming: &mut SrtIncoming) -> Result<SrtSocket> {
        loop {
            let request = incoming
                .incoming()
                .next()
                .await
                .ok_or_else(|| anyhow!("srt listener closed"))?;
            let remote = request.remote();
            let stream_id = request.stream_id().map(|s| s.to_string());
            tracing::info!(%remote, ?stream_id, "SRT connection accepted from OBS");
            match request.accept(None).await {
                Ok(socket) => return Ok(socket),
                Err(e) => {
                    tracing::warn!(?e, "SRT accept failed; retrying");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    /// Caller mode: dial OBS at `addr` (e.g. "192.168.1.3:1234").
    pub async fn call(addr: impl AsRef<str>) -> Result<Self> {
        let addr_str = addr.as_ref().to_string();
        tracing::info!(addr = %addr_str, "SRT caller: dialing OBS…");
        let socket = Self::dial(&addr_str).await?;
        Ok(Self {
            kind: Kind::Caller(addr_str),
            socket: Some(socket),
        })
    }

    async fn dial(addr: &str) -> Result<SrtSocket> {
        let socket_addr: srt_protocol::options::SocketAddress = addr
            .try_into()
            .map_err(|e| anyhow!("invalid SRT address {addr}: {e:?}"))?;
        let socket = SrtSocket::builder()
            .call(socket_addr, None)
            .await
            .map_err(|e| anyhow!("srt call to {addr}: {e}"))?;
        tracing::info!(addr, "SRT caller: connected to OBS");
        Ok(socket)
    }

    async fn reconnect(&mut self) -> Result<SrtSocket> {
        match &mut self.kind {
            Kind::Listener(_, incoming) => {
                tracing::info!("SRT: OBS disconnected; waiting for reconnect…");
                Self::accept_one(incoming).await
            }
            Kind::Caller(addr) => {
                tracing::info!(addr, "SRT caller: re-dialing OBS…");
                loop {
                    match Self::dial(addr).await {
                        Ok(s) => return Ok(s),
                        Err(e) => {
                            tracing::warn!(?e, addr, "SRT reconnect failed; retrying in 2s");
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }
        }
    }
}

#[async_trait]
impl Ingester for SrtIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        loop {
            if let Some(ref mut socket) = self.socket {
                match socket.next().await {
                    Some(Ok(msg)) => return Ok(Some(msg)),
                    Some(Err(e)) => {
                        tracing::warn!(?e, "srt recv error; will attempt reconnect");
                        self.socket = None;
                    }
                    None => {
                        tracing::info!("srt socket closed; attempting reconnect");
                        self.socket = None;
                    }
                }
            }
            match self.reconnect().await {
                Ok(s) => self.socket = Some(s),
                Err(e) => {
                    tracing::error!(?e, "reconnect failed; retrying in 2s");
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    }
}
