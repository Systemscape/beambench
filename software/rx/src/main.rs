//! Beambench RX firmware — central coordinator for antenna pattern measurements.
//!
//! - Discovers TX and turntable peers via ESPNOW broadcast
//! - Receives RSSI measurements from TX packets
//! - Commands turntable to move via ESPNOW
//! - Bridges measurement data to PC over USB-serial (postcard + COBS)

#![no_std]
#![no_main]

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, rmt::Rmt, time::Rate,
    timer::timg::TimerGroup,
};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, Sk68xxTiming};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, PeerInfo, BROADCAST_ADDRESS,
};
use smart_leds::{SmartLedsWrite, RGB8};

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

const DEFAULT_CHANNEL: u8 = 11;
const BEACON_INTERVAL: Duration = Duration::from_secs(1);

// ── State ────────────────────────────────────────────────────────────────────

struct RxState {
    tx_paired: bool,
    tx_mac: [u8; 6],
    turntable_paired: bool,
    turntable_mac: [u8; 6],
}

impl RxState {
    const fn new() -> Self {
        Self {
            tx_paired: false,
            tx_mac: [0u8; 6],
            turntable_paired: false,
            turntable_mac: [0u8; 6],
        }
    }

    fn all_paired(&self) -> bool {
        self.tx_paired && self.turntable_paired
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

    // ── ESP-NOW setup ────────────────────────────────────────────────────────

    let esp_radio_ctrl = &*mk_static!(esp_radio::Controller<'static>, esp_radio::init().unwrap());

    let wifi = peripherals.WIFI;
    let (mut controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, wifi, Default::default()).unwrap();
    controller.set_mode(esp_radio::wifi::WifiMode::Sta).unwrap();
    controller.start().unwrap();

    let esp_now = interfaces.esp_now;
    esp_now.set_channel(11).unwrap();

    info!(
        "ESP-NOW v{} on channel {}",
        esp_now.version().unwrap(),
        DEFAULT_CHANNEL
    );

    // ── LED setup (SK6812 on GPIO2 via RMT) ─────────────────────────────────

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let mut led =
        RmtSmartLeds::<{ buffer_size::<RGB8>(1) }, _, RGB8, color_order::Grb, Sk68xxTiming>::new(
            rmt.channel0,
            peripherals.GPIO2,
        )
        .unwrap();

    // Blue = alive, booting.
    let _ = led.write(core::iter::once(RGB8 { r: 0, g: 0, b: 255 }));

    let (manager, sender, receiver) = esp_now.split();
    let manager = mk_static!(EspNowManager<'static>, manager);
    let sender = mk_static!(
        Mutex::<NoopRawMutex, EspNowSender<'static>>,
        Mutex::<NoopRawMutex, _>::new(sender)
    );
    let state = mk_static!(
        Mutex::<NoopRawMutex, RxState>,
        Mutex::<NoopRawMutex, _>::new(RxState::new())
    );
    let rssi_signal = mk_static!(Signal<NoopRawMutex, i32>, Signal::new());

    spawner.spawn(discovery_task(sender, state)).ok();
    spawner
        .spawn(listener_task(manager, sender, receiver, state, rssi_signal))
        .ok();

    info!("RX coordinator ready, discovering peers...");

    // Wait until both peers are discovered.
    loop {
        if state.lock().await.all_paired() {
            info!("All peers discovered, ready for measurements");
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    // Green = connected to both TX and turntable.
    let _ = led.write(core::iter::once(RGB8 { r: 0, g: 255, b: 0 }));

    // Main loop: await commands from PC over serial.
    // TODO: Implement USB-serial bridge with postcard+COBS.
    // For now, just log RSSI from received TX packets.
    loop {
        let rssi = rssi_signal.wait().await;
        info!("RSSI: {} dBm", rssi);
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// Broadcasts discovery beacons. Slows down after all peers are found but
/// keeps sending so late-booting peers can still discover us.
#[embassy_executor::task]
async fn discovery_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        if state.lock().await.all_paired() {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
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

/// Listens for ESPNOW messages: discovery, turntable responses, TX packets (RSSI).
#[embassy_executor::task]
async fn listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    rssi_signal: &'static Signal<NoopRawMutex, i32>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;
        let rssi = received.info.rx_control.rssi;

        // Try to parse as a protocol message.
        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::Hello(hello)) => {
                handle_hello(manager, sender, state, &src, &hello).await;
            }
            Ok(EspnowMessage::TurntableResp(resp)) => match resp {
                proto::TurntableResponse::MoveComplete { angle_deg } => {
                    info!("Turntable reached {}", angle_deg);
                }
                proto::TurntableResponse::Error { description } => {
                    info!("Turntable error: {}", description.as_str());
                }
            },
            _ => {
                // Unrecognized payload — likely a TX measurement packet.
                // Record the RSSI.
                let is_tx = {
                    let s = state.lock().await;
                    s.tx_paired && s.tx_mac == src
                };
                if is_tx {
                    rssi_signal.signal(rssi);
                }
            }
        }
    }
}

/// Handle a discovery beacon from TX or turntable.
async fn handle_hello(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    src: &[u8; 6],
    hello: &proto::HelloBeacon,
) {
    let already_paired = {
        let s = state.lock().await;
        match hello.role {
            Role::Tx => s.tx_paired,
            Role::Turntable => s.turntable_paired,
            Role::Rx => return, // Ignore other RX boards.
        }
    };

    if already_paired {
        return;
    }

    info!(
        "Discovered {:?} at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        hello.role, src[0], src[1], src[2], src[3], src[4], src[5]
    );

    // Register as unicast peer.
    if !manager.peer_exists(src) {
        manager
            .add_peer(PeerInfo {
                interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                peer_address: *src,
                lmk: None,
                channel: None,
                encrypt: false,
            })
            .unwrap();
    }

    // Update state.
    {
        let mut s = state.lock().await;
        match hello.role {
            Role::Tx => {
                s.tx_paired = true;
                s.tx_mac = *src;
            }
            Role::Turntable => {
                s.turntable_paired = true;
                s.turntable_mac = *src;
            }
            Role::Rx => {}
        }
    }

    // Send pair confirmation unicast to the discovered peer.
    let confirm = EspnowMessage::PairConfirm(proto::PairConfirm { role: Role::Rx, mac: [0; 6] });
    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(&confirm, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(src, data).await;
        info!("Sent PairConfirm to {:?}", hello.role);
    }
}
