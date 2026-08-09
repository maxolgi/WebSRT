//! eframe (egui) GUI for the gateway.
//!
//! Shows a config form mirroring the CLI options, a Start/Stop button pair,
//! live stats from `GatewayStatsHandle`, and a scrolling log panel fed by the
//! `LogBuffer` tracing layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use eframe::egui;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use websrt::{GatewayStats, GatewayStatsHandle};

use crate::log_buffer::LogBuffer;
use crate::{CertMode, Cli, InputMode, SrtMode};

/// Result from the spawned `run_gateway` task.
type StartResult = Result<(GatewayStatsHandle, JoinHandle<anyhow::Result<()>>, Arc<Notify>), String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

/// Form state with `String` fields (egui edits `&mut String`, not `PathBuf`).
struct GuiConfig {
    input: InputMode,
    fixture: String,
    fixture_duration: f64,
    srt_port: u16,
    srt_mode: SrtMode,
    srt_call: String,
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
    #[cfg(feature = "sim-loss")]
    sim_loss: u8,
    #[cfg(feature = "sim-loss")]
    sim_seed: u64,
}

fn opt_string(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

fn opt_path(s: &str) -> Option<PathBuf> {
    opt_string(s).map(PathBuf::from)
}

impl GuiConfig {
    fn from_cli(cli: &Cli) -> Self {
        Self {
            input: cli.input,
            fixture: cli.fixture.display().to_string(),
            fixture_duration: cli.fixture_duration,
            srt_port: cli.srt_port,
            srt_mode: cli.srt_mode,
            srt_call: cli.srt_call.clone().unwrap_or_default(),
            srt_streamid: cli.srt_streamid.clone().unwrap_or_default(),
            wt_port: cli.wt_port,
            bind: cli.bind.clone(),
            cert_mode: cli.cert_mode,
            cert_pem: cli.cert_pem.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            key_pem: cli.key_pem.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            latency: cli.latency,
            srt_passphrase: cli.srt_passphrase.clone().unwrap_or_default(),
            health_port: cli.health_port,
            health_bind: cli.health_bind.clone(),
            auth_token: cli.auth_token.clone().unwrap_or_default(),
            #[cfg(feature = "sim-loss")]
            sim_loss: cli.sim_loss,
            #[cfg(feature = "sim-loss")]
            sim_seed: cli.sim_seed,
        }
    }

    fn to_cli(&self) -> Cli {
        Cli {
            no_gui: false,
            input: self.input,
            fixture: PathBuf::from(&self.fixture),
            fixture_duration: self.fixture_duration,
            srt_port: self.srt_port,
            srt_mode: self.srt_mode,
            srt_call: opt_string(&self.srt_call),
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
    startup_rx: Option<std::sync::mpsc::Receiver<StartResult>>,
    stats_handle: Option<GatewayStatsHandle>,
    shutdown: Option<Arc<Notify>>,
    task: Option<JoinHandle<anyhow::Result<()>>>,

    stats: Option<GatewayStats>,
    last_stats_poll: Instant,
    error: Option<String>,
}

impl GuiApp {
    pub fn new(
        cli: Cli,
        runtime: tokio::runtime::Runtime,
        log_buffer: Arc<LogBuffer>,
        _cc: &eframe::CreationContext<'_>,
    ) -> Self {
        let handle = runtime.handle().clone();
        let config = GuiConfig::from_cli(&cli);
        Self {
            config,
            runtime: Some(runtime),
            handle,
            log_buffer,
            state: RunState::Stopped,
            startup_rx: None,
            stats_handle: None,
            shutdown: None,
            task: None,
            stats: None,
            last_stats_poll: Instant::now(),
            error: None,
        }
    }

    fn start(&mut self) {
        let cli = self.config.to_cli();
        let (tx, rx) = std::sync::mpsc::channel();
        self.startup_rx = Some(rx);
        self.state = RunState::Starting;
        self.error = None;

        self.handle.spawn(async move {
            let shutdown = Arc::new(Notify::new());
            let result = crate::run_gateway(cli, shutdown.clone()).await;
            match result {
                Ok((stats_handle, task)) => {
                    let _ = tx.send(Ok((stats_handle, task, shutdown)));
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                }
            }
        });
    }

    fn stop(&mut self) {
        if let Some(shutdown) = &self.shutdown {
            shutdown.notify_one();
        }
        self.state = RunState::Stopping;
    }

    /// Non-blocking state machine poll — called every frame.
    fn poll(&mut self) {
        // 1. Check startup result (Starting → Running or Error)
        let startup_recv = self.startup_rx.as_ref().and_then(|rx| {
            match rx.try_recv() {
                Ok(v) => Some(Ok(v)),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Err(())),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
            }
        });
        if let Some(result) = startup_recv {
            match result {
                Ok(Ok((stats_handle, task, shutdown))) => {
                    self.stats_handle = Some(stats_handle);
                    self.task = Some(task);
                    self.shutdown = Some(shutdown);
                    self.startup_rx = None;
                    self.state = RunState::Running;
                    self.last_stats_poll = Instant::now();
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.startup_rx = None;
                    self.state = RunState::Stopped;
                }
                Err(()) => {
                    self.error = Some("gateway startup task failed".to_string());
                    self.startup_rx = None;
                    self.state = RunState::Stopped;
                }
            }
        }

        // 2. Check stopping completion (Stopping → Stopped)
        let task_done = self
            .task
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(false);
        if task_done && self.state == RunState::Stopping {
            self.task = None;
            self.shutdown = None;
            self.stats_handle = None;
            self.stats = None;
            self.state = RunState::Stopped;
        }

        // 3. Poll stats (every 500ms while running)
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
            shutdown.notify_one();
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

        eframe::egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("WebSRT Gateway");
            ui.add_space(6.0);

            let editable = self.state == RunState::Stopped && self.startup_rx.is_none();

            draw_config_form(ui, &mut self.config, editable);

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            // Start/Stop buttons
            let running = self.state == RunState::Running;
            let starting = self.state == RunState::Starting;
            let stopping = self.state == RunState::Stopping;
            let stopped = self.state == RunState::Stopped && self.startup_rx.is_none();

            ui.horizontal(|ui| {
                ui.add_space(40.0);
                let start_btn = egui::Button::new(
                    egui::RichText::new(" \u{25B6} Start ").strong(),
                );
                if ui.add_enabled(stopped, start_btn).clicked() {
                    self.start();
                }
                ui.add_space(20.0);
                let stop_btn = egui::Button::new(
                    egui::RichText::new(" \u{25A0} Stop ").strong(),
                );
                if ui.add_enabled(running || starting, stop_btn).clicked() {
                    self.stop();
                }
            });

            // Status line
            ui.add_space(4.0);
            let (dot, label) = match self.state {
                RunState::Stopped => ("\u{25CF}", "Stopped"),
                RunState::Starting => ("\u{25CF}", "Starting\u{2026}"),
                RunState::Running => ("\u{25CF}", "Running"),
                RunState::Stopping => ("\u{25CF}", "Stopping\u{2026}"),
            };
            let color = match self.state {
                RunState::Running => egui::Color32::from_rgb(80, 200, 80),
                RunState::Starting | RunState::Stopping => egui::Color32::from_rgb(220, 180, 60),
                RunState::Stopped => egui::Color32::from_rgb(140, 140, 140),
            };
            ui.horizontal(|ui| {
                ui.colored_label(color, dot);
                ui.label(label);
                if let Some(ref e) = self.error {
                    ui.colored_label(egui::Color32::from_rgb(230, 80, 80), format!("Error: {e}"));
                }
            });

            // Live stats
            if let Some(ref stats) = self.stats {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(format!(
                        "Streams: {}/{}  Viewers: {}  Sessions: {}  Max viewers: {}",
                        stats.alive_streams, stats.streams, stats.total_viewers, stats.active_sessions, stats.max_viewers,
                    ));
                });
                if !stats.per_stream.is_empty() {
                    ui.add_space(2.0);
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
                                ui.small(&s.name);
                                ui.small(format!("{}{}", s.viewers, if s.alive { "" } else { " (dead)" }));
                                ui.small(format!("{}", s.messages_sent));
                                ui.small(format!("{}", s.send_failures));
                                ui.end_row();
                            }
                        });
                }
            }

            // Suppress unused warning when not stopping
            let _ = stopping;

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(4.0);

            // Log panel
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Logs").strong());
            });
            egui::ScrollArea::vertical()
                .max_height(ui.available_height() - 4.0)
                .stick_to_bottom(true)
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    let lines = self.log_buffer.recent(200);
                    if lines.is_empty() {
                        ui.small("(no logs yet)");
                    }
                    for line in &lines {
                        let color = log_color(line);
                        ui.monospace(
                            egui::RichText::new(line)
                                .color(color)
                                .size(12.0),
                        );
                    }
                });
        });
    }
}

// -- free functions (avoid &mut self borrow conflicts inside egui closures) --

fn draw_config_form(ui: &mut egui::Ui, config: &mut GuiConfig, enabled: bool) {
    egui::Grid::new("config_grid")
        .num_columns(2)
        .spacing([10.0, 6.0])
        .show(ui, |ui| {
            // Input source
            ui.label("Input Source:");
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("input")
                    .selected_text(input_label(config.input))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut config.input, InputMode::File, "File (fixture)");
                        ui.selectable_value(&mut config.input, InputMode::Srt, "SRT (OBS)");
                    });
            });
            ui.end_row();

            // Input-specific fields
            match config.input {
                InputMode::File => {
                    ui.label("Fixture path:");
                    ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut config.fixture)
                            .desired_width(280.0)
                            .hint_text("fixtures/test.ts"),
                    );
                    ui.end_row();

                    ui.label("Fixture duration (s):");
                    ui.add_enabled(
                        enabled,
                        egui::DragValue::new(&mut config.fixture_duration)
                            .speed(0.1)
                            .range(0.1..=100_000.0),
                    );
                    ui.end_row();
                }
                InputMode::Srt => {
                    ui.label("SRT mode:");
                    ui.add_enabled_ui(enabled, |ui| {
                        egui::ComboBox::from_id_salt("srt_mode")
                            .selected_text(srt_mode_label(config.srt_mode))
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut config.srt_mode, SrtMode::Listener, "Listener");
                                ui.selectable_value(&mut config.srt_mode, SrtMode::Caller, "Caller");
                            });
                    });
                    ui.end_row();

                    if config.srt_mode == SrtMode::Listener {
                        ui.label("SRT listen port:");
                        ui.add_enabled(
                            enabled,
                            egui::DragValue::new(&mut config.srt_port).range(1..=65535),
                        );
                        ui.end_row();
                    } else {
                        ui.label("SRT call addr:");
                        ui.add_enabled(
                            enabled,
                            egui::TextEdit::singleline(&mut config.srt_call)
                                .desired_width(200.0)
                                .hint_text("192.168.1.3:1234"),
                        );
                        ui.end_row();
                    }

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

                    ui.label("SRT passphrase:");
                    ui.add_enabled(
                        enabled,
                        egui::TextEdit::singleline(&mut config.srt_passphrase)
                            .password(true)
                            .desired_width(200.0),
                    );
                    ui.end_row();
                }
            }

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

            // Cert mode
            ui.label("Cert mode:");
            ui.add_enabled_ui(enabled, |ui| {
                egui::ComboBox::from_id_salt("cert_mode")
                    .selected_text(cert_mode_label(config.cert_mode))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut config.cert_mode, CertMode::Self_, "Self-signed");
                        ui.selectable_value(&mut config.cert_mode, CertMode::Mkcert, "mkcert (PEM)");
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
                        ui.add_enabled(
                            enabled,
                            egui::DragValue::new(&mut config.sim_seed),
                        );
                        ui.end_row();
                    });
            }
        });
}

fn input_label(m: InputMode) -> &'static str {
    match m {
        InputMode::File => "File (fixture)",
        InputMode::Srt => "SRT (OBS)",
    }
}

fn srt_mode_label(m: SrtMode) -> &'static str {
    match m {
        SrtMode::Listener => "Listener",
        SrtMode::Caller => "Caller",
    }
}

fn cert_mode_label(m: CertMode) -> &'static str {
    match m {
        CertMode::Self_ => "Self-signed",
        CertMode::Mkcert => "mkcert (PEM)",
    }
}

fn log_color(line: &str) -> egui::Color32 {
    if line.contains("ERROR") {
        egui::Color32::from_rgb(240, 100, 100)
    } else if line.contains("WARN") {
        egui::Color32::from_rgb(230, 190, 90)
    } else if line.contains("DEBUG") {
        egui::Color32::from_gray(140)
    } else if line.contains("TRACE") {
        egui::Color32::from_gray(100)
    } else {
        egui::Color32::from_gray(200)
    }
}
