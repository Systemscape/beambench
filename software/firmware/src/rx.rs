//! RX role — measurement device.
//!
//! Listens for commands from the Bridge over ESP-NOW and delegates
//! measurement to the active backend (ESP-NOW RSSI or UART external board).

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, BROADCAST_ADDRESS,
};

use beambench_protocol::{self as proto, EspnowMessage, Role};

use crate::common::*;
use crate::measurement::{ActiveRxBackend, RxBackend};
use crate::mk_static;

struct RxState {
    bridge_paired: bool,
    bridge_mac: [u8; 6],
    last_bridge_seen: Option<Instant>,
    measurement: ActiveRxBackend,
}

#[cfg(not(feature = "backend-uart"))]
pub async fn run(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    flash: &'static SharedFlash,
) -> ! {
    run_inner(
        spawner,
        manager,
        sender,
        receiver,
        led_signal,
        flash,
        crate::measurement::espnow::EspNowRxBackend::new(),
    )
    .await
}

#[cfg(feature = "backend-uart")]
pub async fn run(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    flash: &'static SharedFlash,
    uart: esp_hal::uart::Uart<'static, esp_hal::Async>,
) -> ! {
    run_inner(
        spawner,
        manager,
        sender,
        receiver,
        led_signal,
        flash,
        crate::measurement::uart::UartRxBackend::new(uart),
    )
    .await
}

async fn run_inner(
    spawner: Spawner,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    flash: &'static SharedFlash,
    backend: ActiveRxBackend,
) -> ! {
    let state = mk_static!(
        Mutex::<NoopRawMutex, RxState>,
        Mutex::<NoopRawMutex, _>::new(RxState {
            bridge_paired: false,
            bridge_mac: [0u8; 6],
            last_bridge_seen: None,
            measurement: backend,
        })
    );
    let ota = mk_static!(
        Mutex::<NoopRawMutex, crate::ota_responder::OtaState>,
        Mutex::new(crate::ota_responder::OtaState::new())
    );

    spawner.spawn(rx_discovery_task(sender, state, manager, led_signal)).ok();
    spawner.spawn(rx_listener_task(manager, sender, receiver, state, led_signal, flash, ota)).ok();

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
            if s.bridge_paired && is_heartbeat_stale(s.last_bridge_seen) {
                info!("Bridge heartbeat timeout, unpairing");
                let _ = manager.remove_peer(&s.bridge_mac);
                s.bridge_paired = false;
                s.last_bridge_seen = None;
            }
            signal_pairing_led(led_signal, s.bridge_paired);
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
    flash: &'static SharedFlash,
    ota: &'static Mutex<NoopRawMutex, crate::ota_responder::OtaState>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;
        #[cfg(not(feature = "backend-uart"))]
        let rssi = received.info.rx_control.rssi;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::PairConfirm(confirm)) if confirm.role == Role::Bridge => {
                info!(
                    "Paired with Bridge {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                    src[0], src[1], src[2], src[3], src[4], src[5]
                );
                ensure_peer(manager, &src);
                let mut s = state.lock().await;
                s.bridge_paired = true;
                s.bridge_mac = src;
                s.last_bridge_seen = Some(Instant::now());
                drop(s);
                led_signal.signal(LedState::Solid(COLOR_GREEN));
            }
            Ok(EspnowMessage::Hello(hello)) if hello.role == Role::Bridge => {
                let mut s = state.lock().await;
                if !s.bridge_paired {
                    info!(
                        "Discovered Bridge at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        src[0], src[1], src[2], src[3], src[4], src[5]
                    );
                    ensure_peer(manager, &src);
                    s.bridge_paired = true;
                    s.bridge_mac = src;
                }
                s.last_bridge_seen = Some(Instant::now());
            }
            Ok(EspnowMessage::StartMeasurement) => {
                let mut s = state.lock().await;
                s.last_bridge_seen = Some(Instant::now());
                s.measurement.start().await;
                info!("Measurement started");
                led_signal.signal(LedState::Blink { color: COLOR_AMBER, period_ms: 200 });
            }
            Ok(EspnowMessage::ReportMeasurement) => {
                let mut s = state.lock().await;
                s.last_bridge_seen = Some(Instant::now());
                let result = s.measurement.report().await;
                let bridge_mac = s.bridge_mac;
                let paired = s.bridge_paired;
                drop(s);

                info!("Measurement report: {} dBm ({} samples)", result.rssi_dbm, result.sample_count);
                led_signal.signal(LedState::Solid(COLOR_GREEN));

                if paired {
                    let resp = EspnowMessage::MeasurementResult {
                        rssi_dbm: result.rssi_dbm,
                        sample_count: result.sample_count,
                    };
                    let mut buf = [0u8; proto::MAX_MSG_SIZE];
                    if let Ok(data) = proto::serialize(&resp, &mut buf) {
                        let mut s = sender.lock().await;
                        let _ = s.send_async(&bridge_mac, data).await;
                    }
                }
            }
            // ESP-NOW backend only: accumulate RSSI from beacon frames.
            #[cfg(not(feature = "backend-uart"))]
            Ok(EspnowMessage::MeasurementBeacon) => {
                state.lock().await.measurement.accumulate_rssi(rssi);
            }
            Ok(ref espnow_msg) if crate::ota_responder::is_ota_message(espnow_msg) => {
                let bridge_mac = state.lock().await.bridge_mac;
                crate::ota_responder::process_and_respond(espnow_msg, ota, flash, sender, &bridge_mac, led_signal).await;
            }
            _ => {}
        }
    }
}
