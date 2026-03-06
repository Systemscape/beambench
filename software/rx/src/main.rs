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
use embassy_futures::select::{select, Either};
use embassy_sync::{
    blocking_mutex::raw::NoopRawMutex,
    channel::{Channel, Receiver, Sender},
    mutex::Mutex,
    signal::Signal,
};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use embedded_io_async::{Read, Write};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    interrupt::software::SoftwareInterruptControl,
    rmt::Rmt,
    time::Rate,
    timer::timg::TimerGroup,
    usb_serial_jtag::{UsbSerialJtag, UsbSerialJtagRx, UsbSerialJtagTx},
    Async,
};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, Sk68xxTiming};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, PeerInfo, BROADCAST_ADDRESS,
};
use smart_leds::{SmartLedsWrite, RGB8};

use beambench_protocol::{self as proto, EspnowMessage, PcToRx, Role, RxToPc};

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
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

// ── State ────────────────────────────────────────────────────────────────────

struct RxState {
    tx_paired: bool,
    tx_mac: [u8; 6],
    turntable_paired: bool,
    turntable_mac: [u8; 6],
    sweeping: bool,
    last_tx_seen: Option<Instant>,
    last_turntable_seen: Option<Instant>,
}

impl RxState {
    fn new() -> Self {
        Self {
            tx_paired: false,
            tx_mac: [0u8; 6],
            turntable_paired: false,
            turntable_mac: [0u8; 6],
            sweeping: false,
            last_tx_seen: None,
            last_turntable_seen: None,
        }
    }

    fn all_paired(&self) -> bool {
        self.tx_paired && self.turntable_paired
    }
}

/// LED state for the led_task.
#[derive(Clone, Copy)]
enum LedState {
    Solid(RGB8),
    Blink { color: RGB8, period_ms: u64 },
}

const COLOR_BLUE: RGB8 = RGB8 { r: 0, g: 0, b: 20 };
const COLOR_GREEN: RGB8 = RGB8 { r: 0, g: 20, b: 0 };
const COLOR_AMBER: RGB8 = RGB8 { r: 20, g: 6, b: 0 };
const COLOR_OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };

/// Sweep parameters sent from serial_rx_task to sweep_task.
struct SweepCmd {
    start_deg: f32,
    stop_deg: f32,
    step_deg: f32,
    samples_per_angle: u16,
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
    let _ = led.write(core::iter::once(COLOR_BLUE));

    let led_signal = mk_static!(Signal<NoopRawMutex, LedState>, Signal::new());

    // ── USB-Serial-JTAG setup ────────────────────────────────────────────────

    let usb_serial = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();
    let (usb_rx, usb_tx) = usb_serial.split();
    let usb_rx = mk_static!(UsbSerialJtagRx<'static, Async>, usb_rx);
    let usb_tx = mk_static!(
        Mutex::<NoopRawMutex, UsbSerialJtagTx<'static, Async>>,
        Mutex::new(usb_tx)
    );

    // Channel for RxToPc responses (serial_rx_task and main loop produce, serial_tx_task consumes).
    let serial_resp_ch = mk_static!(Channel::<NoopRawMutex, RxToPc, 8>, Channel::new());

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
    let rssi_channel = mk_static!(Channel::<NoopRawMutex, i32, 64>, Channel::new());
    let sweep_cmd_ch = mk_static!(Channel::<NoopRawMutex, SweepCmd, 1>, Channel::new());
    let turntable_resp_signal = mk_static!(
        Signal<NoopRawMutex, proto::TurntableResponse>,
        Signal::new()
    );
    let stop_signal = mk_static!(Signal<NoopRawMutex, ()>, Signal::new());

    spawner
        .spawn(discovery_task(sender, state, manager, led_signal))
        .ok();
    spawner
        .spawn(listener_task(
            manager,
            sender,
            receiver,
            state,
            rssi_signal,
            rssi_channel.sender(),
            led_signal,
            turntable_resp_signal,
        ))
        .ok();
    spawner
        .spawn(serial_rx_task(
            usb_rx,
            state,
            serial_resp_ch.sender(),
            sweep_cmd_ch.sender(),
            stop_signal,
            sender,
            turntable_resp_signal,
        ))
        .ok();
    spawner
        .spawn(serial_tx_task(usb_tx, serial_resp_ch.receiver()))
        .ok();
    spawner
        .spawn(sweep_task(
            sender,
            state,
            turntable_resp_signal,
            sweep_cmd_ch.receiver(),
            stop_signal,
            serial_resp_ch.sender(),
            led_signal,
            rssi_channel.receiver(),
        ))
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

    // Main loop: drive LED state machine + log RSSI.
    let mut led_state = LedState::Solid(COLOR_GREEN);
    loop {
        match led_state {
            LedState::Solid(color) => {
                let _ = led.write(core::iter::once(color));
                // Wait for either LED state change or RSSI reading.
                match select(led_signal.wait(), rssi_signal.wait()).await {
                    Either::First(new) => {
                        led_state = new;
                    }
                    Either::Second(rssi) => {
                        info!("RSSI: {} dBm", rssi);
                    }
                }
            }
            LedState::Blink { color, period_ms } => {
                let half = Duration::from_millis(period_ms / 2);
                loop {
                    let _ = led.write(core::iter::once(color));
                    match with_timeout(half, led_signal.wait()).await {
                        Ok(new) => {
                            led_state = new;
                            break;
                        }
                        Err(_) => {}
                    }
                    let _ = led.write(core::iter::once(COLOR_OFF));
                    match with_timeout(half, led_signal.wait()).await {
                        Ok(new) => {
                            led_state = new;
                            break;
                        }
                        Err(_) => {}
                    }
                }
            }
        }
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// Broadcasts discovery beacons. Slows down after all peers are found but
/// keeps sending so late-booting peers can still discover us.
#[embassy_executor::task]
async fn discovery_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    manager: &'static EspNowManager<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        if state.lock().await.all_paired() {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
        }

        // Check for stale peers and unpair them.
        {
            let mut s = state.lock().await;
            let now = Instant::now();
            if s.tx_paired {
                if let Some(last) = s.last_tx_seen {
                    if now - last > HEARTBEAT_TIMEOUT {
                        info!("TX heartbeat timeout, unpairing");
                        let _ = manager.remove_peer(&s.tx_mac);
                        s.tx_paired = false;
                        s.last_tx_seen = None;
                    }
                }
            }
            if s.turntable_paired {
                if let Some(last) = s.last_turntable_seen {
                    if now - last > HEARTBEAT_TIMEOUT {
                        info!("Turntable heartbeat timeout, unpairing");
                        let _ = manager.remove_peer(&s.turntable_mac);
                        s.turntable_paired = false;
                        s.last_turntable_seen = None;
                    }
                }
            }
            if !s.all_paired() {
                led_signal.signal(LedState::Blink {
                    color: COLOR_BLUE,
                    period_ms: 500,
                });
            } else {
                led_signal.signal(LedState::Solid(COLOR_GREEN));
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

/// Listens for ESPNOW messages: discovery, turntable responses, TX packets (RSSI).
#[embassy_executor::task]
async fn listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    rssi_signal: &'static Signal<NoopRawMutex, i32>,
    rssi_ch_tx: Sender<'static, NoopRawMutex, i32, 64>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
) {
    let mut activity_active = false;
    let mut last_rssi_tick: u64 = 0;

    loop {
        // If activity LED is on, check for timeout (2 seconds without RSSI).
        let received = if activity_active {
            match with_timeout(Duration::from_secs(2), receiver.receive_async()).await {
                Ok(r) => r,
                Err(_) => {
                    // No RSSI for 2s — go back to green.
                    activity_active = false;
                    led_signal.signal(LedState::Solid(COLOR_GREEN));
                    continue;
                }
            }
        } else {
            receiver.receive_async().await
        };

        let data = received.data();
        let src = received.info.src_address;
        let rssi = received.info.rx_control.rssi;

        // Try to parse as a protocol message.
        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::Hello(hello)) => {
                handle_hello(manager, sender, state, &src, &hello).await;
            }
            Ok(EspnowMessage::TurntableResp(resp)) => {
                state.lock().await.last_turntable_seen = Some(Instant::now());
                match &resp {
                    proto::TurntableResponse::MoveComplete { angle_deg } => {
                        info!("Turntable reached {}", angle_deg);
                    }
                    proto::TurntableResponse::Error { description } => {
                        info!("Turntable error: {}", description.as_str());
                    }
                }
                turntable_resp_signal.signal(resp);
            }
            _ => {
                // Unrecognized payload — likely a TX measurement packet.
                // Record the RSSI.
                let is_tx = {
                    let mut s = state.lock().await;
                    let matched = s.tx_paired && s.tx_mac == src;
                    if matched {
                        s.last_tx_seen = Some(Instant::now());
                    }
                    matched
                };
                if is_tx {
                    rssi_signal.signal(rssi);
                    let _ = rssi_ch_tx.try_send(rssi);

                    // Start blinking on first RSSI packet.
                    let now = embassy_time::Instant::now().as_millis();
                    if !activity_active || now - last_rssi_tick > 1000 {
                        led_signal.signal(LedState::Blink {
                            color: COLOR_AMBER,
                            period_ms: 200,
                        });
                        activity_active = true;
                    }
                    last_rssi_tick = now;
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
        let mut s = state.lock().await;
        match hello.role {
            Role::Tx => {
                if s.tx_paired {
                    s.last_tx_seen = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
            Role::Turntable => {
                if s.turntable_paired {
                    s.last_turntable_seen = Some(Instant::now());
                    true
                } else {
                    false
                }
            }
            Role::Rx => return, // Ignore other RX boards.
        }
    };

    if !already_paired {
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
                    s.last_tx_seen = Some(Instant::now());
                }
                Role::Turntable => {
                    s.turntable_paired = true;
                    s.turntable_mac = *src;
                    s.last_turntable_seen = Some(Instant::now());
                }
                Role::Rx => {}
            }
        }
    }

    // Always send PairConfirm — the peer may not have received a previous one
    // (send failures are silently ignored, and the TX can only pair via
    // PairConfirm, unlike the turntable which also pairs on Hello(Rx) broadcasts).
    let confirm = EspnowMessage::PairConfirm(proto::PairConfirm {
        role: Role::Rx,
        mac: [0; 6],
    });
    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(&confirm, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(src, data).await;
        if !already_paired {
            info!("Sent PairConfirm to {:?}", hello.role);
        }
    }
}

// ── USB-Serial Tasks ────────────────────────────────────────────────────────

const COBS_BUF_SIZE: usize = 512;

/// Reads COBS-framed PcToRx commands from USB-Serial-JTAG and dispatches them.
#[embassy_executor::task]
async fn serial_rx_task(
    usb_rx: &'static mut UsbSerialJtagRx<'static, Async>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    resp_tx: embassy_sync::channel::Sender<'static, NoopRawMutex, RxToPc, 8>,
    sweep_cmd_tx: embassy_sync::channel::Sender<'static, NoopRawMutex, SweepCmd, 1>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
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
                // End of COBS frame.
                if accum.len() > 0 {
                    let mut frame_buf = [0u8; COBS_BUF_SIZE];
                    let len = accum.len();
                    frame_buf[..len].copy_from_slice(&accum);
                    // postcard::from_bytes_cobs needs a mutable slice with the trailing 0x00
                    frame_buf[len] = 0x00;
                    match postcard::from_bytes_cobs::<PcToRx>(&mut frame_buf[..len + 1]) {
                        Ok(cmd) => {
                            info!("Serial cmd: {:?}", defmt::Debug2Format(&cmd));
                            handle_serial_cmd(
                                cmd,
                                state,
                                &resp_tx,
                                &sweep_cmd_tx,
                                stop_signal,
                                sender,
                                turntable_resp_signal,
                            )
                            .await;
                        }
                        Err(_) => {
                            info!("Failed to decode serial COBS frame ({} bytes)", len);
                        }
                    }
                }
                accum.clear();
            } else if accum.push(byte).is_err() {
                // Overflow — discard frame.
                info!("Serial RX buffer overflow, discarding");
                accum.clear();
            }
        }
    }
}

/// Handles a decoded PcToRx command.
async fn handle_serial_cmd(
    cmd: PcToRx,
    state: &'static Mutex<NoopRawMutex, RxState>,
    resp_tx: &embassy_sync::channel::Sender<'static, NoopRawMutex, RxToPc, 8>,
    sweep_cmd_tx: &embassy_sync::channel::Sender<'static, NoopRawMutex, SweepCmd, 1>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
) {
    match cmd {
        PcToRx::QueryStatus => {
            let s = state.lock().await;
            resp_tx
                .send(RxToPc::Status {
                    sweeping: s.sweeping,
                    tx_connected: s.tx_paired,
                    turntable_connected: s.turntable_paired,
                })
                .await;
        }
        PcToRx::StartSweep {
            start_deg,
            stop_deg,
            step_deg,
            samples_per_angle,
        } => {
            // Reject if already sweeping.
            if state.lock().await.sweeping {
                let mut desc = heapless::String::new();
                let _ = desc.push_str("Sweep already in progress");
                resp_tx.send(RxToPc::Error { description: desc }).await;
                return;
            }
            // Basic validation.
            if step_deg <= 0.0 {
                let mut desc = heapless::String::new();
                let _ = desc.push_str("step_deg must be positive");
                resp_tx.send(RxToPc::Error { description: desc }).await;
                return;
            }
            // Send sweep command to sweep_task (non-blocking try_send would lose error;
            // channel size 1 means this blocks only if sweep_task hasn't consumed the last cmd).
            sweep_cmd_tx
                .send(SweepCmd {
                    start_deg,
                    stop_deg,
                    step_deg,
                    samples_per_angle,
                })
                .await;
        }
        PcToRx::Stop => {
            info!("Stop command received");
            stop_signal.signal(());
            // Also send immediate Stop to turntable for fast halt.
            send_turntable_cmd(sender, state, proto::TurntableCommand::Stop).await;
        }
        PcToRx::ReturnHome => {
            info!("ReturnHome: moving to 0°");
            // Check turntable is paired.
            if !state.lock().await.turntable_paired {
                let mut desc = heapless::String::new();
                let _ = desc.push_str("Turntable not connected");
                resp_tx.send(RxToPc::Error { description: desc }).await;
                return;
            }
            // Drain stale turntable signals so we don't pick up old MoveComplete.
            turntable_resp_signal.reset();
            send_turntable_cmd(
                sender,
                state,
                proto::TurntableCommand::MoveTo { angle_deg: 0.0 },
            )
            .await;
            // Wait for turntable response with 30 s timeout.
            match with_timeout(Duration::from_secs(30), turntable_resp_signal.wait()).await {
                Ok(proto::TurntableResponse::MoveComplete { .. }) => {
                    info!("ReturnHome: turntable reached 0°");
                    resp_tx.send(RxToPc::HomeComplete).await;
                }
                Ok(proto::TurntableResponse::Error { description }) => {
                    info!("ReturnHome error: {}", description.as_str());
                    let mut desc = heapless::String::<128>::new();
                    let _ = desc.push_str("Homing error: ");
                    let _ = desc.push_str(description.as_str());
                    resp_tx.send(RxToPc::Error { description: desc }).await;
                }
                Err(_) => {
                    info!("ReturnHome timed out");
                    let mut desc = heapless::String::new();
                    let _ = desc.push_str("ReturnHome timeout (30s)");
                    resp_tx.send(RxToPc::Error { description: desc }).await;
                }
            }
        }
        PcToRx::ConfigureTx {
            channel,
            tx_power_dbm,
            packet_rate_hz,
        } => {
            send_tx_cmd(
                sender,
                state,
                proto::TxCommand::Configure {
                    channel,
                    tx_power_dbm,
                    packet_rate_hz,
                },
            )
            .await;
        }
    }
}

// ── ESPNOW command helpers ──────────────────────────────────────────────────

/// Send a TurntableCommand to the paired turntable via ESPNOW.
async fn send_turntable_cmd(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    cmd: proto::TurntableCommand,
) {
    let mac = {
        let s = state.lock().await;
        if !s.turntable_paired {
            info!("Cannot send turntable cmd: not paired");
            return;
        }
        s.turntable_mac
    };
    let msg = EspnowMessage::TurntableCmd(cmd);
    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(&msg, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(&mac, data).await;
    }
}

/// Send a TxCommand to the paired TX board via ESPNOW.
async fn send_tx_cmd(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    cmd: proto::TxCommand,
) {
    let mac = {
        let s = state.lock().await;
        if !s.tx_paired {
            info!("Cannot send TX cmd: not paired");
            return;
        }
        s.tx_mac
    };
    let msg = EspnowMessage::TxCmd(cmd);
    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(&msg, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(&mac, data).await;
    }
}

// ── RSSI collection ─────────────────────────────────────────────────────────

const RSSI_COLLECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Collect RSSI samples from the channel and return the average.
///
/// This function is intentionally decoupled from the RSSI source — any task
/// that feeds `i32` values into the channel works (ESPNOW packet RSSI today,
/// spectrum-analyzer IC tomorrow).
async fn collect_rssi(
    rssi_rx: &Receiver<'static, NoopRawMutex, i32, 64>,
    samples_requested: u16,
    timeout: Duration,
) -> (f32, u16) {
    // Drain stale samples that arrived before this measurement window.
    while rssi_rx.try_receive().is_ok() {}

    let mut sum: i64 = 0;
    let mut count: u16 = 0;

    for _ in 0..samples_requested {
        match with_timeout(timeout, rssi_rx.receive()).await {
            Ok(rssi) => {
                sum += rssi as i64;
                count += 1;
            }
            Err(_) => break, // Timeout waiting for next sample.
        }
    }

    if count == 0 {
        (0.0, 0)
    } else {
        (sum as f32 / count as f32, count)
    }
}

// ── Sweep task ──────────────────────────────────────────────────────────────

/// Pre-spawned sweep coordinator. Waits for SweepCmd on the channel,
/// then executes the sweep and sends results back via serial_resp_ch.
#[embassy_executor::task]
async fn sweep_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
    sweep_cmd_rx: Receiver<'static, NoopRawMutex, SweepCmd, 1>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    resp_tx: Sender<'static, NoopRawMutex, RxToPc, 8>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    rssi_rx: Receiver<'static, NoopRawMutex, i32, 64>,
) {
    loop {
        let cmd = sweep_cmd_rx.receive().await;

        // Mark sweeping.
        state.lock().await.sweeping = true;
        // Drain any stale signals.
        turntable_resp_signal.reset();
        stop_signal.reset();

        info!(
            "Sweep start: {}° to {}° step {}°",
            cmd.start_deg, cmd.stop_deg, cmd.step_deg
        );
        led_signal.signal(LedState::Blink {
            color: COLOR_AMBER,
            period_ms: 500,
        });

        // Tell TX board to start transmitting so we can measure RSSI.
        send_tx_cmd(sender, state, proto::TxCommand::StartTransmit).await;

        let result = run_sweep_inner(
            &cmd,
            sender,
            state,
            turntable_resp_signal,
            stop_signal,
            &resp_tx,
            &rssi_rx,
        )
        .await;

        // Always stop TX — cleanup on success, error, and abort paths.
        send_tx_cmd(sender, state, proto::TxCommand::StopTransmit).await;

        match result {
            Ok(()) => {
                info!("Sweep complete");
                resp_tx.send(RxToPc::SweepComplete).await;
            }
            Err(desc) => {
                info!("Sweep failed: {}", desc.as_str());
                resp_tx.send(RxToPc::Error { description: desc }).await;
            }
        }

        state.lock().await.sweeping = false;
        led_signal.signal(LedState::Solid(COLOR_GREEN));
    }
}

/// Execute a sweep: step turntable through all angles, collect RSSI at each, send DataPoints.
async fn run_sweep_inner(
    cmd: &SweepCmd,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    resp_tx: &Sender<'static, NoopRawMutex, RxToPc, 8>,
    rssi_rx: &Receiver<'static, NoopRawMutex, i32, 64>,
) -> Result<(), heapless::String<128>> {
    // Check turntable is paired.
    if !state.lock().await.turntable_paired {
        let mut desc = heapless::String::new();
        let _ = desc.push_str("Turntable not connected");
        return Err(desc);
    }

    // Use integer step counting to avoid floating-point accumulation errors.
    // ceil() ensures we always have enough steps to reach stop_deg.
    let range = cmd.stop_deg - cmd.start_deg;
    let going_forward = range >= 0.0;
    let abs_range = if going_forward { range } else { -range };
    let sign: f32 = if going_forward { 1.0 } else { -1.0 };

    // Manual ceil to avoid libm dependency: floor + 1 if fractional part > tiny threshold.
    let n = abs_range / cmd.step_deg;
    let floor_n = n as u32;
    let num_steps = if n - floor_n as f32 > 0.001 {
        floor_n + 1
    } else {
        floor_n
    };

    for i in 0..=num_steps {
        // Check for stop signal.
        if stop_signal.signaled() {
            let mut desc = heapless::String::new();
            let _ = desc.push_str("Sweep aborted");
            return Err(desc);
        }

        // Compute angle from step index (avoids accumulation drift).
        // Clamp to stop_deg so the final point lands exactly on the endpoint.
        let raw_angle = cmd.start_deg + i as f32 * cmd.step_deg * sign;
        let angle = if going_forward {
            if raw_angle > cmd.stop_deg {
                cmd.stop_deg
            } else {
                raw_angle
            }
        } else {
            if raw_angle < cmd.stop_deg {
                cmd.stop_deg
            } else {
                raw_angle
            }
        };

        // Send MoveTo command.
        info!(
            "Sweep: moving to {}° (step {}/{})",
            angle,
            i + 1,
            num_steps + 1
        );
        turntable_resp_signal.reset();
        send_turntable_cmd(
            sender,
            state,
            proto::TurntableCommand::MoveTo { angle_deg: angle },
        )
        .await;

        // Wait for turntable response with 30s timeout.
        match with_timeout(Duration::from_secs(30), turntable_resp_signal.wait()).await {
            Ok(proto::TurntableResponse::MoveComplete { .. }) => {
                // Turntable reached target angle.
            }
            Ok(proto::TurntableResponse::Error { description }) => {
                let mut desc = heapless::String::<128>::new();
                let _ = desc.push_str("Turntable error: ");
                let _ = desc.push_str(description.as_str());
                return Err(desc);
            }
            Err(_) => {
                let mut desc = heapless::String::new();
                let _ = desc.push_str("Turntable move timeout (30s)");
                return Err(desc);
            }
        }

        // Collect real RSSI samples and report.
        let (rssi_dbm, sample_count) =
            collect_rssi(rssi_rx, cmd.samples_per_angle, RSSI_COLLECT_TIMEOUT).await;
        info!(
            "  angle={}° rssi={} dBm ({} samples)",
            angle, rssi_dbm, sample_count
        );
        resp_tx
            .send(RxToPc::DataPoint {
                angle_deg: angle,
                rssi_dbm,
                sample_count,
            })
            .await;
    }

    // Return turntable to start position so the next sweep doesn't backtrack first.
    info!("Sweep done, returning to {}°", cmd.start_deg);
    turntable_resp_signal.reset();
    send_turntable_cmd(
        sender,
        state,
        proto::TurntableCommand::MoveTo {
            angle_deg: cmd.start_deg,
        },
    )
    .await;
    match with_timeout(Duration::from_secs(30), turntable_resp_signal.wait()).await {
        Ok(proto::TurntableResponse::MoveComplete { .. }) => {}
        Ok(proto::TurntableResponse::Error { description }) => {
            info!("Return-to-start error: {}", description.as_str());
        }
        Err(_) => {
            info!("Return-to-start timeout");
        }
    }

    Ok(())
}

/// Sends RxToPc responses as COBS-framed postcard over USB-Serial-JTAG.
#[embassy_executor::task]
async fn serial_tx_task(
    usb_tx: &'static Mutex<NoopRawMutex, UsbSerialJtagTx<'static, Async>>,
    resp_rx: embassy_sync::channel::Receiver<'static, NoopRawMutex, RxToPc, 8>,
) {
    let mut buf = [0u8; COBS_BUF_SIZE];

    loop {
        let msg = resp_rx.receive().await;
        match proto::serialize_cobs(&msg, &mut buf) {
            Ok(len) => {
                let mut tx = usb_tx.lock().await;
                let _ = tx.write_all(&buf[..len]).await;
                let _ = tx.flush().await;
            }
            Err(_) => {
                info!("Failed to serialize RxToPc response");
            }
        }
    }
}
