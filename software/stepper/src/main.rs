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
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
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

#[cfg(feature = "step-dir")]
use esp_hal::gpio::{Level, Output, OutputConfig};

use beambench_protocol::{self as proto, EspnowMessage, Role};

use motor::Motor;
#[cfg(feature = "step-dir")]
use motor::step_dir::StepDirMotor;
#[cfg(feature = "servo42c")]
use motor::servo42c::Servo42cMotor;

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
        let stp = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
        let dir = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
        let en = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());
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
    let _ = led.write(core::iter::once(RGB8 { r: 0, g: 0, b: 255 }));

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

    let paired_signal = mk_static!(Signal<NoopRawMutex, ()>, Signal::new());

    spawner.spawn(discovery_task(sender, rx_mac)).ok();
    spawner.spawn(listener_task(manager, receiver, rx_mac, motor_cmd, paired_signal)).ok();
    spawner.spawn(responder_task(sender, rx_mac, motor_result)).ok();

    // Motor task uses the concrete motor type (feature-gated).
    #[cfg(feature = "step-dir")]
    {
        let motor = mk_static!(
            Mutex::<NoopRawMutex, StepDirMotor<'static>>,
            Mutex::new(motor)
        );
        spawner.spawn(motor_task_step_dir(motor, motor_cmd, motor_result)).ok();
    }
    #[cfg(feature = "servo42c")]
    {
        let motor = mk_static!(
            Mutex::<NoopRawMutex, Servo42cMotor<'static>>,
            Mutex::new(motor)
        );
        spawner.spawn(motor_task_servo42c(motor, motor_cmd, motor_result)).ok();
    }

    info!("Turntable controller ready, discovering RX...");

    // Wait for RX pairing, then turn LED green.
    paired_signal.wait().await;
    info!("Paired with RX — LED green");
    let _ = led.write(core::iter::once(RGB8 { r: 0, g: 255, b: 0 }));

    // Main loop does nothing — all work is in tasks.
    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

// ── Tasks ────────────────────────────────────────────────────────────────────

/// Broadcasts discovery beacons. Slows down after pairing but keeps sending
/// so that RX can discover us even if it boots later.
#[embassy_executor::task]
async fn discovery_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    rx_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        if rx_mac.lock().await.is_some() {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
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
                paired_signal.signal(());
            }
            Ok(EspnowMessage::TurntableCmd(cmd)) => {
                // Verify message is from our paired RX.
                let is_rx = {
                    let mac = rx_mac.lock().await;
                    mac.map_or(false, |m| m == src)
                };
                if is_rx {
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
) {
    motor.lock().await.set_enabled(true);
    info!("Motor enabled (step-dir)");
    motor_loop(motor, cmd_signal, result_signal).await;
}

/// Runs motor operations (servo42c backend).
#[cfg(feature = "servo42c")]
#[embassy_executor::task]
async fn motor_task_servo42c(
    motor: &'static Mutex<NoopRawMutex, Servo42cMotor<'static>>,
    cmd_signal: &'static Signal<NoopRawMutex, MotorCmd>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
) {
    motor.lock().await.set_enabled(true);
    info!("Motor enabled (servo42c)");
    motor_loop(motor, cmd_signal, result_signal).await;
}

/// Shared motor control loop — works with any Motor implementation.
async fn motor_loop<M: Motor>(
    motor: &'static Mutex<NoopRawMutex, M>,
    cmd_signal: &'static Signal<NoopRawMutex, MotorCmd>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
) {
    loop {
        let cmd = cmd_signal.wait().await;
        match cmd {
            MotorCmd::MoveTo { angle_deg } => {
                let target_steps = degrees_to_steps(angle_deg);
                info!("Moving to {}° ({} steps)", angle_deg, target_steps);

                let result = motor.lock().await.go_to(target_steps);
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
                motor.lock().await.stop();
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
                EspnowMessage::TurntableResp(proto::TurntableResponse::Error {
                    description: desc,
                })
            }
        };

        if let Ok(data) = proto::serialize(&resp, &mut buf) {
            let mut s = sender.lock().await;
            let _ = s.send_async(&peer, data).await;
        }
    }
}
