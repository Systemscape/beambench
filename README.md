
## Setup
Three ESP32-S3 modules connected via ESPNOW:

Motor Controller (#1): Drives the stepper motor, orchestrates the measurement sequence. Connected to PC via USB serial.
Transmitter (#2): Sits on the turntable with the antenna under test. Battery-powered, no wires.
Receiver (#3): Fixed position at a known distance. Receives packets from #2, measures RSSI. Connected to PC via USB serial.

The PC coordinates the process: step the motor to an angle, trigger a burst of packets, collect RSSI, repeat. The result is a polar radiation pattern of the antenna under test.

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
              ║
              ║
              ║
              ║
        ┌─────╩─────┐
        │  stepper  │
        │  motor    │
        └─────┬─────┘
              ╫ wired
              ╫
        ┌─────┴─────┐         ESPNOW         ┌───────────┐
        │  ESP #1   │                        │  ESP #3   │
        │  motor    │  ◄ ─ ─ ─ ─ ─ ─ ─ ─ ─ ► │  receiver │
        │  ctrl.    │                        │           │
        └─────┬─────┘                        └─────┬─────┘
              │ USB                                │ USB
        ┌─────┴────────────────────────────────────┴─────┐
        │                      PC                        │
        └────────────────────────────────────────────────┘
