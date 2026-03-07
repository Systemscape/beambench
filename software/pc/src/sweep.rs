//! Sweep orchestrator — drives a step-by-step measurement campaign via Bridge.
//!
//! For each angle: MoveTo → wait MoveComplete → StartMeasurement → wait
//! measurement window → ReportMeasurement → wait Measurement result.

use std::sync::Arc;
use std::time::Duration;

use crate::{DataPoint, SweepConfig, WsEvent};
use beambench_protocol::{DeviceEvent, PcCommand};
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info};

/// Timeout for waiting on a turntable MoveComplete response.
const MOVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timeout for waiting on a Measurement result after ReportMeasurement.
const MEASUREMENT_TIMEOUT: Duration = Duration::from_secs(5);

/// Default settling delay after the turntable reaches the target angle.
const SETTLING_DELAY: Duration = Duration::from_millis(200);

/// Default measurement window between StartMeasurement and ReportMeasurement.
const MEASUREMENT_WINDOW: Duration = Duration::from_millis(500);

/// Run a sweep to completion, sending events to the WebSocket channel.
///
/// Returns the collected data points on success.
pub async fn run_sweep(
    config: SweepConfig,
    serial_tx: &mpsc::Sender<PcCommand>,
    serial_rx: &Arc<Mutex<mpsc::Receiver<DeviceEvent>>>,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) -> Result<Vec<DataPoint>, String> {
    config.validate()?;

    info!(
        "Starting sweep: {}° to {}° step {}° ({} samples/angle)",
        config.start_deg, config.stop_deg, config.step_deg, config.samples_per_angle
    );

    // Start TX transmitting.
    serial_tx
        .send(PcCommand::StartTransmitting)
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    // Wait briefly for TX ack (best-effort, don't fail if missed).
    {
        let mut rx = serial_rx.lock().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match rx.recv().await {
                    Some(DeviceEvent::TxAck) => return,
                    Some(_) => continue,
                    None => return,
                }
            }
        })
        .await;
    }

    let mut data = Vec::new();
    let mut angle = config.start_deg;

    while angle <= config.stop_deg {
        // 1. Move turntable to angle.
        serial_tx
            .send(PcCommand::MoveTo { angle_deg: angle })
            .await
            .map_err(|_| "Serial connection lost".to_string())?;

        // 2. Wait for MoveComplete.
        wait_for_move_complete(serial_rx, ws_tx).await?;

        // 3. Settling delay.
        tokio::time::sleep(SETTLING_DELAY).await;

        // 4. Start measurement (resets accumulator on RX).
        serial_tx
            .send(PcCommand::StartMeasurement)
            .await
            .map_err(|_| "Serial connection lost".to_string())?;

        // 5. Measurement window.
        tokio::time::sleep(MEASUREMENT_WINDOW).await;

        // 6. Report measurement.
        serial_tx
            .send(PcCommand::ReportMeasurement)
            .await
            .map_err(|_| "Serial connection lost".to_string())?;

        // 7. Wait for Measurement result.
        let (rssi_dbm, sample_count) = wait_for_measurement(serial_rx, ws_tx).await?;

        let dp = DataPoint {
            angle_deg: angle,
            rssi_dbm,
            sample_count,
        };
        let _ = ws_tx.send(WsEvent::DataPoint(dp.clone()));
        data.push(dp);

        angle += config.step_deg;
    }

    // Stop TX.
    let _ = serial_tx.send(PcCommand::StopTransmitting).await;

    info!("Sweep complete, {} data points collected", data.len());
    let _ = ws_tx.send(WsEvent::SweepComplete);
    Ok(data)
}

/// Wait for a MoveComplete event, skipping other events.
async fn wait_for_move_complete(
    serial_rx: &Arc<Mutex<mpsc::Receiver<DeviceEvent>>>,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) -> Result<f32, String> {
    let mut rx = serial_rx.lock().await;
    let result = tokio::time::timeout(MOVE_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(DeviceEvent::MoveComplete { angle_deg }) => return Ok(angle_deg),
                Some(DeviceEvent::TurntableError { description }) => {
                    return Err(description.to_string());
                }
                Some(DeviceEvent::Error { description }) => {
                    return Err(description.to_string());
                }
                Some(_) => continue,
                None => return Err("Serial connection lost".to_string()),
            }
        }
    })
    .await;

    match result {
        Ok(Ok(angle)) => Ok(angle),
        Ok(Err(e)) => {
            let _ = ws_tx.send(WsEvent::Error { message: e.clone() });
            Err(e)
        }
        Err(_) => {
            let msg = "Turntable move timed out".to_string();
            error!("{}", msg);
            let _ = ws_tx.send(WsEvent::Error {
                message: msg.clone(),
            });
            Err(msg)
        }
    }
}

/// Wait for a Measurement event after ReportMeasurement.
async fn wait_for_measurement(
    serial_rx: &Arc<Mutex<mpsc::Receiver<DeviceEvent>>>,
    ws_tx: &tokio::sync::broadcast::Sender<WsEvent>,
) -> Result<(f32, u16), String> {
    let mut rx = serial_rx.lock().await;
    let result = tokio::time::timeout(MEASUREMENT_TIMEOUT, async {
        loop {
            match rx.recv().await {
                Some(DeviceEvent::Measurement {
                    rssi_dbm,
                    sample_count,
                }) => return Ok((rssi_dbm, sample_count)),
                Some(DeviceEvent::Error { description }) => {
                    return Err(description.to_string());
                }
                Some(_) => continue,
                None => return Err("Serial connection lost".to_string()),
            }
        }
    })
    .await;

    match result {
        Ok(Ok(data)) => Ok(data),
        Ok(Err(e)) => {
            let _ = ws_tx.send(WsEvent::Error { message: e.clone() });
            Err(e)
        }
        Err(_) => {
            let msg = "Measurement timed out".to_string();
            error!("{}", msg);
            let _ = ws_tx.send(WsEvent::Error {
                message: msg.clone(),
            });
            Err(msg)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beambench_protocol::DeviceEvent;
    use tokio::sync::{broadcast, mpsc};

    fn test_config() -> SweepConfig {
        SweepConfig {
            start_deg: 0.0,
            stop_deg: 20.0,
            step_deg: 10.0,
            samples_per_angle: 5,
        }
    }

    fn shared_rx(rx: mpsc::Receiver<DeviceEvent>) -> Arc<Mutex<mpsc::Receiver<DeviceEvent>>> {
        Arc::new(Mutex::new(rx))
    }

    #[tokio::test]
    async fn sweep_collects_data_points() {
        let (serial_tx, mut serial_cmd_rx) = mpsc::channel::<PcCommand>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);
        let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move {
            run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await
        });

        // Respond to commands from the sweep orchestrator.
        tokio::spawn(async move {
            while let Some(cmd) = serial_cmd_rx.recv().await {
                match cmd {
                    PcCommand::StartTransmitting => {
                        resp_tx.send(DeviceEvent::TxAck).await.unwrap();
                    }
                    PcCommand::MoveTo { angle_deg } => {
                        resp_tx
                            .send(DeviceEvent::MoveComplete { angle_deg })
                            .await
                            .unwrap();
                    }
                    PcCommand::StartMeasurement => {
                        // No response needed.
                    }
                    PcCommand::ReportMeasurement => {
                        resp_tx
                            .send(DeviceEvent::Measurement {
                                rssi_dbm: -40.0,
                                sample_count: 5,
                            })
                            .await
                            .unwrap();
                    }
                    PcCommand::StopTransmitting => {}
                    _ => {}
                }
            }
        });

        let result = handle.await.unwrap();
        let data = result.expect("sweep should succeed");
        assert_eq!(data.len(), 3); // 0, 10, 20
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
    async fn sweep_handles_turntable_error() {
        let (serial_tx, mut serial_cmd_rx) = mpsc::channel::<PcCommand>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);
        let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move {
            run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await
        });

        tokio::spawn(async move {
            while let Some(cmd) = serial_cmd_rx.recv().await {
                match cmd {
                    PcCommand::StartTransmitting => {
                        resp_tx.send(DeviceEvent::TxAck).await.unwrap();
                    }
                    PcCommand::MoveTo { .. } => {
                        resp_tx
                            .send(DeviceEvent::TurntableError {
                                description: heapless::String::try_from("motor fault").unwrap(),
                            })
                            .await
                            .unwrap();
                    }
                    _ => {}
                }
            }
        });

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "motor fault");

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
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcCommand>(32);
        let (resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);
        let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();
        let handle = tokio::spawn(async move {
            run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await
        });

        // Drop the sender to simulate disconnection.
        drop(resp_tx);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "Serial connection lost");
    }

    #[tokio::test]
    async fn sweep_times_out_when_no_move_response() {
        let (serial_tx, _serial_cmd_rx) = mpsc::channel::<PcCommand>(32);
        let (_resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);
        let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);
        let resp_rx = shared_rx(resp_rx);

        let config = test_config();

        tokio::time::pause();

        let handle = tokio::spawn(async move {
            run_sweep(config, &serial_tx, &resp_rx, &ws_tx).await
        });

        // Advance past TX ack timeout + move timeout.
        tokio::time::advance(Duration::from_secs(2) + MOVE_TIMEOUT + Duration::from_secs(1)).await;

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(
            result.unwrap_err().contains("timed out"),
            "error should mention timeout"
        );
    }
}
