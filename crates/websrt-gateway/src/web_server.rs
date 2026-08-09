//! Built-in HTTPS web server using axum + axum-server + rust-embed.
//!
//! Embeds `web/dist/` at compile time (single self-contained binary). Serves
//! the web UI over HTTPS with the same self-signed cert used for WebTransport.
//! `cert-hash.js` is served dynamically (changes every boot).

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "$CARGO_MANIFEST_DIR/../../web/dist/"]
struct WebAsset;

/// Run the HTTPS web server. Blocks until the server stops, or until `shutdown`
/// fires (graceful shutdown). The caller should spawn this as a background task.
pub async fn run_web_server(
    bind: String,
    port: u16,
    cert_hash_js: String,
    cert_pem: Vec<u8>,
    key_pem: Vec<u8>,
    shutdown: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .context("invalid web server bind address")?;

    let tls_config = axum_server::tls_rustls::RustlsConfig::from_pem(cert_pem, key_pem)
        .await
        .context("failed to build TLS config for web server")?;

    let app = build_router(cert_hash_js);
    let handle = axum_server::Handle::new();

    tracing::info!(%addr, "HTTPS web server starting");

    // Spawn a watcher that shuts down the server when `shutdown` fires.
    let handle_clone = handle.clone();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        shutdown_clone.notified().await;
        tracing::info!("web server shutting down");
        handle_clone.shutdown();
    });

    axum_server::bind_rustls(addr, tls_config)
        .handle(handle)
        .serve(app.into_make_service())
        .await
        .context("HTTPS web server stopped with error")?;
    Ok(())
}

fn build_router(cert_hash_js: String) -> Router {
    Router::new()
        .route(
            "/cert-hash.js",
            get(move || {
                let js = cert_hash_js.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                        js,
                    )
                }
            }),
        )
        .fallback(serve_embedded)
}

/// Serve embedded static files from `web/dist/`. Falls back to `index.html`
/// for unknown paths (SPA-style).
async fn serve_embedded(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match WebAsset::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref())],
                Body::from(content.data.into_owned()),
            )
                .into_response()
        }
        None => {
            match WebAsset::get("index.html") {
                Some(content) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    Body::from(content.data.into_owned()),
                )
                    .into_response(),
                None => (StatusCode::NOT_FOUND, "web UI not built — run ./build.sh web build").into_response(),
            }
        }
    }
}
