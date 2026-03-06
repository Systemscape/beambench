//! RX role — RSSI measurement device.
//!
//! In the v2 architecture, RX is a passive measurement device:
//! - Listens for MeasurementBeacon broadcasts from TX
//! - Accumulates RSSI samples between StartMeasurement and ReportMeasurement
//! - Reports results to the Bridge on demand

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

struct RxState {
    bridge_paired: bool,
    bridge_mac: [u8; 6],
    last_bridge_seen: Option<Instant>,
    /// Whether we are actively collecting RSSI samples.
    measuring: bool,
    /// Accumulated RSSI sum (for averaging).
    rssi_sum: i64,
    /// Number of samples accumulated.
    rssi_count: u16,
}

impl RxState {
    fn new() -> Self {
        Self {
            bridge_paired: false,
            bridge_mac: [0u8; 6],
            last_bridge_seen: None,
            measuring: false,
            rssi_sum: 0,
            rssi_count: 0,
        }
    }

    fn reset_measurement(&mut self) {
        self.measuring = true;
        self.rssi_sum = 0;
        self.rssi_count = 0;
    }

    fn accumulate_rssi(&mut self, rssi: i32) {
        if self.measuring {
            self.rssi_sum += rssi as i64;
            self.rssi_count += 1;
        }
    }

    fn report_measurement(&mut self) -> (f32, u16) {
        let result = if self.rssi_count == 0 {
            (0.0, 0)
        } else {
            (self.rssi_sum as f32 / self.rssi_count as f32, self.rssi_count)
        };
        self.measuring = false;
        self.rssi_sum = 0;
        self.rssi_count = 0;
        result
    }
}

pub async fn run(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) -> ! {
    let state = mk_static!(
        Mutex::<NoopRawMutex, RxState>,
        Mutex::<NoopRawMutex, _>::new(RxState::new())
    );

    spawner.spawn(rx_discovery_task(sender, state, manager, led_signal)).ok();
    spawner.spawn(rx_listener_task(manager, sender, receiver, state, led_signal)).ok();

    info!("RX role ready, discovering Bridge...");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn rx_discovery_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    manager: &'static EspNowManager<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let paired = state.lock().await.bridge_paired;
        if paired {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
        }

        // Check for stale bridge and unpair.
        {
            let mut s = state.lock().await;
            if s.bridge_paired {
                if let Some(last) = s.last_bridge_seen {
                    if Instant::now() - last > HEARTBEAT_TIMEOUT {
                        info!("Bridge heartbeat timeout, unpairing");
                        let _ = manager.remove_peer(&s.bridge_mac);
                        s.bridge_paired = false;
                        s.last_bridge_seen = None;
                    }
                }
            }
            if s.bridge_paired {
                led_signal.signal(LedState::Solid(COLOR_GREEN));
            } else {
                led_signal.signal(LedState::Blink { color: COLOR_BLUE, period_ms: 500 });
            }
        }

        let beacon = EspnowMessage::Hello(proto::HelloBeacon {
            role: Role::Rx,
            mac: [0; 6],
        });

        if let Ok(data) = proto::serialize(&beacon, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
        }
    }
}

#[embassy_executor::task]
async fn rx_listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;
        let rssi = received.info.rx_control.rssi;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::PairConfirm(confirm)) if confirm.role == Role::Bridge => {
                info!(
                    "Paired with Bridge {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
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
                let mut s = state.lock().await;
                s.bridge_paired = true;
                s.bridge_mac = src;
                s.last_bridge_seen = Some(Instant::now());
                drop(s);
                led_signal.signal(LedState::Solid(COLOR_GREEN));
            }
            Ok(EspnowMessage::Hello(hello)) if hello.role == Role::Bridge => {
                // Bridge is broadcasting — treat as heartbeat if already paired.
                let mut s = state.lock().await;
                if !s.bridge_paired {
                    info!(
                        "Discovered Bridge at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
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
                    s.bridge_paired = true;
                    s.bridge_mac = src;
                }
                s.last_bridge_seen = Some(Instant::now());
            }
            Ok(EspnowMessage::StartMeasurement) => {
                let mut s = state.lock().await;
                s.last_bridge_seen = Some(Instant::now());
                s.reset_measurement();
                info!("Measurement started");
                led_signal.signal(LedState::Blink { color: COLOR_AMBER, period_ms: 200 });
            }
            Ok(EspnowMessage::ReportMeasurement) => {
                let mut s = state.lock().await;
                s.last_bridge_seen = Some(Instant::now());
                let (rssi_dbm, sample_count) = s.report_measurement();
                let bridge_mac = s.bridge_mac;
                let paired = s.bridge_paired;
                drop(s);

                info!("Measurement report: {} dBm ({} samples)", rssi_dbm, sample_count);
                led_signal.signal(LedState::Solid(COLOR_GREEN));

                if paired {
                    let resp = EspnowMessage::MeasurementResult { rssi_dbm, sample_count };
                    let mut buf = [0u8; proto::MAX_MSG_SIZE];
                    if let Ok(data) = proto::serialize(&resp, &mut buf) {
                        let mut s = sender.lock().await;
                        let _ = s.send_async(&bridge_mac, data).await;
                    }
                }
            }
            Ok(EspnowMessage::MeasurementBeacon) => {
                // Accumulate RSSI from TX broadcast packets.
                state.lock().await.accumulate_rssi(rssi);
            }
            _ => {}
        }
    }
}
