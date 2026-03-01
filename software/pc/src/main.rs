//! Beambench PC application — Axum server with WebSocket and embedded frontend.

use std::sync::Arc;

use axum::{
    Router,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::{IntoResponse, Json},
    routing::get,
};
use clap::Parser;
use rust_embed::Embed;
use tokio::sync::{Mutex, broadcast};
use tracing::info;

use beambench_pc::{SystemStatus, WsCommand, WsEvent, export_csv};

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

struct AppState {
    ws_tx: broadcast::Sender<WsEvent>,
    serial: Mutex<Option<beambench_pc::serial::SerialHandle>>,
    data: Mutex<Vec<beambench_pc::DataPoint>>,
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

    let state = Arc::new(AppState {
        ws_tx,
        serial: Mutex::new(None),
        data: Mutex::new(Vec::new()),
    });

    let api = Router::new()
        .route("/api/ports", get(list_ports))
        .route("/api/data", get(get_data))
        .route("/api/export/csv", get(export_csv_handler))
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

async fn list_ports() -> Json<Vec<String>> {
    Json(beambench_pc::serial::list_ports())
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

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: Arc<AppState>) {
    let mut rx = state.ws_tx.subscribe();

    // Send current status on connect.
    let status = {
        let serial = state.serial.lock().await;
        let data = state.data.lock().await;
        SystemStatus {
            sweeping: false,
            tx_connected: false,
            turntable_connected: false,
            serial_connected: serial.is_some(),
            data_points: data.len(),
        }
    };
    let _ = socket
        .send(Message::Text(serde_json::to_string(&WsEvent::Status(status)).unwrap().into()))
        .await;

    loop {
        tokio::select! {
            // Forward events from backend to WebSocket client.
            Ok(event) = rx.recv() => {
                let json = serde_json::to_string(&event).unwrap();
                if socket.send(Message::Text(json.into())).await.is_err() {
                    return;
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
                            let _ = state.ws_tx.send(WsEvent::Error {
                                message: format!("Invalid command: {}", e),
                            });
                        }
                    }
                }
            }
            else => return,
        }
    }
}

async fn handle_command(cmd: WsCommand, state: &Arc<AppState>) {
    match cmd {
        WsCommand::ListPorts => {
            // Respond via WsEvent (ports listed via REST endpoint too).
        }
        WsCommand::Connect { port } => {
            let mut serial = state.serial.lock().await;
            match beambench_pc::serial::SerialHandle::open(&port).await {
                Ok(handle) => {
                    *serial = Some(handle);
                    let _ = state.ws_tx.send(WsEvent::Status(SystemStatus {
                        sweeping: false,
                        tx_connected: false,
                        turntable_connected: false,
                        serial_connected: true,
                        data_points: state.data.lock().await.len(),
                    }));
                }
                Err(e) => {
                    let _ = state.ws_tx.send(WsEvent::Error {
                        message: format!("Failed to open {}: {}", port, e),
                    });
                }
            }
        }
        WsCommand::Disconnect => {
            let mut serial = state.serial.lock().await;
            if let Some(handle) = serial.take() {
                handle.close();
            }
        }
        WsCommand::StartSweep(config) => {
            // Clear previous data.
            state.data.lock().await.clear();
            // TODO: spawn sweep task using serial handle.
            let _ = state.ws_tx.send(WsEvent::Error {
                message: "Sweep not yet implemented".to_string(),
            });
            let _ = config; // suppress unused warning
        }
        WsCommand::Stop => {
            let serial = state.serial.lock().await;
            if let Some(ref handle) = *serial {
                let _ = handle.tx.send(beambench_protocol::PcToRx::Stop).await;
            }
        }
        WsCommand::ConfigureTx(tx_config) => {
            let serial = state.serial.lock().await;
            if let Some(ref handle) = *serial {
                let _ = handle
                    .tx
                    .send(beambench_protocol::PcToRx::ConfigureTx {
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
                let _ = handle.tx.send(beambench_protocol::PcToRx::QueryStatus).await;
            }
        }
        WsCommand::ExportCsv => {
            // CSV export is available via REST endpoint.
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
