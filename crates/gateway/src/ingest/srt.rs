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

    async fn close_socket(&mut self) {
        if let Some(mut socket) = self.socket.take() {
            match tokio::time::timeout(Duration::from_secs(5), socket.close_and_finish()).await {
                Ok(Ok(())) => tracing::info!("SRT socket closed cleanly"),
                Ok(Err(e)) => tracing::warn!(?e, "SRT socket close error"),
                Err(_) => tracing::warn!("SRT socket close timed out after 5s; dropping"),
            }
        }
    }
}

#[async_trait]
impl Ingester for SrtIngester {
    async fn next_message(&mut self) -> Result<Option<TsMessage>> {
        loop {
            if self.socket.is_none() {
                match self.reconnect().await {
                    Ok(s) => self.socket = Some(s),
                    Err(e) => {
                        tracing::error!(?e, "reconnect failed; retrying in 2s");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                continue;
            }

            let result = {
                let socket = self.socket.as_mut().unwrap();
                socket.next().await
            };

            match result {
                Some(Ok(msg)) => return Ok(Some(msg)),
                Some(Err(e)) => {
                    tracing::warn!(?e, "srt recv error; closing socket");
                    self.close_socket().await;
                }
                None => {
                    tracing::info!("srt socket closed; attempting reconnect");
                    self.close_socket().await;
                }
            }
        }
    }
}
