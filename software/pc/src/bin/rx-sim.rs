//! Simulated RX board — speaks postcard+COBS over TCP.
//!
//! Generates synthetic antenna patterns so you can test the full PC app
//! UI without any ESP32 hardware.
//!
//! Usage: cargo run --bin rx-sim [--listen 127.0.0.1:9876]

use std::sync::Arc;

use beambench_protocol::{PcToRx, RxToPc, MAX_MSG_SIZE};
use clap::Parser;
use heapless::String as HString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

#[derive(Parser)]
struct Args {
    /// TCP listen address.
    #[arg(long, default_value = "127.0.0.1:9876")]
    listen: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rx_sim=debug".into()),
        )
        .init();

    let args = Args::parse();

    let listener = TcpListener::bind(&args.listen).await.unwrap();
    tracing::info!("rx-sim listening on {}", args.listen);

    loop {
        let (stream, addr) = listener.accept().await.unwrap();
        tracing::info!("Client connected from {}", addr);
        tokio::spawn(handle_connection(stream));
    }
}

#[allow(unused_assignments)] // `sweeping` is read across loop iterations
async fn handle_connection(stream: tokio::net::TcpStream) {
    let (reader, writer) = tokio::io::split(stream);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<PcToRx>(32);
    let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);

    let stop_flag = Arc::new(tokio::sync::Notify::new());

    // Spawn COBS reader: decode incoming frames into PcToRx commands
    tokio::spawn(cobs_reader(reader, cmd_tx));
    // Spawn COBS writer: encode RxToPc responses into COBS frames
    tokio::spawn(cobs_writer(writer, resp_rx));

    let mut sweeping = false;

    loop {
        let cmd = match cmd_rx.recv().await {
            Some(cmd) => cmd,
            None => {
                tracing::info!("Client disconnected");
                return;
            }
        };

        tracing::debug!("Received: {:?}", cmd);

        match cmd {
            PcToRx::StartSweep {
                start_deg,
                stop_deg,
                step_deg,
                samples_per_angle,
            } => {
                if sweeping {
                    let _ = resp_tx
                        .send(RxToPc::Error {
                            description: HString::try_from("Sweep already in progress").unwrap(),
                        })
                        .await;
                    continue;
                }
                sweeping = true;
                let resp_tx = resp_tx.clone();
                let stop = stop_flag.clone();

                // Run sweep in a spawned task so we can still receive Stop commands
                let sweep_handle = tokio::spawn(async move {
                    simulate_sweep(
                        start_deg,
                        stop_deg,
                        step_deg,
                        samples_per_angle,
                        &resp_tx,
                        &stop,
                    )
                    .await
                });
                tokio::pin!(sweep_handle);

                // Wait for sweep to finish while processing Stop commands
                loop {
                    tokio::select! {
                        _ = &mut sweep_handle => {
                            break;
                        }
                        Some(inner_cmd) = cmd_rx.recv() => {
                            if matches!(inner_cmd, PcToRx::Stop) {
                                tracing::info!("Stop received, aborting sweep");
                                stop_flag.notify_one();
                            }
                        }
                    }
                }
                sweeping = false;
            }
            PcToRx::ConfigureTx { .. } => {
                let _ = resp_tx
                    .send(RxToPc::Status {
                        sweeping,
                        tx_connected: true,
                        turntable_connected: true,
                    })
                    .await;
            }
            PcToRx::QueryStatus => {
                let _ = resp_tx
                    .send(RxToPc::Status {
                        sweeping,
                        tx_connected: true,
                        turntable_connected: true,
                    })
                    .await;
            }
            PcToRx::Stop => {
                if sweeping {
                    stop_flag.notify_one();
                }
            }
        }
    }
}

async fn simulate_sweep(
    start_deg: f32,
    stop_deg: f32,
    step_deg: f32,
    samples_per_angle: u16,
    resp_tx: &mpsc::Sender<RxToPc>,
    stop: &tokio::sync::Notify,
) {
    let mut angle = start_deg;
    while angle <= stop_deg {
        // Check for stop between points
        if tokio::time::timeout(std::time::Duration::from_millis(50), stop.notified())
            .await
            .is_ok()
        {
            tracing::info!("Sweep aborted at {}°", angle);
            let _ = resp_tx
                .send(RxToPc::Error {
                    description: HString::try_from("Sweep aborted").unwrap(),
                })
                .await;
            return;
        }

        // Synthetic RSSI: cardioid-like pattern peaked at 45°
        let rad = (angle - 45.0_f32).to_radians();
        let rssi = -30.0 + 20.0 * rad.cos();

        let _ = resp_tx
            .send(RxToPc::DataPoint {
                angle_deg: angle,
                rssi_dbm: rssi,
                sample_count: samples_per_angle,
            })
            .await;

        tracing::debug!("DataPoint: angle={}° rssi={:.1} dBm", angle, rssi);
        angle += step_deg;
    }

    let _ = resp_tx.send(RxToPc::SweepComplete).await;
    tracing::info!("Sweep complete");
}

async fn cobs_reader(mut reader: impl AsyncReadExt + Unpin, tx: mpsc::Sender<PcToRx>) {
    let mut raw_buf = [0u8; 512];
    let mut accum = Vec::with_capacity(512);

    loop {
        match reader.read(&mut raw_buf).await {
            Ok(0) => return,
            Ok(n) => {
                accum.extend_from_slice(&raw_buf[..n]);
                while let Some(zero_pos) = accum.iter().position(|&b| b == 0) {
                    let frame = &accum[..zero_pos + 1];
                    if frame.len() > 1 {
                        match postcard::from_bytes_cobs::<PcToRx>(&mut frame.to_vec()) {
                            Ok(msg) => {
                                if tx.send(msg).await.is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to decode COBS frame: {:?}", e);
                            }
                        }
                    }
                    accum = accum[zero_pos + 1..].to_vec();
                }
            }
            Err(e) => {
                tracing::error!("Read error: {}", e);
                return;
            }
        }
    }
}

async fn cobs_writer(mut writer: impl AsyncWriteExt + Unpin, mut rx: mpsc::Receiver<RxToPc>) {
    let mut buf = [0u8; MAX_MSG_SIZE];

    while let Some(msg) = rx.recv().await {
        match beambench_protocol::serialize_cobs(&msg, &mut buf) {
            Ok(len) => {
                if let Err(e) = writer.write_all(&buf[..len]).await {
                    tracing::error!("Write error: {}", e);
                    return;
                }
            }
            Err(e) => {
                tracing::error!("Failed to serialize: {:?}", e);
            }
        }
    }
}
