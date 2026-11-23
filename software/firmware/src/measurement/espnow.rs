//! ESP-NOW measurement backend — the default backend using 2.4 GHz RSSI.
//!
//! RX side: wraps `MeasurementState` from the protocol crate, which accumulates
//! RSSI values extracted from received ESP-NOW beacon frames.
//!
//! TX side: wraps `TxState` from the protocol crate, which holds the transmit
//! flag and interval checked by a broadcast loop task.

use beambench_protocol::BackendInfo;
use beambench_protocol::rx_logic::MeasurementState;
use beambench_protocol::tx_logic::TxState;
use embassy_time::Duration;

use super::{MeasurementResult, RxBackend, TxBackend};

/// Backend metadata for ESP-NOW RSSI measurement.
pub fn backend_info() -> BackendInfo {
    BackendInfo {
        name: heapless::String::try_from("espnow").unwrap(),
        frequency_mhz: Some(2400),
    }
}

// ── RX ──────────────────────────────────────────────────────────────────────

/// ESP-NOW RX backend — accumulates RSSI from received `MeasurementBeacon` frames.
pub struct EspNowRxBackend {
    state: MeasurementState,
}

impl EspNowRxBackend {
    pub const fn new() -> Self {
        Self {
            state: MeasurementState::new(),
        }
    }

    /// Feed an RSSI sample from a received ESP-NOW frame.
    ///
    /// Called by the RX listener for each `MeasurementBeacon` packet.
    /// This is ESP-NOW-specific — other backends collect samples internally.
    pub fn accumulate_rssi(&mut self, rssi: i32) {
        self.state.accumulate_rssi(rssi);
    }
}

impl RxBackend for EspNowRxBackend {
    async fn start(&mut self) {
        self.state.start_measurement();
    }

    async fn report(&mut self) -> MeasurementResult {
        let (rssi_dbm, sample_count) = self.state.report_measurement();
        MeasurementResult {
            rssi_dbm,
            sample_count,
        }
    }
}

// ── TX ──────────────────────────────────────────────────────────────────────

/// ESP-NOW TX backend — broadcasts `MeasurementBeacon` frames at a configurable rate.
///
/// The actual broadcast happens in a separate transmit loop task that polls
/// `is_transmitting()` and `tx_interval()`.
pub struct EspNowTxBackend {
    state: TxState,
}

impl EspNowTxBackend {
    pub const fn new() -> Self {
        Self {
            state: TxState::new(),
        }
    }

    /// Whether the transmit loop should be broadcasting.
    pub fn is_transmitting(&self) -> bool {
        self.state.transmitting
    }

    /// Current interval between beacon broadcasts.
    pub fn tx_interval(&self) -> Duration {
        Duration::from_millis(self.state.tx_interval_ms)
    }

    /// Set the beacon broadcast rate. Called on `TxCommand::Configure`.
    pub fn set_packet_rate(&mut self, packet_rate_hz: u16) {
        self.state.set_packet_rate(packet_rate_hz);
    }
}

impl TxBackend for EspNowTxBackend {
    async fn start_transmit(&mut self) {
        self.state.transmitting = true;
    }

    async fn stop_transmit(&mut self) {
        self.state.transmitting = false;
    }
}
