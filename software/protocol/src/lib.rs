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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct HelloBeacon {
    pub role: Role,
    /// MAC address as 6 bytes.
    pub mac: [u8; 6],
}

/// Sent by RX to confirm pairing (unicast).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PairConfirm {
    pub role: Role,
    pub mac: [u8; 6],
}

// ── ESPNOW operational messages (RX ↔ Turntable) ────────────────────────────

/// Command from RX to turntable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TurntableCommand {
    /// Move to an absolute angle in degrees.
    MoveTo { angle_deg: f32 },
    /// Emergency stop.
    Stop,
}

/// Response from turntable to RX.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum TurntableResponse {
    /// Successfully reached target position.
    MoveComplete { angle_deg: f32 },
    /// Error during motor operation.
    Error { description: String<64> },
}

// ── ESPNOW operational messages (RX ↔ TX) ───────────────────────────────────

/// Command from RX to TX.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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

// ── Serialization helpers ───────────────────────────────────────────────────

/// Maximum serialized message size (ESPNOW payload limit is 250 bytes).
pub const MAX_MSG_SIZE: usize = 250;

// TODO: Add a test that verifies that all the protocol messages are below that max msg size when serialized with postcard

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

    use crate::{PcToRx, RxToPc, MAX_MSG_SIZE};

    #[test]
    fn check_max_msg_size_postcard() {
        fn test_ser_deser<'a, T: Deserialize<'a> + Serialize + PartialEq + Debug>(
            msg: &T,
            buf: &'a mut [u8; MAX_MSG_SIZE],
        ) {
            super::serialize(&msg, &mut buf[..]).expect("Serialization failed");
            let msg_deser: T = super::deserialize(&buf[..]).expect("Deerialization failed");

            assert_eq!(
                &msg_deser, msg,
                "Deserialized message should match the one that has been serialized"
            )
        }
        let mut buf = [0u8; MAX_MSG_SIZE];

        test_ser_deser(
            &RxToPc::Status {
                sweeping: true,
                tx_connected: true,
                turntable_connected: true,
            },
            &mut buf,
        );

        test_ser_deser(
            &RxToPc::Error { description: String::try_from("ABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWXYZABCDEFGHIJKLMNOPQRSTUVWX").expect("Convert &str to heapless string") },
            &mut buf,
        );

        test_ser_deser(
            &PcToRx::StartSweep {
                start_deg: 0.0,
                stop_deg: 180.0,
                step_deg: 10.0,
                samples_per_angle: 100,
            },
            &mut buf,
        );
    }
}
