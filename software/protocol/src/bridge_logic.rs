//! Bridge message routing logic — pure functions, no hardware dependencies.
//!
//! Extracts the PcCommand → EspnowMessage routing and EspnowMessage → DeviceEvent
//! translation so they can be tested on the host without ESP-NOW.

use crate::{
    DeviceEvent, EspnowMessage, PcCommand, Role, TurntableCommand, TurntableResponse, TxCommand,
    TxResponse,
};

/// The result of routing a PcCommand: either an ESP-NOW message to send to a
/// specific role, or a locally-handled event (e.g. QueryStatus).
#[derive(Debug, Clone, PartialEq)]
pub enum RouteAction {
    /// Send this EspnowMessage to the given role.
    SendTo { role: Role, msg: EspnowMessage },
    /// Command handled locally — no ESP-NOW send needed.
    Local,
}

/// Route a PcCommand to the appropriate field device.
///
/// Returns `SendTo` with the target role and translated EspnowMessage,
/// or `Local` for commands handled by the Bridge itself (QueryStatus).
pub fn route_command(cmd: &PcCommand) -> RouteAction {
    match cmd {
        PcCommand::ConfigureTx {
            channel,
            tx_power_dbm,
            packet_rate_hz,
        } => RouteAction::SendTo {
            role: Role::Tx,
            msg: EspnowMessage::TxCmd(TxCommand::Configure {
                channel: *channel,
                tx_power_dbm: *tx_power_dbm,
                packet_rate_hz: *packet_rate_hz,
            }),
        },
        PcCommand::StartTransmitting => RouteAction::SendTo {
            role: Role::Tx,
            msg: EspnowMessage::TxCmd(TxCommand::StartTransmit),
        },
        PcCommand::StopTransmitting => RouteAction::SendTo {
            role: Role::Tx,
            msg: EspnowMessage::TxCmd(TxCommand::StopTransmit),
        },
        PcCommand::MoveTo { angle_deg } => RouteAction::SendTo {
            role: Role::Turntable,
            msg: EspnowMessage::TurntableCmd(TurntableCommand::MoveTo {
                angle_deg: *angle_deg,
            }),
        },
        PcCommand::StopStepper => RouteAction::SendTo {
            role: Role::Turntable,
            msg: EspnowMessage::TurntableCmd(TurntableCommand::Stop),
        },
        PcCommand::ReturnHome => RouteAction::SendTo {
            role: Role::Turntable,
            msg: EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 0.0 }),
        },
        PcCommand::StartMeasurement => RouteAction::SendTo {
            role: Role::Rx,
            msg: EspnowMessage::StartMeasurement,
        },
        PcCommand::ReportMeasurement => RouteAction::SendTo {
            role: Role::Rx,
            msg: EspnowMessage::ReportMeasurement,
        },
        PcCommand::QueryStatus => RouteAction::Local,

        // OTA firmware update — relay to the target device.
        PcCommand::OtaBegin {
            target,
            total_size,
            sha256,
        } => RouteAction::SendTo {
            role: *target,
            msg: EspnowMessage::OtaBegin {
                total_size: *total_size,
                sha256: *sha256,
            },
        },
        PcCommand::OtaData { target, seq, data } => RouteAction::SendTo {
            role: *target,
            msg: EspnowMessage::OtaData {
                seq: *seq,
                data: data.clone(),
            },
        },
        PcCommand::OtaFinish { target } => RouteAction::SendTo {
            role: *target,
            msg: EspnowMessage::OtaFinish,
        },
    }
}

/// Translate an ESP-NOW response message into a DeviceEvent for the PC.
///
/// Returns `None` for messages that are not responses (e.g. Hello, PairConfirm,
/// commands, beacons).
pub fn translate_response(msg: &EspnowMessage) -> Option<DeviceEvent> {
    match msg {
        EspnowMessage::TurntableResp(resp) => Some(match resp {
            TurntableResponse::MoveComplete { angle_deg } => DeviceEvent::MoveComplete {
                angle_deg: *angle_deg,
            },
            TurntableResponse::Error { description } => DeviceEvent::StepperError {
                description: description.clone(),
            },
        }),
        EspnowMessage::TxResp(resp) => Some(match resp {
            TxResponse::Ack => DeviceEvent::TxAck,
            TxResponse::Status { .. } => DeviceEvent::TxAck,
        }),
        EspnowMessage::MeasurementResult {
            rssi_dbm,
            sample_count,
        } => Some(DeviceEvent::Measurement {
            rssi_dbm: *rssi_dbm,
            sample_count: *sample_count,
        }),
        // OTA responses — relay back to PC.
        EspnowMessage::OtaReady => Some(DeviceEvent::OtaReady),
        EspnowMessage::OtaAck { seq } => Some(DeviceEvent::OtaAck { seq: *seq }),
        EspnowMessage::OtaComplete => Some(DeviceEvent::OtaComplete),
        EspnowMessage::OtaError { description } => Some(DeviceEvent::OtaError {
            description: description.clone(),
        }),

        _ => None,
    }
}
