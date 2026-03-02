//! End-to-end integration tests: PC app ↔ rx-sim over TCP.
//!
//! These tests start an in-process rx-sim server, connect via SerialHandle,
//! run sweeps, and verify the full pipeline including COBS framing, sweep
//! orchestration, and CSV export.
//! 
//! Assumes the "sim" feature is activated

use beambench_pc::sim;
use beambench_pc::{export_csv, SweepConfig, WsEvent};
use tokio::sync::broadcast;

#[tokio::test]
async fn full_sweep_via_tcp() {
    let addr = sim::start_server().await;
    let addr_str = addr.to_string();

    let handle = beambench_pc::serial::SerialHandle::open_tcp(&addr_str)
        .await
        .expect("should connect to rx-sim");

    let serial_tx = handle.tx.clone();
    let serial_rx = handle.rx.clone();
    let (ws_tx, mut ws_rx) = broadcast::channel::<WsEvent>(64);

    let config = SweepConfig {
        start_deg: 0.0,
        stop_deg: 30.0,
        step_deg: 10.0,
        samples_per_angle: 5,
    };

    let result = beambench_pc::sweep::run_sweep(config, &serial_tx, &serial_rx, &ws_tx)
        .await
        .expect("sweep should succeed");

    // 0, 10, 20, 30 = 4 data points
    assert_eq!(result.len(), 4, "expected 4 data points for 0-30 step 10");

    // Verify angles
    let angles: Vec<f32> = result.iter().map(|dp| dp.angle_deg).collect();
    assert_eq!(angles, vec![0.0, 10.0, 20.0, 30.0]);

    // Verify RSSI matches the cardioid formula
    for dp in &result {
        let expected = sim::expected_rssi(dp.angle_deg);
        assert!(
            (dp.rssi_dbm - expected).abs() < 0.01,
            "RSSI mismatch at {}°: got {}, expected {}",
            dp.angle_deg,
            dp.rssi_dbm,
            expected
        );
    }

    // Verify sample count
    for dp in &result {
        assert_eq!(dp.sample_count, 5);
    }

    // Verify WebSocket events were broadcast
    let mut dp_count = 0;
    let mut complete = false;
    while let Ok(ev) = ws_rx.try_recv() {
        match ev {
            WsEvent::DataPoint(_) => dp_count += 1,
            WsEvent::SweepComplete => complete = true,
            _ => {}
        }
    }
    assert_eq!(dp_count, 4);
    assert!(complete, "should have received SweepComplete event");

    // Verify CSV export
    let csv = export_csv(&result);
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines[0], "angle_deg,rssi_dbm,sample_count");
    assert_eq!(lines.len(), 5); // header + 4 data rows

    handle.close();
}

#[tokio::test]
async fn sweep_stop_via_tcp() {
    let addr = sim::start_server().await;
    let addr_str = addr.to_string();

    let handle = beambench_pc::serial::SerialHandle::open_tcp(&addr_str)
        .await
        .expect("should connect to rx-sim");

    let serial_tx = handle.tx.clone();
    let serial_rx = handle.rx.clone();
    let (ws_tx, _ws_rx) = broadcast::channel::<WsEvent>(64);

    // Long sweep: 0 to 360 in 1° steps (361 points at 50ms each ≈ 18s)
    let config = SweepConfig {
        start_deg: 0.0,
        stop_deg: 360.0,
        step_deg: 1.0,
        samples_per_angle: 5,
    };

    let stop_tx = serial_tx.clone();

    let sweep_handle = tokio::spawn(async move {
        beambench_pc::sweep::run_sweep(config, &serial_tx, &serial_rx, &ws_tx).await
    });

    // Wait a bit for the sweep to start, then send stop
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    stop_tx
        .send(beambench_protocol::PcToRx::Stop)
        .await
        .expect("should send stop command");

    let result = sweep_handle.await.unwrap();
    assert!(result.is_err(), "sweep should have been aborted");
    let err = result.unwrap_err();
    assert!(
        err.contains("aborted") || err.contains("Sweep aborted"),
        "error should mention abort, got: {}",
        err
    );

    handle.close();
}
