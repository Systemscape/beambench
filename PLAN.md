    Beambench — Complete Project Reference

     Everything an LLM (or developer) needs to know to continue working on this project.

     ---
     What Is Beambench?

     An automated antenna radiation pattern measurement system. A turntable rotates an antenna under test (AUT) through 360°, while a transmitter sends RF signals and a receiver measures signal strength at each angle.
     Results are displayed as a polar plot in a web UI.

     Three ESP32-C3 boards communicate over ESPNOW (Wi-Fi direct, no router):
     - TX — transmits RF signal (currently ESPNOW packets; Phase 2: ADF4351 synthesizer)
     - RX — central coordinator: receives signal, measures RSSI, commands turntable, bridges to PC over USB-serial
     - Stepper — turntable motor controller (step/dir or UART servo)

     PC app — Axum web server + SvelteKit frontend, connects to RX over USB-serial (COBS-framed postcard), displays live polar plot.

     ---
     Repository Structure

     beambench/
     ├── justfile                          # Top-level: delegates to per-crate justfiles
     ├── README.md                         # Hardware wiring, build instructions, architecture
     └── software/
         ├── protocol/                     # Shared no_std message types (serde + postcard)
         │   ├── Cargo.toml
         │   └── src/lib.rs
         ├── stepper/                      # Turntable motor controller firmware
         │   ├── Cargo.toml
         │   ├── justfile
         │   ├── .cargo/config.toml
         │   └── src/
         │       ├── main.rs
         │       └── motor/
         │           ├── mod.rs            # Motor trait + Position type
         │           ├── step_dir.rs       # GPIO step/dir/enable backend (default)
         │           └── servo42c.rs       # MKS SERVO42C UART backend
         ├── tx/                           # TX beacon firmware
         │   ├── Cargo.toml
         │   ├── justfile
         │   ├── .cargo/config.toml
         │   └── src/main.rs
         ├── rx/                           # RX coordinator firmware
         │   ├── Cargo.toml
         │   ├── justfile
         │   ├── .cargo/config.toml
         │   └── src/main.rs
         └── pc/                           # PC application (Axum + SvelteKit)
             ├── Cargo.toml
             ├── justfile
             ├── src/
             │   ├── main.rs              # Axum server, WebSocket handler, static file serving
             │   ├── lib.rs               # Public types: DataPoint, SweepConfig, WsEvent, WsCommand
             │   ├── serial.rs            # SerialHandle: USB-serial + TCP transport, COBS framing
             │   ├── sweep.rs             # Sweep state machine (send config, collect data points)
             │   ├── sim.rs               # Cardioid pattern simulator (test/sim feature only)
             │   └── bin/rx-sim.rs        # Standalone TCP RX simulator binary
             └── frontend/
                 ├── package.json          # SvelteKit 2, Svelte 5, Vite 7, Plotly.js 3
                 └── src/
                     ├── lib/ws.ts         # WebSocket client with auto-reconnect
                     └── routes/+page.svelte  # Single-page app: controls + polar plot

     Not a Cargo workspace — firmware targets riscv32imc-unknown-none-elf, PC targets host. Each crate builds independently.

     ---
     Technology Stack

     Firmware (all three boards)

     - Target: riscv32imc-unknown-none-elf (ESP32-C3-DevKit-RUST-1)
     - HAL: esp-hal 1.0.0 with unstable + defmt features
     - RTOS: esp-rtos 0.2.0 (Embassy async executor)
     - Radio: esp-radio 0.17.0 (ESP-NOW, not full Wi-Fi)
     - Logging: defmt 1.0.1 + rtt-target 0.6.2 (via probe-rs)
     - LED: esp-hal-smartled2 0.28.1 (SK6812 on GPIO2 via RMT peripheral)
     - Build: build-std = ["core", "alloc"] required (esp-radio needs heap)
     - Flash/run: probe-rs run --chip esp32c3

     PC Application

     - Backend: Axum 0.8 + Tokio, WebSocket for real-time events
     - Serial: tokio-serial 5.4 at 115200 baud, COBS-framed postcard messages
     - Frontend: SvelteKit 2 (Svelte 5 with $state() runes), Plotly.js 3 polar plot
     - Embedding: rust-embed bundles built frontend into release binary
     - Package manager: pnpm (not npm)

     Protocol

     - Serialization: postcard 1 (compact no_std serde)
     - ESPNOW framing: Raw postcard bytes (≤250 byte payload limit: MAX_MSG_SIZE)
     - Serial framing: COBS (Consistent Overhead Byte Stuffing) — 0x00 delimiter between frames

     ---
     Protocol Messages (software/protocol/src/lib.rs)

     ESPNOW layer (between ESP32 boards)

     enum EspnowMessage {
         Hello(HelloBeacon),           // Discovery broadcast
         PairConfirm(PairConfirm),     // Unicast pairing acknowledgement
         TurntableCmd(TurntableCommand),
         TurntableResp(TurntableResponse),
         TxCmd(TxCommand),
         TxResp(TxResponse),
     }

     struct HelloBeacon { role: Role, mac: [u8; 6] }  // mac filled by hardware
     struct PairConfirm { role: Role, mac: [u8; 6] }
     enum Role { Rx, Tx, Turntable }

     enum TurntableCommand { MoveTo { angle_deg: f32 }, Stop }
     enum TurntableResponse { MoveComplete { angle_deg: f32 }, Error { description: String<64> } }

     enum TxCommand {
         Configure { channel: u8, tx_power_dbm: i8, packet_rate_hz: u16 },
         StartTransmit,
         StopTransmit,
     }
     enum TxResponse { Ack, Status { transmitting: bool, channel: u8, tx_power_dbm: i8, packet_rate_hz: u16 } }

     Serial layer (PC ↔ RX over USB)

     enum PcToRx {
         StartSweep { start_deg: f32, stop_deg: f32, step_deg: f32, samples_per_angle: u16 },
         ConfigureTx { channel: u8, tx_power_dbm: i8, packet_rate_hz: u16 },
         Stop,
         QueryStatus,
     }
     enum RxToPc {
         DataPoint { angle_deg: f32, rssi_dbm: f32, sample_count: u16 },
         SweepComplete,
         Error { description: String<128> },
         Status { sweeping: bool, tx_connected: bool, turntable_connected: bool },
     }

     Stepper math

     - STEPS_PER_REV = 3200.0 (200 full steps × 16 microsteps)
     - degrees_to_steps(deg) -> i32, steps_to_degrees(steps) -> f32

     Serialization helpers

     - serialize(msg, &mut buf) -> Result<&[u8]> — postcard to_slice
     - deserialize::<T>(bytes) -> Result<T> — postcard from_bytes
     - serialize_cobs(msg, &mut buf) -> Result<&[u8]> — postcard to_slice_cobs

     ---
     Discovery & Pairing Protocol

     All three boards continuously broadcast HelloBeacon with their role:
     - Unpaired: broadcast every 1 second
     - Paired: broadcast every 10 seconds (so late-booting peers can still discover)

     Flow:
     1. TX and Stepper broadcast Hello(role=Tx) / Hello(role=Turntable)
     2. RX receives Hello, registers sender as ESP-NOW unicast peer, sends PairConfirm(role=Rx) back
     3. TX/Stepper receive PairConfirm, register RX as peer, signal paired
     4. Stepper also pairs on receiving Hello(role=Rx) directly from RX broadcasts

     LED status (all boards, SK6812 on GPIO2):
     - Blue = alive, booting / discovering
     - Green = paired with peer(s)

     Critical: Beacons must never stop. Earlier bug: a continue after pairing skipped the beacon send, causing race conditions when boards boot in different order.

     ---
     Firmware Details

     ESP-NOW Init Pattern (CRITICAL — get this wrong and set_channel panics)

     let esp_radio_ctrl = &*mk_static!(esp_radio::Controller<'static>, esp_radio::init().unwrap());
     let (mut wifi_controller, interfaces) =
         esp_radio::wifi::new(esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
     wifi_controller.set_mode(esp_radio::wifi::WifiMode::Sta).unwrap();  // MUST call before set_channel
     wifi_controller.start().unwrap();                                     // MUST call before set_channel
     let esp_now = interfaces.esp_now;
     esp_now.set_channel(11).unwrap();  // Panics without set_mode + start above!

     mk_static! macro (all firmware crates)

     Embassy tasks require 'static references. This macro allocates in a StaticCell:
     macro_rules! mk_static {
         ($t:ty, $val:expr) => {{
             static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
             STATIC_CELL.uninit().write($val)
         }};
     }

     TX firmware (software/tx/src/main.rs)

     - Tasks: discovery_task, listener_task, transmit_task
     - State: TxState { transmitting, tx_interval, rx_paired, rx_mac }
     - transmit_task sends b"beambench-tx" payload at configured rate when enabled
     - Listens for TxCommand from RX to configure rate, start/stop

     RX firmware (software/rx/src/main.rs)

     - Tasks: discovery_task, listener_task
     - State: RxState { tx_paired, tx_mac, turntable_paired, turntable_mac }
     - all_paired() requires both TX and turntable
     - Listens for: Hello (discovery), TurntableResp, and raw TX packets (RSSI via received.info.rx_control.rssi)
     - ⚠️  TODO: USB-serial bridge NOT implemented (line 152). Currently just logs RSSI to defmt. This is the biggest Phase 1 gap.

     Stepper firmware (software/stepper/src/main.rs)

     - Tasks: discovery_task, listener_task, motor_task_*, responder_task
     - Motor backends (Cargo features):
       - step-dir (default): GPIO4=STEP, GPIO5=DIR, GPIO6=EN, configurable pulse delay
       - servo42c: UART on GPIO4(RX)/GPIO5(TX) at 38400 baud, uses mks-servo42-rs
     - Motor trait in motor/mod.rs: set_enabled, go_to(position) -> Result<Position>, stop, position
     - Inter-task: Signal<MotorCmd> and Signal<MotorResult> for command/response
     - responder_task sends TurntableResponse back to RX via ESPNOW

     .cargo/config.toml (all firmware)

     [build]
     target = "riscv32imc-unknown-none-elf"
     rustflags = ["-C", "link-arg=-Tdevice.x", "-C", "link-arg=-Tdefmt.x", "-C", "link-arg=-Tlinkall.x"]
     [unstable]
     build-std = ["core", "alloc"]
     [target.riscv32imc-unknown-none-elf]
     runner = "probe-rs run --chip esp32c3 --always-print-stacktrace"
     [env]
     DEFMT_LOG = "info"

     ---
     PC Application Details

     Backend (software/pc/src/main.rs)

     - Routes: GET /api/ports, GET /api/data, GET /api/export/csv, GET /ws
     - AppState: ws_tx: broadcast::Sender<WsEvent>, serial: Mutex<Option<SerialHandle>>, data: Mutex<Vec<DataPoint>>, sweeping: AtomicBool
     - --dev flag: frontend served by Vite dev server; production: embedded via rust-embed
     - WebSocket handler: bidirectional — forwards WsEvent to client, receives WsCommand from client

     Serial transport (software/pc/src/serial.rs)

     - SerialHandle { tx: mpsc::Sender<PcToRx>, rx: Arc<Mutex<mpsc::Receiver<RxToPc>>>, cancel: watch::Sender<bool> }
     - open(port) — USB-serial at 115200 baud via tokio-serial
     - open_tcp(addr) — TCP connection (for rx-sim testing)
     - Background writer_task serializes with COBS, reader_task accumulates bytes and splits on 0x00 delimiter
     - list_ports() lists serial ports + probes tcp://127.0.0.1:9876 for rx-sim

     Sweep engine (software/pc/src/sweep.rs)

     - run_sweep(config, serial_tx, serial_rx, ws_tx) -> Result<Vec<DataPoint>>
     - Validates config, sends PcToRx::StartSweep, collects DataPoint messages until SweepComplete or Error
     - 30-second timeout per receive; concurrent sweep guard via AtomicBool

     Frontend (software/pc/frontend/)

     - SvelteKit 2 with Svelte 5 runes ($state())
     - Port selector (dropdown + text input for TCP), Connect/Disconnect
     - Status indicators: Serial, TX, Turntable connection states
     - Sweep config form (start/stop/step degrees, samples per angle)
     - Plotly.js Scatterpolar chart for antenna pattern
     - WebSocket client with 2-second auto-reconnect (src/lib/ws.ts)

     RX Simulator (software/pc/src/bin/rx-sim.rs)

     - TCP server on port 9876, simulates RX behavior for PC app testing
     - Generates synthetic cardioid pattern: rssi = -30 + 20*cos(angle - 45°)
     - Run with: just sim or cargo run --bin rx-sim --features sim

     ---
     Build & Run

     # Build all firmware
     just build-firmware

     # Flash individual boards (requires probe-rs + board connected via USB)
     cd software/stepper && just flash
     cd software/tx && just flash
     cd software/rx && just flash

     # Run tests (host-side only — protocol + PC)
     just test

     # PC app development
     cd software/pc && just dev        # Axum + Vite dev server
     cd software/pc && just sim        # RX simulator on TCP

     # PC app production build
     cd software/pc && just build      # Builds frontend + Rust binary

     ---
     Tests

     - Protocol: 12 tests — postcard round-trips, COBS framing, stepper angle math, MAX_MSG_SIZE enforcement
     - PC unit: 17 tests — serial frame parsing, sweep state machine, timeout handling, CSV export, config validation
     - PC integration: 2 tests — full rx-sim → serial → sweep pipeline (requires --features sim)
     - Run all: just test

     ---
     Implementation Status

     ✅ Done (Phase 1)

     - Protocol crate with all message types (ESPNOW + serial layers)
     - Stepper firmware: Embassy async, ESPNOW discovery, motor control (step-dir + servo42c backends)
     - TX firmware: ESPNOW discovery, configurable packet transmission
     - RX firmware: ESPNOW discovery, RSSI measurement from TX packets
     - Discovery protocol: continuous beacons, PairConfirm handshake, late-boot resilience
     - LED status indicators: blue=booting, green=paired (all three boards)
     - PC app: Axum backend with WebSocket, REST endpoints
     - PC app: SvelteKit frontend with polar plot, connection UI, sweep controls
     - PC app: Serial transport (USB + TCP) with COBS framing
     - PC app: Sweep state machine with timeout and concurrent guard
     - PC app: RX simulator (rx-sim) for testing without hardware
     - PC app: CSV export
     - Test suite: protocol + PC unit + integration tests (31 total)

     ⚠️  Partially Done

     - RX USB-serial bridge — the RX firmware does NOT yet process serial commands from the PC or send data back. The listener_task just logs RSSI via defmt. This is the critical missing link: rx/src/main.rs line 152 has //
      TODO: Implement USB-serial bridge with postcard+COBS.
       - Needs: UART init, COBS reader/writer tasks, sweep coordinator that orchestrates turntable moves + RSSI collection + serial responses
       - The PC app and protocol are ready for this — the RX firmware is the bottleneck

     ✅ Done (Phase 1 polish)

     - [x] Port identification in web UI — port list now shows USB vendor/product descriptions (PortInfo struct, tokio-serial SerialPortType extraction)
     - [x] Status log area in web UI — scrollable timestamped log panel in sidebar showing connect/disconnect/sweep/error events (WsEvent::Log variant)
     - [x] LED activity blinking — all boards blink amber during activity (stepper: motor movement, TX: transmitting, RX: receiving RSSI packets). LED state machine runs in main() loop using Signal + select pattern.

     ---
     Known Issues & Gotchas

     1. ESP-NOW init order: Must call set_mode(Sta) + start() before set_channel() or it panics. Found via crash at rx/src/main.rs:114.
     2. Discovery beacons must never stop: All boards must keep broadcasting even after pairing (at reduced rate). Stopping causes race conditions when boards boot in different order.
     3. heapless version: Must use 0.9.2+ for defmt support. Protocol crate gates this with defmt = ["dep:defmt", "heapless/defmt"].
     4. riscv-rt conflict: Do NOT add riscv-rt as a direct dependency — it conflicts with esp-hal's esp-riscv-rt via the links key.
     5. build-std: Required in all firmware .cargo/config.toml when using esp-radio (needs heap/alloc).
     6. RX justfile: Has cargo run (no --release) for flash — this is intentional for dev builds but the comment wrongly says "TX firmware".
     7. LED state machine pattern: LED stays as a local in main() — tasks signal state changes via a Signal<LedState>. Main's loop drives the LED using select() to check for new states during blink cycles.

     ---
     Future Plans

     Phase 2: Wideband Measurement (ADF4351 + AD8318)

     Replace ESPNOW RSSI measurement with dedicated RF hardware for calibrated wideband measurements (35 MHz – 4.4 GHz):

     - TX board: ADF4351 PLL synthesizer module controlled via SPI (GPIO4=CLK, GPIO5=DAT, GPIO6=LE). Replaces ESPNOW packet transmission with configurable CW signal.
     - RX board: AD8318 logarithmic detector module, output read via ADC. 60 dB dynamic range, 1 MHz – 8 GHz. Replaces ESPNOW RSSI with calibrated power measurement.
     - ESPNOW stays for command/control between boards (frequency config, start/stop, turntable commands).
     - Protocol changes: Add SetFrequency { frequency_hz: u32, power_level: u8 }, EnableOutput, DisableOutput to TxCommand. Add frequency_hz: Option<u32> to PcToRx::StartSweep.
     - Calibration: Start with AD8318 datasheet nominal slope (~-24 mV/dB), refine later with known source.
     - Hardware cost: ~$45–60 for ADF4351 + AD8318 breakout boards + SMA cables.

     Other Future Ideas

     - Multiple frequency sweep (iterate frequencies at each angle)
     - 3D pattern measurement (elevation + azimuth)
     - Continuous rotation mode (vs step-and-measure)
     - Data persistence / measurement history in PC app
