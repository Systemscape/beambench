//! Minimal step/dir driver for MKS SERVO42D/57D on M5Stamp C3U (ESP32-C3).
//!
//! Wiring:
//!
//!   C3U 3.3V     ──► SERVO42D COM  (common anode, must match signal level)
//!   C3U GPIO4    ──► SERVO42D STP  (step pulse)
//!   C3U GPIO5    ──► SERVO42D DIR  (direction)
//!   C3U GPIO6    ──► SERVO42D EN   (enable, active level configurable via menu)
//!   GND          ──  GND
//!   12-24V       ──► SERVO42D V+
//!
//! GPIO18/19 are the built-in USB-Serial bridge on the C3U; leave them free.
//! GPIO9: on-board button (active low, internal pull-up).
//! GPIO2: on-board SK6812 RGB LED.

#![no_std]
#![no_main]

use defmt::info;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull},
    rmt::Rmt,
    time::Rate,
};
use esp_hal_smartled::{RmtSmartLeds, Sk68xxTiming, buffer_size, color_order};
use smart_leds::{RGB8, SmartLedsWrite, brightness};

// defmt requires these two symbols from the binary crate.
#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {}
}

defmt::timestamp!("");

// App descriptor required by the ESP-IDF second-stage bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

// ── Configuration ────────────────────────────────────────────────────────────

/// Speed presets: step delay in microseconds (lower = faster).
/// Cycles through these on each button press.
const SPEEDS: [u32; 5] = [
    2000, // very slow  (250 steps/s)
    500,  // slow       (1000 steps/s)
    200,  // medium     (2500 steps/s)
    100,  // fast       (5000 steps/s)
    50,   // very fast  (10000 steps/s)
];

/// LED colour per speed level (dim).
const SPEED_COLORS: [RGB8; 5] = [
    RGB8 { r: 0,   g: 0,   b: 255 }, // blue   — very slow
    RGB8 { r: 0,   g: 255, b: 255 }, // cyan   — slow
    RGB8 { r: 0,   g: 255, b: 0   }, // green  — medium
    RGB8 { r: 255, g: 255, b: 0   }, // yellow — fast
    RGB8 { r: 255, g: 0,   b: 0   }, // red    — very fast
];

// ── Entry point ──────────────────────────────────────────────────────────────

#[esp_hal::main]
fn main() -> ! {
    // Initialise defmt RTT transport so probe-rs can read logs.
    rtt_target::rtt_init_defmt!();

    let peripherals = esp_hal::init(esp_hal::Config::default());

    let mut stp = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
    let mut dir = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
    let mut en = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());

    // On-board button on GPIO9 (active low).
    let btn = Input::new(peripherals.GPIO9, InputConfig::default().with_pull(Pull::Up));

    // On-board SK6812 RGB LED on GPIO2 via RMT peripheral.
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).expect("RMT init failed");
    let mut led = RmtSmartLeds::<{ buffer_size::<RGB8>(1) }, _, RGB8, color_order::Grb, Sk68xxTiming>::new(
        rmt.channel0,
        peripherals.GPIO2,
    )
    .expect("SmartLed init failed");

    let delay = Delay::new();

    // Show green to indicate boot.
    led.write(brightness([RGB8 { r: 0, g: 255, b: 0 }].into_iter(), 10)).ok();

    // Give the SERVO42D time to finish its startup / calibration.
    info!("Waiting 500 ms for SERVO42D startup...");
    delay.delay_millis(500u32);

    // Direction: LOW = clockwise (depends on Dir menu setting).
    dir.set_low();
    info!("DIR=LOW (clockwise)");

    // Enable the motor (default active level: LOW = enabled; adjustable in menu "En").
    en.set_low();
    info!("EN=LOW (enabled)");

    // Start at the slowest speed.
    let mut speed_idx: usize = 0;
    let mut step_delay = SPEEDS[speed_idx];
    info!("Speed {}: delay={}us", speed_idx, step_delay);
    led.write(brightness([SPEED_COLORS[speed_idx]].into_iter(), 20)).ok();

    info!("Starting step pulses. Press button to cycle speed.");

    let mut btn_was_pressed = false;

    loop {
        // Generate one step pulse.
        stp.set_high();
        delay.delay_micros(step_delay);
        stp.set_low();
        delay.delay_micros(step_delay);

        // Check button (active low) — advance speed on press, debounce on release.
        let btn_pressed = btn.is_low();
        if btn_pressed && !btn_was_pressed {
            speed_idx = (speed_idx + 1) % SPEEDS.len();
            step_delay = SPEEDS[speed_idx];
            info!("Speed {}: delay={}us", speed_idx, step_delay);
            led.write(brightness([SPEED_COLORS[speed_idx]].into_iter(), 20)).ok();
        }
        btn_was_pressed = btn_pressed;
    }
}
