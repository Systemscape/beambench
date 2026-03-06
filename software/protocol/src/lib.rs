//! Shared message types for the beambench antenna measurement system.
//!
//! This crate is `no_std`-compatible and used by all firmware crates
//! and the PC application. Messages are serialized with `postcard`.
//!
//! Two protocol layers:
//! - **Serial** (PC ↔ RX board): COBS-framed postcard over USB-serial
//! - **ESPNOW** (between boards): postcard-serialized payloads in ESPNOW frames

#![no_std]

pub mod bridge_logic;
pub mod rx_logic;
pub mod tx_logic;

use heapless::{String, Vec};
use serde::{Deserialize, Serialize};

// ── Board roles (used during ESPNOW discovery) ──────────────────────────────

/// Role a board advertises during ESPNOW discovery.
///
/// Explicit `#[repr(u8)]` discriminants ensure the stored role byte in flash
/// remains stable across firmware versions that add or reorder variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[repr(u8)]
pub enum Role {
    Rx = 0,
    Tx = 1,
    Turntable = 2,
    Bridge = 3,
}

impl Role {
    /// Serialize to a single byte for flash storage.
    pub fn to_byte(self) -> u8 {
        self as u8
    }

    /// Deserialize from a flash-stored byte.
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Role::Rx),
            1 => Some(Role::Tx),
            2 => Some(Role::Turntable),
            3 => Some(Role::Bridge),
            _ => None,
        }
    }
}

// ── ESPNOW discovery messages ───────────────────────────────────────────────

/// Broadcast periodically until paired.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct HelloBeacon {
    pub role: Role,
    /// MAC address as 6 bytes.
    pub mac: [u8; 6],
}

/// Sent by RX to confirm pairing (unicast).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PairConfirm {
    pub role: Role,
    pub mac: [u8; 6],
}

// ── ESPNOW operational messages (RX ↔ Turntable) ────────────────────────────

/// Command from RX to turntable.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TurntableCommand {
    /// Move to an absolute angle in degrees.
    MoveTo { angle_deg: f32 },
    /// Emergency stop.
    Stop,
}

/// Response from turntable to RX.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TurntableResponse {
    /// Successfully reached target position.
    MoveComplete { angle_deg: f32 },
    /// Error during motor operation.
    Error { description: String<64> },
}

// ── ESPNOW operational messages (RX ↔ TX) ───────────────────────────────────

/// Command from RX to TX.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxCommand {
    /// Configure transmission parameters.
    Configure {
        /// WiFi channel (1-14).
        channel: u8,
        /// Transmit power in dBm.
        tx_power_dbm: i8,
        /// Packet transmission rate in Hz.
        packet_rate_hz: u16,
    },
    /// Start transmitting packets.
    StartTransmit,
    /// Stop transmitting packets.
    StopTransmit,
}

/// Response from TX to RX.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TxResponse {
    /// Acknowledge a command.
    Ack,
    /// Status report.
    Status {
        transmitting: bool,
        channel: u8,
        tx_power_dbm: i8,
        packet_rate_hz: u16,
    },
}

// ── Wrapper for all ESPNOW messages ─────────────────────────────────────────

/// Top-level ESPNOW message envelope.
///
/// Every ESPNOW frame payload is a postcard-serialized `EspnowMessage`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum EspnowMessage {
    // Discovery
    Hello(HelloBeacon),
    PairConfirm(PairConfirm),

    // Turntable
    TurntableCmd(TurntableCommand),
    TurntableResp(TurntableResponse),

    // TX
    TxCmd(TxCommand),
    TxResp(TxResponse),

    // Measurement (v2)
    /// Broadcast by TX at configured rate. RX accumulates RSSI from these
    /// only when actively measuring.
    MeasurementBeacon,
    /// Bridge -> RX: reset accumulator and start collecting.
    StartMeasurement,
    /// Bridge -> RX: stop collecting, report result.
    ReportMeasurement,
    /// RX -> Bridge: measurement result.
    MeasurementResult { rssi_dbm: f32, sample_count: u16 },

    // OTA firmware update (Bridge ↔ field device)
    /// Bridge -> target: begin OTA update with expected size and hash.
    OtaBegin { total_size: u32, sha256: [u8; 32] },
    /// Target -> Bridge: ready to receive OTA data.
    OtaReady,
    /// Bridge -> target: firmware chunk (max 240 bytes to fit ESP-NOW payload).
    OtaData { seq: u16, data: Vec<u8, 240> },
    /// Target -> Bridge: acknowledge receipt of chunk.
    OtaAck { seq: u16 },
    /// Bridge -> target: all chunks sent.
    OtaFinish,
    /// Target -> Bridge: OTA verified and applied, rebooting.
    OtaComplete,
    /// Target -> Bridge: OTA failed.
    OtaError { description: String<64> },
}

// ── Serial protocol v2 (PC ↔ Bridge) ────────────────────────────────────────

/// Command from PC to Bridge (over USB-serial, COBS-framed postcard).
///
/// The Bridge translates these into ESP-NOW messages and routes them
/// to the appropriate field device by role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PcCommand {
    // TX control
    ConfigureTx {
        channel: u8,
        tx_power_dbm: i8,
        packet_rate_hz: u16,
    },
    StartTransmitting,
    StopTransmitting,

    // Stepper control
    MoveTo { angle_deg: f32 },
    StopStepper,
    ReturnHome,

    // RX control
    StartMeasurement,
    ReportMeasurement,

    // System
    QueryStatus,

    // OTA firmware update (PC -> Bridge -> target)
    /// Begin OTA: target role, firmware size, and SHA-256 hash.
    OtaBegin { target: Role, total_size: u32, sha256: [u8; 32] },
    /// Stream a firmware chunk (Bridge relays to target via ESP-NOW).
    OtaData { target: Role, seq: u16, data: Vec<u8, 240> },
    /// All chunks sent — target should verify and apply.
    OtaFinish { target: Role },
}

/// Event from Bridge to PC (over USB-serial, COBS-framed postcard).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum DeviceEvent {
    // TX responses
    TxAck,

    // Stepper responses
    MoveComplete { angle_deg: f32 },
    StepperError { description: String<64> },
    HomeComplete,

    // RX responses
    Measurement { rssi_dbm: f32, sample_count: u16 },

    // System
    Status {
        tx_connected: bool,
        rx_connected: bool,
        stepper_connected: bool,
    },
    Error { description: String<128> },

    // OTA firmware update (target -> Bridge -> PC)
    /// Target is ready to receive OTA data.
    OtaReady,
    /// Target acknowledged a chunk.
    OtaAck { seq: u16 },
    /// OTA verified and applied, target is rebooting.
    OtaComplete,
    /// OTA failed on the target device.
    OtaError { description: String<64> },
}

// ── Serial protocol v1 (PC ↔ RX, legacy) ───────────────────────────────────

/// Command from PC to RX board (over USB-serial).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PcToRx {
    /// Start an antenna pattern sweep.
    StartSweep {
        /// Start angle in degrees.
        start_deg: f32,
        /// Stop angle in degrees.
        stop_deg: f32,
        /// Angular step size in degrees.
        step_deg: f32,
        /// Number of RSSI samples to average per angle.
        samples_per_angle: u16,
    },
    /// Configure the TX board (forwarded via ESPNOW).
    ConfigureTx {
        channel: u8,
        tx_power_dbm: i8,
        packet_rate_hz: u16,
    },
    /// Abort the current sweep.
    Stop,
    /// Request status from RX.
    QueryStatus,
    /// Return turntable to home (0°) position.
    ReturnHome,
}

/// Message from RX board to PC (over USB-serial).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RxToPc {
    /// One measurement data point.
    DataPoint {
        /// Turntable angle in degrees.
        angle_deg: f32,
        /// Measured RSSI in dBm (averaged).
        rssi_dbm: f32,
        /// Number of samples actually collected.
        sample_count: u16,
    },
    /// Sweep finished successfully.
    SweepComplete,
    /// Turntable reached home (0°) after a `PcToRx::ReturnHome` command.
    HomeComplete,
    /// Error during operation.
    Error { description: String<128> },
    /// Status report.
    Status {
        /// Whether a sweep is in progress.
        sweeping: bool,
        /// Whether TX peer is connected.
        tx_connected: bool,
        /// Whether turntable peer is connected.
        turntable_connected: bool,
    },
}

// ── Stepper angle conversion (pure math, testable on host) ──────────────────

pub mod stepper {
    /// Steps per revolution: 200 full steps × 16 microsteps.
    pub const STEPS_PER_REV: f32 = 200.0 * 16.0;

    /// Convert an angle in degrees to stepper motor steps.
    pub fn degrees_to_steps(angle_deg: f32) -> i32 {
        (angle_deg / 360.0 * STEPS_PER_REV) as i32
    }

    /// Convert stepper motor steps to an angle in degrees.
    pub fn steps_to_degrees(steps: i32) -> f32 {
        (steps as f32) / STEPS_PER_REV * 360.0
    }
}

// ── Serialization helpers ───────────────────────────────────────────────────

/// Maximum serialized message size (ESPNOW payload limit is 250 bytes).
pub const MAX_MSG_SIZE: usize = 250;

/// Serialize a message into a buffer. Returns the serialized slice.
pub fn serialize<'a, T: Serialize>(
    msg: &T,
    buf: &'a mut [u8],
) -> Result<&'a mut [u8], postcard::Error> {
    postcard::to_slice(msg, buf)
}

/// Deserialize a message from a byte slice.
pub fn deserialize<'a, T: Deserialize<'a>>(buf: &'a [u8]) -> Result<T, postcard::Error> {
    postcard::from_bytes(buf)
}

/// Serialize a message with COBS framing (for serial protocol).
/// Returns the number of bytes written to `buf`.
pub fn serialize_cobs<T: Serialize>(msg: &T, buf: &mut [u8]) -> Result<usize, postcard::Error> {
    let used = postcard::to_slice_cobs(msg, buf)?;
    Ok(used.len())
}

#[cfg(test)]
mod test {
    use core::fmt::Debug;

    use heapless::{String, Vec};
    use serde::{Deserialize, Serialize};

    use crate::{
        DeviceEvent, EspnowMessage, HelloBeacon, PairConfirm, PcCommand, PcToRx, Role, RxToPc,
        TurntableCommand, TurntableResponse, TxCommand, TxResponse, MAX_MSG_SIZE,
    };

    /// Serialize `msg` into `buf` with postcard, then deserialize and assert equality.
    /// Verifies that the message fits within MAX_MSG_SIZE and survives a round-trip.
    fn test_ser_deser<'a, T: Deserialize<'a> + Serialize + PartialEq + Debug>(
        msg: &T,
        buf: &'a mut [u8; MAX_MSG_SIZE],
    ) {
        super::serialize(&msg, &mut buf[..]).expect("Serialization failed");
        let msg_deser: T = super::deserialize(&buf[..]).expect("Deserialization failed");

        assert_eq!(
            &msg_deser, msg,
            "Deserialized message should match the one that has been serialized"
        )
    }

    #[test]
    fn check_max_msg_size_postcard() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        // RxToPc variants
        test_ser_deser(
            &RxToPc::Status {
                sweeping: true,
                tx_connected: true,
                turntable_connected: true,
            },
            &mut buf,
        );

        test_ser_deser(
            &RxToPc::Error {
                description: String::try_from(
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZ\
                     ABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZ\
                     ABCDEFGHIJKLMNOPQRSTUVWX",
                )
                .expect("Convert &str to heapless string"),
            },
            &mut buf,
        );

        test_ser_deser(
            &RxToPc::DataPoint {
                angle_deg: 45.0,
                rssi_dbm: -42.5,
                sample_count: 10,
            },
            &mut buf,
        );

        test_ser_deser(&RxToPc::SweepComplete, &mut buf);

        test_ser_deser(&RxToPc::HomeComplete, &mut buf);

        // PcToRx variants
        test_ser_deser(
            &PcToRx::StartSweep {
                start_deg: 0.0,
                stop_deg: 180.0,
                step_deg: 10.0,
                samples_per_angle: 100,
            },
            &mut buf,
        );

        test_ser_deser(
            &PcToRx::ConfigureTx {
                channel: 6,
                tx_power_dbm: 20,
                packet_rate_hz: 100,
            },
            &mut buf,
        );

        test_ser_deser(&PcToRx::Stop, &mut buf);

        test_ser_deser(&PcToRx::QueryStatus, &mut buf);
    }

    /// Serialize with COBS, verify zero-delimiter, deserialize and check equality.
    fn test_cobs_round_trip<T: Serialize + for<'a> Deserialize<'a> + PartialEq + Debug>(
        msg: &T,
        buf: &mut [u8; MAX_MSG_SIZE],
    ) {
        let len = super::serialize_cobs(msg, buf).expect("COBS serialize failed");
        assert_eq!(buf[len - 1], 0x00, "COBS frame must end with zero byte");
        let decoded: T =
            postcard::from_bytes_cobs(&mut buf[..len]).expect("COBS deserialize failed");
        assert_eq!(&decoded, msg);
    }

    #[test]
    fn cobs_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        // PcToRx variants
        test_cobs_round_trip(
            &PcToRx::StartSweep {
                start_deg: 0.0,
                stop_deg: 360.0,
                step_deg: 5.0,
                samples_per_angle: 50,
            },
            &mut buf,
        );
        test_cobs_round_trip(
            &PcToRx::ConfigureTx {
                channel: 1,
                tx_power_dbm: -10,
                packet_rate_hz: 200,
            },
            &mut buf,
        );
        test_cobs_round_trip(&PcToRx::Stop, &mut buf);
        test_cobs_round_trip(&PcToRx::QueryStatus, &mut buf);

        // RxToPc variants
        test_cobs_round_trip(
            &RxToPc::DataPoint {
                angle_deg: 90.0,
                rssi_dbm: -30.0,
                sample_count: 25,
            },
            &mut buf,
        );
        test_cobs_round_trip(&RxToPc::SweepComplete, &mut buf);
        test_cobs_round_trip(&RxToPc::HomeComplete, &mut buf);
        test_cobs_round_trip(
            &RxToPc::Error {
                description: String::try_from("test error").unwrap(),
            },
            &mut buf,
        );
        test_cobs_round_trip(
            &RxToPc::Status {
                sweeping: false,
                tx_connected: true,
                turntable_connected: false,
            },
            &mut buf,
        );
    }

    #[test]
    fn espnow_message_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        // Discovery
        test_ser_deser(
            &EspnowMessage::Hello(HelloBeacon {
                role: Role::Tx,
                mac: [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF],
            }),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::PairConfirm(PairConfirm {
                role: Role::Turntable,
                mac: [1, 2, 3, 4, 5, 6],
            }),
            &mut buf,
        );

        // Turntable commands/responses
        test_ser_deser(
            &EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 180.0 }),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::TurntableCmd(TurntableCommand::Stop),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::TurntableResp(TurntableResponse::MoveComplete { angle_deg: 90.0 }),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::TurntableResp(TurntableResponse::Error {
                description: String::try_from("motor stall detected").unwrap(),
            }),
            &mut buf,
        );

        // TX commands/responses
        test_ser_deser(
            &EspnowMessage::TxCmd(TxCommand::Configure {
                channel: 6,
                tx_power_dbm: 20,
                packet_rate_hz: 100,
            }),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::TxCmd(TxCommand::StartTransmit),
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::TxCmd(TxCommand::StopTransmit),
            &mut buf,
        );
        test_ser_deser(&EspnowMessage::TxResp(TxResponse::Ack), &mut buf);
        test_ser_deser(
            &EspnowMessage::TxResp(TxResponse::Status {
                transmitting: true,
                channel: 11,
                tx_power_dbm: -5,
                packet_rate_hz: 200,
            }),
            &mut buf,
        );
    }

    #[test]
    fn max_length_turntable_error_fits() {
        let mut buf = [0u8; MAX_MSG_SIZE];
        // 64-char description (max capacity of String<64>)
        let desc = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert_eq!(desc.len(), 64);
        test_ser_deser(
            &EspnowMessage::TurntableResp(TurntableResponse::Error {
                description: String::try_from(desc).unwrap(),
            }),
            &mut buf,
        );
    }

    #[test]
    fn v2_pc_command_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_cobs_round_trip(
            &PcCommand::ConfigureTx {
                channel: 6,
                tx_power_dbm: 20,
                packet_rate_hz: 100,
            },
            &mut buf,
        );
        test_cobs_round_trip(&PcCommand::StartTransmitting, &mut buf);
        test_cobs_round_trip(&PcCommand::StopTransmitting, &mut buf);
        test_cobs_round_trip(&PcCommand::MoveTo { angle_deg: 45.0 }, &mut buf);
        test_cobs_round_trip(&PcCommand::StopStepper, &mut buf);
        test_cobs_round_trip(&PcCommand::ReturnHome, &mut buf);
        test_cobs_round_trip(&PcCommand::StartMeasurement, &mut buf);
        test_cobs_round_trip(&PcCommand::ReportMeasurement, &mut buf);
        test_cobs_round_trip(&PcCommand::QueryStatus, &mut buf);
    }

    #[test]
    fn v2_device_event_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_cobs_round_trip(&DeviceEvent::TxAck, &mut buf);
        test_cobs_round_trip(
            &DeviceEvent::MoveComplete { angle_deg: 90.0 },
            &mut buf,
        );
        test_cobs_round_trip(
            &DeviceEvent::StepperError {
                description: String::try_from("stall").unwrap(),
            },
            &mut buf,
        );
        test_cobs_round_trip(&DeviceEvent::HomeComplete, &mut buf);
        test_cobs_round_trip(
            &DeviceEvent::Measurement {
                rssi_dbm: -42.5,
                sample_count: 100,
            },
            &mut buf,
        );
        test_cobs_round_trip(
            &DeviceEvent::Status {
                tx_connected: true,
                rx_connected: true,
                stepper_connected: false,
            },
            &mut buf,
        );
        test_cobs_round_trip(
            &DeviceEvent::Error {
                description: String::try_from("timeout").unwrap(),
            },
            &mut buf,
        );
    }

    #[test]
    fn v2_espnow_measurement_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_ser_deser(&EspnowMessage::MeasurementBeacon, &mut buf);
        test_ser_deser(&EspnowMessage::StartMeasurement, &mut buf);
        test_ser_deser(&EspnowMessage::ReportMeasurement, &mut buf);
        test_ser_deser(
            &EspnowMessage::MeasurementResult {
                rssi_dbm: -55.0,
                sample_count: 42,
            },
            &mut buf,
        );
    }

    #[test]
    fn ota_message_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        // EspnowMessage OTA variants
        test_ser_deser(
            &EspnowMessage::OtaBegin {
                total_size: 0x40000,
                sha256: [0xAB; 32],
            },
            &mut buf,
        );
        test_ser_deser(&EspnowMessage::OtaReady, &mut buf);
        test_ser_deser(&EspnowMessage::OtaFinish, &mut buf);
        test_ser_deser(&EspnowMessage::OtaComplete, &mut buf);
        test_ser_deser(
            &EspnowMessage::OtaAck { seq: 1000 },
            &mut buf,
        );
        test_ser_deser(
            &EspnowMessage::OtaError {
                description: String::try_from("flash write failed").unwrap(),
            },
            &mut buf,
        );

        // OtaData with max-size payload (240 bytes)
        let mut data = Vec::<u8, 240>::new();
        data.resize(240, 0xFF).unwrap();
        test_ser_deser(
            &EspnowMessage::OtaData { seq: 0xFFFF, data },
            &mut buf,
        );
    }

    #[test]
    fn ota_pc_command_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_cobs_round_trip(
            &PcCommand::OtaBegin {
                target: Role::Rx,
                total_size: 0x40000,
                sha256: [0xCD; 32],
            },
            &mut buf,
        );

        let mut data = Vec::<u8, 240>::new();
        data.resize(128, 0xAA).unwrap();
        test_cobs_round_trip(
            &PcCommand::OtaData {
                target: Role::Rx,
                seq: 42,
                data,
            },
            &mut buf,
        );
        test_cobs_round_trip(&PcCommand::OtaFinish { target: Role::Tx }, &mut buf);
    }

    #[test]
    fn ota_device_event_round_trip() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_cobs_round_trip(&DeviceEvent::OtaReady, &mut buf);
        test_cobs_round_trip(&DeviceEvent::OtaAck { seq: 500 }, &mut buf);
        test_cobs_round_trip(&DeviceEvent::OtaComplete, &mut buf);
        test_cobs_round_trip(
            &DeviceEvent::OtaError {
                description: String::try_from("sha256 mismatch").unwrap(),
            },
            &mut buf,
        );
    }

    // ── Bridge routing tests ──────────────────────────────────────────────

    mod bridge_routing_tests {
        use crate::bridge_logic::{route_command, translate_response, RouteAction};
        use crate::*;

        #[test]
        fn start_transmitting_routes_to_tx() {
            let action = route_command(&PcCommand::StartTransmitting);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Tx,
                    msg: EspnowMessage::TxCmd(TxCommand::StartTransmit),
                }
            );
        }

        #[test]
        fn stop_transmitting_routes_to_tx() {
            let action = route_command(&PcCommand::StopTransmitting);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Tx,
                    msg: EspnowMessage::TxCmd(TxCommand::StopTransmit),
                }
            );
        }

        #[test]
        fn configure_tx_routes_to_tx() {
            let action = route_command(&PcCommand::ConfigureTx {
                channel: 6,
                tx_power_dbm: 20,
                packet_rate_hz: 100,
            });
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Tx,
                    msg: EspnowMessage::TxCmd(TxCommand::Configure {
                        channel: 6,
                        tx_power_dbm: 20,
                        packet_rate_hz: 100,
                    }),
                }
            );
        }

        #[test]
        fn move_to_routes_to_turntable() {
            let action = route_command(&PcCommand::MoveTo { angle_deg: 90.0 });
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Turntable,
                    msg: EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 90.0 }),
                }
            );
        }

        #[test]
        fn stop_stepper_routes_to_turntable() {
            let action = route_command(&PcCommand::StopStepper);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Turntable,
                    msg: EspnowMessage::TurntableCmd(TurntableCommand::Stop),
                }
            );
        }

        #[test]
        fn return_home_routes_to_turntable_zero() {
            let action = route_command(&PcCommand::ReturnHome);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Turntable,
                    msg: EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 0.0 }),
                }
            );
        }

        #[test]
        fn start_measurement_routes_to_rx() {
            let action = route_command(&PcCommand::StartMeasurement);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Rx,
                    msg: EspnowMessage::StartMeasurement,
                }
            );
        }

        #[test]
        fn report_measurement_routes_to_rx() {
            let action = route_command(&PcCommand::ReportMeasurement);
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Rx,
                    msg: EspnowMessage::ReportMeasurement,
                }
            );
        }

        #[test]
        fn query_status_is_local() {
            assert_eq!(route_command(&PcCommand::QueryStatus), RouteAction::Local);
        }

        #[test]
        fn translate_turntable_move_complete() {
            let msg =
                EspnowMessage::TurntableResp(TurntableResponse::MoveComplete { angle_deg: 45.0 });
            assert_eq!(
                translate_response(&msg),
                Some(DeviceEvent::MoveComplete { angle_deg: 45.0 })
            );
        }

        #[test]
        fn translate_turntable_error() {
            let msg = EspnowMessage::TurntableResp(TurntableResponse::Error {
                description: String::try_from("stall").unwrap(),
            });
            assert_eq!(
                translate_response(&msg),
                Some(DeviceEvent::StepperError {
                    description: String::try_from("stall").unwrap(),
                })
            );
        }

        #[test]
        fn translate_tx_ack() {
            let msg = EspnowMessage::TxResp(TxResponse::Ack);
            assert_eq!(translate_response(&msg), Some(DeviceEvent::TxAck));
        }

        #[test]
        fn translate_measurement_result() {
            let msg = EspnowMessage::MeasurementResult {
                rssi_dbm: -42.5,
                sample_count: 10,
            };
            assert_eq!(
                translate_response(&msg),
                Some(DeviceEvent::Measurement {
                    rssi_dbm: -42.5,
                    sample_count: 10,
                })
            );
        }

        #[test]
        fn translate_ignores_non_responses() {
            assert_eq!(translate_response(&EspnowMessage::MeasurementBeacon), None);
            assert_eq!(translate_response(&EspnowMessage::StartMeasurement), None);
            assert_eq!(
                translate_response(&EspnowMessage::Hello(HelloBeacon {
                    role: Role::Tx,
                    mac: [0; 6],
                })),
                None
            );
        }

        // ── OTA routing tests ────────────────────────────────────────────

        #[test]
        fn ota_begin_routes_to_target() {
            let action = route_command(&PcCommand::OtaBegin {
                target: Role::Rx,
                total_size: 0x40000,
                sha256: [0xAB; 32],
            });
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Rx,
                    msg: EspnowMessage::OtaBegin {
                        total_size: 0x40000,
                        sha256: [0xAB; 32],
                    },
                }
            );
        }

        #[test]
        fn ota_data_routes_to_target() {
            let mut data = Vec::<u8, 240>::new();
            data.resize(10, 0xFF).unwrap();
            let action = route_command(&PcCommand::OtaData {
                target: Role::Tx,
                seq: 5,
                data: data.clone(),
            });
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Tx,
                    msg: EspnowMessage::OtaData { seq: 5, data },
                }
            );
        }

        #[test]
        fn ota_finish_routes_to_target() {
            let action = route_command(&PcCommand::OtaFinish {
                target: Role::Turntable,
            });
            assert_eq!(
                action,
                RouteAction::SendTo {
                    role: Role::Turntable,
                    msg: EspnowMessage::OtaFinish,
                }
            );
        }

        #[test]
        fn translate_ota_ready() {
            assert_eq!(
                translate_response(&EspnowMessage::OtaReady),
                Some(DeviceEvent::OtaReady),
            );
        }

        #[test]
        fn translate_ota_ack() {
            assert_eq!(
                translate_response(&EspnowMessage::OtaAck { seq: 42 }),
                Some(DeviceEvent::OtaAck { seq: 42 }),
            );
        }

        #[test]
        fn translate_ota_complete() {
            assert_eq!(
                translate_response(&EspnowMessage::OtaComplete),
                Some(DeviceEvent::OtaComplete),
            );
        }

        #[test]
        fn translate_ota_error() {
            let desc = String::try_from("flash error").unwrap();
            assert_eq!(
                translate_response(&EspnowMessage::OtaError {
                    description: desc.clone(),
                }),
                Some(DeviceEvent::OtaError { description: desc }),
            );
        }
    }

    // ── RX measurement logic tests ───────────────────────────────────────

    mod rx_logic_tests {
        use crate::rx_logic::MeasurementState;
        use crate::EspnowMessage;

        #[test]
        fn initially_not_measuring() {
            let state = MeasurementState::new();
            assert!(!state.is_measuring());
            assert_eq!(state.sample_count(), 0);
        }

        #[test]
        fn ignores_rssi_when_not_measuring() {
            let mut state = MeasurementState::new();
            state.accumulate_rssi(-40);
            state.accumulate_rssi(-50);
            assert_eq!(state.sample_count(), 0);
        }

        #[test]
        fn start_then_accumulate_then_report() {
            let mut state = MeasurementState::new();
            state.start_measurement();
            assert!(state.is_measuring());

            state.accumulate_rssi(-40);
            state.accumulate_rssi(-50);
            state.accumulate_rssi(-60);
            assert_eq!(state.sample_count(), 3);

            let (rssi, count) = state.report_measurement();
            assert_eq!(count, 3);
            assert!((rssi - (-50.0)).abs() < 0.01); // average of -40, -50, -60
            assert!(!state.is_measuring());
            assert_eq!(state.sample_count(), 0);
        }

        #[test]
        fn report_with_no_samples() {
            let mut state = MeasurementState::new();
            state.start_measurement();
            let (rssi, count) = state.report_measurement();
            assert_eq!(count, 0);
            assert_eq!(rssi, 0.0);
        }

        #[test]
        fn start_resets_previous_data() {
            let mut state = MeasurementState::new();
            state.start_measurement();
            state.accumulate_rssi(-30);
            state.accumulate_rssi(-30);
            // Start again without reporting — should reset.
            state.start_measurement();
            state.accumulate_rssi(-70);
            let (rssi, count) = state.report_measurement();
            assert_eq!(count, 1);
            assert!((rssi - (-70.0)).abs() < 0.01);
        }

        #[test]
        fn handle_message_measurement_cycle() {
            let mut state = MeasurementState::new();

            // StartMeasurement via handle_message.
            let resp = state.handle_message(&EspnowMessage::StartMeasurement, 0);
            assert!(resp.is_none());
            assert!(state.is_measuring());

            // Feed beacons.
            let resp = state.handle_message(&EspnowMessage::MeasurementBeacon, -45);
            assert!(resp.is_none());
            let resp = state.handle_message(&EspnowMessage::MeasurementBeacon, -55);
            assert!(resp.is_none());

            // ReportMeasurement.
            let resp = state.handle_message(&EspnowMessage::ReportMeasurement, 0);
            assert_eq!(
                resp,
                Some(EspnowMessage::MeasurementResult {
                    rssi_dbm: -50.0,
                    sample_count: 2,
                })
            );
            assert!(!state.is_measuring());
        }

        #[test]
        fn handle_message_ignores_unrelated() {
            let mut state = MeasurementState::new();
            let resp = state.handle_message(
                &EspnowMessage::TxCmd(crate::TxCommand::StartTransmit),
                0,
            );
            assert!(resp.is_none());
            assert!(!state.is_measuring());
        }
    }

    // ── TX state logic tests ─────────────────────────────────────────────

    mod tx_logic_tests {
        use crate::tx_logic::TxState;
        use crate::{EspnowMessage, TxCommand, TxResponse};

        #[test]
        fn initially_not_transmitting() {
            let state = TxState::new();
            assert!(!state.transmitting);
            assert_eq!(state.tx_interval_ms, 100);
        }

        #[test]
        fn start_transmit() {
            let mut state = TxState::new();
            let resp = state.handle_message(&EspnowMessage::TxCmd(TxCommand::StartTransmit));
            assert!(state.transmitting);
            assert_eq!(resp, Some(EspnowMessage::TxResp(TxResponse::Ack)));
        }

        #[test]
        fn stop_transmit() {
            let mut state = TxState::new();
            state.transmitting = true;
            let resp = state.handle_message(&EspnowMessage::TxCmd(TxCommand::StopTransmit));
            assert!(!state.transmitting);
            assert_eq!(resp, Some(EspnowMessage::TxResp(TxResponse::Ack)));
        }

        #[test]
        fn configure_updates_interval() {
            let mut state = TxState::new();
            let resp = state.handle_message(&EspnowMessage::TxCmd(TxCommand::Configure {
                channel: 6,
                tx_power_dbm: 20,
                packet_rate_hz: 200,
            }));
            assert_eq!(state.tx_interval_ms, 5); // 1000/200
            assert_eq!(resp, Some(EspnowMessage::TxResp(TxResponse::Ack)));
        }

        #[test]
        fn ignores_unrelated_messages() {
            let mut state = TxState::new();
            let resp = state.handle_message(&EspnowMessage::MeasurementBeacon);
            assert!(resp.is_none());
        }
    }

    // ── End-to-end measurement simulation ────────────────────────────────

    mod measurement_simulation {
        use crate::bridge_logic::{route_command, translate_response, RouteAction};
        use crate::rx_logic::MeasurementState;
        use crate::tx_logic::TxState;
        use crate::*;

        /// Helper: route a PcCommand and assert it routes to a specific role.
        fn route_expect(cmd: &PcCommand, expected_role: Role) -> EspnowMessage {
            match route_command(cmd) {
                RouteAction::SendTo { role, msg } => {
                    assert_eq!(role, expected_role);
                    msg
                }
                RouteAction::Local => panic!("expected SendTo, got Local"),
            }
        }

        /// Helper: do one measurement cycle at an angle. Returns (rssi_dbm, sample_count).
        fn measure_at_angle(
            rx: &mut MeasurementState,
            rssi_samples: &[i32],
        ) -> (f32, u16) {
            let msg = route_expect(&PcCommand::StartMeasurement, Role::Rx);
            let resp = rx.handle_message(&msg, 0);
            assert!(resp.is_none());

            for &rssi in rssi_samples {
                rx.accumulate_rssi(rssi);
            }

            let msg = route_expect(&PcCommand::ReportMeasurement, Role::Rx);
            let resp = rx.handle_message(&msg, 0).expect("should produce MeasurementResult");
            let event = translate_response(&resp).expect("should translate");
            match event {
                DeviceEvent::Measurement { rssi_dbm, sample_count } => (rssi_dbm, sample_count),
                other => panic!("expected Measurement, got {:?}", other),
            }
        }

        /// Simulate a minimal single-angle measurement: PC sends commands through
        /// the Bridge routing, TX and RX process them, and Bridge translates
        /// responses back to DeviceEvents for the PC.
        #[test]
        fn single_angle_measurement() {
            let mut tx = TxState::new();
            let mut rx = MeasurementState::new();

            // 1. PC -> Bridge -> TX: StartTransmitting
            let msg = route_expect(&PcCommand::StartTransmitting, Role::Tx);
            let resp = tx.handle_message(&msg);
            assert!(tx.transmitting);
            let event = translate_response(&resp.unwrap()).unwrap();
            assert_eq!(event, DeviceEvent::TxAck);

            // 2. PC -> Bridge -> Stepper: MoveTo 90°
            let msg = route_expect(&PcCommand::MoveTo { angle_deg: 90.0 }, Role::Turntable);
            assert_eq!(
                msg,
                EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 90.0 })
            );
            // Simulate turntable responding.
            let turntable_resp =
                EspnowMessage::TurntableResp(TurntableResponse::MoveComplete { angle_deg: 90.0 });
            let event = translate_response(&turntable_resp).unwrap();
            assert_eq!(event, DeviceEvent::MoveComplete { angle_deg: 90.0 });

            // 3-5. StartMeasurement → accumulate RSSI → ReportMeasurement
            let (rssi_dbm, sample_count) = measure_at_angle(&mut rx, &[-42, -44, -40, -43, -41]);
            assert_eq!(sample_count, 5);
            // Average of -42, -44, -40, -43, -41 = -210/5 = -42.0
            assert!((rssi_dbm - (-42.0)).abs() < 0.01);

            // 6. PC -> Bridge -> TX: StopTransmitting
            let msg = route_expect(&PcCommand::StopTransmitting, Role::Tx);
            tx.handle_message(&msg);
            assert!(!tx.transmitting);
        }

        /// Simulate a full 3-angle sweep.
        #[test]
        fn three_angle_sweep() {
            let mut tx = TxState::new();
            let mut rx = MeasurementState::new();

            // Start TX.
            let msg = route_expect(&PcCommand::StartTransmitting, Role::Tx);
            tx.handle_message(&msg);
            assert!(tx.transmitting);

            // Angle 0°: avg(-30,-32,-31) = -31.0, 3 samples
            let _ = route_expect(&PcCommand::MoveTo { angle_deg: 0.0 }, Role::Turntable);
            let (rssi, count) = measure_at_angle(&mut rx, &[-30, -32, -31]);
            assert!((rssi - (-31.0)).abs() < 0.01);
            assert_eq!(count, 3);

            // Angle 10°: avg(-40,-42) = -41.0, 2 samples
            let _ = route_expect(&PcCommand::MoveTo { angle_deg: 10.0 }, Role::Turntable);
            let (rssi, count) = measure_at_angle(&mut rx, &[-40, -42]);
            assert!((rssi - (-41.0)).abs() < 0.01);
            assert_eq!(count, 2);

            // Angle 20°: avg(-50,-48,-52,-49) = -49.75, 4 samples
            let _ = route_expect(&PcCommand::MoveTo { angle_deg: 20.0 }, Role::Turntable);
            let (rssi, count) = measure_at_angle(&mut rx, &[-50, -48, -52, -49]);
            assert!((rssi - (-49.75)).abs() < 0.01);
            assert_eq!(count, 4);

            // Stop TX.
            let msg = route_expect(&PcCommand::StopTransmitting, Role::Tx);
            tx.handle_message(&msg);
            assert!(!tx.transmitting);
        }
    }

    // ── Stepper angle conversion tests ──────────────────────────────────────

    mod stepper_tests {
        use crate::stepper::*;

        #[test]
        fn zero_degrees_is_zero_steps() {
            assert_eq!(degrees_to_steps(0.0), 0);
        }

        #[test]
        fn full_revolution() {
            assert_eq!(degrees_to_steps(360.0), 3200);
        }

        #[test]
        fn half_revolution() {
            assert_eq!(degrees_to_steps(180.0), 1600);
        }

        #[test]
        fn negative_angle() {
            assert_eq!(degrees_to_steps(-90.0), -800);
        }

        #[test]
        fn round_trip_cardinal_angles() {
            for angle in [0.0_f32, 45.0, 90.0, 180.0, 270.0, 360.0] {
                let steps = degrees_to_steps(angle);
                let recovered = steps_to_degrees(steps);
                assert!(
                    (recovered - angle).abs() < 0.12,
                    "round-trip for {angle}°: got {recovered}°"
                );
            }
        }

        #[test]
        fn round_trip_fractional_angles() {
            // Resolution is 360/3200 ≈ 0.1125°/step
            let angle = 33.75; // exactly 300 steps
            let steps = degrees_to_steps(angle);
            assert_eq!(steps, 300);
            let recovered = steps_to_degrees(steps);
            assert!((recovered - angle).abs() < 0.001);
        }

        #[test]
        fn truncation_behavior() {
            // 0.05° → 0.05/360*3200 = 0.444 → truncates to 0
            assert_eq!(degrees_to_steps(0.05), 0);
            // 0.12° → 0.12/360*3200 = 1.067 → truncates to 1
            assert_eq!(degrees_to_steps(0.12), 1);
        }

        #[test]
        fn steps_per_rev_value() {
            assert_eq!(STEPS_PER_REV, 3200.0);
        }
    }
}
