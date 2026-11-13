# Beambench Architecture

## Topology

```
PC (Axum + Svelte)
  | USB-serial: COBS + postcard
Bridge (ESP32-C3, USB powered)
  | ESP-NOW: postcard
  +--------+-----------+
  |        |           |
  TX       RX          Turntable
```

| Device | Job |
|---|---|
| PC | Plans and runs sweeps, stores data, serves the UI |
| Bridge | Relays between serial and ESP-NOW, routes by role, no sweep logic |
| TX | Broadcasts `MeasurementBeacon` at a configured rate while transmitting |
| RX | Averages the RSSI of beacons between `StartMeasurement` and `ReportMeasurement` |
| Turntable | Moves the step/dir motor to an angle, replies `MoveComplete` |

Field devices only act on commands from the Bridge.
Exception: TX beacons go by ESP-NOW broadcast directly to RX. They are not acknowledged; losses only reduce the sample count.

## Crates

| Crate | Content |
|---|---|
| `protocol` | `no_std` message types, serialization, `bridge_logic`, `tx_logic`, `rx_logic` |
| `firmware` | One binary for all roles |
| `pc` | Backend, frontend, bridge simulator (`--features sim`) |

The routing and state machines in `protocol` run on the firmware and in the simulator.

## Protocol

| Link | Types | Framing |
|---|---|---|
| PC → Bridge | `PcCommand` | COBS + postcard |
| Bridge → PC | `DeviceEvent` | COBS + postcard |
| Bridge ↔ field devices | `EspnowMessage` | postcard in one ESP-NOW frame (≤ 250 bytes) |

`bridge_logic::route_command` maps a `PcCommand` to a target role and an `EspnowMessage`, or handles it locally (`QueryStatus`).
`bridge_logic::translate_response` maps the reply back to a `DeviceEvent`.

### Versioning

- Every frame starts with `PROTOCOL_VERSION` (`u16`, varint).
- Receivers read only this header first. Frames of another version are not decoded.
- The Bridge reports boards with another version in `DeviceEvent::Status { mismatched_peers }`.
- The PC reports a Bridge with another version as an error.
- `protocol/src/golden.rs` pins the bytes of every variant. A layout change fails the test; then bump `PROTOCOL_VERSION`.

## Firmware

### Roles

The role is stored in the `role` partition at `0xF000` (4 bytes magic, 1 byte role).

- Built with `role-<name>`: writes this role at boot, then runs it. Used once per board (`just flash-<role>`).
- Built without a role feature: reads the role from flash. Panics if none is stored.
- At most one `role-*` feature is allowed (`compile_error!`).
- `Role` has fixed `#[repr(u8)]` discriminants, so the stored byte stays valid across updates.

`main.rs` dispatches to `bridge::run`, `rx::run`, `tx::run` or `turntable::run`. Each spawns its Embassy tasks.

### Features

| Feature | Effect |
|---|---|
| `role-bridge`, `role-rx`, `role-tx`, `role-turntable` | Provision this role |
| `step-dir` (default) | Step/dir motor backend for the turntable |
| `backend-uart` | RX/TX measure via an external board on UART instead of ESP-NOW RSSI |

### Discovery and heartbeat

- Devices broadcast `Hello { role, mac }` every second until paired, then every 10 s as heartbeat.
- The Bridge stores one MAC per role and answers with `PairConfirm`.
- Peers not heard from for 15 s are dropped, on both sides.

### Partition table

```
# Name,    Type, SubType, Offset,   Size
nvs,       data, nvs,     0x9000,   0x6000
role,      data, phy,     0xf000,   0x1000
otadata,   data, ota,     0x10000,  0x2000
ota_0,     app,  ota_0,   0x20000,  0x1C0000
ota_1,     app,  ota_1,   0x1E0000, 0x1C0000
```

## Sweep (PC)

```
1. StartTransmitting           wait up to 2 s for TxAck
2. per angle:
   a. MoveTo { angle_deg }     wait for MoveComplete (30 s timeout)
   b. settle 200 ms
   c. StartMeasurement
   d. measurement window
   e. ReportMeasurement        wait for Measurement (5 s timeout)
3. StopTransmitting
```

The PC sends one command at a time and waits for its reply.
`ConfigureTx` and `ReturnHome` are separate UI commands.

## OTA

The PC streams the image through the Bridge to one field device.

1. `OtaBegin { target, total_size, sha256, ack_interval }`. The target erases the inactive slot and replies `OtaReady`.
2. `OtaData { seq, data }` in chunks of up to 240 bytes. The target writes each chunk and sends `OtaAck` every `ack_interval` chunks and on the last one.
3. The PC retries a window up to 3 times on timeout, then aborts.
4. `OtaFinish`. The target checks SHA-256, switches the boot slot, replies `OtaComplete` and reboots.

After a successful boot the firmware marks its slot as valid. If it fails before that, the bootloader rolls back.
`just ota-<role>` builds the image and uploads it via the PC app's REST API.
