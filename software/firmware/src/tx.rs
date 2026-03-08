//! TX role — signal transmitter for antenna pattern measurements.
//!
//! Broadcasts discovery beacons, waits for the Bridge to pair, then controls
//! signal transmission via the active backend (ESP-NOW beacons or UART to
//! an external TX board).

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, BROADCAST_ADDRESS,
};

use beambench_protocol::{self as proto, EspnowMessage, Role};

use crate::common::*;
use crate::measurement::{ActiveTxBackend, TxBackend};
use crate::mk_static;

struct TxState {
    backend: ActiveTxBackend,
    bridge_paired: bool,
    bridge_mac: [u8; 6],
    last_bridge_seen: Option<Instant>,
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
        crate::measurement::espnow::EspNowTxBackend::new(),
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
        crate::measurement::uart::UartTxBackend::new(uart),
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
    backend: ActiveTxBackend,
) -> ! {
    let state = mk_static!(
        Mutex::<NoopRawMutex, TxState>,
        Mutex::<NoopRawMutex, _>::new(TxState {
            backend,
            bridge_paired: false,
            bridge_mac: [0u8; 6],
            last_bridge_seen: None,
        })
    );
    let ota = mk_static!(
        Mutex::<NoopRawMutex, crate::ota_responder::OtaState>,
        Mutex::new(crate::ota_responder::OtaState::new())
    );

    spawner.spawn(tx_discovery_task(manager, sender, state, led_signal)).ok();
    spawner.spawn(tx_listener_task(manager, sender, receiver, state, led_signal, flash, ota)).ok();

    // ESP-NOW backend: spawn the beacon broadcast loop.
    #[cfg(not(feature = "backend-uart"))]
    spawner.spawn(tx_transmit_task(sender, state)).ok();

    info!("TX role ready, broadcasting discovery beacons");

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
            if s.bridge_paired && is_heartbeat_stale(s.last_bridge_seen) {
                info!("Bridge heartbeat timeout, unpairing");
                let _ = manager.remove_peer(&s.bridge_mac);
                s.bridge_paired = false;
                s.backend.stop_transmit().await;
                s.last_bridge_seen = None;
            }
            signal_pairing_led(led_signal, s.bridge_paired);
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
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, TxState>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    flash: &'static SharedFlash,
    ota: &'static Mutex<NoopRawMutex, crate::ota_responder::OtaState>,
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

                ensure_peer(manager, &src);
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
                        // ESP-NOW backend: adjust beacon broadcast rate.
                        #[cfg(not(feature = "backend-uart"))]
                        state.lock().await.backend.set_packet_rate(packet_rate_hz);
                    }
                    proto::TxCommand::StartTransmit => {
                        info!("Start transmitting");
                        state.lock().await.backend.start_transmit().await;
                        led_signal.signal(LedState::Blink {
                            color: COLOR_AMBER,
                            period_ms: 200,
                        });
                    }
                    proto::TxCommand::StopTransmit => {
                        info!("Stop transmitting");
                        state.lock().await.backend.stop_transmit().await;
                        led_signal.signal(LedState::Solid(COLOR_GREEN));
                    }
                }
            }
            Ok(ref espnow_msg) if crate::ota_responder::is_ota_message(espnow_msg) => {
                let bridge_mac = state.lock().await.bridge_mac;
                crate::ota_responder::process_and_respond(espnow_msg, ota, flash, sender, &bridge_mac, led_signal).await;
            }
            _ => {}
        }
    }
}

/// ESP-NOW backend only: broadcasts `MeasurementBeacon` frames at the configured rate.
#[cfg(not(feature = "backend-uart"))]
#[embassy_executor::task]
async fn tx_transmit_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let (transmitting, interval) = {
            let s = state.lock().await;
            (s.backend.is_transmitting(), s.backend.tx_interval())
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
