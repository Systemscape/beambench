//! RX measurement state machine — pure logic, no hardware dependencies.
//!
//! Manages RSSI accumulation between StartMeasurement and ReportMeasurement.

use crate::EspnowMessage;

/// RSSI measurement accumulator.
///
/// The RX device uses this to collect RSSI samples from MeasurementBeacon
/// packets between StartMeasurement and ReportMeasurement commands.
#[derive(Debug, Clone)]
pub struct MeasurementState {
    measuring: bool,
    rssi_sum: i64,
    rssi_count: u16,
}

impl MeasurementState {
    pub const fn new() -> Self {
        Self {
            measuring: false,
            rssi_sum: 0,
            rssi_count: 0,
        }
    }

    /// Whether RSSI samples are currently being collected.
    pub fn is_measuring(&self) -> bool {
        self.measuring
    }

    /// Number of samples collected so far in the current window.
    pub fn sample_count(&self) -> u16 {
        self.rssi_count
    }

    /// Reset accumulator and start collecting.
    pub fn start_measurement(&mut self) {
        self.measuring = true;
        self.rssi_sum = 0;
        self.rssi_count = 0;
    }

    /// Accumulate an RSSI sample (only if currently measuring).
    pub fn accumulate_rssi(&mut self, rssi: i32) {
        if self.measuring {
            self.rssi_sum += rssi as i64;
            self.rssi_count += 1;
        }
    }

    /// Stop collecting, compute average, and return (rssi_dbm, sample_count).
    /// Resets state for the next measurement window.
    pub fn report_measurement(&mut self) -> (f32, u16) {
        let result = if self.rssi_count == 0 {
            (0.0, 0)
        } else {
            (
                self.rssi_sum as f32 / self.rssi_count as f32,
                self.rssi_count,
            )
        };
        self.measuring = false;
        self.rssi_sum = 0;
        self.rssi_count = 0;
        result
    }

    /// Process an incoming ESP-NOW message. Returns a response message if one
    /// should be sent back (i.e. MeasurementResult after ReportMeasurement).
    ///
    /// `rssi` is the RSSI of the received packet (only relevant for
    /// MeasurementBeacon).
    pub fn handle_message(&mut self, msg: &EspnowMessage, rssi: i32) -> Option<EspnowMessage> {
        match msg {
            EspnowMessage::StartMeasurement => {
                self.start_measurement();
                None
            }
            EspnowMessage::ReportMeasurement => {
                let (rssi_dbm, sample_count) = self.report_measurement();
                Some(EspnowMessage::MeasurementResult {
                    rssi_dbm,
                    sample_count,
                })
            }
            EspnowMessage::MeasurementBeacon => {
                self.accumulate_rssi(rssi);
                None
            }
            _ => None,
        }
    }
}

impl Default for MeasurementState {
    fn default() -> Self {
        Self::new()
    }
}
