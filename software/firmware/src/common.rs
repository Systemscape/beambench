//! Shared code for all firmware roles: LED control, ESP-NOW init, discovery helpers.

use embassy_futures::select::{select, Either};
use embassy_time::{Duration, Timer};
use smart_leds::{SmartLedsWrite, RGB8};

use core::sync::atomic::{AtomicBool, Ordering};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, signal::Signal};

/// Global flag set during OTA updates to prevent discovery tasks from
/// overriding the LED state.
pub static OTA_IN_PROGRESS: AtomicBool = AtomicBool::new(false);

// ── LED colors (dim — SK6812 is very bright) ────────────────────────────────

pub const COLOR_BLUE: RGB8 = RGB8 { r: 0, g: 0, b: 20 };
pub const COLOR_GREEN: RGB8 = RGB8 { r: 0, g: 20, b: 0 };
pub const COLOR_RED: RGB8 = RGB8 { r: 20, g: 0, b: 0 };
pub const COLOR_AMBER: RGB8 = RGB8 { r: 20, g: 6, b: 0 };
pub const COLOR_OFF: RGB8 = RGB8 { r: 0, g: 0, b: 0 };

// ── LED state machine ───────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub enum LedState {
    Solid(RGB8),
    Blink { color: RGB8, period_ms: u64 },
}

/// Drives the LED state machine forever. Spawned as `led_task` in main.
pub async fn run_led_loop(
    led: &mut impl SmartLedsWrite<Color = RGB8>,
    led_signal: &Signal<NoopRawMutex, LedState>,
    initial: LedState,
) {
    let mut led_state = initial;
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

// ── mk_static! macro ────────────────────────────────────────────────────────

#[macro_export]
macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.uninit().write($val)
    }};
}

/// Flash storage behind a mutex, shared between role provisioning and OTA.
pub type SharedFlash = embassy_sync::mutex::Mutex<NoopRawMutex, esp_storage::FlashStorage<'static>>;

// ── ESP-NOW channel ─────────────────────────────────────────────────────────

pub const DEFAULT_CHANNEL: u8 = 11;
pub const BEACON_INTERVAL: Duration = Duration::from_secs(1);
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(15);

// ── Shared helpers ──────────────────────────────────────────────────────────

/// Add an ESP-NOW peer if not already registered.
pub fn ensure_peer(manager: &esp_radio::esp_now::EspNowManager, mac: &[u8; 6]) {
    if !manager.peer_exists(mac) {
        manager
            .add_peer(esp_radio::esp_now::PeerInfo {
                interface: esp_radio::esp_now::EspNowWifiInterface::Sta,
                peer_address: *mac,
                lmk: None,
                channel: None,
                encrypt: false,
            })
            .unwrap();
    }
}

/// Signal LED to reflect pairing state: solid green when paired, blinking blue otherwise.
/// Skipped while OTA is in progress to avoid overriding the red blink.
pub fn signal_pairing_led(
    led_signal: &Signal<NoopRawMutex, LedState>,
    paired: bool,
) {
    if OTA_IN_PROGRESS.load(Ordering::Relaxed) {
        return;
    }
    if paired {
        led_signal.signal(LedState::Solid(COLOR_GREEN));
    } else {
        led_signal.signal(LedState::Blink {
            color: COLOR_BLUE,
            period_ms: 500,
        });
    }
}

/// Check whether a heartbeat timestamp has expired.
pub fn is_heartbeat_stale(last_seen: Option<embassy_time::Instant>) -> bool {
    match last_seen {
        Some(t) => embassy_time::Instant::now() - t > HEARTBEAT_TIMEOUT,
        None => false,
    }
}
