//! Simulated RX board logic — extracted for reuse in integration tests.
//!
//! Speaks postcard+COBS, generates synthetic antenna patterns so you can
//! test the full PC pipeline without ESP32 hardware.

use std::sync::Arc;

use beambench_protocol::{PcToRx, RxToPc, MAX_MSG_SIZE};
use heapless::String as HString;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Handle a single client connection: decode commands, run sweeps, encode responses.
#[allow(unused_assignments)] // `sweeping` is read across loop iterations
pub async fn handle_connection(stream: tokio::net::TcpStream) {
    let (reader, writer) = tokio::io::split(stream);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<PcToRx>(32);
    let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);

    let stop_flag = Arc::new(tokio::sync::Notify::new());

    tokio::spawn(cobs_reader(reader, cmd_tx));
    tokio::spawn(cobs_writer(writer, resp_rx));

    let mut sweeping = false;

    loop {
        let cmd = match cmd_rx.recv().await {
            Some(cmd) => cmd,
            None => return,
        };

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

                loop {
                    tokio::select! {
                        _ = &mut sweep_handle => {
                            break;
                        }
                        Some(inner_cmd) = cmd_rx.recv() => {
                            if matches!(inner_cmd, PcToRx::Stop) {
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
            PcToRx::ReturnHome => {
                // Sim: pretend turntable reached home instantly.
                let _ = resp_tx.send(RxToPc::HomeComplete).await;
            }
        }
    }
}

/// Generate synthetic RSSI data for a sweep (cardioid pattern peaked at 45°).
pub async fn simulate_sweep(
    start_deg: f32,
    stop_deg: f32,
    step_deg: f32,
    samples_per_angle: u16,
    resp_tx: &mpsc::Sender<RxToPc>,
    stop: &tokio::sync::Notify,
) {
    let mut angle = start_deg;
    while angle <= stop_deg {
        if tokio::time::timeout(std::time::Duration::from_millis(50), stop.notified())
            .await
            .is_ok()
        {
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

        angle += step_deg;
    }

    let _ = resp_tx.send(RxToPc::SweepComplete).await;
}

/// Compute the expected synthetic RSSI for a given angle (matches simulate_sweep).
pub fn expected_rssi(angle_deg: f32) -> f32 {
    let rad = (angle_deg - 45.0_f32).to_radians();
    -30.0 + 20.0 * rad.cos()
}

/// Decode COBS-framed postcard messages from a reader into a channel.
pub async fn cobs_reader(mut reader: impl AsyncReadExt + Unpin, tx: mpsc::Sender<PcToRx>) {
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
                            Err(_) => {}
                        }
                    }
                    accum = accum[zero_pos + 1..].to_vec();
                }
            }
            Err(_) => return,
        }
    }
}

/// Encode RxToPc messages from a channel as COBS frames into a writer.
pub async fn cobs_writer(mut writer: impl AsyncWriteExt + Unpin, mut rx: mpsc::Receiver<RxToPc>) {
    let mut buf = [0u8; MAX_MSG_SIZE];

    while let Some(msg) = rx.recv().await {
        match beambench_protocol::serialize_cobs(&msg, &mut buf) {
            Ok(len) => {
                if writer.write_all(&buf[..len]).await.is_err() {
                    return;
                }
            }
            Err(_) => {}
        }
    }
}

/// Start an in-process rx-sim TCP server on a random port.
/// Returns the address it's listening on.
pub async fn start_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle_connection(stream));
            }
        }
    });

    addr
}
