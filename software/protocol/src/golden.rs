//! Golden wire bytes for every message variant.
//!
//! If a test here fails, the wire layout changed. Bump `PROTOCOL_VERSION` and
//! replace the table with the one printed by the failing test.
//! A new variant fails to compile in the `match` of its `samples` function.

extern crate std;

use std::{format, string::String, vec::Vec};

use serde::Serialize;

use crate::*;

/// Protocol version the tables below belong to.
const GOLDEN_VERSION: u16 = 1;

fn hex<T: Serialize>(msg: &T) -> String {
    let mut buf = [0u8; MAX_MSG_SIZE];
    let bytes = serialize(msg, &mut buf).unwrap();
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    parts.join(" ")
}

fn check<T: Serialize>(samples: &[(&str, T)], golden: &[(&str, &str)]) {
    let actual: Vec<(&str, String)> = samples.iter().map(|(n, m)| (*n, hex(m))).collect();
    let expected: Vec<(&str, String)> =
        golden.iter().map(|(n, h)| (*n, String::from(*h))).collect();
    if actual != expected {
        let table: Vec<String> = actual
            .iter()
            .map(|(n, h)| format!("    (\"{n}\", \"{h}\"),"))
            .collect();
        panic!(
            "Wire layout changed. Bump PROTOCOL_VERSION and GOLDEN_VERSION, then use:\n{}",
            table.join("\n")
        );
    }
}

fn s<const N: usize>(text: &str) -> heapless::String<N> {
    heapless::String::try_from(text).unwrap()
}

fn v<const N: usize>(bytes: &[u8]) -> heapless::Vec<u8, N> {
    heapless::Vec::from_slice(bytes).unwrap()
}

#[test]
fn golden_version_matches() {
    assert_eq!(
        PROTOCOL_VERSION, GOLDEN_VERSION,
        "Regenerate the golden tables for the new protocol version"
    );
}

// ── PcCommand ───────────────────────────────────────────────────────────────

fn pc_command_samples() -> Vec<(&'static str, PcCommand)> {
    let all = [
        PcCommand::ConfigureTx {
            channel: 6,
            tx_power_dbm: -3,
            packet_rate_hz: 100,
        },
        PcCommand::StartTransmitting,
        PcCommand::StopTransmitting,
        PcCommand::MoveTo { angle_deg: 90.0 },
        PcCommand::StopTurntable,
        PcCommand::ReturnHome,
        PcCommand::StartMeasurement,
        PcCommand::ReportMeasurement,
        PcCommand::QueryStatus,
        PcCommand::OtaBegin {
            target: Role::Tx,
            total_size: 1000,
            sha256: [0xAB; 32],
            ack_interval: 4,
        },
        PcCommand::OtaData {
            target: Role::Rx,
            seq: 300,
            data: v(&[1, 2]),
        },
        PcCommand::OtaFinish {
            target: Role::Turntable,
        },
    ];
    all.into_iter()
        .map(|m| {
            let name = match m {
                PcCommand::ConfigureTx { .. } => "ConfigureTx",
                PcCommand::StartTransmitting => "StartTransmitting",
                PcCommand::StopTransmitting => "StopTransmitting",
                PcCommand::MoveTo { .. } => "MoveTo",
                PcCommand::StopTurntable => "StopTurntable",
                PcCommand::ReturnHome => "ReturnHome",
                PcCommand::StartMeasurement => "StartMeasurement",
                PcCommand::ReportMeasurement => "ReportMeasurement",
                PcCommand::QueryStatus => "QueryStatus",
                PcCommand::OtaBegin { .. } => "OtaBegin",
                PcCommand::OtaData { .. } => "OtaData",
                PcCommand::OtaFinish { .. } => "OtaFinish",
            };
            (name, m)
        })
        .collect()
}

const PC_COMMAND_V1: &[(&str, &str)] = &[
    ("ConfigureTx", "01 00 06 fd 64"),
    ("StartTransmitting", "01 01"),
    ("StopTransmitting", "01 02"),
    ("MoveTo", "01 03 00 00 b4 42"),
    ("StopTurntable", "01 04"),
    ("ReturnHome", "01 05"),
    ("StartMeasurement", "01 06"),
    ("ReportMeasurement", "01 07"),
    ("QueryStatus", "01 08"),
    (
        "OtaBegin",
        "01 09 01 e8 07 ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab 04",
    ),
    ("OtaData", "01 0a 00 ac 02 02 01 02"),
    ("OtaFinish", "01 0b 02"),
];

#[test]
fn pc_command_wire_layout() {
    check(&pc_command_samples(), PC_COMMAND_V1);
}

// ── DeviceEvent ─────────────────────────────────────────────────────────────

fn device_event_samples() -> Vec<(&'static str, DeviceEvent)> {
    let all = [
        DeviceEvent::TxAck,
        DeviceEvent::MoveComplete { angle_deg: 90.0 },
        DeviceEvent::TurntableError {
            description: s("stall"),
        },
        DeviceEvent::HomeComplete,
        DeviceEvent::Measurement {
            rssi_dbm: -42.5,
            sample_count: 10,
        },
        DeviceEvent::Status {
            tx_connected: true,
            rx_connected: false,
            turntable_connected: true,
            backend: BackendInfo {
                name: s("espnow"),
                frequency_mhz: Some(2400),
            },
            mismatched_peers: heapless::Vec::from_slice(&[PeerMismatch {
                mac: [1, 2, 3, 4, 5, 6],
                protocol: 2,
            }])
            .unwrap(),
        },
        DeviceEvent::Error {
            description: s("oops"),
        },
        DeviceEvent::OtaReady,
        DeviceEvent::OtaAck { seq: 300 },
        DeviceEvent::OtaComplete,
        DeviceEvent::OtaError {
            description: s("hash"),
        },
    ];
    all.into_iter()
        .map(|m| {
            let name = match m {
                DeviceEvent::TxAck => "TxAck",
                DeviceEvent::MoveComplete { .. } => "MoveComplete",
                DeviceEvent::TurntableError { .. } => "TurntableError",
                DeviceEvent::HomeComplete => "HomeComplete",
                DeviceEvent::Measurement { .. } => "Measurement",
                DeviceEvent::Status { .. } => "Status",
                DeviceEvent::Error { .. } => "Error",
                DeviceEvent::OtaReady => "OtaReady",
                DeviceEvent::OtaAck { .. } => "OtaAck",
                DeviceEvent::OtaComplete => "OtaComplete",
                DeviceEvent::OtaError { .. } => "OtaError",
            };
            (name, m)
        })
        .collect()
}

const DEVICE_EVENT_V1: &[(&str, &str)] = &[
    ("TxAck", "01 00"),
    ("MoveComplete", "01 01 00 00 b4 42"),
    ("TurntableError", "01 02 05 73 74 61 6c 6c"),
    ("HomeComplete", "01 03"),
    ("Measurement", "01 04 00 00 2a c2 0a"),
    (
        "Status",
        "01 05 01 00 01 06 65 73 70 6e 6f 77 01 e0 12 01 01 02 03 04 05 06 02",
    ),
    ("Error", "01 06 04 6f 6f 70 73"),
    ("OtaReady", "01 07"),
    ("OtaAck", "01 08 ac 02"),
    ("OtaComplete", "01 09"),
    ("OtaError", "01 0a 04 68 61 73 68"),
];

#[test]
fn device_event_wire_layout() {
    check(&device_event_samples(), DEVICE_EVENT_V1);
}

// ── EspnowMessage ───────────────────────────────────────────────────────────

fn espnow_message_samples() -> Vec<(&'static str, EspnowMessage)> {
    let mac = [1, 2, 3, 4, 5, 6];
    let all = [
        EspnowMessage::Hello(HelloBeacon {
            role: Role::Rx,
            mac,
        }),
        EspnowMessage::PairConfirm(PairConfirm {
            role: Role::Bridge,
            mac,
        }),
        EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg: 90.0 }),
        EspnowMessage::TurntableCmd(TurntableCommand::Stop),
        EspnowMessage::TurntableResp(TurntableResponse::MoveComplete { angle_deg: 90.0 }),
        EspnowMessage::TurntableResp(TurntableResponse::Error {
            description: s("stall"),
        }),
        EspnowMessage::TxCmd(TxCommand::Configure {
            channel: 6,
            tx_power_dbm: -3,
            packet_rate_hz: 100,
        }),
        EspnowMessage::TxCmd(TxCommand::StartTransmit),
        EspnowMessage::TxCmd(TxCommand::StopTransmit),
        EspnowMessage::TxResp(TxResponse::Ack),
        EspnowMessage::TxResp(TxResponse::Status {
            transmitting: true,
            channel: 6,
            tx_power_dbm: -3,
            packet_rate_hz: 100,
        }),
        EspnowMessage::MeasurementBeacon,
        EspnowMessage::StartMeasurement,
        EspnowMessage::ReportMeasurement,
        EspnowMessage::MeasurementResult {
            rssi_dbm: -42.5,
            sample_count: 10,
        },
        EspnowMessage::OtaBegin {
            total_size: 1000,
            sha256: [0xAB; 32],
            ack_interval: 4,
        },
        EspnowMessage::OtaReady,
        EspnowMessage::OtaData {
            seq: 300,
            data: v(&[1, 2]),
        },
        EspnowMessage::OtaAck { seq: 300 },
        EspnowMessage::OtaFinish,
        EspnowMessage::OtaComplete,
        EspnowMessage::OtaError {
            description: s("hash"),
        },
    ];
    all.into_iter()
        .map(|m| {
            let name = match m {
                EspnowMessage::Hello(_) => "Hello",
                EspnowMessage::PairConfirm(_) => "PairConfirm",
                EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { .. }) => {
                    "TurntableCmd::MoveTo"
                }
                EspnowMessage::TurntableCmd(TurntableCommand::Stop) => "TurntableCmd::Stop",
                EspnowMessage::TurntableResp(TurntableResponse::MoveComplete { .. }) => {
                    "TurntableResp::MoveComplete"
                }
                EspnowMessage::TurntableResp(TurntableResponse::Error { .. }) => {
                    "TurntableResp::Error"
                }
                EspnowMessage::TxCmd(TxCommand::Configure { .. }) => "TxCmd::Configure",
                EspnowMessage::TxCmd(TxCommand::StartTransmit) => "TxCmd::StartTransmit",
                EspnowMessage::TxCmd(TxCommand::StopTransmit) => "TxCmd::StopTransmit",
                EspnowMessage::TxResp(TxResponse::Ack) => "TxResp::Ack",
                EspnowMessage::TxResp(TxResponse::Status { .. }) => "TxResp::Status",
                EspnowMessage::MeasurementBeacon => "MeasurementBeacon",
                EspnowMessage::StartMeasurement => "StartMeasurement",
                EspnowMessage::ReportMeasurement => "ReportMeasurement",
                EspnowMessage::MeasurementResult { .. } => "MeasurementResult",
                EspnowMessage::OtaBegin { .. } => "OtaBegin",
                EspnowMessage::OtaReady => "OtaReady",
                EspnowMessage::OtaData { .. } => "OtaData",
                EspnowMessage::OtaAck { .. } => "OtaAck",
                EspnowMessage::OtaFinish => "OtaFinish",
                EspnowMessage::OtaComplete => "OtaComplete",
                EspnowMessage::OtaError { .. } => "OtaError",
            };
            (name, m)
        })
        .collect()
}

const ESPNOW_MESSAGE_V1: &[(&str, &str)] = &[
    ("Hello", "01 00 00 01 02 03 04 05 06"),
    ("PairConfirm", "01 01 03 01 02 03 04 05 06"),
    ("TurntableCmd::MoveTo", "01 02 00 00 00 b4 42"),
    ("TurntableCmd::Stop", "01 02 01"),
    ("TurntableResp::MoveComplete", "01 03 00 00 00 b4 42"),
    ("TurntableResp::Error", "01 03 01 05 73 74 61 6c 6c"),
    ("TxCmd::Configure", "01 04 00 06 fd 64"),
    ("TxCmd::StartTransmit", "01 04 01"),
    ("TxCmd::StopTransmit", "01 04 02"),
    ("TxResp::Ack", "01 05 00"),
    ("TxResp::Status", "01 05 01 01 06 fd 64"),
    ("MeasurementBeacon", "01 06"),
    ("StartMeasurement", "01 07"),
    ("ReportMeasurement", "01 08"),
    ("MeasurementResult", "01 09 00 00 2a c2 0a"),
    (
        "OtaBegin",
        "01 0a e8 07 ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab ab 04",
    ),
    ("OtaReady", "01 0b"),
    ("OtaData", "01 0c ac 02 02 01 02"),
    ("OtaAck", "01 0d ac 02"),
    ("OtaFinish", "01 0e"),
    ("OtaComplete", "01 0f"),
    ("OtaError", "01 10 04 68 61 73 68"),
];

#[test]
fn espnow_message_wire_layout() {
    check(&espnow_message_samples(), ESPNOW_MESSAGE_V1);
}
