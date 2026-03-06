//! Bridge role — USB-serial to ESP-NOW relay.
//!
//! Receives COBS-framed `PcCommand` from PC over USB-serial, translates them
//! to `EspnowMessage` and routes to the appropriate field device.
//! Forwards ESP-NOW responses back as `DeviceEvent` over USB-serial.

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex,
    channel::{Channel, Sender},
    mutex::Mutex,
    signal::Signal,
};
use embassy_time::{Duration, Instant, Timer};
use embedded_io_async::{Read, Write};
use esp_hal::{
    usb_serial_jtag::{UsbSerialJtagRx, UsbSerialJtagTx},
    Async,
};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, BROADCAST_ADDRESS,
};

use beambench_protocol::{self as proto, DeviceEvent, EspnowMessage, PcCommand, Role};
use beambench_protocol::bridge_logic;

use crate::common::*;
use crate::mk_static;

const COBS_BUF_SIZE: usize = 512;

struct PeerTable {
    rx: Option<([u8; 6], Instant)>,
    tx: Option<([u8; 6], Instant)>,
    turntable: Option<([u8; 6], Instant)>,
}

impl PeerTable {
    const fn new() -> Self {
        Self {
            rx: None,
            tx: None,
            turntable: None,
        }
    }

    fn entry(&self, role: Role) -> &Option<([u8; 6], Instant)> {
        match role {
            Role::Rx => &self.rx,
            Role::Tx => &self.tx,
            Role::Turntable => &self.turntable,
            Role::Bridge => &None,
        }
    }

    fn entry_mut(&mut self, role: Role) -> &mut Option<([u8; 6], Instant)> {
        match role {
            Role::Rx => &mut self.rx,
            Role::Tx => &mut self.tx,
            Role::Turntable => &mut self.turntable,
            Role::Bridge => unreachable!(),
        }
    }

    fn peer_for_role(&self, role: Role) -> Option<[u8; 6]> {
        self.entry(role).map(|(mac, _)| mac)
    }

    fn set_peer(&mut self, role: Role, mac: [u8; 6]) {
        *self.entry_mut(role) = Some((mac, Instant::now()));
    }

    fn update_seen(&mut self, role: Role) {
        if let Some((_, t)) = self.entry_mut(role) {
            *t = Instant::now();
        }
    }

    fn role_for_mac(&self, mac: &[u8; 6]) -> Option<Role> {
        for role in [Role::Rx, Role::Tx, Role::Turntable] {
            if self.peer_for_role(role) == Some(*mac) {
                return Some(role);
            }
        }
        None
    }

    fn all_connected(&self) -> bool {
        self.rx.is_some() && self.tx.is_some() && self.turntable.is_some()
    }

    fn status(&self) -> DeviceEvent {
        DeviceEvent::Status {
            tx_connected: self.tx.is_some(),
            rx_connected: self.rx.is_some(),
            stepper_connected: self.turntable.is_some(),
        }
    }
}

pub async fn run(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    usb_rx: &'static mut UsbSerialJtagRx<'static, Async>,
    usb_tx: &'static mut UsbSerialJtagTx<'static, Async>,
    _flash: &'static crate::common::SharedFlash,
) -> ! {
    let peers = mk_static!(
        Mutex::<NoopRawMutex, PeerTable>,
        Mutex::<NoopRawMutex, _>::new(PeerTable::new())
    );

    // Channel for DeviceEvent responses to send over serial.
    let event_ch = mk_static!(Channel::<NoopRawMutex, DeviceEvent, 8>, Channel::new());

    spawner
        .spawn(bridge_discovery_task(manager, sender, peers, led_signal))
        .ok();
    spawner
        .spawn(bridge_listener_task(
            manager,
            sender,
            receiver,
            peers,
            event_ch.sender(),
        ))
        .ok();
    spawner
        .spawn(bridge_serial_rx_task(
            usb_rx,
            sender,
            peers,
            event_ch.sender(),
        ))
        .ok();
    spawner
        .spawn(bridge_serial_tx_task(usb_tx, event_ch.receiver()))
        .ok();

    info!("Bridge role ready, discovering peers...");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

// ── Serial tasks ────────────────────────────────────────────────────────────

/// Reads COBS-framed PcCommand from USB-Serial and routes to ESP-NOW peers.
#[embassy_executor::task]
async fn bridge_serial_rx_task(
    usb_rx: &'static mut UsbSerialJtagRx<'static, Async>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    event_tx: Sender<'static, NoopRawMutex, DeviceEvent, 8>,
) {
    let mut raw_buf = [0u8; 64];
    let mut accum: heapless::Vec<u8, COBS_BUF_SIZE> = heapless::Vec::new();

    loop {
        let n = match usb_rx.read(&mut raw_buf).await {
            Ok(n) if n > 0 => n,
            Ok(_) => continue,
            Err(_) => {
                Timer::after(Duration::from_millis(100)).await;
                continue;
            }
        };

        for &byte in &raw_buf[..n] {
            if byte == 0x00 {
                if !accum.is_empty() {
                    let mut frame_buf = [0u8; COBS_BUF_SIZE];
                    let len = accum.len();
                    frame_buf[..len].copy_from_slice(&accum);
                    frame_buf[len] = 0x00;
                    match postcard::from_bytes_cobs::<PcCommand>(&mut frame_buf[..len + 1]) {
                        Ok(cmd) => {
                            info!("PC cmd: {:?}", defmt::Debug2Format(&cmd));
                            handle_pc_command(cmd, sender, peers, &event_tx).await;
                        }
                        Err(_) => {
                            info!("Failed to decode PcCommand ({} bytes)", len);
                        }
                    }
                }
                accum.clear();
            } else if accum.push(byte).is_err() {
                info!("Serial RX buffer overflow, discarding");
                accum.clear();
            }
        }
    }
}

/// Translates a PcCommand into ESP-NOW messages and sends to the appropriate peer.
async fn handle_pc_command(
    cmd: PcCommand,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    event_tx: &Sender<'static, NoopRawMutex, DeviceEvent, 8>,
) {
    match bridge_logic::route_command(&cmd) {
        bridge_logic::RouteAction::SendTo { role, msg } => {
            send_to_role(sender, peers, role, &msg, event_tx).await;
        }
        bridge_logic::RouteAction::Local => {
            // QueryStatus — handled locally.
            let status = peers.lock().await.status();
            event_tx.send(status).await;
        }
    }
}

/// Send an EspnowMessage to a peer by role. Sends an error event if the peer is not connected.
async fn send_to_role(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    role: Role,
    msg: &EspnowMessage,
    event_tx: &Sender<'static, NoopRawMutex, DeviceEvent, 8>,
) {
    let mac = peers.lock().await.peer_for_role(role);
    let Some(mac) = mac else {
        let mut desc = heapless::String::new();
        let _ = desc.push_str("Device not connected: ");
        let _ = desc.push_str(match role {
            Role::Rx => "RX",
            Role::Tx => "TX",
            Role::Turntable => "Stepper",
            Role::Bridge => "Bridge",
        });
        event_tx.send(DeviceEvent::Error { description: desc }).await;
        return;
    };

    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(msg, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(&mac, data).await;
    }
}

/// Sends DeviceEvent responses as COBS-framed postcard over USB-Serial.
#[embassy_executor::task]
async fn bridge_serial_tx_task(
    usb_tx: &'static mut UsbSerialJtagTx<'static, Async>,
    event_rx: embassy_sync::channel::Receiver<'static, NoopRawMutex, DeviceEvent, 8>,
) {
    let mut buf = [0u8; COBS_BUF_SIZE];

    loop {
        let event = event_rx.receive().await;
        match proto::serialize_cobs(&event, &mut buf) {
            Ok(len) => {
                let _ = usb_tx.write_all(&buf[..len]).await;
                let _ = usb_tx.flush().await;
            }
            Err(_) => {
                info!("Failed to serialize DeviceEvent");
            }
        }
    }
}

// ── Discovery task ──────────────────────────────────────────────────────────

#[embassy_executor::task]
async fn bridge_discovery_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let all = peers.lock().await.all_connected();
        if all {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
        }

        // Check for stale peers.
        {
            let mut p = peers.lock().await;
            for role in [Role::Rx, Role::Tx, Role::Turntable] {
                let stale = p.entry(role).map_or(false, |(_, t)| is_heartbeat_stale(Some(t)));
                if stale {
                    if let Some(mac) = p.peer_for_role(role) {
                        info!(
                            "Heartbeat timeout for {:?}, unpairing",
                            defmt::Debug2Format(&role)
                        );
                        let _ = manager.remove_peer(&mac);
                    }
                    *p.entry_mut(role) = None;
                }
            }

            signal_pairing_led(led_signal, p.all_connected());
        }

        // Broadcast discovery beacon.
        let beacon = EspnowMessage::Hello(proto::HelloBeacon {
            role: Role::Bridge,
            mac: [0; 6],
        });
        if let Ok(data) = proto::serialize(&beacon, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
        }
    }
}

// ── ESP-NOW listener (forwards responses to serial) ─────────────────────────

#[embassy_executor::task]
async fn bridge_listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    event_tx: Sender<'static, NoopRawMutex, DeviceEvent, 8>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::Hello(hello)) if hello.role != Role::Bridge => {
                let role = hello.role;
                let already = peers.lock().await.peer_for_role(role).is_some();

                if !already {
                    info!(
                        "Discovered {:?} at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        defmt::Debug2Format(&role),
                        src[0], src[1], src[2], src[3], src[4], src[5]
                    );
                    ensure_peer(manager, &src);
                    peers.lock().await.set_peer(role, src);
                } else {
                    peers.lock().await.update_seen(role);
                }

                // Always send PairConfirm.
                let confirm = EspnowMessage::PairConfirm(proto::PairConfirm {
                    role: Role::Bridge,
                    mac: [0; 6],
                });
                let mut buf = [0u8; proto::MAX_MSG_SIZE];
                if let Ok(resp_data) = proto::serialize(&confirm, &mut buf) {
                    let mut s = sender.lock().await;
                    let _ = s.send_async(&src, resp_data).await;
                }
            }
            Ok(ref espnow_msg) => {
                // Identify the source role to update heartbeat.
                let role = match espnow_msg {
                    EspnowMessage::TurntableResp(_) => Some(Role::Turntable),
                    EspnowMessage::TxResp(_) => Some(Role::Tx),
                    EspnowMessage::MeasurementResult { .. } => Some(Role::Rx),
                    // OTA responses: look up role by source MAC.
                    EspnowMessage::OtaReady
                    | EspnowMessage::OtaAck { .. }
                    | EspnowMessage::OtaComplete
                    | EspnowMessage::OtaError { .. } => {
                        peers.lock().await.role_for_mac(&src)
                    }
                    _ => None,
                };
                if let Some(role) = role {
                    peers.lock().await.update_seen(role);
                }
                if let Some(event) = bridge_logic::translate_response(espnow_msg) {
                    event_tx.send(event).await;
                }
            }
            _ => {}
        }
    }
}
