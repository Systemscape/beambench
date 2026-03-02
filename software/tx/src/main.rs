//! Beambench TX firmware — ESPNOW transmitter for antenna pattern measurements.
//!
//! Broadcasts discovery beacons, waits for RX to pair, then transmits
//! ESPNOW packets at a configurable rate for RSSI measurement.

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    rmt::Rmt,
    time::Rate,
    timer::timg::TimerGroup,
};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, Sk68xxTiming};
use smart_leds::{SmartLedsWrite, RGB8};
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

/// LED state for the led_task.
#[derive(Clone, Copy)]
enum LedState {
    Solid(RGB8),
    Blink { color: RGB8, period_ms: u64 },
}

const COLOR_BLUE: RGB8 = RGB8 { r: 0, g: 0, b: 255 };
const COLOR_GREEN: RGB8 = RGB8 { r: 0, g: 255, b: 0 };
const COLOR_AMBER: RGB8 = RGB8 { r: 255, g: 80, b: 0 };
const COLOR_OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };

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

    // ── ESP-NOW setup ────────────────────────────────────────────────────────

    let esp_radio_ctrl = &*mk_static!(esp_radio::Controller<'static>, esp_radio::init().unwrap());

    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    wifi_controller.set_mode(esp_radio::wifi::WifiMode::Sta).unwrap();
    wifi_controller.start().unwrap();

    let esp_now = interfaces.esp_now;
    esp_now.set_channel(DEFAULT_CHANNEL).unwrap();
    info!("ESP-NOW v{} on channel {}", esp_now.version().unwrap(), DEFAULT_CHANNEL);

    // ── LED setup (SK6812 on GPIO2 via RMT) ─────────────────────────────────

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let mut led = RmtSmartLeds::<
        { buffer_size::<RGB8>(1) },
        _,
        RGB8,
        color_order::Grb,
        Sk68xxTiming,
    >::new(rmt.channel0, peripherals.GPIO2)
    .unwrap();

    // Blue = alive, booting.
    let _ = led.write(core::iter::once(COLOR_BLUE));

    let led_signal = mk_static!(Signal<NoopRawMutex, LedState>, Signal::new());

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

    let paired_signal = mk_static!(Signal<NoopRawMutex, ()>, Signal::new());

    spawner.spawn(discovery_task(manager, sender, state)).ok();
    spawner.spawn(listener_task(manager, sender, receiver, state, paired_signal, led_signal)).ok();
    spawner.spawn(transmit_task(sender, state)).ok();

    info!("TX firmware ready, broadcasting discovery beacons");

    // Wait for RX pairing, then turn LED green.
    paired_signal.wait().await;
    info!("Paired with RX — LED green");

    // Main loop: drive LED state machine. Tasks signal state changes via led_signal.
    let mut led_state = LedState::Solid(COLOR_GREEN);
    loop {
        match led_state {
            LedState::Solid(color) => {
                let _ = led.write(core::iter::once(color));
                led_state = led_signal.wait().await;
            }
            LedState::Blink { color, period_ms } => {
                let half = Duration::from_millis(period_ms / 2);
                loop {
                    let _ = led.write(core::iter::once(color));
                    match select(led_signal.wait(), Timer::after(half)).await {
                        Either::First(new) => { led_state = new; break; }
                        Either::Second(_) => {}
                    }
                    let _ = led.write(core::iter::once(COLOR_OFF));
                    match select(led_signal.wait(), Timer::after(half)).await {
                        Either::First(new) => { led_state = new; break; }
                        Either::Second(_) => {}
                    }
                }
            }
        }
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// Broadcasts discovery beacons. Slows down after pairing but keeps sending
/// so that RX can discover us even if it boots later.
#[embassy_executor::task]
async fn discovery_task(
    _manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, TxState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let paired = state.lock().await.rx_paired;
        if paired {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
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
    paired_signal: &'static Signal<NoopRawMutex, ()>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
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
                    drop(s);
                    paired_signal.signal(());
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
                        led_signal.signal(LedState::Blink { color: COLOR_AMBER, period_ms: 200 });
                    }
                    proto::TxCommand::StopTransmit => {
                        info!("Stop transmitting");
                        state.lock().await.transmitting = false;
                        led_signal.signal(LedState::Solid(COLOR_GREEN));
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
