//! Beambench PC application — Axum server with WebSocket and embedded frontend.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use axum::{
    Router,
    extract::{
        Multipart, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Json},
    routing::{get, post},
};
use clap::Parser;
use rust_embed::Embed;
use tokio::sync::{Mutex, broadcast, watch};
use tracing::info;

use beambench_pc::{PortInfo, SystemStatus, WsCommand, WsEvent, export_csv};

/// Initial delay before sending QueryStatus to a freshly-opened device.
/// Gives the USB-serial interface time to stabilize.
const VERIFY_INITIAL_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Timeout for the device verification probe. The Bridge responds in <100 ms;
/// 1 s is generous enough for slow USB bridges while still detecting wrong
/// devices quickly.
const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// Timeout for turntable move commands (ReturnHome, Jog).
const MOVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Interval between periodic status polls to the Bridge.
const STATUS_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Timeout for a status poll response.
const STATUS_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Parser)]
struct Args {
    /// Run in development mode (expect Vite dev server for frontend).
    #[arg(long)]
    dev: bool,

    /// HTTP server listen address.
    #[arg(long, default_value = "127.0.0.1:3000")]
    listen: String,
}

#[derive(Embed)]
#[folder = "frontend/build/"]
struct FrontendAssets;

/// Shared application state, accessible from all handlers and spawned tasks.
struct AppState {
    /// Broadcast channel for pushing events to all connected WebSocket clients.
    ws_tx: broadcast::Sender<WsEvent>,
    /// Active serial/TCP connection to the Bridge device (if any).
    serial: Mutex<Option<beambench_pc::serial::SerialHandle>>,
    /// Measurement data from the most recent (or in-progress) sweep.
    data: Mutex<Vec<beambench_pc::DataPoint>>,
    sweeping: AtomicBool,
    /// Whether a ReturnHome operation is in progress. Prevents concurrent
    /// homing and blocks sweep starts while the turntable is returning.
    homing: AtomicBool,
    tx_connected: AtomicBool,
    rx_connected: AtomicBool,
    turntable_connected: AtomicBool,
    /// Tracked turntable angle in centi-degrees (×100) for atomic access.
    turntable_angle_cdeg: AtomicI32,
    /// Whether a jog move is in progress.
    jogging: AtomicBool,
    /// Whether an OTA firmware update is in progress.
    ota_in_progress: AtomicBool,
    /// Incremented on each connect/disconnect to cancel stale status pollers.
    poll_generation: watch::Sender<u64>,
}

impl AppState {
    /// Build a `SystemStatus` snapshot from the current atomic state.
    ///
    /// When the serial link is down, peer status atomics may be stale — this
    /// method clamps them to `false` so the UI never shows ghost connections.
    async fn status(&self) -> SystemStatus {
        let serial_connected = self.serial.lock().await.is_some();
        SystemStatus {
            sweeping: self.sweeping.load(Ordering::SeqCst),
            tx_connected: serial_connected && self.tx_connected.load(Ordering::SeqCst),
            rx_connected: serial_connected && self.rx_connected.load(Ordering::SeqCst),
            turntable_connected: serial_connected
                && self.turntable_connected.load(Ordering::SeqCst),
            serial_connected,
            data_points: self.data.lock().await.len(),
        }
    }

    /// Send a `Status` event to all WebSocket clients.
    async fn broadcast_status(&self) {
        let _ = self.ws_tx.send(WsEvent::Status(self.status().await));
    }

    /// Log a message and broadcast an error event to all WebSocket clients.
    fn broadcast_error(&self, log_msg: &str, error_msg: &str) {
        let _ = self.ws_tx.send(WsEvent::Log {
            message: log_msg.to_string(),
        });
        let _ = self.ws_tx.send(WsEvent::Error {
            message: error_msg.to_string(),
        });
    }
}

type SerialTx = tokio::sync::mpsc::Sender<beambench_protocol::PcCommand>;
type SerialRx = Arc<Mutex<tokio::sync::mpsc::Receiver<beambench_protocol::DeviceEvent>>>;

impl AppState {
    /// Clone the serial tx/rx handles if connected.
    async fn clone_serial(&self) -> Option<(SerialTx, SerialRx)> {
        let serial = self.serial.lock().await;
        serial.as_ref().map(|h| (h.tx.clone(), h.rx.clone()))
    }
}

/// Spawn the OTA streaming task. Assumes `ota_in_progress` is already set.
fn spawn_ota(
    state: Arc<AppState>,
    firmware: Vec<u8>,
    role: beambench_protocol::Role,
    ack_interval: u16,
    serial_tx: SerialTx,
    serial_rx: SerialRx,
) {
    tokio::spawn(async move {
        match beambench_pc::ota::stream_firmware(
            &firmware, role, ack_interval, &serial_tx, &serial_rx, &state.ws_tx,
        )
        .await
        {
            Ok(()) => {
                let _ = state.ws_tx.send(WsEvent::OtaFinished);
                let _ = state.ws_tx.send(WsEvent::Log {
                    message: "OTA complete — device will reboot".to_string(),
                });
            }
            Err(e) => {
                state.broadcast_error(&format!("OTA failed: {e}"), &e);
            }
        }
        state.ota_in_progress.store(false, Ordering::SeqCst);
    });
}

/// Send a command and wait for a matching response, with timeout.
/// Returns `Ok(event)` on match, `Err(description)` on protocol error or timeout.
async fn send_and_wait_move(
    serial_tx: &SerialTx,
    serial_rx: &SerialRx,
    cmd: beambench_protocol::PcCommand,
) -> Result<f32, String> {
    let _ = serial_tx.send(cmd).await;

    tokio::time::timeout(MOVE_TIMEOUT, async {
        let mut rx = serial_rx.lock().await;
        loop {
            match rx.recv().await {
                Some(beambench_protocol::DeviceEvent::MoveComplete { angle_deg }) => {
                    return Ok(angle_deg);
                }
                Some(beambench_protocol::DeviceEvent::HomeComplete) => {
                    return Ok(0.0);
                }
                Some(beambench_protocol::DeviceEvent::TurntableError { description }) => {
                    return Err(description.to_string());
                }
                Some(beambench_protocol::DeviceEvent::Error { description }) => {
                    return Err(description.to_string());
                }
                Some(_) => continue,
                None => return Err("Serial connection lost".to_string()),
            }
        }
    })
    .await
    .unwrap_or(Err("Move timed out — turntable may be stuck".to_string()))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "beambench_pc=debug,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    let (ws_tx, _) = broadcast::channel::<WsEvent>(256);
    let (poll_gen_tx, _) = watch::channel(0u64);

    let state = Arc::new(AppState {
        ws_tx,
        serial: Mutex::new(None),
        data: Mutex::new(Vec::new()),
        sweeping: AtomicBool::new(false),
        homing: AtomicBool::new(false),
        tx_connected: AtomicBool::new(false),
        rx_connected: AtomicBool::new(false),
        turntable_connected: AtomicBool::new(false),
        turntable_angle_cdeg: AtomicI32::new(0),
        jogging: AtomicBool::new(false),
        ota_in_progress: AtomicBool::new(false),
        poll_generation: poll_gen_tx,
    });

    let api = Router::new()
        .route("/api/ports", get(list_ports))
        .route("/api/data", get(get_data))
        .route("/api/export/csv", get(export_csv_handler))
        .route("/api/ota", post(ota_upload))
        .route("/ws", get(ws_handler));

    let app = if args.dev {
        info!("Development mode — frontend served by Vite");
        api
    } else {
        info!("Production mode — serving embedded frontend");
        api.fallback(get(static_handler))
    };

    let app = app
        .layer(tower_http::cors::CorsLayer::permissive())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&args.listen).await.unwrap();
    info!("Listening on http://{}", args.listen);
    axum::serve(listener, app).await.unwrap();
}

async fn list_ports() -> Json<Vec<PortInfo>> {
    Json(beambench_pc::serial::list_ports().await)
}

async fn get_data(State(state): State<Arc<AppState>>) -> Json<Vec<beambench_pc::DataPoint>> {
    let data = state.data.lock().await;
    Json(data.clone())
}

async fn export_csv_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let data = state.data.lock().await;
    let csv = export_csv(&data);
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/csv; charset=utf-8",
        )],
        csv,
    )
}

/// Accept a firmware binary via multipart form and start OTA streaming.
///
/// Fields: `target` (string: "rx", "tx", "turntable") and `file` (binary).
async fn ota_upload(
    State(state): State<Arc<AppState>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let mut target: Option<String> = None;
    let mut firmware: Option<Vec<u8>> = None;
    let mut ack_interval: Option<u16> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("target") => {
                target = field.text().await.ok();
            }
            Some("file") => {
                firmware = field.bytes().await.ok().map(|b| b.to_vec());
            }
            Some("ack_interval") => {
                ack_interval = field.text().await.ok().and_then(|s| s.parse().ok());
            }
            _ => {}
        }
    }

    let ack_interval = ack_interval.unwrap_or(beambench_pc::ota::DEFAULT_ACK_INTERVAL);

    let Some(target_str) = target else {
        return Json(serde_json::json!({"error": "Missing 'target' field"}));
    };
    let Some(firmware) = firmware else {
        return Json(serde_json::json!({"error": "Missing 'file' field"}));
    };
    let Some(role) = beambench_pc::ota::parse_role(&target_str) else {
        return Json(serde_json::json!({"error": format!("Unknown target: {}", target_str)}));
    };

    if state.sweeping.load(Ordering::SeqCst) {
        return Json(serde_json::json!({"error": "Sweep in progress"}));
    }
    if state.ota_in_progress.swap(true, Ordering::SeqCst) {
        return Json(serde_json::json!({"error": "OTA already in progress"}));
    }

    let Some((serial_tx, serial_rx)) = state.clone_serial().await else {
        state.ota_in_progress.store(false, Ordering::SeqCst);
        return Json(serde_json::json!({"error": "Not connected"}));
    };

    let size = firmware.len();
    let _ = state.ws_tx.send(WsEvent::Log {
        message: format!("Starting OTA for {}: {} bytes", target_str, size),
    });

    spawn_ota(state.clone(), firmware, role, ack_interval, serial_tx, serial_rx);

    Json(serde_json::json!({"ok": true, "size": size}))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(socket, state))
}

/// Main WebSocket connection handler. Bridges broadcast events to the client
/// and dispatches incoming commands to [`handle_command`].
async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    info!("WebSocket client connected");
    let mut rx = state.ws_tx.subscribe();

    // Send current status on connect.
    let status = state.status().await;
    let _ = socket
        .send(Message::Text(serde_json::to_string(&WsEvent::Status(status)).unwrap().into()))
        .await;

    loop {
        tokio::select! {
            // Forward events from backend to WebSocket client.
            result = rx.recv() => {
                match result {
                    Ok(event) => {
                        let json = serde_json::to_string(&event).unwrap();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            info!("WebSocket send failed, closing");
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("WebSocket receiver lagged, skipped {} events", n);
                        // Continue — the receiver auto-advances past the gap.
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("Broadcast channel closed, WebSocket shutting down");
                        return;
                    }
                }
            }
            // Handle commands from WebSocket client.
            Some(Ok(msg)) = socket.recv() => {
                if let Message::Text(text) = msg {
                    match serde_json::from_str::<WsCommand>(&text) {
                        Ok(cmd) => {
                            handle_command(cmd, &state).await;
                        }
                        Err(e) => {
                            tracing::warn!("Invalid WS command: {}", e);
                            let _ = state.ws_tx.send(WsEvent::Error {
                                message: format!("Invalid command: {}", e),
                            });
                        }
                    }
                }
            }
            else => {
                info!("WebSocket client disconnected");
                return;
            }
        }
    }
}

/// Dispatch a single [`WsCommand`] received from a WebSocket client.
async fn handle_command(cmd: WsCommand, state: &Arc<AppState>) {
    match cmd {
        WsCommand::ListPorts => {
            // Respond via WsEvent (ports listed via REST endpoint too).
        }
        WsCommand::Connect { port } => {
            info!("Connect requested: {}", port);
            let mut serial = state.serial.lock().await;

            // Close previous connection if any.
            if let Some(old) = serial.take() {
                info!("Closing previous serial connection");
                old.close();
            }

            let result = if let Some(addr) = port.strip_prefix("tcp://") {
                beambench_pc::serial::SerialHandle::open_tcp(addr).await
            } else {
                beambench_pc::serial::SerialHandle::open(&port).await
            };
            match result {
                Ok(handle) => {
                    let serial_tx = handle.tx.clone();
                    let serial_rx = handle.rx.clone();
                    *serial = Some(handle);
                    drop(serial); // Release lock before spawning verification.
                    info!("Serial connected to {}", port);
                    let _ = state.ws_tx.send(WsEvent::Log {
                        message: format!("Connected to {}, verifying device...", port),
                    });
                    state.tx_connected.store(false, Ordering::SeqCst);
                    state.rx_connected.store(false, Ordering::SeqCst);
                    state.turntable_connected.store(false, Ordering::SeqCst);
                    state.broadcast_status().await;

                    // Cancel any previous status poller and start a new generation.
                    state.poll_generation.send_modify(|g| *g += 1);
                    let generation = *state.poll_generation.borrow();
                    let mut poll_cancel = state.poll_generation.subscribe();

                    // Probe the device: send QueryStatus and wait for a response.
                    let state2 = state.clone();
                    let port_name = port.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(VERIFY_INITIAL_DELAY).await;
                        let _ = serial_tx.send(beambench_protocol::PcCommand::QueryStatus).await;

                        let result = tokio::time::timeout(
                            VERIFY_TIMEOUT,
                            async {
                                let mut rx = serial_rx.lock().await;
                                rx.recv().await
                            },
                        )
                        .await;

                        match result {
                            Ok(Some(beambench_protocol::DeviceEvent::Status { tx_connected, rx_connected, turntable_connected })) => {
                                tracing::info!("Device verified as Bridge");
                                let _ = state2.ws_tx.send(WsEvent::Log {
                                    message: format!("{} confirmed as Bridge (TX: {}, RX: {}, Turntable: {})",
                                        port_name,
                                        if tx_connected { "connected" } else { "not found" },
                                        if rx_connected { "connected" } else { "not found" },
                                        if turntable_connected { "connected" } else { "not found" }),
                                });
                                update_peer_status(&state2, tx_connected, rx_connected, turntable_connected);
                                state2.broadcast_status().await;
                            }
                            Ok(Some(other)) => {
                                tracing::info!("Unexpected response from device: {:?}", other);
                                let _ = state2.ws_tx.send(WsEvent::Log {
                                    message: format!("{}: got unexpected response — may not be Bridge", port_name),
                                });
                            }
                            Ok(None) => {
                                tracing::warn!("Serial channel closed during verification");
                                let _ = state2.ws_tx.send(WsEvent::Log {
                                    message: format!("{}: connection lost during verification", port_name),
                                });
                            }
                            Err(_) => {
                                tracing::warn!("No response from {} — not a Bridge or firmware not running", port_name);
                                state2.broadcast_error(
                                    &format!("{}: no response — is this the Bridge device?", port_name),
                                    "Device did not respond. Is this the Bridge?",
                                );
                            }
                        }
                    });

                    // Spawn periodic status poller.
                    let state3 = state.clone();
                    tokio::spawn(async move {
                        status_poll_loop(&state3, generation, &mut poll_cancel).await;
                    });
                }
                Err(e) => {
                    info!("Failed to connect to {}: {}", port, e);
                    state.broadcast_error(
                        &format!("Failed to connect to {}: {}", port, e),
                        &format!("Failed to open {}: {}", port, e),
                    );
                }
            }
        }
        WsCommand::Disconnect => {
            // Cancel the status poller for this connection.
            state.poll_generation.send_modify(|g| *g += 1);
            {
                let mut serial = state.serial.lock().await;
                if let Some(handle) = serial.take() {
                    info!("Disconnecting serial");
                    handle.close();
                }
            }
            let _ = state.ws_tx.send(WsEvent::Log {
                message: "Disconnected".to_string(),
            });
            state.tx_connected.store(false, Ordering::SeqCst);
            state.rx_connected.store(false, Ordering::SeqCst);
            state.turntable_connected.store(false, Ordering::SeqCst);
            state.broadcast_status().await;
        }
        WsCommand::StartSweep(config) => {
            // Prevent concurrent sweeps and sweep-during-homing/OTA.
            if state.homing.load(Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Cannot start sweep while homing".to_string(),
                });
                return;
            }
            if state.ota_in_progress.load(Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Cannot start sweep while OTA is in progress".to_string(),
                });
                return;
            }
            if state.sweeping.swap(true, Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Sweep already in progress".to_string(),
                });
                return;
            }

            // Clear previous data.
            state.data.lock().await.clear();

            if let Some((serial_tx, serial_rx)) = state.clone_serial().await {
                let state = state.clone();

                let _ = state.ws_tx.send(WsEvent::Log {
                    message: format!(
                        "Sweep started: {}° to {}° (step {}°, {} samples)",
                        config.start_deg, config.stop_deg, config.step_deg, config.samples_per_angle
                    ),
                });
                let _ = state.ws_tx.send(WsEvent::Status(state.status().await));

                tokio::spawn(async move {
                    let result =
                        beambench_pc::sweep::run_sweep(config, &serial_tx, &serial_rx, &state.ws_tx)
                            .await;
                    match &result {
                        Ok(data) => {
                            let _ = state.ws_tx.send(WsEvent::Log {
                                message: format!("Sweep complete: {} data points", data.len()),
                            });
                            let mut store = state.data.lock().await;
                            *store = data.clone();
                        }
                        Err(e) => {
                            let _ = state.ws_tx.send(WsEvent::Log {
                                message: format!("Sweep failed: {}", e),
                            });
                        }
                    }
                    state.sweeping.store(false, Ordering::SeqCst);
                    state.broadcast_status().await;
                });
            } else {
                state.sweeping.store(false, Ordering::SeqCst);
                state.broadcast_error(
                    "Sweep failed: not connected",
                    "Not connected to serial/TCP",
                );
            }
        }
        WsCommand::Stop => {
            let serial = state.serial.lock().await;
            if let Some(ref handle) = *serial {
                let _ = handle.tx.send(beambench_protocol::PcCommand::StopTransmitting).await;
                let _ = handle.tx.send(beambench_protocol::PcCommand::StopTurntable).await;
            }
        }
        WsCommand::ConfigureTx(tx_config) => {
            let serial = state.serial.lock().await;
            if let Some(ref handle) = *serial {
                let _ = handle
                    .tx
                    .send(beambench_protocol::PcCommand::ConfigureTx {
                        channel: tx_config.channel,
                        tx_power_dbm: tx_config.tx_power_dbm,
                        packet_rate_hz: tx_config.packet_rate_hz,
                    })
                    .await;
            }
        }
        WsCommand::QueryStatus => {
            let serial = state.serial.lock().await;
            if let Some(ref handle) = *serial {
                let _ = handle.tx.send(beambench_protocol::PcCommand::QueryStatus).await;
            }
        }
        WsCommand::ExportCsv => {
            // CSV export is available via REST endpoint.
        }
        WsCommand::ReturnHome => {
            if state.sweeping.load(Ordering::SeqCst) || state.ota_in_progress.load(Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Cannot return home while sweep or OTA is in progress".to_string(),
                });
                return;
            }
            if state.homing.swap(true, Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Already returning home".to_string(),
                });
                return;
            }

            if let Some((serial_tx, serial_rx)) = state.clone_serial().await {
                let _ = state.ws_tx.send(WsEvent::Log {
                    message: "Returning to home position...".to_string(),
                });

                let state = state.clone();
                tokio::spawn(async move {
                    match send_and_wait_move(&serial_tx, &serial_rx, beambench_protocol::PcCommand::ReturnHome).await {
                        Ok(angle_deg) => {
                            state.turntable_angle_cdeg.store((angle_deg * 100.0) as i32, Ordering::SeqCst);
                            let _ = state.ws_tx.send(WsEvent::HomeComplete);
                            let _ = state.ws_tx.send(WsEvent::Log {
                                message: "Turntable reached home position".to_string(),
                            });
                        }
                        Err(e) => {
                            state.broadcast_error(&format!("ReturnHome failed: {}", e), &e);
                        }
                    }
                    state.homing.store(false, Ordering::SeqCst);
                    state.broadcast_status().await;
                });
            } else {
                state.homing.store(false, Ordering::SeqCst);
                state.broadcast_error("ReturnHome failed: not connected", "Not connected to serial/TCP");
            }
        }
        WsCommand::OtaUpload { target, firmware_path } => {
            if state.sweeping.load(Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Cannot start OTA while sweep is in progress".to_string(),
                });
                return;
            }
            if state.ota_in_progress.swap(true, Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "OTA already in progress".to_string(),
                });
                return;
            }

            let role = match beambench_pc::ota::parse_role(&target) {
                Some(r) => r,
                None => {
                    state.ota_in_progress.store(false, Ordering::SeqCst);
                    let _ = state.ws_tx.send(WsEvent::Error {
                        message: format!("Unknown target role: {}", target),
                    });
                    return;
                }
            };

            let firmware = match tokio::fs::read(&firmware_path).await {
                Ok(data) => data,
                Err(e) => {
                    state.ota_in_progress.store(false, Ordering::SeqCst);
                    state.broadcast_error(
                        &format!("Failed to read firmware: {}", e),
                        &format!("Cannot read {}: {}", firmware_path, e),
                    );
                    return;
                }
            };

            if let Some((serial_tx, serial_rx)) = state.clone_serial().await {
                let _ = state.ws_tx.send(WsEvent::Log {
                    message: format!(
                        "Starting OTA for {}: {} ({} bytes)",
                        target,
                        firmware_path,
                        firmware.len()
                    ),
                });

                spawn_ota(
                    state.clone(),
                    firmware,
                    role,
                    beambench_pc::ota::DEFAULT_ACK_INTERVAL,
                    serial_tx,
                    serial_rx,
                );
            } else {
                state.ota_in_progress.store(false, Ordering::SeqCst);
                state.broadcast_error(
                    "OTA failed: not connected",
                    "Not connected to serial/TCP",
                );
            }
        }
        WsCommand::Jog { delta_deg } => {
            if state.sweeping.load(Ordering::SeqCst) || state.homing.load(Ordering::SeqCst) || state.ota_in_progress.load(Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Cannot jog while sweep, homing, or OTA is in progress".to_string(),
                });
                return;
            }
            if state.jogging.swap(true, Ordering::SeqCst) {
                let _ = state.ws_tx.send(WsEvent::Error {
                    message: "Jog already in progress".to_string(),
                });
                return;
            }

            let current_cdeg = state.turntable_angle_cdeg.load(Ordering::SeqCst);
            let target_deg = (current_cdeg as f32 / 100.0) + delta_deg;

            if let Some((serial_tx, serial_rx)) = state.clone_serial().await {
                let _ = state.ws_tx.send(WsEvent::Log {
                    message: format!("Jogging to {:.1}°", target_deg),
                });

                let state = state.clone();
                tokio::spawn(async move {
                    match send_and_wait_move(&serial_tx, &serial_rx, beambench_protocol::PcCommand::MoveTo { angle_deg: target_deg }).await {
                        Ok(angle_deg) => {
                            state.turntable_angle_cdeg.store((angle_deg * 100.0) as i32, Ordering::SeqCst);
                            let _ = state.ws_tx.send(WsEvent::Log {
                                message: format!("Turntable at {:.1}°", angle_deg),
                            });
                            let _ = state.ws_tx.send(WsEvent::JogComplete { angle_deg });
                        }
                        Err(e) => {
                            state.broadcast_error(&format!("Jog failed: {}", e), &e);
                        }
                    }
                    state.jogging.store(false, Ordering::SeqCst);
                });
            } else {
                state.jogging.store(false, Ordering::SeqCst);
                state.broadcast_error("Jog failed: not connected", "Not connected to serial/TCP");
            }
        }
    }
}

/// Update peer connection atomics from a bridge Status response.
fn update_peer_status(state: &AppState, tx: bool, rx: bool, turntable: bool) {
    state.tx_connected.store(tx, Ordering::SeqCst);
    state.rx_connected.store(rx, Ordering::SeqCst);
    state.turntable_connected.store(turntable, Ordering::SeqCst);
}

/// Periodically poll the bridge for device connection status.
///
/// Stops when the poll generation changes (new connect or disconnect).
/// Skips polls while an exclusive operation (sweep, OTA, homing, jogging)
/// holds the serial_rx lock.
async fn status_poll_loop(
    state: &Arc<AppState>,
    generation: u64,
    cancel: &mut watch::Receiver<u64>,
) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(STATUS_POLL_INTERVAL) => {}
            _ = cancel.changed() => return,
        }

        // Check if this poller is still current.
        if *state.poll_generation.borrow() != generation {
            return;
        }

        // Skip polling during exclusive operations — they hold serial_rx.
        if state.sweeping.load(Ordering::SeqCst)
            || state.ota_in_progress.load(Ordering::SeqCst)
            || state.homing.load(Ordering::SeqCst)
            || state.jogging.load(Ordering::SeqCst)
        {
            continue;
        }

        let Some((serial_tx, serial_rx)) = state.clone_serial().await else {
            return; // Disconnected.
        };

        if serial_tx.send(beambench_protocol::PcCommand::QueryStatus).await.is_err() {
            return;
        }

        let result = tokio::time::timeout(STATUS_POLL_TIMEOUT, async {
            let mut rx = serial_rx.lock().await;
            loop {
                match rx.recv().await {
                    Some(beambench_protocol::DeviceEvent::Status { tx_connected, rx_connected, turntable_connected }) => {
                        return Some((tx_connected, rx_connected, turntable_connected));
                    }
                    Some(_) => continue,
                    None => return None,
                }
            }
        })
        .await;

        match result {
            Ok(Some((tx, rx, turntable))) => {
                update_peer_status(state, tx, rx, turntable);
                state.broadcast_status().await;
            }
            Ok(None) => {
                // Channel closed — serial disconnected.
                return;
            }
            Err(_) => {
                // Timeout — bridge may be busy or connection lost.
                tracing::debug!("Status poll timed out");
            }
        }
    }
}

async fn static_handler(uri: axum::http::Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match FrontendAssets::get(path) {
        Some(content) => {
            let mime = mime_guess::from_path(path)
                .first_or_octet_stream()
                .to_string();
            ([(axum::http::header::CONTENT_TYPE, mime)], content.data.into_response()).into_response()
        }
        None => {
            // SPA fallback — serve index.html for client-side routing.
            match FrontendAssets::get("index.html") {
                Some(content) => {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/html".to_string())],
                        content.data.into_response(),
                    )
                        .into_response()
                }
                None => (axum::http::StatusCode::NOT_FOUND, "Not found").into_response(),
            }
        }
    }
}
