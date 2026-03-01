//! Shared message types for the beambench antenna measurement system.
//!
//! This crate is `no_std`-compatible and used by all firmware crates
//! and the PC application. Messages are serialized with `postcard`.
//!
//! Two protocol layers:
//! - **Serial** (PC ↔ RX board): COBS-framed postcard over USB-serial
//! - **ESPNOW** (between boards): postcard-serialized payloads in ESPNOW frames

#![no_std]

use heapless::String;
use serde::{Deserialize, Serialize};

// ── Board roles (used during ESPNOW discovery) ──────────────────────────────

/// Role a board advertises during ESPNOW discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum Role {
    Rx,
    Tx,
    Turntable,
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
}

// ── Serial protocol messages (PC ↔ RX) ──────────────────────────────────────

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

    use heapless::String;
    use serde::{Deserialize, Serialize};

    use crate::{
        EspnowMessage, HelloBeacon, PairConfirm, PcToRx, Role, RxToPc, TurntableCommand,
        TurntableResponse, TxCommand, TxResponse, MAX_MSG_SIZE,
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
