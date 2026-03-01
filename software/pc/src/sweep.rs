//! Sweep state machine — orchestrates a measurement campaign.

use std::sync::Arc;
use std::time::Duration;

use crate::{DataPoint, SweepConfig, WsEvent};
use beambench_protocol::{PcToRx, RxToPc};
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};

/// Default timeout for waiting on a response from the RX board.
const SWEEP_RECV_TIMEOUT: Duration = Duration::from_secs(30);

/// Run a sweep to completion, sending events to the WebSocket channel.
///
/// Returns the collected data points on success.
pub async fn run_sweep(
    config: SweepConfig,
    serial_tx: &mpsc::Sender<PcToRx>,
    serial_rx: &Arc<Mutex<mpsc::Receiver<RxToPc>>>,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) -> Result<Vec<DataPoint>, String> {
    config.validate()?;

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
    let mut rx = serial_rx.lock().await;

    // Collect data points until sweep completes or an error occurs.
    loop {
        let recv_result = tokio::time::timeout(SWEEP_RECV_TIMEOUT, rx.recv()).await;
        match recv_result {
            Err(_elapsed) => {
                error!("Sweep timed out waiting for data from RX board");
                let msg = "Sweep timed out waiting for data".to_string();
                let _ = ws_tx.send(WsEvent::Error {
                    message: msg.clone(),
                });
                return Err(msg);
            }
            Ok(None) => {
                error!("Serial connection closed during sweep");
                return Err("Serial connection lost".to_string());
            }
            Ok(Some(msg)) => match msg {
                RxToPc::DataPoint {
                    angle_deg,
                    rssi_dbm,
                    sample_count,
                } => {
                    let dp = DataPoint {
                        angle_deg,
                        rssi_dbm,
                        sample_count,
                    };
                    let _ = ws_tx.send(WsEvent::DataPoint(dp.clone()));
                    data.push(dp);
                }
                RxToPc::SweepComplete => {
                    info!("Sweep complete, {} data points collected", data.len());
                    let _ = ws_tx.send(WsEvent::SweepComplete);
                    return Ok(data);
                }
                RxToPc::Error { description } => {
                    let msg = description.to_string();
                    warn!("RX reported error during sweep: {}", msg);
                    let _ = ws_tx.send(WsEvent::Error {
                        message: msg.clone(),
                    });
                    return Err(msg);
                }
                RxToPc::Status { .. } => {
                    // Ignore status updates during sweep.
                }
            },
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

    fn shared_rx(rx: mpsc::Receiver<RxToPc>) -> Arc<Mutex<mpsc::Receiver<RxToPc>>> {
        Arc::new(Mutex::new(rx))
    }

    #[tokio::test]
    async fn sweep_collects_data_points() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await });

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
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await });

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
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move { run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await });

        // Drop the sender to simulate disconnection
        drop(resp_tx);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Serial connection lost");
    }

    #[tokio::test]
    async fn sweep_times_out_when_no_response() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (_resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();

        // Use a very short timeout for testing by overriding via tokio::time::pause
        tokio::time::pause();

        let handle = tokio::spawn(async move {
            run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await
        });

        // Advance time past the timeout
        tokio::time::advance(SWEEP_RECV_TIMEOUT + Duration::from_secs(1)).await;

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("timed out"),
            "error should mention timeout"
        );
    }

    #[tokio::test]
    async fn sweep_allows_subsequent_operations() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcToRx>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<RxToPc>(64);
        let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        // First sweep
        let config = test_config();
        let rx_clone = resp_rx.clone();
        let tx_ref = serial_tx.clone();
        let ws_ref = ws_tx.clone();
        let handle = tokio::spawn(async move {
            run_sweep(config, &tx_ref, &rx_clone, &ws_ref).await
        });

        resp_tx
            .send(RxToPc::DataPoint {
                angle_deg: 0.0,
                rssi_dbm: -40.0,
                sample_count: 5,
            })
            .await
            .unwrap();
        resp_tx.send(RxToPc::SweepComplete).await.unwrap();
        let result = handle.await.unwrap();
        assert!(result.is_ok());

        // Second sweep — should work because rx is shared, not consumed
        let config2 = test_config();
        let rx_clone2 = resp_rx.clone();
        let handle2 = tokio::spawn(async move {
            run_sweep(config2, &serial_tx, &rx_clone2, &ws_tx).await
        });

        resp_tx
            .send(RxToPc::DataPoint {
                angle_deg: 10.0,
                rssi_dbm: -35.0,
                sample_count: 5,
            })
            .await
            .unwrap();
        resp_tx.send(RxToPc::SweepComplete).await.unwrap();
        let result2 = handle2.await.unwrap();
        assert!(result2.is_ok());
        assert_eq!(result2.unwrap().len(), 1);
    }
}
