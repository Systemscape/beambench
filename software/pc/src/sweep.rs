//! Sweep state machine — orchestrates a measurement campaign.

use crate::{DataPoint, SweepConfig, WsEvent};
use beambench_protocol::{PcToRx, RxToPc};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Run a sweep to completion, sending events to the WebSocket channel.
///
/// Returns the collected data points on success.
pub async fn run_sweep(
    config: SweepConfig,
    serial_tx: &mpsc::Sender<PcToRx>,
    serial_rx: &mut mpsc::Receiver<RxToPc>,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) -> Result<Vec<DataPoint>, String> {
    info!(
        "Starting sweep: {}° to {}° step {}° ({} samples/angle)",
        config.start_deg, config.stop_deg, config.step_deg, config.samples_per_angle
    );

    // Send sweep command to RX board.
    let cmd = PcToRx::StartSweep {
        start_deg: config.start_deg,
        stop_deg: config.stop_deg,
        step_deg: config.step_deg,
        samples_per_angle: config.samples_per_angle,
    };

    serial_tx
        .send(cmd)
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    let mut data = Vec::new();

    // Collect data points until sweep completes or an error occurs.
    loop {
        match serial_rx.recv().await {
            Some(RxToPc::DataPoint {
                angle_deg,
                rssi_dbm,
                sample_count,
            }) => {
                let dp = DataPoint {
                    angle_deg,
                    rssi_dbm,
                    sample_count,
                };
                let _ = ws_tx.send(WsEvent::DataPoint(dp.clone()));
                data.push(dp);
            }
            Some(RxToPc::SweepComplete) => {
                info!("Sweep complete, {} data points collected", data.len());
                let _ = ws_tx.send(WsEvent::SweepComplete);
                return Ok(data);
            }
            Some(RxToPc::Error { description }) => {
                let msg = description.to_string();
                warn!("RX reported error during sweep: {}", msg);
                let _ = ws_tx.send(WsEvent::Error {
                    message: msg.clone(),
                });
                return Err(msg);
            }
            Some(RxToPc::Status { .. }) => {
                // Ignore status updates during sweep.
            }
            None => {
                error!("Serial connection closed during sweep");
                return Err("Serial connection lost".to_string());
            }
        }
    }
}
