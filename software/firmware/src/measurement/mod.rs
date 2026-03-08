//! Measurement backend abstraction.
//!
//! Defines traits for swappable RF measurement backends (RX and TX sides).
//! The default backend is ESP-NOW RSSI; the `backend-uart` feature selects
//! an external devboard connected over UART.

pub mod espnow;
#[cfg(feature = "backend-uart")]
pub mod uart;

/// Result of a measurement window.
pub struct MeasurementResult {
    /// Average signal strength in dBm.
    pub rssi_dbm: f32,
    /// Number of samples collected during the window.
    pub sample_count: u16,
}

/// RX-side measurement backend — accumulates samples during a measurement window.
///
/// Between `start()` and `report()`, the backend collects signal strength data.
/// How that happens is backend-specific:
/// - ESP-NOW: RSSI extracted from received beacon frames (accumulated externally)
/// - UART: external devboard collects samples and reports on demand
pub trait RxBackend {
    async fn start(&mut self);
    async fn report(&mut self) -> MeasurementResult;
}

/// TX-side measurement backend — controls the signal source.
///
/// - ESP-NOW: sets a flag; a separate task broadcasts `MeasurementBeacon` frames
/// - UART: sends start/stop commands to an external TX devboard
pub trait TxBackend {
    async fn start_transmit(&mut self);
    async fn stop_transmit(&mut self);
}

// ── Active backend (selected at compile time) ───────────────────────────────

#[cfg(not(feature = "backend-uart"))]
pub type ActiveRxBackend = espnow::EspNowRxBackend;
#[cfg(feature = "backend-uart")]
pub type ActiveRxBackend = uart::UartRxBackend;

#[cfg(not(feature = "backend-uart"))]
pub type ActiveTxBackend = espnow::EspNowTxBackend;
#[cfg(feature = "backend-uart")]
pub type ActiveTxBackend = uart::UartTxBackend;

/// Return metadata for the compile-time-selected backend.
pub fn active_backend_info() -> beambench_protocol::BackendInfo {
    #[cfg(not(feature = "backend-uart"))]
    {
        espnow::backend_info()
    }
    #[cfg(feature = "backend-uart")]
    {
        uart::backend_info()
    }
}
