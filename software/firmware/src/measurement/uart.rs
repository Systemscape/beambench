//! UART measurement backend — communicates with external RF devboards.
//!
//! Sends text commands over UART to an external TX/RX board and parses
//! text responses. Protocol:
//!
//! ```text
//! ESP32 TX → ext TX:    "START\n"  /  "STOP\n"
//! ESP32 RX → ext RX:    "START\n"  /  "REPORT\n"
//! ext RX → ESP32 RX:    "RSSI:-45.3,N:50\n"
//! ```

use defmt::info;
use embassy_time::Duration;
use esp_hal::uart::Uart;
use esp_hal::Async;

use super::{MeasurementResult, RxBackend, TxBackend};

/// Timeout for waiting for a report response from the external RX board.
const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// Per-byte read timeout inside the line reader.
const BYTE_TIMEOUT: Duration = Duration::from_millis(200);

// ── RX ──────────────────────────────────────────────────────────────────────

/// UART RX backend — delegates measurement to an external devboard.
///
/// On `start()`, sends `"START\n"` so the external board begins collecting.
/// On `report()`, sends `"REPORT\n"` and reads back `"RSSI:<val>,N:<count>\n"`.
pub struct UartRxBackend {
    uart: Uart<'static, Async>,
}

impl UartRxBackend {
    pub fn new(uart: Uart<'static, Async>) -> Self {
        Self { uart }
    }
}

impl RxBackend for UartRxBackend {
    async fn start(&mut self) {
        let _ = write_all_async(&mut self.uart, b"START\n").await;
        info!("UART RX: sent START");
    }

    async fn report(&mut self) -> MeasurementResult {
        let _ = write_all_async(&mut self.uart, b"REPORT\n").await;

        match read_line(&mut self.uart, REPORT_TIMEOUT).await {
            Some(line) => {
                let result = parse_report(line.as_str());
                info!(
                    "UART RX: {} dBm, {} samples",
                    result.rssi_dbm, result.sample_count
                );
                result
            }
            None => {
                defmt::warn!("UART RX: timeout waiting for report");
                MeasurementResult {
                    rssi_dbm: 0.0,
                    sample_count: 0,
                }
            }
        }
    }
}

// ── TX ──────────────────────────────────────────────────────────────────────

/// UART TX backend — controls an external transmitter devboard.
///
/// Sends `"START\n"` / `"STOP\n"` over UART.
pub struct UartTxBackend {
    uart: Uart<'static, Async>,
}

impl UartTxBackend {
    pub fn new(uart: Uart<'static, Async>) -> Self {
        Self { uart }
    }
}

impl TxBackend for UartTxBackend {
    async fn start_transmit(&mut self) {
        let _ = write_all_async(&mut self.uart, b"START\n").await;
        info!("UART TX: sent START");
    }

    async fn stop_transmit(&mut self) {
        let _ = write_all_async(&mut self.uart, b"STOP\n").await;
        info!("UART TX: sent STOP");
    }
}

// ── Backend info ────────────────────────────────────────────────────────────

/// Backend metadata for UART-connected external devboards.
pub fn backend_info() -> beambench_protocol::BackendInfo {
    beambench_protocol::BackendInfo {
        name: heapless::String::try_from("uart").unwrap(),
        frequency_mhz: None,
    }
}

// ── Async helpers ────────────────────────────────────────────────────────

/// Write all bytes to UART using the native async method.
///
/// `Uart<'_, Async>` implements both blocking `embedded_io::Write` and async
/// `embedded_io_async::Write`, so calling `.write()` is ambiguous.  We use the
/// native `write_async` method directly to avoid the conflict.
async fn write_all_async(
    uart: &mut Uart<'static, Async>,
    mut data: &[u8],
) -> Result<(), esp_hal::uart::TxError> {
    while !data.is_empty() {
        let n = uart.write_async(data).await?;
        data = &data[n..];
    }
    Ok(())
}

// ── Line reader ─────────────────────────────────────────────────────────────

/// Read bytes from UART until `'\n'` or timeout.
async fn read_line(
    uart: &mut Uart<'static, Async>,
    timeout: Duration,
) -> Option<heapless::String<64>> {
    let mut buf = [0u8; 64];
    let mut pos = 0;
    let deadline = embassy_time::Instant::now() + timeout;

    loop {
        if embassy_time::Instant::now() > deadline {
            return None;
        }

        let mut byte = [0u8; 1];
        match embassy_time::with_timeout(BYTE_TIMEOUT, uart.read_async(&mut byte)).await {
            Ok(Ok(1)) => {
                if byte[0] == b'\n' {
                    let s = core::str::from_utf8(&buf[..pos]).ok()?;
                    return heapless::String::try_from(s).ok();
                }
                if byte[0] != b'\r' && pos < buf.len() {
                    buf[pos] = byte[0];
                    pos += 1;
                }
            }
            // Timeout or read error — keep trying until deadline.
            _ => continue,
        }
    }
}

// ── Response parser ─────────────────────────────────────────────────────────

/// Parse `"RSSI:-45.3,N:50"` into a `MeasurementResult`.
fn parse_report(line: &str) -> MeasurementResult {
    let mut rssi_dbm = 0.0f32;
    let mut sample_count = 0u16;

    for part in line.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("RSSI:") {
            if let Some(v) = parse_f32(val) {
                rssi_dbm = v;
            }
        } else if let Some(val) = part.strip_prefix("N:") {
            if let Some(v) = parse_u16(val) {
                sample_count = v;
            }
        }
    }

    MeasurementResult {
        rssi_dbm,
        sample_count,
    }
}

/// Simple no_std f32 parser for format: `[-]digits[.digits]`.
fn parse_f32(s: &str) -> Option<f32> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (neg, s) = if let Some(rest) = s.strip_prefix('-') {
        (true, rest)
    } else {
        (false, s)
    };

    let mut integer: i32 = 0;
    let mut frac: i32 = 0;
    let mut frac_digits: i32 = 0;
    let mut in_frac = false;

    for c in s.bytes() {
        match c {
            b'0'..=b'9' => {
                if in_frac {
                    frac = frac * 10 + (c - b'0') as i32;
                    frac_digits += 1;
                } else {
                    integer = integer * 10 + (c - b'0') as i32;
                }
            }
            b'.' => {
                if in_frac {
                    return None;
                }
                in_frac = true;
            }
            _ => return None,
        }
    }

    let mut val = integer as f32;
    if frac_digits > 0 {
        let mut divisor = 1.0f32;
        for _ in 0..frac_digits {
            divisor *= 10.0;
        }
        val += frac as f32 / divisor;
    }
    if neg {
        val = -val;
    }
    Some(val)
}

/// Simple no_std u16 parser.
fn parse_u16(s: &str) -> Option<u16> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut result: u16 = 0;
    for c in s.bytes() {
        match c {
            b'0'..=b'9' => {
                result = result.checked_mul(10)?.checked_add((c - b'0') as u16)?;
            }
            _ => return None,
        }
    }
    Some(result)
}
