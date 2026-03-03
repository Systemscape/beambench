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
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel, mutex::Mutex, signal::Signal};
use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer, with_timeout};
use embedded_io_async::{Read, Write};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, interrupt::software::SoftwareInterruptControl, rmt::Rmt, time::Rate,
    timer::timg::TimerGroup,
    usb_serial_jtag::{UsbSerialJtagRx, UsbSerialJtagTx, UsbSerialJtag},
    Async,
};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, Sk68xxTiming};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, PeerInfo, BROADCAST_ADDRESS,
};
use smart_leds::{SmartLedsWrite, RGB8};

use beambench_protocol::{self as proto, EspnowMessage, PcToRx, RxToPc, Role};

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
    sweeping: bool,
}

impl RxState {
    const fn new() -> Self {
        Self {
            tx_paired: false,
            tx_mac: [0u8; 6],
            turntable_paired: false,
            turntable_mac: [0u8; 6],
            sweeping: false,
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

const COLOR_BLUE: RGB8 = RGB8 { r: 0, g: 0, b: 255 };
const COLOR_GREEN: RGB8 = RGB8 { r: 0, g: 255, b: 0 };
const COLOR_AMBER: RGB8 = RGB8 { r: 255, g: 80, b: 0 };
const COLOR_OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };

/// Sweep parameters sent from serial_rx_task to sweep_task.
struct SweepCmd {
    start_deg: f32,
    stop_deg: f32,
    step_deg: f32,
    _samples_per_angle: u16, // Phase B: used for RSSI averaging
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
    let usb_tx = mk_static!(Mutex::<NoopRawMutex, UsbSerialJtagTx<'static, Async>>, Mutex::new(usb_tx));

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
    let sweep_cmd_ch = mk_static!(Channel::<NoopRawMutex, SweepCmd, 1>, Channel::new());
    let turntable_resp_signal = mk_static!(
        Signal<NoopRawMutex, proto::TurntableResponse>,
        Signal::new()
    );
    let stop_signal = mk_static!(Signal<NoopRawMutex, ()>, Signal::new());

    spawner.spawn(discovery_task(sender, state)).ok();
    spawner
        .spawn(listener_task(manager, sender, receiver, state, rssi_signal, led_signal, turntable_resp_signal))
        .ok();
    spawner.spawn(serial_rx_task(usb_rx, state, serial_resp_ch.sender(), sweep_cmd_ch.sender(), stop_signal, sender)).ok();
    spawner.spawn(serial_tx_task(usb_tx, serial_resp_ch.receiver())).ok();
    spawner.spawn(sweep_task(
        sender, state, turntable_resp_signal,
        sweep_cmd_ch.receiver(), stop_signal,
        serial_resp_ch.sender(), led_signal,
    )).ok();

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
                    Either::First(new) => { led_state = new; }
                    Either::Second(rssi) => { info!("RSSI: {} dBm", rssi); }
                }
            }
            LedState::Blink { color, period_ms } => {
                let half = Duration::from_millis(period_ms / 2);
                loop {
                    let _ = led.write(core::iter::once(color));
                    match with_timeout(half, led_signal.wait()).await {
                        Ok(new) => { led_state = new; break; }
                        Err(_) => {}
                    }
                    let _ = led.write(core::iter::once(COLOR_OFF));
                    match with_timeout(half, led_signal.wait()).await {
                        Ok(new) => { led_state = new; break; }
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
                    let s = state.lock().await;
                    s.tx_paired && s.tx_mac == src
                };
                if is_tx {
                    rssi_signal.signal(rssi);

                    // Start blinking on first RSSI packet.
                    let now = embassy_time::Instant::now().as_millis();
                    if !activity_active || now - last_rssi_tick > 1000 {
                        led_signal.signal(LedState::Blink { color: COLOR_AMBER, period_ms: 200 });
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
                            handle_serial_cmd(cmd, state, &resp_tx, &sweep_cmd_tx, stop_signal, sender).await;
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
) {
    match cmd {
        PcToRx::QueryStatus => {
            let s = state.lock().await;
            resp_tx.send(RxToPc::Status {
                sweeping: s.sweeping,
                tx_connected: s.tx_paired,
                turntable_connected: s.turntable_paired,
            }).await;
        }
        PcToRx::StartSweep { start_deg, stop_deg, step_deg, samples_per_angle } => {
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
            sweep_cmd_tx.send(SweepCmd {
                start_deg,
                stop_deg,
                step_deg,
                _samples_per_angle: samples_per_angle,
            }).await;
        }
        PcToRx::Stop => {
            info!("Stop command received");
            stop_signal.signal(());
            // Also send immediate Stop to turntable for fast halt.
            send_turntable_cmd(sender, state, proto::TurntableCommand::Stop).await;
        }
        PcToRx::ConfigureTx { channel, tx_power_dbm, packet_rate_hz } => {
            send_tx_cmd(sender, state, proto::TxCommand::Configure {
                channel,
                tx_power_dbm,
                packet_rate_hz,
            }).await;
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

// ── Sweep task ──────────────────────────────────────────────────────────────

/// Pre-spawned sweep coordinator. Waits for SweepCmd on the channel,
/// then executes the sweep and sends results back via serial_resp_ch.
#[embassy_executor::task]
async fn sweep_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
    sweep_cmd_rx: embassy_sync::channel::Receiver<'static, NoopRawMutex, SweepCmd, 1>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    resp_tx: embassy_sync::channel::Sender<'static, NoopRawMutex, RxToPc, 8>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
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
        led_signal.signal(LedState::Blink { color: COLOR_AMBER, period_ms: 500 });

        let result = run_sweep_inner(
            &cmd, sender, state, turntable_resp_signal, stop_signal, &resp_tx,
        ).await;

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

/// Phase A sweep: step turntable through all angles, send DataPoints with dummy RSSI.
async fn run_sweep_inner(
    cmd: &SweepCmd,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    state: &'static Mutex<NoopRawMutex, RxState>,
    turntable_resp_signal: &'static Signal<NoopRawMutex, proto::TurntableResponse>,
    stop_signal: &'static Signal<NoopRawMutex, ()>,
    resp_tx: &embassy_sync::channel::Sender<'static, NoopRawMutex, RxToPc, 8>,
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
    let num_steps = if n - floor_n as f32 > 0.001 { floor_n + 1 } else { floor_n };

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
            if raw_angle > cmd.stop_deg { cmd.stop_deg } else { raw_angle }
        } else {
            if raw_angle < cmd.stop_deg { cmd.stop_deg } else { raw_angle }
        };

        // Send MoveTo command.
        info!("Sweep: moving to {}° (step {}/{})", angle, i + 1, num_steps + 1);
        turntable_resp_signal.reset();
        send_turntable_cmd(sender, state, proto::TurntableCommand::MoveTo { angle_deg: angle }).await;

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

        // Phase A: send DataPoint with dummy RSSI.
        resp_tx.send(RxToPc::DataPoint {
            angle_deg: angle,
            rssi_dbm: 0.0,
            sample_count: 0,
        }).await;
    }

    // Return turntable to start position so the next sweep doesn't backtrack first.
    info!("Sweep done, returning to {}°", cmd.start_deg);
    turntable_resp_signal.reset();
    send_turntable_cmd(sender, state, proto::TurntableCommand::MoveTo { angle_deg: cmd.start_deg }).await;
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
