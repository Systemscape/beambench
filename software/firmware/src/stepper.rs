//! Stepper/Turntable role — motor controller receiving commands via ESPNOW.

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use esp_radio::esp_now::{
    EspNowManager, EspNowReceiver, EspNowSender, BROADCAST_ADDRESS,
};

use beambench_protocol::{self as proto, EspnowMessage, Role};
use beambench_protocol::stepper::{degrees_to_steps, steps_to_degrees};

use crate::common::*;
use crate::mk_static;
use crate::motor::Motor;

#[cfg(feature = "step-dir")]
use crate::motor::step_dir::StepDirMotor;

/// Default step delay in microseconds for step-dir mode.
#[cfg(feature = "step-dir")]
pub const DEFAULT_STEP_DELAY_US: u32 = 200;

enum MotorCmd {
    MoveTo { angle_deg: f32 },
    Stop,
}

enum MotorResult {
    MoveComplete { angle_deg: f32 },
    Error { msg: &'static str },
}

#[cfg(feature = "step-dir")]
pub async fn run(
    spawner: Spawner,
    motor: &'static Mutex<NoopRawMutex, StepDirMotor<'static>>,
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    receiver: EspNowReceiver<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
    flash: &'static SharedFlash,
) -> ! {
    let motor_cmd = mk_static!(Signal<NoopRawMutex, MotorCmd>, Signal::new());
    let motor_result = mk_static!(Signal<NoopRawMutex, MotorResult>, Signal::new());
    let bridge_mac = mk_static!(Mutex<NoopRawMutex, Option<[u8; 6]>>, Mutex::new(None));
    let last_bridge_seen = mk_static!(Mutex<NoopRawMutex, Option<Instant>>, Mutex::new(None));
    let ota = mk_static!(
        Mutex<NoopRawMutex, crate::ota_responder::OtaState>,
        Mutex::new(crate::ota_responder::OtaState::new())
    );

    spawner
        .spawn(stepper_discovery_task(sender, bridge_mac, last_bridge_seen, manager, led_signal))
        .ok();
    spawner
        .spawn(stepper_listener_task(manager, sender, receiver, bridge_mac, last_bridge_seen, motor_cmd, flash, ota))
        .ok();
    spawner
        .spawn(stepper_responder_task(sender, bridge_mac, motor_result))
        .ok();
    spawner
        .spawn(motor_task_step_dir(motor, motor_cmd, motor_result, led_signal))
        .ok();

    info!("Turntable controller ready, discovering Bridge...");

    loop {
        Timer::after(Duration::from_secs(60)).await;
    }
}

#[embassy_executor::task]
async fn stepper_discovery_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    bridge_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    last_bridge_seen: &'static Mutex<NoopRawMutex, Option<Instant>>,
    manager: &'static EspNowManager<'static>,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let paired = bridge_mac.lock().await.is_some();
        if paired {
            Timer::after(Duration::from_secs(10)).await;
        } else {
            Timer::after(BEACON_INTERVAL).await;
        }

        // Check for stale bridge and unpair.
        {
            let mut mac = bridge_mac.lock().await;
            let last = *last_bridge_seen.lock().await;
            if mac.is_some() && is_heartbeat_stale(last) {
                info!("Bridge heartbeat timeout, unpairing");
                let _ = manager.remove_peer(&mac.unwrap());
                *mac = None;
                *last_bridge_seen.lock().await = None;
            }
            signal_pairing_led(led_signal, mac.is_some());
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

#[embassy_executor::task]
async fn stepper_listener_task(
    manager: &'static EspNowManager<'static>,
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    mut receiver: EspNowReceiver<'static>,
    bridge_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    last_bridge_seen: &'static Mutex<NoopRawMutex, Option<Instant>>,
    motor_cmd: &'static Signal<NoopRawMutex, MotorCmd>,
    flash: &'static SharedFlash,
    ota: &'static Mutex<NoopRawMutex, crate::ota_responder::OtaState>,
) {
    loop {
        let received = receiver.receive_async().await;
        let data = received.data();
        let src = received.info.src_address;

        let msg: Result<EspnowMessage, _> = proto::deserialize(data);
        match msg {
            Ok(EspnowMessage::Hello(hello)) if hello.role == Role::Bridge => {
                let already_paired = bridge_mac.lock().await.is_some();
                if !already_paired {
                    info!(
                        "Discovered Bridge at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        src[0], src[1], src[2], src[3], src[4], src[5]
                    );
                    ensure_peer(manager, &src);
                    *bridge_mac.lock().await = Some(src);
                }
                *last_bridge_seen.lock().await = Some(Instant::now());
            }
            Ok(EspnowMessage::PairConfirm(confirm)) if confirm.role == Role::Bridge => {
                info!("Received pair confirmation from Bridge");
                ensure_peer(manager, &src);
                *bridge_mac.lock().await = Some(src);
                *last_bridge_seen.lock().await = Some(Instant::now());
            }
            Ok(EspnowMessage::TurntableCmd(cmd)) => {
                let is_bridge = {
                    let mac = bridge_mac.lock().await;
                    mac.map_or(false, |m| m == src)
                };
                if is_bridge {
                    *last_bridge_seen.lock().await = Some(Instant::now());
                    match cmd {
                        proto::TurntableCommand::MoveTo { angle_deg } => {
                            info!("Received MoveTo({})", angle_deg);
                            motor_cmd.signal(MotorCmd::MoveTo { angle_deg });
                        }
                        proto::TurntableCommand::Stop => {
                            info!("Received Stop");
                            motor_cmd.signal(MotorCmd::Stop);
                        }
                    }
                }
            }
            Ok(ref espnow_msg) if crate::ota_responder::is_ota_message(espnow_msg) => {
                if let Some(peer) = *bridge_mac.lock().await {
                    crate::ota_responder::process_and_respond(espnow_msg, ota, flash, sender, &peer).await;
                }
            }
            _ => {}
        }
    }
}

#[embassy_executor::task]
async fn stepper_responder_task(
    sender: &'static Mutex<NoopRawMutex, EspNowSender<'static>>,
    bridge_mac: &'static Mutex<NoopRawMutex, Option<[u8; 6]>>,
    result_signal: &'static Signal<NoopRawMutex, MotorResult>,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];

    loop {
        let result = result_signal.wait().await;

        let mac = *bridge_mac.lock().await;
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
                info!("Moving to {} ({} steps)", angle_deg, target_steps);

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
                        info!("Reached position: {} ({} steps)", actual_deg, pos);
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
