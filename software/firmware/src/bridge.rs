//! Bridge role — USB-serial to ESP-NOW relay.
//!
//! Phase 2 stub: for now, just discovers peers and logs their connection.
//! The full implementation will relay COBS-framed PcCommand messages from
//! USB-serial to the appropriate ESP-NOW peer, and forward responses back.

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, PeerInfo, BROADCAST_ADDRESS,
};

use beambench_protocol::{self as proto, EspnowMessage, Role};

use crate::common::*;
use crate::mk_static;

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

    fn peer_for_role(&self, role: Role) -> Option<[u8; 6]> {
        match role {
            Role::Rx => self.rx.map(|(mac, _)| mac),
            Role::Tx => self.tx.map(|(mac, _)| mac),
            Role::Turntable => self.turntable.map(|(mac, _)| mac),
            Role::Bridge => None,
        }
    }

    fn set_peer(&mut self, role: Role, mac: [u8; 6]) {
        let entry = Some((mac, Instant::now()));
        match role {
            Role::Rx => self.rx = entry,
            Role::Tx => self.tx = entry,
            Role::Turntable => self.turntable = entry,
            Role::Bridge => {}
        }
    }

    fn update_seen(&mut self, role: Role) {
        let now = Instant::now();
        match role {
            Role::Rx => {
                if let Some((_, ref mut t)) = self.rx {
                    *t = now;
                }
            }
            Role::Tx => {
                if let Some((_, ref mut t)) = self.tx {
                    *t = now;
                }
            }
            Role::Turntable => {
                if let Some((_, ref mut t)) = self.turntable {
                    *t = now;
                }
            }
            Role::Bridge => {}
        }
    }

    fn all_connected(&self) -> bool {
        self.rx.is_some() && self.tx.is_some() && self.turntable.is_some()
    }
}

pub async fn run(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) -> ! {
    let peers = mk_static!(
        Mutex::<NoopRawMutex, PeerTable>,
        Mutex::<NoopRawMutex, _>::new(PeerTable::new())
    );

    spawner.spawn(bridge_discovery_task(manager, sender, peers, led_signal)).ok();
    spawner.spawn(bridge_listener_task(manager, sender, receiver, peers, led_signal)).ok();

    info!("Bridge role ready, discovering peers...");

    // TODO (Phase 2): spawn serial_rx_task and serial_tx_task for PC communication.

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

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
            let now = Instant::now();
            for role in [Role::Rx, Role::Tx, Role::Turntable] {
                let stale = match role {
                    Role::Rx => p.rx.as_ref().map_or(false, |(_, t)| now - *t > HEARTBEAT_TIMEOUT),
                    Role::Tx => p.tx.as_ref().map_or(false, |(_, t)| now - *t > HEARTBEAT_TIMEOUT),
                    Role::Turntable => p.turntable.as_ref().map_or(false, |(_, t)| now - *t > HEARTBEAT_TIMEOUT),
                    Role::Bridge => false,
                };
                if stale {
                    if let Some(mac) = p.peer_for_role(role) {
                        info!("Heartbeat timeout for {:?}, unpairing", defmt::Debug2Format(&role));
                        let _ = manager.remove_peer(&mac);
                    }
                    match role {
                        Role::Rx => p.rx = None,
                        Role::Tx => p.tx = None,
                        Role::Turntable => p.turntable = None,
                        Role::Bridge => {}
                    }
                }
            }

            if p.all_connected() {
                led_signal.signal(LedState::Solid(COLOR_GREEN));
            } else {
                led_signal.signal(LedState::Blink { color: COLOR_BLUE, period_ms: 500 });
            }
        }

        // Broadcast our own discovery beacon.
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

#[embassy_executor::task]
async fn bridge_listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    peers: &'static Mutex<NoopRawMutex, PeerTable>,
    _led_signal: &'static Signal<NoopRawMutex, LedState>,
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

                    if !manager.peer_exists(&src) {
                        manager
                            .add_peer(PeerInfo {
                                interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                                peer_address: src,
                                lmk: None,
                                channel: None,
                                encrypt: false,
                            })
                            .unwrap();
                    }

                    peers.lock().await.set_peer(role, src);
                } else {
                    peers.lock().await.update_seen(role);
                }

                // Always send PairConfirm so the peer knows we exist.
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
            Ok(EspnowMessage::TurntableResp(_)) => {
                peers.lock().await.update_seen(Role::Turntable);
                // TODO (Phase 2): forward to PC via serial.
                info!("Turntable response received (forwarding not yet implemented)");
            }
            Ok(EspnowMessage::TxResp(_)) => {
                peers.lock().await.update_seen(Role::Tx);
                // TODO (Phase 2): forward to PC via serial.
                info!("TX response received (forwarding not yet implemented)");
            }
            Ok(EspnowMessage::MeasurementResult { rssi_dbm, sample_count }) => {
                peers.lock().await.update_seen(Role::Rx);
                // TODO (Phase 2): forward to PC via serial.
                info!("Measurement: {} dBm ({} samples)", rssi_dbm, sample_count);
            }
            _ => {}
        }
    }
}
