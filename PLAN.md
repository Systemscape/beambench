# Beambench — Antenna Pattern Measurement System

## Overview

Automated antenna pattern measurement using ESP32-C3-DevKit-RUST-1 boards,
a motorized turntable, and a PC application for real-time visualization.

Phase 1: 2.4 GHz patterns using ESPNOW RSSI.
Phase 2: Wideband measurements using ADF4351 (TX) + AD8318 (RX).

## Architecture

Three ESP32-C3-DevKit-RUST-1 boards, all communicating via ESPNOW,
coordinated by a PC application over USB-serial:

```
                                                            ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐
                                                            │ Turntable (rotating)│
                                                            │                     │
┌─────────────┐   USB-serial    ┌─────────────┐   ESPNOW    │ ┌─────────────┐     │
│  PC App     │◄──(postcard/───►│  RX Board   │◄───────────►  │  TX Board   │     │
│             │    COBS)        │  (USB to PC)│             │ │  (battery)  │     │
└─────────────┘                 │             │             │ └─────────────┘     │
                                │             │   ESPNOW    │        │            │
                                │             │◄──────────┐ │  ┌─────┴─────┐      │
                                └─────────────┘           │ │  │  Antenna  │      │
                                   (fixed)                │ │  │   (AUT)   │      │
                                                          │ │  └───────────┘      │
                                                          │ └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘
                                                          │
                                                     ┌────┴────────┐
                                                     │  Turntable  │
                                                     │   Board     │
                                                     │  (battery)  │
                                                     └──────┬──────┘
                                                            │  (stationary,
                                                            │   near turntable)
                                                     ┌──────┴──────┐
                                                     │ MKS Stepper │
                                                     └─────────────┘
```

The TX board and antenna under test (AUT) rotate together on the turntable.
The turntable controller board is stationary (mounted near the turntable base,
not on the rotating platform). The RX board is fixed at a distance.

### RX Board (Central Coordinator)

- Connected to PC via USB-serial (postcard/COBS binary protocol)
- Measures RSSI from TX board's ESPNOW packets (phase 1)
- Reads AD8318 RF power detector (phase 2)
- Sends move commands to turntable board via ESPNOW
- Sends configuration to TX board via ESPNOW
- Reports measurement data and errors back to PC

### TX Board (Transmitter)

- Battery-powered, mounted on turntable with the AUT
- Configured by RX board via ESPNOW (channel, power, packet rate)
- Transmits ESPNOW packets for RSSI measurement (phase 1)
- Drives ADF4351 signal generator (phase 2)

### Turntable Board (Motor Controller)

- Battery/powerbank-powered, stationary near turntable base
- Controls MKS stepper motor (SERVO42C via UART or SERVO42D via step/dir)
- Receives move commands (in degrees) from RX board via ESPNOW
- Translates degrees to microsteps internally (motor steps/rev and
  microstepping divisor defined as firmware constants)
- Sends "move complete" acknowledgement back to RX
- Servo42C backend: uses absolute encoder position (user must calibrate
  zero once via the motor's menu, see stepper README)
- Step/dir backend: assumes 0° at power-on (manual alignment)
- Uses Embassy async executor to handle ESPNOW and motor control concurrently

### PC Application

- Configures measurement campaigns (start/stop angle, step size)
- Communicates with RX board over USB-serial (postcard + COBS framing)
- Live polar plot of antenna pattern during measurement
- Data export: CSV, PNG/SVG image
- Displays errors reported by RX (timeouts, communication failures)

## Physical Setup

The AUT is mounted on the turntable and rotates. The TX board is
co-located on the turntable, feeding the AUT (or the AUT is the TX
board's own PCB antenna). Both rotate together.

The RX board is placed at a fixed position at a distance from the
turntable, with its antenna pointed at the AUT. The turntable board
sits next to the turntable base (stationary) and drives the motor.

## ESPNOW Protocol

### Discovery Phase

1. All boards power on and broadcast periodic "hello" beacons containing
   their role (TX / RX / turntable) and MAC address.
2. When RX discovers TX and turntable, it registers their MACs as ESPNOW
   unicast peers and sends a "paired" confirmation.
3. TX and turntable register RX's MAC as their peer.
4. Discovery re-triggers if a peer is lost (heartbeat timeout).

### Operational Phase (unicast, hardware ACKs)

Message types (postcard-serialized, same `protocol` crate):

**RX → Turntable**:
- `MoveTo { angle_deg: f32 }` — move to absolute angle
- `Stop` — emergency stop

**Turntable → RX**:
- `MoveComplete { angle_deg: f32 }` — reached target position
- `Error { code, message }` — motor or communication error

**RX → TX**:
- `Configure { channel, tx_power, packet_rate_hz }` — set transmission parameters
- `StartTransmit` / `StopTransmit` — begin/end packet transmission

**TX → RX**:
- ESPNOW data packets (RX reads RSSI from the received frame metadata,
  not from the packet payload)
- `Status { ... }` — battery level, current config

### Radio Discipline During Measurement

At each angle step:
1. RX sends `MoveTo` to turntable (unicast).
2. Turntable moves, sends `MoveComplete` back (unicast).
3. **Guard interval**: RX waits a short period (e.g. 10 ms) after
   receiving `MoveComplete` to let the channel settle.
4. **Measurement window**: RX collects N RSSI samples from TX packets.
   During this window, the turntable board is radio-silent (no
   transmissions). The turntable board enforces this by not initiating
   any ESPNOW sends between `MoveComplete` and the next `MoveTo`.
5. After collecting enough samples, RX sends the next `MoveTo`, which
   implicitly ends the turntable's radio-silent period.

This sequencing is enforced by the RX coordinator — it never sends
`MoveTo` while a measurement window is active, and never measures
while waiting for `MoveComplete`.

## Measurement Flow

1. PC sends sweep configuration to RX (start angle, stop angle, step size,
   samples per angle)
2. RX sends TX configuration via ESPNOW (channel, power, packet rate)
3. RX commands TX to start transmitting
4. For each angle step:
   a. RX sends `MoveTo` to turntable via ESPNOW
   b. Turntable moves to target angle
   c. Turntable sends `MoveComplete` to RX via ESPNOW
   d. RX waits guard interval, then measures RSSI (averages N packets)
   e. RX sends data point (angle, RSSI) to PC via serial
   f. PC updates live polar plot
5. RX commands TX to stop transmitting
6. PC saves complete dataset

**Error handling**: All commands have timeouts. If turntable doesn't
respond within a timeout, RX retries once, then reports an error to
the PC. If too few RSSI samples are received at a given angle, RX
reports a partial measurement with a warning. The PC displays all
errors in its UI and the user decides whether to abort or continue.

## Serial Protocol (PC ↔ RX)

Binary protocol using `postcard` serialization with COBS framing over USB-serial.

Message types (both directions):
- **PC → RX**: `StartSweep { start_deg, stop_deg, step_deg, samples_per_angle }`,
  `ConfigureTx { ... }`, `Stop`, `QueryStatus`
- **RX → PC**: `DataPoint { angle_deg, rssi_dbm, sample_count }`,
  `SweepComplete`, `Error { description }`, `Status { ... }`

## Repository Layout

Separate crates (not a Cargo workspace, since firmware and PC targets differ).
Each crate has a `justfile` for build/flash/run commands.

```
beambench/
  PLAN.md
  README.md
  justfile                (top-level: delegates to sub-crates)
  software/
    stepper/              (turntable motor control + ESPNOW — exists)
      justfile
    tx/                   (TX firmware)
      justfile
    rx/                   (RX firmware — coordinator)
      justfile
    pc/                   (PC application — Axum + Svelte)
      justfile
    protocol/             (shared message types, no_std lib)
      justfile
```

Each firmware crate has its own `.cargo/config.toml` targeting
`riscv32imc-unknown-none-elf` with `build-std`. The `pc` crate
targets the host. The `protocol` crate is a `no_std` library
depended on by all others via path dependency.

### Shared `protocol` crate

`no_std`-compatible library defining all message types shared between
firmware crates and the PC application. Uses `serde` + `postcard` for
serialization. Defines messages for both the serial protocol (PC ↔ RX)
and the ESPNOW protocol (RX ↔ TX, RX ↔ turntable).

### Justfiles

**Top-level** (`beambench/justfile`):
```just
# Build all firmware
build-firmware:
    just software/stepper/build
    just software/tx/build
    just software/rx/build

# Build PC application (backend + frontend)
build-pc:
    just software/pc/build

# Check all crates compile
check:
    just software/protocol/check
    just software/stepper/check
    just software/tx/check
    just software/rx/check
    just software/pc/check
```

**Firmware crates** (`stepper/justfile`, `tx/justfile`, `rx/justfile`):
```just
# Build firmware
build:
    cargo build --release

# Build and flash to connected board
flash:
    cargo run --release

# Check without flashing
check:
    cargo check
```

**PC application** (`pc/justfile`):
```just
# Development: run Axum backend + Vite dev server concurrently
dev:
    #!/usr/bin/env bash
    cd frontend && npm run dev &
    VITE_PID=$!
    cargo run -- --dev
    kill $VITE_PID 2>/dev/null

# Build for release: compile Svelte to static files, build Rust binary
build:
    cd frontend && npm run build
    cargo build --release

# Run release build (serves embedded frontend)
run:
    cargo run --release

# Check Rust code only
check:
    cargo check
```

**Protocol** (`protocol/justfile`):
```just
check:
    cargo check
    cargo check --target riscv32imc-unknown-none-elf
```

## PC Application

**Stack: Axum + Svelte SPA** with Plotly.js for polar plotting.

Rust backend (Axum) handles serial communication, sweep state machine,
and data storage. Svelte frontend connects via WebSocket for real-time
data push and renders the polar plot with Plotly.js (`Scatterpolar`).

The core application logic (serial comms, sweep orchestration, data
management, CSV export) lives in a library crate (`pc/src/lib.rs`)
with no GUI dependency. The Axum server and Svelte frontend are a
thin presentation layer on top. This separation allows switching to
a different GUI stack (egui, Slint, Tauri) later without rewriting
the measurement logic.

### Build Modes

- **Development** (`just dev`): Vite dev server (HMR, fast iteration)
  runs alongside the Axum backend. Vite proxies `/api` and `/ws`
  requests to Axum. Two processes, managed by the justfile.
- **Release** (`just build`): Svelte compiles to static files in
  `frontend/build/`. Axum embeds them via `rust-embed` and serves
  them directly. Single binary output, no Node.js needed at runtime.

### Layout

```
pc/
  justfile
  Cargo.toml
  src/
    lib.rs          (core: serial protocol, sweep state, data store, export)
    main.rs         (Axum server: REST + WebSocket, serves embedded frontend)
  frontend/         (SvelteKit SPA with Plotly.js)
    src/
      routes/
      lib/
    package.json
    vite.config.ts  (proxy /api and /ws to Axum in dev mode)
```

## RSSI Measurement Limitations

ESP32-C3 ESPNOW RSSI is integer dBm with approximately ±6 dB accuracy
and limited dynamic range (~30 dB usable). This is sufficient for
characterizing gross antenna pattern features (main lobe direction,
null locations, front-to-back ratio) but not for precision measurements.

Phase 2 replaces RSSI with dedicated RF hardware (ADF4351 signal
generator + AD8318 logarithmic power detector) for calibrated,
wideband measurements with >60 dB dynamic range.

## Technology Stack

- **Language**: Rust (nightly, for ESP32-C3 `build-std`)
- **Target**: `riscv32imc-unknown-none-elf` (ESP32-C3)
- **HAL**: esp-hal 1.0.0
- **Async**: Embassy executor (firmware)
- **Logging**: defmt + probe-rs
- **Wireless**: ESPNOW (via esp-wifi)
- **Serial protocol**: postcard + COBS
- **Motor control**: mks-servo42-rs (UART) or GPIO step/dir
- **PC backend**: Axum (HTTP/WebSocket server)
- **PC frontend**: Svelte + Plotly.js (polar chart)

## Implementation Phases

### Phase 0 — Turntable Motor Control (done)

- [x] Step/dir backend for MKS SERVO42D
- [x] Motor trait with position tracking
- [x] Servo42C UART backend (compiles, untested on hardware)
- [x] Feature-gated backend selection

### Phase 1 — ESPNOW RSSI at 2.4 GHz

1. Create `protocol` crate with shared message types (serial + ESPNOW)
2. Refactor stepper firmware to Embassy async, add ESPNOW peer discovery
   and command handling (non-blocking motor control)
3. TX firmware: ESPNOW peer discovery, configurable packet transmission
4. RX firmware: ESPNOW coordinator (discover peers, send commands),
   RSSI measurement, USB-serial bridge to PC
5. PC application: configure sweep, live polar plot, CSV/image export,
   error display
6. Integration test: full sweep with all three boards

### Phase 2 — Wideband Measurements

1. Add ADF4351 SPI driver to TX firmware
2. Add AD8318 ADC reading to RX firmware
3. Extend protocol for frequency sweep parameters
4. Update PC app for frequency-domain visualization
