//! Core application logic for beambench PC application.
//!
//! This module contains serial communication, sweep orchestration, and data
//! management — all independent of the GUI/web layer.

pub mod serial;
pub mod sweep;

use serde::{Deserialize, Serialize};

/// A single measurement data point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataPoint {
    pub angle_deg: f32,
    pub rssi_dbm: f32,
    pub sample_count: u16,
}

/// Sweep configuration sent from UI to backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepConfig {
    pub start_deg: f32,
    pub stop_deg: f32,
    pub step_deg: f32,
    pub samples_per_angle: u16,
}

/// TX configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxConfig {
    pub channel: u8,
    pub tx_power_dbm: i8,
    pub packet_rate_hz: u16,
}

/// Current system status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemStatus {
    pub sweeping: bool,
    pub tx_connected: bool,
    pub turntable_connected: bool,
    pub serial_connected: bool,
    pub data_points: usize,
}

/// Events pushed to the frontend over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsEvent {
    DataPoint(DataPoint),
    SweepComplete,
    Status(SystemStatus),
    Error { message: String },
}

/// Commands received from the frontend over WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsCommand {
    StartSweep(SweepConfig),
    ConfigureTx(TxConfig),
    Stop,
    QueryStatus,
    ListPorts,
    Connect { port: String },
    Disconnect,
    ExportCsv,
}

/// Export measurement data as CSV.
pub fn export_csv(data: &[DataPoint]) -> String {
    let mut csv = String::from("angle_deg,rssi_dbm,sample_count\n");
    for dp in data {
        csv.push_str(&format!("{},{},{}\n", dp.angle_deg, dp.rssi_dbm, dp.sample_count));
    }
    csv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_csv_empty() {
        let csv = export_csv(&[]);
        assert_eq!(csv, "angle_deg,rssi_dbm,sample_count\n");
    }

    #[test]
    fn export_csv_with_data() {
        let data = vec![
            DataPoint {
                angle_deg: 0.0,
                rssi_dbm: -40.0,
                sample_count: 10,
            },
            DataPoint {
                angle_deg: 10.0,
                rssi_dbm: -35.5,
                sample_count: 10,
            },
        ];
        let csv = export_csv(&data);
        assert_eq!(
            csv,
            "angle_deg,rssi_dbm,sample_count\n\
             0,-40,10\n\
             10,-35.5,10\n"
        );
    }
}
