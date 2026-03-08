//! ESP-NOW measurement backend — the default backend using 2.4 GHz RSSI.
//!
//! RX side: wraps `MeasurementState` from the protocol crate, which accumulates
//! RSSI values extracted from received ESP-NOW beacon frames.
//!
//! TX side: controls a transmit flag checked by a broadcast loop task.

use beambench_protocol::rx_logic::MeasurementState;
use beambench_protocol::BackendInfo;
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

const DEFAULT_TX_INTERVAL: Duration = Duration::from_millis(100);

/// ESP-NOW TX backend — broadcasts `MeasurementBeacon` frames at a configurable rate.
///
/// The actual broadcast happens in a separate transmit loop task that polls
/// `is_transmitting()` and `tx_interval()`.
pub struct EspNowTxBackend {
    transmitting: bool,
    tx_interval: Duration,
}

impl EspNowTxBackend {
    pub const fn new() -> Self {
        Self {
            transmitting: false,
            tx_interval: DEFAULT_TX_INTERVAL,
        }
    }

    /// Whether the transmit loop should be broadcasting.
    pub fn is_transmitting(&self) -> bool {
        self.transmitting
    }

    /// Current interval between beacon broadcasts.
    pub fn tx_interval(&self) -> Duration {
        self.tx_interval
    }

    /// Set the beacon broadcast rate. Called on `TxCommand::Configure`.
    pub fn set_packet_rate(&mut self, packet_rate_hz: u16) {
        if packet_rate_hz > 0 {
            self.tx_interval = Duration::from_millis(1000 / packet_rate_hz as u64);
        }
    }
}

impl TxBackend for EspNowTxBackend {
    async fn start_transmit(&mut self) {
        self.transmitting = true;
    }

    async fn stop_transmit(&mut self) {
        self.transmitting = false;
    }
}
