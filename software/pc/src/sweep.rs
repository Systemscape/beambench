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

#[cfg(test)]
mod tests {
    use super::*;
    use beambench_protocol::RxToPc;
    use heapless::String as HString;
    use tokio::sync::{broadcast, mpsc};

    fn test_config() -> SweepConfig {
        SweepConfig {
            start_deg: 0.0,
            stop_deg: 20.0,
            step_deg: 10.0,
            samples_per_angle: 5,
        }
    }

    #[tokio::test]
    async fn sweep_collects_data_points() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);
        let mut resp_rx = resp_rx;

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &mut resp_rx, &ws_tx).await });

        // Feed 3 data points then SweepComplete
        for i in 0..3 {
            resp_tx
                .send(RxToPc::DataPoint {
                    angle_deg: i as f32 * 10.0,
                    rssi_dbm: -40.0 + i as f32,
                    sample_count: 5,
                })
                .await
                .unwrap();
        }
        resp_tx.send(RxToPc::SweepComplete).await.unwrap();

        let result = handle.await.unwrap();
        let data = result.expect("sweep should succeed");
        assert_eq!(data.len(), 3);
        assert_eq!(data[0].angle_deg, 0.0);
        assert_eq!(data[1].angle_deg, 10.0);
        assert_eq!(data[2].angle_deg, 20.0);

        // Verify broadcast events: 3 DataPoints + 1 SweepComplete
        let mut dp_count = 0;
        let mut complete = false;
        while let Ok(ev) = ws_rx.try_recv() {
            match ev {
                WsEvent::DataPoint(_) => dp_count += 1,
                WsEvent::SweepComplete => complete = true,
                _ => {}
            }
        }
        assert_eq!(dp_count, 3);
        assert!(complete);
    }

    #[tokio::test]
    async fn sweep_handles_error() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);
        let mut resp_rx = resp_rx;

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &mut resp_rx, &ws_tx).await });

        // Send one data point, then an error
        resp_tx
            .send(RxToPc::DataPoint {
                angle_deg: 0.0,
                rssi_dbm: -40.0,
                sample_count: 5,
            })
            .await
            .unwrap();
        resp_tx
            .send(RxToPc::Error {
                description: HString::try_from("motor fault").unwrap(),
            })
            .await
            .unwrap();

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "motor fault");

        // Verify error was broadcast
        let mut got_error = false;
        while let Ok(ev) = ws_rx.try_recv() {
            if let WsEvent::Error { message } = ev {
                assert_eq!(message, "motor fault");
                got_error = true;
            }
        }
        assert!(got_error);
    }

    #[tokio::test]
    async fn sweep_handles_disconnection() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);
        let mut resp_rx = resp_rx;

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &mut resp_rx, &ws_tx).await });

        // Drop the sender to simulate disconnection
        drop(resp_tx);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Serial connection lost");
    }
}
