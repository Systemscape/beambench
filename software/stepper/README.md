# Beambench Stepper Controller

Bare-metal firmware for the beambench turntable stepper motor, running on the
[ESP32-C3-DevKit-RUST-1](https://github.com/esp-rs/esp-rust-board).

Two motor backends are available, selected at compile time via Cargo features:

| Feature      | Motor             | Interface           |
|-------------|-------------------|---------------------|
| `step-dir`  | MKS SERVO42D/57D  | GPIO step/dir/enable |
| `servo42c`  | MKS SERVO42C      | UART (38400 baud)   |

## Build & Flash

Prerequisites: nightly Rust toolchain and [probe-rs](https://probe.rs/).

```sh
# Step/dir mode (default)
cargo run

# Servo42c mode
cargo run --features servo42c --no-default-features
```

## Wiring

### Step/Dir Mode (MKS SERVO42D)

| ESP32-C3 | SERVO42D | Function       |
|----------|----------|----------------|
| 3.3V     | COM      | Common anode   |
| GPIO4    | STP      | Step pulse     |
| GPIO5    | DIR      | Direction      |
| GPIO6    | EN       | Enable (active low) |
| GND      | GND      | Ground         |
| —        | V+       | 12–24V supply  |

### Servo42c Mode (MKS SERVO42C)

| ESP32-C3 | SERVO42C | Function       |
|----------|----------|----------------|
| GPIO4    | RX       | UART receive   |
| GPIO5    | TX       | UART transmit  |
| GND      | GND      | Ground         |
| —        | V+       | 12–24V supply  |

### On-board Peripherals

| GPIO  | Function                               |
|-------|----------------------------------------|
| GPIO2 | SK6812 RGB LED (active, RMT driven)    |
| GPIO7 | Plain LED                              |
| GPIO9 | Boot button (active low, user input)    |
| GPIO8 | I2C SCL (SHTC3 + ICM-42670-P)          |
| GPIO10| I2C SDA (SHTC3 + ICM-42670-P)          |
| GPIO18/19 | USB-Serial/JTAG — do not use       |

## Links

- [ESP32-C3-DevKit-RUST-1](https://github.com/esp-rs/esp-rust-board)
- [MKS SERVO42D manual](https://github.com/makerbase-motor/MKS-SERVO42D-57D)
- [MKS SERVO42C](https://github.com/makerbase-mks/MKS-SERVO42C)
- [mks-servo42-rs crate](https://crates.io/crates/mks-servo42-rs)
