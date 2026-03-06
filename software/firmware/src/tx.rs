//! TX role — ESPNOW transmitter for antenna pattern measurements.
//!
//! Broadcasts discovery beacons, waits for the Bridge to pair, then transmits
//! MeasurementBeacon packets at a configurable rate for RSSI measurement.

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

const DEFAULT_TX_INTERVAL: Duration = Duration::from_millis(100);

struct TxState {
    transmitting: bool,
    tx_interval: Duration,
    bridge_paired: bool,
    bridge_mac: [u8; 6],
    last_bridge_seen: Option<Instant>,
}

impl TxState {
    fn new() -> Self {
        Self {
            transmitting: false,
            tx_interval: DEFAULT_TX_INTERVAL,
            bridge_paired: false,
            bridge_mac: [0u8; 6],
            last_bridge_seen: None,
        }
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
        Mutex::<NoopRawMutex, TxState>,
        Mutex::<NoopRawMutex, _>::new(TxState::new())
    );

    spawner.spawn(tx_discovery_task(manager, sender, state, led_signal)).ok();
    spawner.spawn(tx_listener_task(manager, receiver, state, led_signal)).ok();
    spawner.spawn(tx_transmit_task(sender, state)).ok();

    info!("TX role ready, broadcasting discovery beacons");

    // Keep this task alive — LED loop runs in main.
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn tx_discovery_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
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
                        s.transmitting = false;
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
            role: Role::Tx,
            mac: [0; 6],
        });

        if let Ok(data) = proto::serialize(&beacon, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
        }
    }
}

#[embassy_executor::task]
async fn tx_listener_task(
    manager: &'static EspNowManager<'static>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, TxState>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;

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
            Ok(EspnowMessage::TxCmd(cmd)) => {
                state.lock().await.last_bridge_seen = Some(Instant::now());
                match cmd {
                    proto::TxCommand::Configure {
                        channel: _,
                        tx_power_dbm: _,
                        packet_rate_hz,
                    } => {
                        info!("Configured: rate={}Hz", packet_rate_hz);
                        let mut s = state.lock().await;
                        if packet_rate_hz > 0 {
                            s.tx_interval =
                                Duration::from_millis(1000 / packet_rate_hz as u64);
                        }
                    }
                    proto::TxCommand::StartTransmit => {
                        info!("Start transmitting");
                        state.lock().await.transmitting = true;
                        led_signal.signal(LedState::Blink {
                            color: COLOR_AMBER,
                            period_ms: 200,
                        });
                    }
                    proto::TxCommand::StopTransmit => {
                        info!("Stop transmitting");
                        state.lock().await.transmitting = false;
                        led_signal.signal(LedState::Solid(COLOR_GREEN));
                    }
                }
            }
            _ => {}
        }
    }
}

#[embassy_executor::task]
async fn tx_transmit_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    // MeasurementBeacon is broadcast so RX can measure the direct TX->RX RF path.
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let (transmitting, interval) = {
            let s = state.lock().await;
            (s.transmitting, s.tx_interval)
        };

        if transmitting {
            let beacon = EspnowMessage::MeasurementBeacon;
            if let Ok(data) = proto::serialize(&beacon, &mut buf) {
                let mut s = sender.lock().await;
                let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
            }
            Timer::after(interval).await;
        } else {
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}
