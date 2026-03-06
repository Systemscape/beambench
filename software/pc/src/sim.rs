//! Simulated Bridge device — extracted for reuse in integration tests.
//!
//! Speaks postcard+COBS using PcCommand/DeviceEvent, generates synthetic
//! antenna patterns so you can test the full PC pipeline without hardware.

use std::sync::Arc;

use beambench_protocol::{DeviceEvent, PcCommand, MAX_MSG_SIZE};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Handle a single client connection: decode commands, respond with events.
pub async fn handle_connection(stream: tokio::net::TcpStream) {
    let (reader, writer) = tokio::io::split(stream);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<PcCommand>(32);
    let (resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);

    tokio::spawn(cobs_reader(reader, cmd_tx));
    tokio::spawn(cobs_writer(writer, resp_rx));

    let mut transmitting = false;
    let mut measuring = false;
    let mut rssi_sum: f32 = 0.0;
    let mut rssi_count: u16 = 0;
    let mut current_angle: f32 = 0.0;

    // Simulated beacon generator — sends synthetic RSSI samples while transmitting + measuring.
    let beacon_notify = Arc::new(tokio::sync::Notify::new());

    loop {
        let cmd = match cmd_rx.recv().await {
            Some(cmd) => cmd,
            None => return,
        };

        match cmd {
            PcCommand::ConfigureTx { .. } => {
                let _ = resp_tx.send(DeviceEvent::TxAck).await;
            }
            PcCommand::StartTransmitting => {
                transmitting = true;
                let _ = resp_tx.send(DeviceEvent::TxAck).await;
            }
            PcCommand::StopTransmitting => {
                transmitting = false;
                let _ = resp_tx.send(DeviceEvent::TxAck).await;
            }
            PcCommand::MoveTo { angle_deg } => {
                // Simulate instant move.
                current_angle = angle_deg;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                let _ = resp_tx
                    .send(DeviceEvent::MoveComplete { angle_deg })
                    .await;
            }
            PcCommand::StopStepper => {
                let _ = resp_tx
                    .send(DeviceEvent::MoveComplete {
                        angle_deg: current_angle,
                    })
                    .await;
            }
            PcCommand::ReturnHome => {
                current_angle = 0.0;
                let _ = resp_tx.send(DeviceEvent::HomeComplete).await;
            }
            PcCommand::StartMeasurement => {
                measuring = true;
                rssi_sum = 0.0;
                rssi_count = 0;

                // Simulate beacon reception: generate synthetic RSSI samples.
                if transmitting {
                    let rssi = synthetic_rssi(current_angle);
                    // Simulate receiving several beacons.
                    let n = 10u16;
                    rssi_sum = rssi * n as f32;
                    rssi_count = n;
                }
            }
            PcCommand::ReportMeasurement => {
                let (rssi_dbm, sample_count) = if rssi_count > 0 {
                    (rssi_sum / rssi_count as f32, rssi_count)
                } else {
                    (-100.0, 0)
                };
                measuring = false;
                let _ = resp_tx
                    .send(DeviceEvent::Measurement {
                        rssi_dbm,
                        sample_count,
                    })
                    .await;
            }
            PcCommand::QueryStatus => {
                let _ = resp_tx
                    .send(DeviceEvent::Status {
                        tx_connected: true,
                        rx_connected: true,
                        stepper_connected: true,
                    })
                    .await;
            }
        }
    }
}

/// Synthetic RSSI: cardioid-like pattern peaked at 45°.
fn synthetic_rssi(angle_deg: f32) -> f32 {
    let rad = (angle_deg - 45.0_f32).to_radians();
    -30.0 + 20.0 * rad.cos()
}

/// Compute the expected synthetic RSSI for a given angle (matches synthetic_rssi).
pub fn expected_rssi(angle_deg: f32) -> f32 {
    synthetic_rssi(angle_deg)
}

/// Decode COBS-framed postcard messages from a reader into a channel.
pub async fn cobs_reader(mut reader: impl AsyncReadExt + Unpin, tx: mpsc::Sender<PcCommand>) {
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
                        match postcard::from_bytes_cobs::<PcCommand>(&mut frame.to_vec()) {
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

/// Encode DeviceEvent messages from a channel as COBS frames into a writer.
pub async fn cobs_writer(
    mut writer: impl AsyncWriteExt + Unpin,
    mut rx: mpsc::Receiver<DeviceEvent>,
) {
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

/// Start an in-process bridge-sim TCP server on a random port.
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
