//! Beambench TX firmware — ESPNOW transmitter for antenna pattern measurements.
//!
//! Broadcasts discovery beacons, waits for RX to pair, then transmits
//! ESPNOW packets at a configurable rate for RSSI measurement.

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex};
use embassy_time::{Duration, Ticker, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    timer::timg::TimerGroup,
};
use esp_radio::esp_now::{
    BROADCAST_ADDRESS, EspNowManager, EspNowReceiver, EspNowSender, PeerInfo,
};

use beambench_protocol::{self as proto, EspnowMessage, Role};

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {}
}

defmt::timestamp!("");
esp_bootloader_esp_idf::esp_app_desc!();

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

// ── Configuration ────────────────────────────────────────────────────────────

/// Default ESPNOW channel.
const DEFAULT_CHANNEL: u8 = 11;

/// Discovery beacon interval.
const BEACON_INTERVAL: Duration = Duration::from_secs(1);

/// Default packet transmission interval (10 Hz).
const DEFAULT_TX_INTERVAL: Duration = Duration::from_millis(100);

// ── State ────────────────────────────────────────────────────────────────────

/// Shared transmitter state.
struct TxState {
    transmitting: bool,
    tx_interval: Duration,
    rx_paired: bool,
    rx_mac: [u8; 6],
}

impl TxState {
    const fn new() -> Self {
        Self {
            transmitting: false,
            tx_interval: DEFAULT_TX_INTERVAL,
            rx_paired: false,
            rx_mac: [0u8; 6],
        }
    }
}

// ── Entry point ──────────────────────────────────────────────────────────────

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let controller = mk_static!(
        esp_radio::Controller<'static>,
        esp_radio::init().unwrap()
    );
    let (_wifi_controller, interfaces) =
        esp_radio::wifi::new(controller, peripherals.WIFI, Default::default()).unwrap();

    let esp_now = interfaces.esp_now;
    esp_now.set_channel(DEFAULT_CHANNEL).unwrap();
    info!("ESP-NOW v{} on channel {}", esp_now.version().unwrap(), DEFAULT_CHANNEL);

    let (manager, sender, receiver) = esp_now.split();
    let manager = mk_static!(EspNowManager<'static>, manager);
    let sender = mk_static!(
        Mutex::<NoopRawMutex, EspNowSender<'static>>,
        Mutex::<NoopRawMutex, _>::new(sender)
    );
    let state = mk_static!(
        Mutex::<NoopRawMutex, TxState>,
        Mutex::<NoopRawMutex, _>::new(TxState::new())
    );

    spawner.spawn(discovery_task(manager, sender, state)).ok();
    spawner.spawn(listener_task(manager, sender, receiver, state)).ok();
    spawner.spawn(transmit_task(sender, state)).ok();

    info!("TX firmware ready, broadcasting discovery beacons");

    // Main loop: just keep alive.
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// Broadcasts discovery beacons until paired with RX.
#[embassy_executor::task]
async fn discovery_task(
    _manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    let mut ticker = Ticker::every(BEACON_INTERVAL);
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        ticker.next().await;

        let paired = state.lock().await.rx_paired;
        if paired {
            // Already paired, stop beaconing.
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        let beacon = EspnowMessage::Hello(proto::HelloBeacon {
            role: Role::Tx,
            mac: [0; 6], // Filled by ESP-NOW hardware; receiver uses src_address.
        });

        if let Ok(data) = proto::serialize(&beacon, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
        }
    }
}

/// Listens for incoming ESPNOW messages (pairing, commands from RX).
#[embassy_executor::task]
async fn listener_task(
    manager: &'static EspNowManager<'static>,
    _sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::PairConfirm(confirm)) => {
                if confirm.role == Role::Rx {
                    info!("Paired with RX {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        src[0], src[1], src[2], src[3], src[4], src[5]);

                    // Register RX as unicast peer.
                    if !manager.peer_exists(&src) {
                        manager.add_peer(PeerInfo {
                            interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                            peer_address: src,
                            lmk: None,
                            channel: None,
                            encrypt: false,
                        }).unwrap();
                    }

                    let mut s = state.lock().await;
                    s.rx_paired = true;
                    s.rx_mac = src;
                }
            }
            Ok(EspnowMessage::TxCmd(cmd)) => {
                match cmd {
                    proto::TxCommand::Configure { channel: _, tx_power_dbm: _, packet_rate_hz } => {
                        info!("Configured: rate={}Hz", packet_rate_hz);
                        let mut s = state.lock().await;
                        if packet_rate_hz > 0 {
                            s.tx_interval = Duration::from_millis(1000 / packet_rate_hz as u64);
                        }
                    }
                    proto::TxCommand::StartTransmit => {
                        info!("Start transmitting");
                        state.lock().await.transmitting = true;
                    }
                    proto::TxCommand::StopTransmit => {
                        info!("Stop transmitting");
                        state.lock().await.transmitting = false;
                    }
                }
            }
            _ => {} // Ignore other messages.
        }
    }
}

/// Transmits ESPNOW packets at the configured rate when enabled.
#[embassy_executor::task]
async fn transmit_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    // Simple payload — RX only cares about RSSI, not content.
    let payload = b"beambench-tx";

    loop {
        let (transmitting, interval, rx_mac) = {
            let s = state.lock().await;
            (s.transmitting, s.tx_interval, s.rx_mac)
        };

        if transmitting {
            let mut s = sender.lock().await;
            let _ = s.send_async(&rx_mac, payload).await;
            Timer::after(interval).await;
        } else {
            // Not transmitting, poll slowly.
            Timer::after(Duration::from_millis(100)).await;
        }
    }
}
