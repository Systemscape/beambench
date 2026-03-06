//! TX state machine — pure logic, no hardware dependencies.
//!
//! Tracks transmit state and handles TxCommand messages.

use crate::{EspnowMessage, TxCommand, TxResponse};

/// TX device state.
#[derive(Debug, Clone)]
pub struct TxState {
    pub transmitting: bool,
    /// Transmission interval in milliseconds.
    pub tx_interval_ms: u64,
}

impl TxState {
    pub const fn new() -> Self {
        Self {
            transmitting: false,
            tx_interval_ms: 100,
        }
    }

    /// Process an incoming ESP-NOW message. Returns a response if applicable.
    pub fn handle_message(&mut self, msg: &EspnowMessage) -> Option<EspnowMessage> {
        match msg {
            EspnowMessage::TxCmd(cmd) => match cmd {
                TxCommand::Configure {
                    packet_rate_hz, ..
                } => {
                    if *packet_rate_hz > 0 {
                        self.tx_interval_ms = 1000 / *packet_rate_hz as u64;
                    }
                    Some(EspnowMessage::TxResp(TxResponse::Ack))
                }
                TxCommand::StartTransmit => {
                    self.transmitting = true;
                    Some(EspnowMessage::TxResp(TxResponse::Ack))
                }
                TxCommand::StopTransmit => {
                    self.transmitting = false;
                    Some(EspnowMessage::TxResp(TxResponse::Ack))
                }
            },
            _ => None,
        }
    }
}

impl Default for TxState {
    fn default() -> Self {
        Self::new()
    }
}
