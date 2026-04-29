# Beambench Architecture v2

## Overview

This document describes the planned architecture changes to beambench:

1. **Bridge-based topology** — a dedicated USB-connected ESP32-C3 relays between PC and wireless field devices
2. **PC-driven orchestration** — all campaign planning and sweep control lives on the PC; field devices only execute commands
3. **Unified firmware** — a single binary for all ESP32-C3 devices, with role stored in flash
4. **ESP-NOW OTA** — over-the-air firmware updates pushed from PC through the Bridge

## Device Topology

```
PC (Axum + Svelte)
  | USB-serial (COBS)
Bridge (ESP32-C3, USB powered)
  | ESP-NOW
  +-------+-------+
  |       |       |
  RX    Stepper   TX
(battery) (mains) (battery)
```

- **Bridge**: serial-to-ESP-NOW relay. Receives COBS-framed commands from the PC, routes them to the addressed device via ESP-NOW, and forwards responses back over serial.
- **RX**: measures RSSI (or other RF metrics) from TX packets. Accumulates samples passively, reports on demand.
- **TX**: transmits `MeasurementBeacon` packets at a configured rate when told to.
- **Stepper**: moves the turntable to commanded positions, reports completion.

All field devices (RX, TX, Stepper) are passive — they act only when instructed by the PC via the Bridge. The Bridge itself has no orchestration logic.

**Exception — MeasurementBeacon**: TX broadcasts `MeasurementBeacon` packets directly via ESP-NOW broadcast (not routed through the Bridge). RX receives these over the air and reads `rx_control.rssi` from the direct TX->RX RF path. This is the only device-to-device communication that bypasses the Bridge. Broadcast means no MAC-level ACK, but packet loss is acceptable since RSSI samples are averaged. Bridge and Stepper receive these packets too but ignore them.

---

## Phase 1: Unified Firmware + Role Provisioning

### Goal

Replace the four separate firmware crates (`rx`, `tx`, `stepper`, and the new `bridge`) with a single crate. Each physical device reads its role from a dedicated flash region.

### Role Storage

- A custom partition `role` (type `data`, subtype `nvs` or raw, 4 KB) in the partition table
- Stores a single `Role` enum value: `Bridge`, `Rx`, `Tx`, `Stepper`
- The `Role` enum must use explicit stable discriminants (`#[repr(u8)]` with fixed values) so that the stored byte survives firmware updates that might reorder or add variants
- Read at boot before any peripherals are initialized

### Cargo Features for Provisioning

```
[features]
role-bridge  = []
role-rx      = []
role-tx      = []
role-stepper = []
```

- **Feature set** (e.g. `--features role-tx`): firmware writes that role to the flash partition at boot, then proceeds to run as that role. This is the provisioning step — done once per device (or to re-provision).
- **No feature set**: firmware reads the role from flash. If the partition is empty/invalid, the device panics with a defmt error message indicating it needs provisioning.
- At most one `role-*` feature may be active (enforced with a compile-time check).

### Runtime Dispatch

After role resolution, the firmware branches into the role-specific task set:

```rust
match role {
    Role::Bridge  => bridge::run(spawner, ...).await,
    Role::Rx      => rx::run(spawner, ...).await,
    Role::Tx      => tx::run(spawner, ...).await,
    Role::Stepper => stepper::run(spawner, ...).await,
}
```

Each `run()` function spawns the Embassy tasks for that role. Shared code (ESP-NOW init, discovery, LED control, heartbeat) lives in common modules.

### Justfile Targets

```
just flash-rx       # cargo run --features role-rx
just flash-tx       # cargo run --features role-tx
just flash-stepper  # cargo run --features role-stepper
just flash-bridge   # cargo run --features role-bridge
just flash          # cargo run (no feature = use stored role)
```

---

## Phase 2: Bridge + PC Orchestration

### Bridge Firmware

The Bridge is a transparent relay with two jobs:

1. **Serial -> ESP-NOW**: Decode COBS frames from USB-serial, deserialize `PcCommand`, look up the target device's MAC from the discovery table, serialize the inner command as an `EspnowMessage`, and send via ESP-NOW.
2. **ESP-NOW -> Serial**: Receive `EspnowMessage` from any paired device, wrap it as a `DeviceEvent`, COBS-encode, and send over USB-serial.

The Bridge maintains an auto-discovery table mapping `Role -> MAC` (one device per role). It pairs with field devices using the existing `Hello`/`PairConfirm` beacon protocol.

### Protocol Changes

#### Serial Protocol (PC <-> Bridge)

The current `PcToRx` / `RxToPc` enums are replaced with role-addressed messages:

```rust
/// PC -> Bridge (serial)
enum PcCommand {
    // TX control
    ConfigureTx { channel: u8, tx_power_dbm: i8, packet_rate_hz: u16 },
    StartTransmitting,
    StopTransmitting,

    // Stepper control
    MoveTo { angle_deg: f32 },
    StopStepper,
    ReturnHome,

    // RX control
    StartMeasurement,
    ReportMeasurement,

    // System
    QueryStatus,
    /// Initiate OTA update for a target role (Phase 3)
    OtaBegin { target: Role, total_size: u32, sha256: [u8; 32] },
    OtaData { seq: u16, chunk: Vec<u8> },  // heapless on firmware side
    OtaFinish,
}

/// Bridge -> PC (serial)
enum DeviceEvent {
    // TX responses
    TxAck,
    TxStatus { transmitting: bool, channel: u8, tx_power_dbm: i8, packet_rate_hz: u16 },

    // Stepper responses
    MoveComplete { angle_deg: f32 },
    StepperError { description: String<64> },
    HomeComplete,

    // RX responses
    Measurement { rssi_dbm: f32, sample_count: u16 },

    // System
    Status { tx_connected: bool, rx_connected: bool, stepper_connected: bool },
    Error { description: String<128> },
}
```

The Bridge translates between these serial messages and the existing `EspnowMessage` variants, routing by role.

#### ESP-NOW Protocol Changes

Add a new message variant for measurement beacons and RX measurement commands:

```rust
enum EspnowMessage {
    // Existing
    Hello(HelloBeacon),
    PairConfirm(PairConfirm),
    TurntableCmd(TurntableCommand),
    TurntableResp(TurntableResponse),
    TxCmd(TxCommand),
    TxResp(TxResponse),

    // New
    /// Sent by TX at configured rate. RX accumulates RSSI from these
    /// only when actively measuring.
    MeasurementBeacon,
    /// Bridge -> RX: reset accumulator and start collecting.
    StartMeasurement,
    /// Bridge -> RX: stop collecting, report result.
    ReportMeasurement,
    /// RX -> Bridge: measurement result.
    MeasurementResult { rssi_dbm: f32, sample_count: u16 },
}
```

### Sweep Orchestration (PC Side)

The PC app replaces the current `SweepStateMachine` (which sent a single `StartSweep` to RX) with a step-by-step orchestrator:

```
1. PC -> Bridge -> TX:      ConfigureTx { ... }
2. PC -> Bridge -> TX:      StartTransmit
3. for each angle:
   a. PC -> Bridge -> Stepper:  MoveTo { angle_deg }
   b. wait for MoveComplete
   c. settling delay (configurable)
   d. PC -> Bridge -> RX:       StartMeasurement  (resets accumulator, starts collecting)
   e. measurement window (configurable)
   f. PC -> Bridge -> RX:       ReportMeasurement (returns result, stops collecting)
   g. record data point
4. PC -> Bridge -> TX:      StopTransmit
5. PC -> Bridge -> Stepper: ReturnHome
6. wait for HomeComplete
```

RX ignores `MeasurementBeacon` packets except between `StartMeasurement` and `ReportMeasurement`, ensuring a clean measurement window with no stale data from rotation.

This gives the PC full control over timing. During steps 3a-3f, the PC ensures no other ESP-NOW commands are in flight (the Bridge sends one command at a time and waits for the response before accepting the next).

### RX Firmware (Simplified)

RX's responsibilities shrink to:

- On `StartMeasurement` from Bridge: reset RSSI accumulator and begin collecting from `MeasurementBeacon` packets (ignored at all other times)
- On `ReportMeasurement` from Bridge: compute average, reply with `MeasurementResult`, stop collecting
- Discovery beacons (shared code)

No serial handling, no sweep state machine, no turntable control.

### Discovery

All field devices broadcast `Hello { role, mac }` beacons periodically until paired. The Bridge listens for these and builds its role -> MAC table, sending `PairConfirm` to each. The PC can query connection status via `QueryStatus`.

Heartbeat-based disconnect detection (already implemented for TX) is generalized: the Bridge periodically pings each paired device, and devices unpair from the Bridge if they haven't heard from it within a timeout.

---

## Phase 3: ESP-NOW OTA

### Partition Table

All devices use a shared partition table with dual OTA slots:

```
# Name,    Type, SubType, Offset,  Size
nvs,       data, nvs,     0x9000,  0x6000
role,      data, nvs,     0xf000,  0x1000
otadata,   data, ota,     0x10000, 0x2000
ota_0,     app,  ota_0,   0x20000, 0x1C0000
ota_1,     app,  ota_1,   0x1E0000,0x1C0000
```

Each OTA slot is ~1.75 MB, sufficient for the unified binary.

### OTA Protocol

The OTA flow is modeled after [Espressif's ESP-NOW OTA example](https://components.espressif.com/components/espressif/esp-now/versions/2.1.1/examples/ota), adapted for the Rust/esp-hal stack:

#### Initiator (Bridge, commanded by PC)

1. PC sends `OtaBegin { target: Role, total_size, sha256 }` over serial
2. Bridge looks up the target device's MAC, sends `EspnowOtaBegin { total_size, sha256 }` via ESP-NOW
3. Target responds with `EspnowOtaReady`
4. PC streams `OtaData { seq, chunk }` over serial (chunk size <= 240 bytes to fit ESP-NOW payload)
5. Bridge forwards each chunk as `EspnowOtaData { seq, data }` to the target
6. Target ACKs each chunk with `EspnowOtaAck { seq }` (Bridge relays ACK to PC for flow control)
7. On missing ACK, Bridge retransmits (configurable retry count)
8. PC sends `OtaFinish`, Bridge sends `EspnowOtaFinish`
9. Target verifies SHA-256 over the full image, marks the new OTA slot as bootable, responds with `EspnowOtaComplete`, and reboots

#### Responder (All Field Devices)

Implemented as a shared module in the unified firmware:

1. On `EspnowOtaBegin`: erase the inactive OTA partition, respond `EspnowOtaReady`
2. On `EspnowOtaData { seq, data }`: write chunk at `seq * chunk_size` offset, respond `EspnowOtaAck { seq }`
3. On `EspnowOtaFinish`: compute SHA-256 over written data, compare with expected hash
   - Match: set boot partition to the newly written slot, respond `EspnowOtaComplete`, reboot after 2s
   - Mismatch: respond `EspnowOtaError`, stay on current partition

#### ESP-NOW OTA Messages

```rust
enum EspnowMessage {
    // ... existing variants ...

    // OTA
    OtaBegin { total_size: u32, sha256: [u8; 32] },
    OtaReady,
    OtaData { seq: u16, data: heapless::Vec<u8, 240> },
    OtaAck { seq: u16 },
    OtaFinish,
    OtaComplete,
    OtaError { description: String<64> },
}
```

### PC Side

- `just ota-rx` / `just ota-tx` / `just ota-stepper`: build the unified firmware, then stream it to the Bridge with the target role specified
- Progress bar and retry stats displayed in the terminal
- Frontend could show OTA status for connected devices (stretch goal)

### Rollback

If the new firmware fails to boot (e.g., panic before reaching the "mark as valid" checkpoint), the ESP-IDF bootloader automatically rolls back to the previous OTA slot. The firmware should call the "mark current partition valid" API only after successful initialization (ESP-NOW up, discovery beacons sending).

---

## Phase 2+ : Extensibility for Other Frequencies

The architecture supports future measurement hardware beyond 2.4 GHz ESP-NOW RSSI:

- The `MeasurementBeacon` / `ReportMeasurement` / `MeasurementResult` pattern is generic. RX reports whatever its measurement backend produces.
- RX could be extended with an SPI/I2C-connected RF frontend (sub-GHz, 5 GHz, mmWave). The `ReportMeasurement` handler reads from the appropriate backend.
- Alternatively, a completely separate measurement device could be queried directly by the PC (e.g., USB SDR, spectrum analyzer via SCPI), bypassing the ESP-NOW mesh entirely. The PC orchestrator already controls timing, so it can interleave ESP-NOW commands with direct instrument queries.
- The `MeasurementResult` type could be extended to include metadata (frequency, bandwidth, measurement type) when multiple backends are supported.

---

## Implementation Order

```
Phase 1: Unified firmware + role provisioning
   |
Phase 2: Bridge relay + PC orchestration refactor
   |      (serial protocol, sweep orchestrator, simplified RX)
   |
Phase 3: ESP-NOW OTA
   |      (dual OTA partitions, chunked transfer, rollback)
   |
Future:  Multi-frequency measurement backends
```

Phase 1 is prerequisite for Phase 3 (single binary = single OTA image).
Phase 2 is prerequisite for Phase 3 (Bridge is the OTA initiator).
Phases 1 and 2 can be developed concurrently to some degree — the protocol changes in Phase 2 are independent of the firmware unification in Phase 1.
