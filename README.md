# Beambench — Automated Antenna Pattern Measurement

Measures antenna radiation patterns by rotating the antenna under test on a turntable while recording RSSI at each angle. Produces polar plots.

## Hardware Setup

Three ESP32-C3-DevKit-RUST-1 boards connected via ESPNOW:

| Board | Role | Connection |
|-------|------|------------|
| Motor Controller | Drives stepper motor | Wired to motor + ESPNOW to RX |
| Transmitter (TX) | Sits on turntable with antenna under test | Battery-powered, ESPNOW to RX |
| Receiver (RX) | Fixed position, measures RSSI | USB-serial to PC + ESPNOW to both |

```
    ┌───────────┐
    │  ESP #2   │        ESPNOW         ┌───────────┐
    │  TX       │                       │  ESP #3   │
    │  (battery)│           ─ ─ ─ ─ ─ ► │  receiver │
    │  + antenna│                       │           │
    │  under    │                       └─────┬─────┘
    │  test     │
    └─────┬─────┘
          │ sits on
    ┌─────┴─────┐
    │ turntable │
    └─────╦─────┘
    ┌─────╩─────┐
    │  stepper  │
    │  motor    │
    └─────┬─────┘
          ╫ wired
    ┌─────┴─────┐         ESPNOW         ┌───────────┐
    │  ESP #1   │                        │  ESP #3   │
    │  motor    │  ◄ ─ ─ ─ ─ ─ ─ ─ ─ ─ ► │  receiver │
    │  ctrl.    │                        │           │
    └─────┬─────┘                        └─────┬─────┘
          │ USB                                │ USB
    ┌─────┴────────────────────────────────────┴─────┐
    │                      PC                        │
    └────────────────────────────────────────────────┘
```

## Software Structure

Separate crates (not a Cargo workspace — mixed host/embedded targets):

```
software/
├── protocol/    Shared no_std message types (serde + postcard)
├── stepper/     Turntable motor controller firmware (Embassy async + ESPNOW)
├── tx/          TX beacon firmware
├── rx/          RX coordinator firmware (ESPNOW + USB-serial bridge)
└── pc/          PC app (Axum backend + Svelte frontend + Plotly.js polar plot)
```

Each crate has a `justfile`. The top-level `justfile` delegates.

### Protocol Layers

- **Serial (PC ↔ RX):** COBS-framed postcard over USB-serial or TCP
- **ESPNOW (between boards):** postcard-serialized `EspnowMessage` payloads

## Stepper Configuration

The stepper firmware supports two motor backends, selected at compile time:

| Feature | Motor | Interface |
|---------|-------|-----------|
| `step-dir` (default) | MKS SERVO42D/57D | GPIO step/dir/enable |
| `servo42c` | MKS SERVO42C | UART (38400 baud) |

### Motor Parameters

- **Steps per revolution:** 3200 (200 full steps × 16 microsteps)
- **Angular resolution:** ~0.1125°/step
- **Step delay:** 200 µs (step-dir mode)
- **ESPNOW channel:** 11

The angle conversion math lives in `protocol::stepper` so it can be tested on the host. The stepper firmware imports it.

### Wiring

**Step/Dir Mode (MKS SERVO42D):**

| ESP32-C3 | SERVO42D | Function |
|----------|----------|----------|
| 3.3V | COM | Common anode |
| GPIO4 | STP | Step pulse |
| GPIO5 | DIR | Direction |
| GPIO6 | EN | Enable (active low) |
| GND | GND | Ground |
| — | V+ | 12–24V supply |

**Servo42C Mode (MKS SERVO42C):**

| ESP32-C3 | SERVO42C | Function |
|----------|----------|----------|
| GPIO4 | RX | UART receive |
| GPIO5 | TX | UART transmit |
| GND | GND | Ground |
| — | V+ | 12–24V supply |

### Build & Flash

Requires nightly Rust and [probe-rs](https://probe.rs/).

```sh
cd software/stepper

# Step/dir mode (default)
cargo run

# Servo42c mode
cargo run --features servo42c --no-default-features
```

## PC App

Axum backend with WebSocket control + Svelte SPA with Plotly.js polar plot.

```sh
cd software/pc

# Development (backend + Vite dev server)
just dev

# Run the RX simulator (no hardware needed)
just sim

# Production build (embeds frontend in binary)
just build
```

The PC app communicates with the RX board over USB-serial (COBS-framed postcard). For development without hardware, use the **rx-sim** TCP simulator which generates synthetic cardioid antenna patterns.

Connect to the simulator via the UI by selecting `tcp://127.0.0.1:9876`.

## Testing

### Quick Reference

```sh
just test                              # all host-side tests
cd software/protocol && cargo test     # protocol only
cd software/pc && cargo test --lib     # PC unit tests only
cd software/pc && cargo test --features sim  # unit + integration
```

### What's Tested

**Protocol (12 tests):**
- Postcard serialization round-trips for all `PcToRx` and `RxToPc` variants
- COBS framing round-trips
- `EspnowMessage` envelope round-trips (all discovery, turntable, TX variants)
- Max-length error messages fit within `MAX_MSG_SIZE` (250 bytes, ESPNOW limit)
- Stepper angle↔step conversion: zero, full/half revolution, negatives, fractional angles, truncation behavior

**PC Unit Tests (17 tests):**
- WebSocket JSON contract: `WsCommand`/`WsEvent` round-trips through `serde(tag = "type")` tagging
- `SweepConfig` validation (zero step, reversed range, zero samples)
- COBS serial framing: single frame decode, fragmented frame reassembly, multiple frames in one read
- Sweep state machine: data collection, error propagation, disconnect handling, 30s timeout, subsequent sweeps reuse the serial channel
- CSV export

**Integration Tests (2 tests, requires `--features sim`):**
- `full_sweep_via_tcp` — starts in-process rx-sim, connects via TCP, runs a sweep, verifies data points match the cardioid formula, checks CSV export
- `sweep_stop_via_tcp` — starts a long sweep, sends abort, verifies clean cancellation

### RX Simulator

The `rx-sim` binary (and `sim` module) simulates the RX board over TCP. It implements the full COBS framing protocol and generates synthetic RSSI data using a cardioid pattern: `rssi = -30 + 20 * cos(angle - 45°)`.

Used for:
- Manual UI testing without hardware (`just sim`)
- Automated integration tests (in-process, random port)

## Phase 2: Wideband Measurements (ADF4351 + AD8318)

Phase 1 uses ESPNOW RSSI (integer dBm, ~30 dB range, 2.4 GHz only). Phase 2 adds dedicated RF hardware for calibrated, wideband measurements at any frequency from 35 MHz to 4.4 GHz.

### Hardware

| Component | Module | Interface | Connected to |
|-----------|--------|-----------|-------------|
| ADF4351 | PLL synthesizer breakout (35 MHz – 4.4 GHz) | SPI (CLK, DAT, LE) | TX board (ESP32-C3) |
| AD8318 | Log detector breakout (1 MHz – 8 GHz, 60 dB range) | Analog voltage → ADC | RX board (ESP32-C3) |

**ADF4351 → TX board wiring:**

| ESP32-C3 | ADF4351 | Function |
|----------|---------|----------|
| GPIO4 | CLK | SPI clock |
| GPIO5 | DAT | SPI data (MOSI) |
| GPIO6 | LE | Latch enable (CS) |
| GND | GND | Ground |

**AD8318 → RX board wiring:**

| ESP32-C3 | AD8318 | Function |
|----------|--------|----------|
| GPIO2 (ADC) | VOUT (via voltage divider if needed) | Detected RF power |
| GND | GND | Ground |

### Architecture

ESPNOW remains the command/control channel between boards (discovery, frequency configuration, move commands). The RF measurement path is separate:

```
TX board → ADF4351 → SMA → antenna (AUT) → air → RX antenna → SMA → AD8318 → ADC → RX board
```

Both measurement modes coexist: `frequency_hz: None` in sweep config uses Phase 1 ESPNOW RSSI, `frequency_hz: Some(freq)` uses ADF4351 + AD8318.

### Calibration

AD8318 outputs a voltage proportional to input power in dBm (slope ~-24 mV/dB). Initial implementation uses datasheet nominal values. Per-module calibration (slope + intercept) can be added later for ±1 dB accuracy.
