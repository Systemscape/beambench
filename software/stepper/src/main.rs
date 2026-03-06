//! Beambench turntable controller — stepper motor + ESPNOW on ESP32-C3.
//!
//! Receives movement commands from the RX board via ESPNOW, drives the motor,
//! and sends completion acknowledgements back.
//!
//! Two motor backends (selected via Cargo features):
//! - **step-dir** (default): GPIO step/dir/enable pulses for MKS SERVO42D/57D
//! - **servo42c**: UART commands via mks-servo42-rs for MKS SERVO42C

#![no_std]
#![no_main]

mod motor;

use defmt::info;
use embassy_executor::Spawner;
use embassy_futures::select::{select, Either};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock, delay::Delay, interrupt::software::SoftwareInterruptControl, rmt::Rmt,
    time::Rate, timer::timg::TimerGroup,
};
use esp_hal_smartled::{buffer_size, color_order, RmtSmartLeds, Sk68xxTiming};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, PeerInfo, BROADCAST_ADDRESS,
};
use smart_leds::{SmartLedsWrite, RGB8};

#[cfg(feature = "step-dir")]
use esp_hal::gpio::{Level, Output, OutputConfig};

use beambench_protocol::{self as proto, EspnowMessage, Role};

#[cfg(feature = "servo42c")]
use motor::servo42c::Servo42cMotor;
#[cfg(feature = "step-dir")]
use motor::step_dir::StepDirMotor;
use motor::Motor;

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
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

/// Default step delay in microseconds for step-dir mode.
#[cfg(feature = "step-dir")]
const DEFAULT_STEP_DELAY_US: u32 = 200;

// ── Motor command channel ────────────────────────────────────────────────────

/// Command sent from the ESPNOW listener to the motor task.
enum MotorCmd {
    MoveTo { angle_deg: f32 },
    Stop,
}

/// Result sent from the motor task back to the ESPNOW sender.
enum MotorResult {
    MoveComplete { angle_deg: f32 },
    Error { msg: &'static str },
}

/// LED state for the led_task.
#[derive(Clone, Copy)]
enum LedState {
    Solid(RGB8),
    /// Blink between `color` and off, with `period_ms` total cycle time.
    Blink {
        color: RGB8,
        period_ms: u64,
    },
}

const COLOR_BLUE: RGB8 = RGB8 { r: 0, g: 0, b: 20 };
const COLOR_GREEN: RGB8 = RGB8 { r: 0, g: 20, b: 0 };
const COLOR_AMBER: RGB8 = RGB8 { r: 20, g: 6, b: 0 };
const COLOR_OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };

// Re-export stepper math from protocol crate.
use beambench_protocol::stepper::{degrees_to_steps, steps_to_degrees};

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

    // ── Motor setup ──────────────────────────────────────────────────────────

    let delay = Delay::new();

    #[cfg(feature = "step-dir")]
    let motor = {
        let en = Output::new(peripherals.GPIO6, Level::High, OutputConfig::default());
        let stp = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
        let dir = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
        StepDirMotor::new(stp, dir, en, delay, DEFAULT_STEP_DELAY_US)
    };

    #[cfg(feature = "servo42c")]
    let motor = {
        let uart = esp_hal::uart::Uart::new(
            peripherals.UART1,
            esp_hal::uart::Config::default().with_baudrate(38400),
        )
        .expect("UART init failed")
        .with_rx(peripherals.GPIO4)
        .with_tx(peripherals.GPIO5);
        Servo42cMotor::new(uart, delay)
    };

    // ── ESP-NOW setup ────────────────────────────────────────────────────────

    let esp_radio_ctrl = &*mk_static!(esp_radio::Controller<'static>, esp_radio::init().unwrap());

    let (mut wifi_controller, interfaces) =
        esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    wifi_controller
        .set_mode(esp_radio::wifi::WifiMode::Sta)
        .unwrap();
    wifi_controller.start().unwrap();

    let esp_now = interfaces.esp_now;
    esp_now.set_channel(DEFAULT_CHANNEL).unwrap();
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

    let (manager, sender, receiver) = esp_now.split();
    let manager = mk_static!(EspNowManager<'static>, manager);
    let sender = mk_static!(
        Mutex::<NoopRawMutex, EspNowSender<'static>>,
        Mutex::<NoopRawMutex, _>::new(sender)
    );

    // Signals for inter-task communication.
    let motor_cmd = mk_static!(Signal<NoopRawMutex, MotorCmd>, Signal::new());
    let motor_result = mk_static!(Signal<NoopRawMutex, MotorResult>, Signal::new());
    let rx_mac = mk_static!(Mutex<NoopRawMutex, Option<[u8; 6]>>, Mutex::new(None));
    let last_rx_seen = mk_static!(Mutex<NoopRawMutex, Option<Instant>>, Mutex::new(None));

    let paired_signal = mk_static!(Signal<NoopRawMutex, ()>, Signal::new());

    spawner
        .spawn(discovery_task(
            sender,
            rx_mac,
            last_rx_seen,
            manager,
            led_signal,
        ))
        .ok();
    spawner
        .spawn(listener_task(
            manager,
            receiver,
            rx_mac,
            last_rx_seen,
            motor_cmd,
            paired_signal,
        ))
        .ok();
    spawner
        .spawn(responder_task(sender, rx_mac, motor_result))
        .ok();

    // Motor task uses the concrete motor type (feature-gated).
    #[cfg(feature = "step-dir")]
    {
        let motor = mk_static!(
            Mutex::<NoopRawMutex, StepDirMotor<'static>>,
            Mutex::new(motor)
        );
        spawner
            .spawn(motor_task_step_dir(
                motor,
                motor_cmd,
                motor_result,
                led_signal,
            ))
            .ok();
    }
    #[cfg(feature = "servo42c")]
    {
        let motor = mk_static!(
            Mutex::<NoopRawMutex, Servo42cMotor<'static>>,
            Mutex::new(motor)
        );
        spawner
            .spawn(motor_task_servo42c(
                motor,
                motor_cmd,
                motor_result,
                led_signal,
            ))
            .ok();
    }

    info!("Turntable controller ready, discovering RX...");

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
                        Either::First(new) => {
                            led_state = new;
                            break;
                        }
                        Either::Second(_) => {}
                    }
                    let _ = led.write(core::iter::once(COLOR_OFF));
                    match select(led_signal.wait(), Timer::after(half)).await {
                        Either::First(new) => {
                            led_state = new;
                            break;
                        }
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
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    rx_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    last_rx_seen: &'static Mutex<NoopRawMutex, Option<Instant>>,
    manager: &'static EspNowManager<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let paired = rx_mac.lock().await.is_some();
        if paired {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
        }

        // Check for stale RX and unpair.
        {
            let mut mac = rx_mac.lock().await;
            if let Some(peer) = *mac {
                if let Some(last) = *last_rx_seen.lock().await {
                    if Instant::now() - last > HEARTBEAT_TIMEOUT {
                        info!("RX heartbeat timeout, unpairing");
                        let _ = manager.remove_peer(&peer);
                        *mac = None;
                        *last_rx_seen.lock().await = None;
                    }
                }
            }
            if mac.is_some() {
                led_signal.signal(LedState::Solid(COLOR_GREEN));
            } else {
                led_signal.signal(LedState::Blink {
                    color: COLOR_BLUE,
                    period_ms: 500,
                });
            }
        }

        let beacon = EspnowMessage::Hello(proto::HelloBeacon {
            role: Role::Turntable,
            mac: [0; 6],
        });

        if let Ok(data) = proto::serialize(&beacon, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&BROADCAST_ADDRESS, data).await;
        }
    }
}

/// Listens for ESPNOW messages: discovery from RX, turntable commands.
#[embassy_executor::task]
async fn listener_task(
    manager: &'static EspNowManager<'static>,
    mut receiver: EspNowReceiver<'static>,
    rx_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    last_rx_seen: &'static Mutex<NoopRawMutex, Option<Instant>>,
    motor_cmd: &'static Signal<NoopRawMutex, MotorCmd>,
    paired_signal: &'static Signal<NoopRawMutex, ()>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::Hello(hello)) if hello.role == Role::Rx => {
                let already_paired = rx_mac.lock().await.is_some();
                if !already_paired {
                    info!(
                        "Discovered RX at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
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

                    *rx_mac.lock().await = Some(src);
                    paired_signal.signal(());
                }
                *last_rx_seen.lock().await = Some(Instant::now());
            }
            Ok(EspnowMessage::PairConfirm(confirm)) if confirm.role == Role::Rx => {
                info!("Received pair confirmation from RX");
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
                *rx_mac.lock().await = Some(src);
                *last_rx_seen.lock().await = Some(Instant::now());
                paired_signal.signal(());
            }
            Ok(EspnowMessage::TurntableCmd(cmd)) => {
                // Verify message is from our paired RX.
                let is_rx = {
                    let mac = rx_mac.lock().await;
                    mac.map_or(false, |m| m == src)
                };
                if is_rx {
                    *last_rx_seen.lock().await = Some(Instant::now());
                    match cmd {
                        proto::TurntableCommand::MoveTo { angle_deg } => {
                            info!("Received MoveTo({}°)", angle_deg);
                            motor_cmd.signal(MotorCmd::MoveTo { angle_deg });
                        }
                        proto::TurntableCommand::Stop => {
                            info!("Received Stop");
                            motor_cmd.signal(MotorCmd::Stop);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Runs motor operations (step-dir backend).
#[cfg(feature = "step-dir")]
#[embassy_executor::task]
async fn motor_task_step_dir(
    motor: &'static Mutex<NoopRawMutex, StepDirMotor<'static>>,
    cmd_signal: &'static Signal<NoopRawMutex, MotorCmd>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    info!("Motor ready (step-dir, disabled until first move)");
    motor_loop(motor, cmd_signal, result_signal, led_signal).await;
}

/// Runs motor operations (servo42c backend).
#[cfg(feature = "servo42c")]
#[embassy_executor::task]
async fn motor_task_servo42c(
    motor: &'static Mutex<NoopRawMutex, Servo42cMotor<'static>>,
    cmd_signal: &'static Signal<NoopRawMutex, MotorCmd>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    info!("Motor ready (servo42c, disabled until first move)");
    motor_loop(motor, cmd_signal, result_signal, led_signal).await;
}

/// Shared motor control loop — works with any Motor implementation.
///
/// The motor is kept disabled (enable pin HIGH) while idle to save power and
/// avoid heating the stepper. It is enabled immediately before each move and
/// disabled again once the move completes.
async fn motor_loop<M: Motor>(
    motor: &'static Mutex<NoopRawMutex, M>,
    cmd_signal: &'static Signal<NoopRawMutex, MotorCmd>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    loop {
        let cmd = cmd_signal.wait().await;
        match cmd {
            MotorCmd::MoveTo { angle_deg } => {
                let target_steps = degrees_to_steps(angle_deg);
                info!("Moving to {}° ({} steps)", angle_deg, target_steps);

                led_signal.signal(LedState::Blink {
                    color: COLOR_AMBER,
                    period_ms: 200,
                });

                let mut m = motor.lock().await;
                m.set_enabled(true);
                let result = m.go_to(target_steps);
                m.set_enabled(false);
                drop(m);

                led_signal.signal(LedState::Solid(COLOR_GREEN));

                match result {
                    Ok(pos) => {
                        let actual_deg = steps_to_degrees(pos);
                        info!("Reached position: {}° ({} steps)", actual_deg, pos);
                        result_signal.signal(MotorResult::MoveComplete {
                            angle_deg: actual_deg,
                        });
                    }
                    Err(e) => {
                        info!("Motor error: {}", e);
                        result_signal.signal(MotorResult::Error {
                            msg: "Motor error during move",
                        });
                    }
                }
            }
            MotorCmd::Stop => {
                let mut m = motor.lock().await;
                m.stop();
                m.set_enabled(false);
                drop(m);
                led_signal.signal(LedState::Solid(COLOR_GREEN));
                info!("Motor stopped");
            }
        }
    }
}

/// Sends motor results back to RX via ESPNOW.
#[embassy_executor::task]
async fn responder_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    rx_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let result = result_signal.wait().await;

        let mac = *rx_mac.lock().await;
        let Some(peer) = mac else {
            continue;
        };

        let resp = match result {
            MotorResult::MoveComplete { angle_deg } => {
                EspnowMessage::TurntableResp(proto::TurntableResponse::MoveComplete { angle_deg })
            }
            MotorResult::Error { msg } => {
                let mut desc = heapless::String::new();
                let _ = desc.push_str(msg);
                EspnowMessage::TurntableResp(proto::TurntableResponse::Error { description: desc })
            }
        };

        if let Ok(data) = proto::serialize(&resp, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&peer, data).await;
        }
    }
}
