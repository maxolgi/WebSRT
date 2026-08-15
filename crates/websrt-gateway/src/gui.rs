//! eframe (egui) GUI for the gateway.
//!
//! Shows a config form mirroring the CLI options, a Start/Stop button pair,
//! live stats from `GatewayStatsHandle`, and a scrolling log panel fed by the
//! `LogBuffer` tracing layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use websrt::{GatewayStats, GatewayStatsHandle};

use crate::log_buffer::LogBuffer;
use crate::{CertMode, Cli};

/// Messages from the spawned gateway task to the GUI thread.
enum GatewayMessage {
    /// Gateway setup succeeded, now running. Includes stats handle + shutdown trigger.
    Started(GatewayStatsHandle, Arc<Notify>),
    /// Gateway exited cleanly (Stop button or ctrl-c).
    Stopped,
    /// Gateway failed — setup error, bind error, or runtime crash.
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Gateway,
    Logs,
}

/// Form state with `String` fields (egui edits `&mut String`, not `PathBuf`).
#[derive(Serialize, Deserialize)]
struct GuiConfig {
    srt_port: u16,
    srt_streamid: String,
    wt_port: u16,
    bind: String,
    cert_mode: CertMode,
    cert_pem: String,
    key_pem: String,
    latency: u64,
    srt_passphrase: String,
    health_port: u16,
    health_bind: String,
    auth_token: String,
    no_web: bool,
    web_port: u16,
    web_bind: String,
    max_viewers: usize,
    max_bandwidth: u64,
    #[cfg(feature = "sim-loss")]
    #[serde(default)]
    sim_loss: u8,
    #[cfg(feature = "sim-loss")]
    #[serde(default)]
    sim_seed: u64,
}

fn opt_string(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn opt_path(s: &str) -> Option<PathBuf> {
    opt_string(s).map(PathBuf::from)
}

impl GuiConfig {
    fn config_dir() -> std::path::PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        std::path::PathBuf::from(format!("{home}/.config/websrt"))
    }

    fn config_path() -> std::path::PathBuf {
        Self::config_dir().join("gateway-config.json")
    }

    fn save_to_file(&self) {
        let path = Self::config_path();
        if let Err(e) = std::fs::create_dir_all(path.parent().unwrap_or(std::path::Path::new(".")))
        {
            tracing::warn!(?e, "failed to create config dir");
            return;
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, &json) {
                    tracing::warn!(?e, "failed to save config");
                }
            }
            Err(e) => tracing::warn!(?e, "failed to serialize config"),
        }
    }

    fn load_from_file() -> Option<Self> {
        let path = Self::config_path();
        let json = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str::<Self>(&json) {
            Ok(config) => {
                tracing::info!("loaded config from {}", path.display());
                Some(config)
            }
            Err(e) => {
                tracing::warn!(?e, "failed to parse saved config, using defaults");
                None
            }
        }
    }

    fn from_cli(cli: &Cli) -> Self {
        Self {
            srt_port: cli.srt_port,
            srt_streamid: cli.srt_streamid.clone().unwrap_or_default(),
            wt_port: cli.wt_port,
            bind: "0.0.0.0".to_string(),
            cert_mode: cli.cert_mode,
            cert_pem: cli
                .cert_pem
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            key_pem: cli
                .key_pem
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            latency: cli.latency,
            srt_passphrase: cli.srt_passphrase.clone().unwrap_or_default(),
            health_port: cli.health_port,
            health_bind: cli.health_bind.clone(),
            auth_token: cli.auth_token.clone().unwrap_or_default(),
            no_web: cli.no_web,
            web_port: cli.web_port,
            web_bind: "0.0.0.0".to_string(),
            max_viewers: cli.max_viewers,
            max_bandwidth: cli.max_bandwidth,
            #[cfg(feature = "sim-loss")]
            sim_loss: cli.sim_loss,
            #[cfg(feature = "sim-loss")]
            sim_seed: cli.sim_seed,
        }
    }

    fn to_cli(&self) -> Cli {
        Cli {
            no_gui: false,
            input: crate::InputMode::Srt,
            fixture: std::path::PathBuf::from("fixtures/test.ts"),
            fixture_duration: 10.0,
            srt_port: self.srt_port,
            srt_mode: crate::SrtMode::Listener,
            srt_call: None,
            srt_streamid: opt_string(&self.srt_streamid),
            wt_port: self.wt_port,
            bind: self.bind.clone(),
            cert_mode: self.cert_mode,
            cert_pem: opt_path(&self.cert_pem),
            key_pem: opt_path(&self.key_pem),
            latency: self.latency,
            srt_passphrase: opt_string(&self.srt_passphrase),
            health_port: self.health_port,
            health_bind: self.health_bind.clone(),
            auth_token: opt_string(&self.auth_token),
            no_web: self.no_web,
            web_port: self.web_port,
            web_bind: self.web_bind.clone(),
            web_root: None,
            max_viewers: self.max_viewers,
            max_bandwidth: self.max_bandwidth,
            #[cfg(feature = "sim-loss")]
            sim_loss: self.sim_loss,
            #[cfg(feature = "sim-loss")]
            sim_seed: self.sim_seed,
        }
    }
}

pub struct GuiApp {
    config: GuiConfig,
    /// Owned runtime — taken out in `Drop` for `shutdown_timeout`.
    runtime: Option<tokio::runtime::Runtime>,
    /// Cheap clone for spawning tasks from the GUI thread.
    handle: tokio::runtime::Handle,
    log_buffer: Arc<LogBuffer>,

    state: RunState,
    msg_rx: Option<std::sync::mpsc::Receiver<GatewayMessage>>,
    stats_handle: Option<GatewayStatsHandle>,
    shutdown: Option<Arc<Notify>>,

    stats: Option<GatewayStats>,
    last_stats_poll: Instant,
    error: Option<String>,

    /// Current text selection in the log panel, if any.
    /// Updated each frame from the TextEdit's persisted cursor state.
    log_selection: Option<String>,

    tab: Tab,
}

impl GuiApp {
    pub fn new(
        cli: Cli,
        runtime: tokio::runtime::Runtime,
        log_buffer: Arc<LogBuffer>,
        _cc: &eframe::CreationContext<'_>,
    ) -> Self {
        let handle = runtime.handle().clone();
        let config = GuiConfig::load_from_file().unwrap_or_else(|| GuiConfig::from_cli(&cli));
        let mut app = Self {
            config,
            runtime: Some(runtime),
            handle,
            log_buffer,
            state: RunState::Stopped,
            msg_rx: None,
            stats_handle: None,
            shutdown: None,
            stats: None,
            last_stats_poll: Instant::now(),
            error: None,
            log_selection: None,
            tab: Tab::Gateway,
        };
        app.start();
        app
    }

    fn start(&mut self) {
        self.config.save_to_file();
        let cli = self.config.to_cli();
        let (tx, rx) = std::sync::mpsc::channel();
        self.msg_rx = Some(rx);
        self.state = RunState::Starting;
        self.error = None;

        self.handle.spawn(async move {
            let shutdown = Arc::new(Notify::new());
            match crate::run_gateway(cli, shutdown.clone()).await {
                Ok((stats_handle, gateway_task)) => {
                    let _ = tx.send(GatewayMessage::Started(stats_handle, shutdown.clone()));
                    // Await the gateway run loop — reports bind errors, crashes, etc.
                    match gateway_task.await {
                        Ok(Ok(())) => {
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let _ = tx.send(GatewayMessage::Stopped);
                        }
                        Ok(Err(e)) => {
                            shutdown.notify_waiters();
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let _ = tx.send(GatewayMessage::Error(format!("{e}")));
                        }
                        Err(e) => {
                            shutdown.notify_waiters();
                            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            let _ = tx.send(GatewayMessage::Error(format!("gateway task: {e}")));
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(GatewayMessage::Error(e.to_string()));
                }
            }
        });
    }

    fn stop(&mut self) {
        if let Some(shutdown) = &self.shutdown {
            shutdown.notify_waiters();
        }
        self.state = RunState::Stopping;
    }

    /// Non-blocking state machine poll — called every frame.
    fn poll(&mut self) {
        // 1. Drain all pending messages from the gateway task.
        loop {
            let msg = self.msg_rx.as_ref().and_then(|rx| rx.try_recv().ok());
            match msg {
                Some(GatewayMessage::Started(stats_handle, shutdown)) => {
                    self.stats_handle = Some(stats_handle);
                    self.shutdown = Some(shutdown.clone());
                    if self.state == RunState::Stopping {
                        // User clicked Stop before gateway finished starting.
                        shutdown.notify_waiters();
                    } else {
                        self.state = RunState::Running;
                        self.last_stats_poll = Instant::now();
                    }
                }
                Some(GatewayMessage::Error(e)) => {
                    self.error = Some(e);
                    self.stats_handle = None;
                    self.shutdown = None;
                    self.stats = None;
                    self.state = RunState::Stopped;
                    self.msg_rx = None;
                }
                Some(GatewayMessage::Stopped) => {
                    self.stats_handle = None;
                    self.shutdown = None;
                    self.stats = None;
                    self.state = RunState::Stopped;
                    self.msg_rx = None;
                }
                None => break,
            }
        }

        // 2. Poll stats (every 500ms while running)
        if self.state == RunState::Running
            && self.last_stats_poll.elapsed() > Duration::from_millis(500)
        {
            let new_stats = self.stats_handle.as_ref().map(|h| h.stats());
            if let Some(s) = new_stats {
                self.stats = Some(s);
            }
            self.last_stats_poll = Instant::now();
        }
    }
}

impl Drop for GuiApp {
    fn drop(&mut self) {
        if let Some(shutdown) = &self.shutdown {
            shutdown.notify_waiters();
        }
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(5));
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll();

        // Repaint while in a transitional or running state
        if self.state != RunState::Stopped {
            ctx.request_repaint_after(Duration::from_millis(200));
        }

        // Tab bar + Start/Stop buttons (always visible)
        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Gateway, "Gateway");
                ui.selectable_value(&mut self.tab, Tab::Logs, "Logs");

                // Start/Stop buttons pushed to the right edge
                let running = self.state == RunState::Running;
                let starting = self.state == RunState::Starting;
                let stopped = self.state == RunState::Stopped && self.msg_rx.is_none();

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let stop_btn =
                        egui::Button::new(egui::RichText::new(" \u{25A0} Stop ").strong());
                    if ui.add_enabled(running || starting, stop_btn).clicked() {
                        self.stop();
                    }
                    let start_btn =
                        egui::Button::new(egui::RichText::new(" \u{25B6} Start ").strong());
                    if ui.add_enabled(stopped, start_btn).clicked() {
                        self.start();
                    }
                });
            });
            ui.add_space(2.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Gateway => {
                ui.add_space(4.0);
                ui.heading("WebSRT Gateway");
                ui.add_space(6.0);

                let editable = self.state == RunState::Stopped && self.msg_rx.is_none();

                draw_config_form(ui, &mut self.config, editable);

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(6.0);

                // Error
                if let Some(ref e) = self.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 80, 80), format!("Error: {e}"));
                    ui.add_space(4.0);
                }

                // Live stats
                if let Some(ref stats) = self.stats {
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        ui.label(format!(
                            "Streams: {}/{}  Viewers: {}  Sessions: {}  Max viewers: {}",
                            stats.alive_streams,
                            stats.streams,
                            stats.total_viewers,
                            stats.active_sessions,
                            stats.max_viewers,
                        ));
                    });
                    if !stats.per_stream.is_empty() {
                        ui.add_space(2.0);
                        let host = web_host(&self.config.web_bind);
                        let web_port = self.config.web_port;
                        let web_enabled = !self.config.no_web;
                        egui::Grid::new("stream_stats")
                            .num_columns(4)
                            .spacing([16.0, 2.0])
                            .striped(true)
                            .show(ui, |ui| {
                                ui.small("stream");
                                ui.small("viewers");
                                ui.small("msgs");
                                ui.small("drops");
                                ui.end_row();
                                for s in &stats.per_stream {
                                    let link = egui::Label::new(
                                        egui::RichText::new(&s.name)
                                            .small()
                                            .color(egui::Color32::from_rgb(100, 149, 237)),
                                    )
                                    .sense(egui::Sense::click());
                                    let resp = ui.add_enabled(web_enabled, link);
                                    if resp.clicked() {
                                        let url =
                                            format!("https://{host}:{web_port}/?stream={}", s.name);
                                        ui.ctx().open_url(egui::OpenUrl::new_tab(url));
                                    }
                                    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
                                        .on_hover_text("Open viewer in browser");
                                    ui.small(format!(
                                        "{}{}",
                                        s.viewers,
                                        if s.alive { "" } else { " (dead)" }
                                    ));
                                    ui.small(format!("{}", s.messages_sent));
                                    ui.small(format!("{}", s.send_failures));
                                    ui.end_row();
                                }
                            });
                    }
                    if !stats.per_session.is_empty() {
                        ui.add_space(2.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.small(format!(
                                "Ticker: avg {}µs / max {}µs ({} ticks)",
                                stats.ticker_avg_us, stats.ticker_max_us, stats.ticker_count,
                            ));
                        });
                        ui.add_space(2.0);
                        egui::Grid::new("session_stats")
                            .num_columns(7)
                            .spacing([16.0, 2.0])
                            .striped(true)
                            .show(ui, |ui| {
                                ui.small("session");
                                ui.small("stream");
                                ui.small("tx_data");
                                ui.small("tx_buf");
                                ui.small("tx_retx");
                                ui.small("rtt_ms");
                                ui.small("pushed");
                                ui.end_row();
                                for s in &stats.per_session {
                                    let srt = s.srt.as_ref();
                                    ui.small(format!("#{}", s.session_id));
                                    ui.small(&s.stream_name);
                                    ui.small(format!("{}", srt.map(|v| v.tx_data).unwrap_or(0)));
                                    ui.small(format!(
                                        "{}",
                                        srt.map(|v| v.tx_buffered).unwrap_or(0)
                                    ));
                                    ui.small(format!(
                                        "{}",
                                        srt.map(|v| v.tx_retransmit).unwrap_or(0)
                                    ));
                                    ui.small(format!(
                                        "{}",
                                        srt.and_then(|v| {
                                            v.tx_rtt.map(|d| d.as_millis() as u64)
                                        })
                                        .unwrap_or(0)
                                    ));
                                    ui.small(format!("{}", s.messages_pushed));
                                    ui.end_row();
                                }
                            });
                    }
                }
            }
            Tab::Logs => {
                let lines = self.log_buffer.recent(200);
                let log_text = lines.join("\n");
                let has_logs = !log_text.is_empty();

                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Logs").strong());
                    if ui
                        .add_enabled(has_logs, egui::Button::new("Copy"))
                        .clicked()
                    {
                        let text = self
                            .log_selection
                            .clone()
                            .unwrap_or_else(|| log_text.clone());
                        ctx.copy_text(text);
                    }
                    if ui
                        .add_enabled(has_logs, egui::Button::new("Clear"))
                        .clicked()
                    {
                        self.log_buffer.clear();
                        self.log_selection = None;
                    }
                });
                egui::ScrollArea::vertical()
                    .max_height(ui.available_height() - 4.0)
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        if has_logs {
                            let mut text_ref = log_text.as_str();
                            let output = egui::TextEdit::multiline(&mut text_ref)
                                .frame(false)
                                .desired_width(f32::MAX)
                                .font(egui::FontId::monospace(12.0))
                                .show(ui);
                            if let Some(cr) = output.state.cursor.range(&output.galley) {
                                if cr.is_empty() {
                                    self.log_selection = None;
                                } else {
                                    self.log_selection = Some(cr.slice_str(&log_text).to_owned());
                                }
                            }
                        } else {
                            ui.small("(no logs yet)");
                        }
                    });
            }
        });
    }
}

// -- free functions (avoid &mut self borrow conflicts inside egui closures) --

fn draw_config_form(ui: &mut egui::Ui, config: &mut GuiConfig, enabled: bool) {
    egui::Grid::new("config_grid")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            // SRT listener params
            ui.label("SRT listen port:");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.srt_port).range(1..=65535),
            );
            ui.end_row();

            ui.label("SRT streamid:");
            ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut config.srt_streamid).desired_width(200.0),
            );
            ui.end_row();

            ui.label("SRT latency (ms):");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.latency).range(1..=10_000),
            );
            ui.end_row();

            ui.label("Max bandwidth (kbps):");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.max_bandwidth).range(0..=1_000_000),
            )
            .on_hover_text("0 = unlimited");
            ui.end_row();

            ui.label("SRT passphrase:");
            ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut config.srt_passphrase)
                    .password(true)
                    .desired_width(200.0),
            );
            ui.end_row();

            // WebTransport
            ui.label("WT port:");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.wt_port).range(1..=65535),
            );
            ui.end_row();

            ui.label("WT bind addr:");
            ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut config.bind).desired_width(160.0),
            );
            ui.end_row();

            ui.label("Max viewers/stream:");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.max_viewers).range(1..=10_000),
            );
            ui.end_row();

            // Web server
            ui.label("Web UI:");
            ui.add_enabled(enabled, egui::Checkbox::new(&mut config.no_web, "disable"));
            ui.end_row();

            ui.label("Web HTTPS port:");
            ui.add_enabled(
                enabled,
                egui::DragValue::new(&mut config.web_port).range(0..=65535),
            );
            ui.end_row();

            ui.label("Web bind addr:");
            ui.add_enabled(
                enabled,
                egui::TextEdit::singleline(&mut config.web_bind).desired_width(160.0),
            );
            ui.end_row();

            // Cert mode
            ui.label("Cert mode:");
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("cert_mode")
                    .selected_text(cert_mode_label(config.cert_mode))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut config.cert_mode, CertMode::Self_, "Self-signed");
                        ui.selectable_value(
                            &mut config.cert_mode,
                            CertMode::Mkcert,
                            "mkcert (PEM)",
                        );
                    });
            });
            ui.end_row();

            if config.cert_mode == CertMode::Mkcert {
                ui.label("Cert PEM:");
                ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut config.cert_pem).desired_width(280.0),
                );
                ui.end_row();

                ui.label("Key PEM:");
                ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut config.key_pem).desired_width(280.0),
                );
                ui.end_row();
            }
        });

    // Advanced section
    ui.add_space(4.0);
    egui::CollapsingHeader::new("Advanced")
        .default_open(false)
        .show(ui, |ui| {
            egui::Grid::new("advanced_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Auth token:");
                    ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut config.auth_token)
                            .password(true)
                            .desired_width(200.0),
                    );
                    ui.end_row();

                    ui.label("Health port:");
                    ui.add_enabled(
                        enabled,
                        egui::DragValue::new(&mut config.health_port).range(0..=65535),
                    );
                    ui.end_row();

                    ui.label("Health bind addr:");
                    ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut config.health_bind).desired_width(160.0),
                    );
                    ui.end_row();
                });

            #[cfg(feature = "sim-loss")]
            {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new("Simulated Loss (testing only)")
                        .small()
                        .color(egui::Color32::from_rgb(220, 180, 60)),
                );
                egui::Grid::new("simloss_grid")
                    .num_columns(2)
                    .spacing([10.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Loss %:");
                        ui.add_enabled(
                            enabled,
                            egui::DragValue::new(&mut config.sim_loss).range(0..=100),
                        );
                        ui.end_row();

                        ui.label("Seed:");
                        ui.add_enabled(enabled, egui::DragValue::new(&mut config.sim_seed));
                        ui.end_row();
                    });
            }
        });
}

fn cert_mode_label(m: CertMode) -> &'static str {
    match m {
        CertMode::Self_ => "Self-signed",
        CertMode::Mkcert => "mkcert (PEM)",
    }
}

fn web_host(web_bind: &str) -> String {
    match web_bind {
        "0.0.0.0" | "::" | "" => local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".to_string()),
        other => other.to_string(),
    }
}
