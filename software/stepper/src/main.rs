//! Stepper motor controller for the beambench turntable on ESP32-C3-DevKit-RUST-1.
//!
//! Two mutually-exclusive backends (selected via Cargo features):
//!
//! - **step-dir** (default): GPIO step/dir/enable pulses for MKS SERVO42D/57D.
//! - **servo42c**: UART commands via mks-servo42-rs for MKS SERVO42C.
//!
//! ## Wiring (step-dir mode)
//!
//!   3.3V   ──► SERVO42D COM  (common anode)
//!   GPIO4  ──► SERVO42D STP  (step pulse)
//!   GPIO5  ──► SERVO42D DIR  (direction)
//!   GPIO6  ──► SERVO42D EN   (enable, active low)
//!   GND    ──  GND
//!   12-24V ──► SERVO42D V+
//!
//! ## Wiring (servo42c mode)
//!
//!   GPIO4  ──► SERVO42C RX
//!   GPIO5  ──► SERVO42C TX
//!   GND    ──  GND
//!   12-24V ──► SERVO42C V+

#![no_std]
#![no_main]

mod motor;

use defmt::info;
use esp_backtrace as _;
use esp_hal::{
    delay::Delay,
    gpio::{Input, InputConfig, Pull},
    rmt::Rmt,
    time::Rate,
};
#[cfg(feature = "step-dir")]
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal_smartled::{RmtSmartLeds, Sk68xxTiming, buffer_size, color_order};
use smart_leds::{RGB8, SmartLedsWrite, brightness};

use motor::Motor;

#[cfg(feature = "step-dir")]
use motor::step_dir::StepDirMotor;

#[cfg(feature = "servo42c")]
use motor::servo42c::Servo42cMotor;

// defmt requires these two symbols from the binary crate.
#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {}
}

defmt::timestamp!("");

// App descriptor required by the ESP-IDF second-stage bootloader.
esp_bootloader_esp_idf::esp_app_desc!();

// ── Configuration ────────────────────────────────────────────────────────────

/// Number of microsteps per button press.
const STEP_INCREMENT: i32 = 200;

/// Speed presets: step delay in microseconds (lower = faster).
#[cfg(feature = "step-dir")]
const SPEEDS: [u32; 5] = [
    2000, // very slow  (250 steps/s)
    500,  // slow       (1000 steps/s)
    200,  // medium     (2500 steps/s)
    100,  // fast       (5000 steps/s)
    50,   // very fast  (10000 steps/s)
];

/// LED colour per speed level.
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

    info!("Waiting 500 ms for motor startup...");
    delay.delay_millis(500u32);

    // ── Motor backend ────────────────────────────────────────────────────────

    #[cfg(feature = "step-dir")]
    let mut motor = {
        let stp = Output::new(peripherals.GPIO4, Level::Low, OutputConfig::default());
        let dir = Output::new(peripherals.GPIO5, Level::Low, OutputConfig::default());
        let en = Output::new(peripherals.GPIO6, Level::Low, OutputConfig::default());
        StepDirMotor::new(stp, dir, en, delay, SPEEDS[0])
    };

    #[cfg(feature = "servo42c")]
    let mut motor = {
        let uart = esp_hal::uart::Uart::new(
            peripherals.UART1,
            esp_hal::uart::Config::default().with_baudrate(38400),
        )
        .expect("UART init failed")
        .with_rx(peripherals.GPIO4)
        .with_tx(peripherals.GPIO5);
        Servo42cMotor::new(uart, delay)
    };

    motor.set_enabled(true);
    info!("Motor enabled");

    let speed_idx: usize = 0;
    let mut target: i32 = 0;
    led.write(brightness([SPEED_COLORS[speed_idx]].into_iter(), 20)).ok();

    info!("Ready. Press button to advance {} steps.", STEP_INCREMENT);

    let mut btn_was_pressed = false;

    loop {
        let btn_pressed = btn.is_low();
        if btn_pressed && !btn_was_pressed {
            target += STEP_INCREMENT;
            info!("Moving to position {}", target);
            led.write(brightness([RGB8 { r: 255, g: 255, b: 255 }].into_iter(), 30)).ok();

            match motor.go_to(target) {
                Ok(pos) => info!("Reached position {}", pos),
                Err(e) => info!("Motor error: {}", e),
            }

            led.write(brightness([SPEED_COLORS[speed_idx]].into_iter(), 20)).ok();
        }
        btn_was_pressed = btn_pressed;

        // Small delay to debounce.
        Delay::new().delay_millis(10u32);
    }
}
