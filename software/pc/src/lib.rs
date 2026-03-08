//! Core application logic for beambench PC application.
//!
//! This module contains serial communication, sweep orchestration, and data
//! management — all independent of the GUI/web layer.

pub mod ota;
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

/// Current system status broadcast to all WebSocket clients on state changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemStatus {
    /// Whether an antenna pattern sweep is in progress.
    pub sweeping: bool,
    /// Whether the TX ESP32 is paired via ESP-NOW.
    pub tx_connected: bool,
    /// Whether the RX ESP32 is paired via ESP-NOW.
    pub rx_connected: bool,
    /// Whether the turntable ESP32 is paired via ESP-NOW.
    pub turntable_connected: bool,
    /// Whether the PC is connected to the Bridge over serial/TCP.
    pub serial_connected: bool,
    /// Number of data points stored on the backend.
    pub data_points: usize,
    /// Active RF measurement backend name (e.g. "espnow", "dect-nr+").
    pub backend_name: String,
    /// Carrier frequency in MHz, if applicable.
    pub backend_freq_mhz: Option<u16>,
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
    /// A new measurement data point arrived during a sweep.
    DataPoint(DataPoint),
    /// The sweep finished successfully; all data points have been sent.
    SweepComplete,
    /// Turntable reached the home (0°) position after a ReturnHome command.
    HomeComplete,
    /// Turntable completed a jog move and is now at the given angle.
    JogComplete { angle_deg: f32 },
    /// Full system status snapshot (sent on connect and on every state change).
    Status(SystemStatus),
    /// OTA progress update during firmware streaming.
    OtaProgress { chunks_sent: u16, total_chunks: u16 },
    /// OTA update completed successfully — target device will reboot.
    OtaFinished,
    /// An error occurred (displayed as a transient banner in the UI).
    Error { message: String },
    /// Informational log message (appended to the log panel).
    Log { message: String },
}

/// Commands received from the frontend over WebSocket.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum WsCommand {
    /// Start a new antenna pattern sweep with the given configuration.
    StartSweep(SweepConfig),
    /// Forward TX radio configuration to the TX board via the RX bridge.
    ConfigureTx(TxConfig),
    /// Abort the current sweep immediately.
    Stop,
    /// Request a fresh status snapshot from the RX board.
    QueryStatus,
    /// List available serial ports (response sent via REST, not WS).
    ListPorts,
    /// Open a serial/TCP connection to the given port.
    Connect { port: String },
    /// Close the current serial/TCP connection.
    Disconnect,
    /// Export measurement data as CSV (available via REST endpoint).
    ExportCsv,
    /// Return the turntable to its home (0°) position.
    ReturnHome,
    /// Jog the turntable by a relative angle (positive = CW, negative = CCW).
    Jog { delta_deg: f32 },
    /// Start an OTA firmware update for the specified target device.
    /// Target is "rx", "tx", "turntable", or "bridge".
    OtaUpload { target: String, firmware_path: String },
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
        json_round_trip(&WsCommand::Jog { delta_deg: -10.0 });
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
        json_round_trip(&WsCommand::OtaUpload {
            target: "rx".to_string(),
            firmware_path: "/tmp/firmware.bin".to_string(),
        });
    }

    #[test]
    fn ws_event_json_round_trip() {
        json_round_trip(&WsEvent::SweepComplete);
        json_round_trip(&WsEvent::HomeComplete);
        json_round_trip(&WsEvent::JogComplete { angle_deg: 45.0 });
        json_round_trip(&WsEvent::Error {
            message: "test error".to_string(),
        });
        json_round_trip(&WsEvent::Log {
            message: "Connected to /dev/ttyUSB0".to_string(),
        });
        json_round_trip(&WsEvent::OtaProgress {
            chunks_sent: 42,
            total_chunks: 100,
        });
        json_round_trip(&WsEvent::OtaFinished);
        json_round_trip(&WsEvent::DataPoint(DataPoint {
            angle_deg: 45.0,
            rssi_dbm: -30.0,
            sample_count: 5,
        }));
        json_round_trip(&WsEvent::Status(SystemStatus {
            sweeping: true,
            tx_connected: false,
            rx_connected: true,
            turntable_connected: true,
            serial_connected: true,
            data_points: 42,
            backend_name: "espnow".to_string(),
            backend_freq_mhz: Some(2400),
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
