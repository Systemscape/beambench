//! Beambench unified firmware — single binary for all ESP32-C3 devices.
//!
//! The device role (Bridge, RX, TX, Turntable) is stored in a flash partition.
//! Use `--features role-<name>` to provision a device with a specific role.
//! Without a role feature, the firmware reads the role from flash.

#![no_std]
#![no_main]

mod bridge;
mod common;
mod measurement;
mod motor;
mod ota_responder;
mod role_provision;
mod rx;
mod turntable;
mod tx;

use defmt::info;
use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
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
use esp_radio::esp_now::{EspNowManager, EspNowSender};
use smart_leds::{SmartLedsWrite, RGB8};

use beambench_protocol::Role;
use common::*;

type Led = RmtSmartLeds<'static, { buffer_size::<RGB8>(1) }, esp_hal::Blocking, RGB8, color_order::Grb, Sk68xxTiming>;

#[defmt::panic_handler]
fn defmt_panic() -> ! {
    loop {}
}

defmt::timestamp!("");
esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    rtt_target::rtt_init_defmt!();

    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(config);

    esp_alloc::heap_allocator!(size: 72 * 1024);

    // ── Flash + role ──────────────────────────────────────────────────────────

    let mut flash_storage = esp_storage::FlashStorage::new(peripherals.FLASH);
    let role = role_provision::resolve_role(&mut flash_storage);
    info!("Booting as {:?}", defmt::Debug2Format(&role));

    // Make flash available as 'static for OTA responder.
    let flash = mk_static!(
        Mutex::<NoopRawMutex, esp_storage::FlashStorage<'static>>,
        Mutex::new(flash_storage)
    );

    // ── RTOS + Embassy setup ─────────────────────────────────────────────────

    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // ── ESP-NOW setup ────────────────────────────────────────────────────────

    let esp_radio_ctrl =
        &*mk_static!(esp_radio::Controller<'static>, esp_radio::init().unwrap());

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

    // ── Mark OTA slot as valid (prevents rollback after successful boot) ────
    ota_responder::mark_current_valid(&mut *flash.lock().await);

    // ── LED setup (SK6812 on GPIO2 via RMT) ──────────────────────────────────

    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let mut led =
        RmtSmartLeds::<{ buffer_size::<RGB8>(1) }, _, RGB8, color_order::Grb, Sk68xxTiming>::new(
            rmt.channel0,
            peripherals.GPIO2,
        )
        .unwrap();

    let _ = led.write(core::iter::once(COLOR_BLUE));

    let led_signal = mk_static!(Signal<NoopRawMutex, LedState>, Signal::new());

    let led = mk_static!(Led, led);
    spawner.spawn(led_task(led, led_signal)).ok();

    // ── ESP-NOW split ────────────────────────────────────────────────────────

    let (manager, sender, receiver) = esp_now.split();
    let manager = mk_static!(EspNowManager<'static>, manager);
    let sender = mk_static!(
        Mutex::<NoopRawMutex, EspNowSender<'static>>,
        Mutex::<NoopRawMutex, _>::new(sender)
    );

    // ── Role dispatch ────────────────────────────────────────────────────────

    match role {
        Role::Bridge => {
            let usb_serial = UsbSerialJtag::new(peripherals.USB_DEVICE).into_async();
            let (usb_rx, usb_tx) = usb_serial.split();
            let usb_rx = mk_static!(UsbSerialJtagRx<'static, Async>, usb_rx);
            let usb_tx = mk_static!(UsbSerialJtagTx<'static, Async>, usb_tx);
            bridge::run(spawner, manager, sender, receiver, led_signal, usb_rx, usb_tx).await;
        }
        Role::Rx => {
            #[cfg(not(feature = "backend-uart"))]
            {
                rx::run(spawner, manager, sender, receiver, led_signal, flash).await;
            }
            #[cfg(feature = "backend-uart")]
            {
                let uart = esp_hal::uart::Uart::new(
                    peripherals.UART1,
                    esp_hal::uart::Config::default(),
                )
                .unwrap()
                .with_rx(peripherals.GPIO5)
                .with_tx(peripherals.GPIO4)
                .into_async();
                rx::run(spawner, manager, sender, receiver, led_signal, flash, uart).await;
            }
        }
        Role::Tx => {
            #[cfg(not(feature = "backend-uart"))]
            {
                tx::run(spawner, manager, sender, receiver, led_signal, flash).await;
            }
            #[cfg(feature = "backend-uart")]
            {
                let uart = esp_hal::uart::Uart::new(
                    peripherals.UART1,
                    esp_hal::uart::Config::default(),
                )
                .unwrap()
                .with_rx(peripherals.GPIO5)
                .with_tx(peripherals.GPIO4)
                .into_async();
                tx::run(spawner, manager, sender, receiver, led_signal, flash, uart).await;
            }
        }
        Role::Turntable => {
            // Create motor here so GPIO pins don't need to cross task boundaries.
            #[cfg(feature = "step-dir")]
            {
                use esp_hal::gpio::{Level, Output, OutputConfig};
                use motor::step_dir::StepDirMotor;

                let en = Output::new(peripherals.GPIO6, Level::High, OutputConfig::default());
                let stp = Output::new(peripherals.GPIO7, Level::Low, OutputConfig::default());
                let dir = Output::new(peripherals.GPIO8, Level::Low, OutputConfig::default());
                let motor = StepDirMotor::new(
                    stp,
                    dir,
                    en,
                    esp_hal::delay::Delay::new(),
                    turntable::DEFAULT_STEP_DELAY_US,
                );
                let motor = mk_static!(
                    Mutex::<NoopRawMutex, StepDirMotor<'static>>,
                    Mutex::new(motor)
                );
                turntable::run(spawner, motor, manager, sender, receiver, led_signal, flash).await;
            }
            #[cfg(not(feature = "step-dir"))]
            {
                defmt::panic!("No motor backend selected for turntable role");
            }
        }
    }

    // Role::run() functions loop forever, but the compiler can't prove it.
    // This is unreachable.
    #[allow(unreachable_code)]
    loop {
        embassy_time::Timer::after(embassy_time::Duration::from_secs(3600)).await;
    }
}

#[embassy_executor::task]
async fn led_task(
    led: &'static mut Led,
    led_signal: &'static Signal<NoopRawMutex, LedState>,
) {
    run_led_loop(led, led_signal, LedState::Blink { color: COLOR_BLUE, period_ms: 500 }).await;
}
