//! Core application logic for beambench PC application.
//!
//! This module contains serial communication, sweep orchestration, and data
//! management — all independent of the GUI/web layer.

pub mod serial;
#[cfg(any(test, feature = "sim"))]
pub mod sim;
pub mod sweep;

use serde::{Deserialize, Serialize};

/// A single measurement data point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DataPoint {
    pub angle_deg: f32,
    pub rssi_dbm: f32,
    pub sample_count: u16,
}

/// Sweep configuration sent from UI to backend.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepConfig {
    pub start_deg: f32,
    pub stop_deg: f32,
    pub step_deg: f32,
    pub samples_per_angle: u16,
}

/// TX configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TxConfig {
    pub channel: u8,
    pub tx_power_dbm: i8,
    pub packet_rate_hz: u16,
}

/// Current system status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStatus {
    pub sweeping: bool,
    pub tx_connected: bool,
    pub turntable_connected: bool,
    pub serial_connected: bool,
    pub data_points: usize,
}

/// Serial port information returned by the list_ports endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PortInfo {
    pub name: String,
    pub description: String,
}

/// Events pushed to the frontend over WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsEvent {
    DataPoint(DataPoint),
    SweepComplete,
    Status(SystemStatus),
    Error { message: String },
    Log { message: String },
}

/// Commands received from the frontend over WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    ReturnHome,
}

impl SweepConfig {
    /// Validate sweep configuration parameters.
    pub fn validate(&self) -> Result<(), String> {
        if self.step_deg <= 0.0 {
            return Err("Step size must be positive".to_string());
        }
        if self.start_deg >= self.stop_deg {
            return Err("Start angle must be less than stop angle".to_string());
        }
        if self.samples_per_angle == 0 {
            return Err("Samples per angle must be at least 1".to_string());
        }
        Ok(())
    }
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

    // ── JSON round-trip tests ───────────────────────────────────────────

    fn json_round_trip<T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
        val: &T,
    ) {
        let json = serde_json::to_string(val).expect("serialize");
        let decoded: T = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&decoded, val);
    }

    #[test]
    fn ws_command_json_round_trip() {
        json_round_trip(&WsCommand::ListPorts);
        json_round_trip(&WsCommand::Stop);
        json_round_trip(&WsCommand::Disconnect);
        json_round_trip(&WsCommand::ExportCsv);
        json_round_trip(&WsCommand::QueryStatus);
        json_round_trip(&WsCommand::Connect {
            port: "tcp://127.0.0.1:9876".to_string(),
        });
        json_round_trip(&WsCommand::StartSweep(SweepConfig {
            start_deg: 0.0,
            stop_deg: 360.0,
            step_deg: 5.0,
            samples_per_angle: 10,
        }));
        json_round_trip(&WsCommand::ConfigureTx(TxConfig {
            channel: 6,
            tx_power_dbm: 20,
            packet_rate_hz: 100,
        }));
    }

    #[test]
    fn ws_event_json_round_trip() {
        json_round_trip(&WsEvent::SweepComplete);
        json_round_trip(&WsEvent::Error {
            message: "test error".to_string(),
        });
        json_round_trip(&WsEvent::Log {
            message: "Connected to /dev/ttyUSB0".to_string(),
        });
        json_round_trip(&WsEvent::DataPoint(DataPoint {
            angle_deg: 45.0,
            rssi_dbm: -30.0,
            sample_count: 5,
        }));
        json_round_trip(&WsEvent::Status(SystemStatus {
            sweeping: true,
            tx_connected: false,
            turntable_connected: true,
            serial_connected: true,
            data_points: 42,
        }));
    }

    #[test]
    fn ws_command_connect_preserves_port() {
        let cmd = WsCommand::Connect {
            port: "tcp://192.168.1.100:9876".to_string(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let decoded: WsCommand = serde_json::from_str(&json).unwrap();
        if let WsCommand::Connect { port } = decoded {
            assert_eq!(port, "tcp://192.168.1.100:9876");
        } else {
            panic!("Expected Connect variant");
        }
    }

    // ── SweepConfig validation tests ────────────────────────────────────

    #[test]
    fn sweep_config_valid() {
        let config = SweepConfig {
            start_deg: 0.0,
            stop_deg: 360.0,
            step_deg: 10.0,
            samples_per_angle: 5,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn sweep_config_zero_step_rejected() {
        let config = SweepConfig {
            start_deg: 0.0,
            stop_deg: 360.0,
            step_deg: 0.0,
            samples_per_angle: 5,
        };
        assert!(config.validate().unwrap_err().contains("Step size"));
    }

    #[test]
    fn sweep_config_reversed_range_rejected() {
        let config = SweepConfig {
            start_deg: 180.0,
            stop_deg: 0.0,
            step_deg: 10.0,
            samples_per_angle: 5,
        };
        assert!(config.validate().unwrap_err().contains("Start angle"));
    }

    #[test]
    fn sweep_config_zero_samples_rejected() {
        let config = SweepConfig {
            start_deg: 0.0,
            stop_deg: 360.0,
            step_deg: 10.0,
            samples_per_angle: 0,
        };
        assert!(config.validate().unwrap_err().contains("Samples"));
    }
}
