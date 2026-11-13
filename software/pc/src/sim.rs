//! Simulated Bridge device — extracted for reuse in integration tests.
//!
//! Speaks postcard+COBS using PcCommand/DeviceEvent, generates synthetic
//! antenna patterns so you can test the full PC pipeline without hardware.
//!
//! Routing, response translation and the TX/RX state machines are the same
//! code the firmware runs (`bridge_logic`, `tx_logic`, `rx_logic`). Only the
//! radio link, the motor and the RSSI values are simulated.

use beambench_protocol::bridge_logic::{self, RouteAction};
use beambench_protocol::rx_logic::MeasurementState;
use beambench_protocol::tx_logic::TxState;
use beambench_protocol::{
    BackendInfo, DeviceEvent, EspnowMessage, PcCommand, Role, TurntableCommand, TurntableResponse,
    MAX_MSG_SIZE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Beacons the simulated RX receives per measurement window.
const BEACONS_PER_MEASUREMENT: u16 = 10;

/// Simulated field devices behind the bridge.
struct FieldDevices {
    tx: TxState,
    rx: MeasurementState,
    angle_deg: f32,
}

impl FieldDevices {
    fn new() -> Self {
        Self {
            tx: TxState::new(),
            rx: MeasurementState::new(),
            angle_deg: 0.0,
        }
    }

    /// Deliver a message to the device with the given role, as the bridge
    /// does over ESP-NOW. Returns the device's response, if any.
    async fn deliver(&mut self, role: Role, msg: &EspnowMessage) -> Option<EspnowMessage> {
        match role {
            Role::Tx => self.tx.handle_message(msg),
            Role::Rx => {
                let resp = self.rx.handle_message(msg, 0);
                // The RX only hears beacons while the TX is transmitting.
                if matches!(msg, EspnowMessage::StartMeasurement) && self.tx.transmitting {
                    let rssi = synthetic_rssi(self.angle_deg);
                    for _ in 0..BEACONS_PER_MEASUREMENT {
                        self.rx
                            .handle_message(&EspnowMessage::MeasurementBeacon, rssi);
                    }
                }
                resp
            }
            Role::Turntable => match msg {
                EspnowMessage::TurntableCmd(TurntableCommand::MoveTo { angle_deg }) => {
                    self.angle_deg = *angle_deg;
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    Some(EspnowMessage::TurntableResp(
                        TurntableResponse::MoveComplete {
                            angle_deg: *angle_deg,
                        },
                    ))
                }
                // Like the firmware: Stop halts the motor without a response.
                _ => None,
            },
            Role::Bridge => None,
        }
    }
}

/// Handle a single client connection: decode commands, respond with events.
pub async fn handle_connection(stream: tokio::net::TcpStream) {
    let (reader, writer) = tokio::io::split(stream);
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<PcCommand>(32);
    let (resp_tx, resp_rx) = mpsc::channel::<DeviceEvent>(64);

    tokio::spawn(cobs_reader(reader, cmd_tx));
    tokio::spawn(cobs_writer(writer, resp_rx));

    let mut devices = FieldDevices::new();

    while let Some(cmd) = cmd_rx.recv().await {
        let event = match bridge_logic::route_command(&cmd) {
            // OTA is not simulated: no device answers those messages.
            RouteAction::SendTo { role, msg } => devices
                .deliver(role, &msg)
                .await
                .and_then(|resp| bridge_logic::translate_response(&resp)),
            RouteAction::Local => Some(DeviceEvent::Status {
                tx_connected: true,
                rx_connected: true,
                turntable_connected: true,
                backend: BackendInfo {
                    name: heapless::String::try_from("sim").unwrap(),
                    frequency_mhz: Some(2400),
                },
                mismatched_peers: heapless::Vec::new(),
            }),
        };

        if let Some(event) = event {
            let _ = resp_tx.send(event).await;
        }
    }
}

/// Synthetic RSSI in whole dBm: cardioid-like pattern peaked at 45°.
fn synthetic_rssi(angle_deg: f32) -> i32 {
    let rad = (angle_deg - 45.0_f32).to_radians();
    (-30.0 + 20.0 * rad.cos()).round() as i32
}

/// Compute the expected synthetic RSSI for a given angle (matches synthetic_rssi).
pub fn expected_rssi(angle_deg: f32) -> f32 {
    synthetic_rssi(angle_deg) as f32
}

/// Decode COBS-framed postcard messages from a reader into a channel.
pub async fn cobs_reader(mut reader: impl AsyncReadExt + Unpin, tx: mpsc::Sender<PcCommand>) {
    let mut raw_buf = [0u8; 512];
    let mut accum = Vec::with_capacity(512);

    loop {
        match reader.read(&mut raw_buf).await {
            Ok(0) => return,
            Ok(n) => {
                accum.extend_from_slice(&raw_buf[..n]);
                while let Some(zero_pos) = accum.iter().position(|&b| b == 0) {
                    let frame = &accum[..zero_pos + 1];
                    if frame.len() > 1 {
                        match beambench_protocol::deserialize_cobs::<PcCommand>(&mut frame.to_vec())
                        {
                            Ok(msg) => {
                                if tx.send(msg).await.is_err() {
                                    return;
                                }
                            }
                            Err(_) => {}
                        }
                    }
                    accum = accum[zero_pos + 1..].to_vec();
                }
            }
            Err(_) => return,
        }
    }
}

/// Encode DeviceEvent messages from a channel as COBS frames into a writer.
pub async fn cobs_writer(
    mut writer: impl AsyncWriteExt + Unpin,
    mut rx: mpsc::Receiver<DeviceEvent>,
) {
    let mut buf = [0u8; MAX_MSG_SIZE];

    while let Some(msg) = rx.recv().await {
        match beambench_protocol::serialize_cobs(&msg, &mut buf) {
            Ok(len) => {
                if writer.write_all(&buf[..len]).await.is_err() {
                    return;
                }
            }
            Err(_) => {}
        }
    }
}

/// Start an in-process bridge-sim TCP server on a random port.
/// Returns the address it's listening on.
pub async fn start_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            if let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(handle_connection(stream));
            }
        }
    });

    addr
}
