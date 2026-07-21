//! SRT-over-WebTransport gateway library.
//!
//! Bridges native SRT input to browser-side SRT receivers over WebTransport
//! datagrams. Each browser gets its own independent SRT sender with full
//! NAK/retransmit support.
//!
//! ## Quick start
//!
//! ```no_run
//! use websrt::Gateway;
//! use websrt::cert::{Cert, CertSource};
//! use websrt::ingest::srt::SrtIngester;
//!
//! # async fn run() -> anyhow::Result<()> {
//! let cert = Cert::build(CertSource::SelfSigned { sans: vec!["localhost".into()] }).await?;
//!
//! let gateway = Gateway::builder()
//!     .bind_addr("127.0.0.1:4433".parse::<std::net::SocketAddr>()?)
//!     .identity(cert.identity.clone_identity())
//!     .latency_ms(1000)
//!     .build()?;
//!
//! // Deferred ingester: connect OBS in background
//! let source = gateway.source_handle();
//! tokio::spawn(async move {
//!     let ingester = SrtIngester::bind(9000).await.unwrap();
//!     source.publish_stream("default", ingester);
//! });
//!
//! gateway.run(async {
//!     let _ = tokio::signal::ctrl_c().await;
//! }).await?;
//! # Ok(())
//! # }
//! ```

pub mod broadcaster;
pub mod cert;
pub mod gateway;
pub mod hooks;
pub mod ingest;
mod registry;
pub mod session;
pub mod srt_sender;
pub mod stream_registry;

pub use broadcaster::{Broadcaster, ViewerRx};
pub use cert::{Cert, CertSource};
pub use gateway::{Gateway, GatewayBuilder, GatewaySourceHandle, GatewayStats, GatewayStatsHandle};
pub use hooks::{Decision, SessionPolicy, SessionRequest};
pub use ingest::{ChannelIngester, Ingester, TsMessage};
pub use session::BrowserSession;
pub use srt_sender::{SenderAction, SrtConfig, SrtInitiator};
pub use stream_registry::{StreamRegistry, StreamStats};
