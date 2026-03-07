//! OTA firmware update streaming over the serial protocol.
//!
//! # Protocol overview
//!
//! The PC streams a firmware binary to a target device (RX, TX, or Turntable)
//! through the Bridge, which relays each message over ESP-NOW.
//!
//! ```text
//! PC ──serial──▸ Bridge ──ESP-NOW──▸ Target device
//!                  ◂── ack ──────────┘
//! ```
//!
//! ## Phases
//!
//! 1. **OtaBegin** — PC sends firmware size, SHA-256 hash, and `ack_interval`
//!    to the target. The target locates the inactive OTA partition, validates
//!    the size fits, and responds with `OtaReady`.
//!
//! 2. **Streaming** — PC sends 240-byte `OtaData` chunks (the maximum that
//!    fits in an ESP-NOW frame). The target writes each chunk to flash and
//!    updates a running SHA-256 hash.
//!
//!    Flow control uses a **windowed ack** scheme controlled by `ack_interval`:
//!    - The target sends `OtaAck { seq }` every `ack_interval` chunks and
//!      always on the last chunk.
//!    - The PC sends up to `ack_interval` chunks before pausing to wait for
//!      the batch ack, then sends the next window.
//!    - An `ack_interval` of 1 degrades to stop-and-wait (one ack per chunk).
//!    - Typical value: 16 (sends 16 chunks ≈ 3.8 KB, then waits for ack).
//!
//!    If a batch ack times out, the entire window is retransmitted (up to
//!    `OTA_WINDOW_RETRIES` times). The target is idempotent for duplicate
//!    writes to the same flash offset.
//!
//! 3. **OtaFinish** — PC signals that all chunks were sent. The target
//!    finalizes the SHA-256 hash, compares it to the expected value from
//!    OtaBegin, switches the boot slot, and responds with `OtaComplete`.
//!    The device reboots after a short delay.
//!
//! ## Safety
//!
//! - End-to-end SHA-256 verification: the hash is computed on the PC before
//!   streaming and verified on the target after all chunks are written. A
//!   single bit flip in any chunk causes the update to be rejected.
//! - The target writes to the *inactive* OTA partition. The active slot is
//!   never modified. If verification fails or the device loses power, the
//!   bootloader stays on the current (working) firmware.
//! - After a successful OTA boot, the firmware calls `mark_current_valid()`
//!   to confirm the new slot. If the new firmware crashes before doing so,
//!   the bootloader rolls back to the previous slot.

use std::sync::Arc;

use beambench_protocol::{DeviceEvent, OTA_CHUNK_SIZE, PcCommand, Role};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::WsEvent;

/// Errors returned by [`wait_for`].
enum WaitError {
    Timeout,
    Device(String),
    Disconnected,
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaitError::Timeout => write!(f, "Timeout"),
            WaitError::Device(msg) => write!(f, "Device: {}", msg),
            WaitError::Disconnected => write!(f, "Serial connection lost"),
        }
    }
}

/// Timeout for the target to respond to `OtaBegin` with `OtaReady`.
const OTA_BEGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Timeout for a batch ack covering `ack_interval` chunks.
const OTA_WINDOW_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Number of retries per window before aborting the update.
const OTA_WINDOW_RETRIES: usize = 3;

/// Timeout for the target to respond to `OtaFinish` with `OtaComplete`.
/// This is longer because the target verifies SHA-256 and switches the boot
/// slot, which involves flash operations.
const OTA_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default ack interval (chunks per ack). Higher values increase throughput
/// by reducing round-trip overhead, but require the target to buffer more
/// data before acknowledging. 16 chunks × 240 bytes = 3,840 bytes per window.
pub const DEFAULT_ACK_INTERVAL: u16 = 16;

/// Parse a role string into a protocol `Role`.
pub fn parse_role(s: &str) -> Option<Role> {
    match s.to_lowercase().as_str() {
        "rx" => Some(Role::Rx),
        "tx" => Some(Role::Tx),
        "turntable" => Some(Role::Turntable),
        "bridge" => Some(Role::Bridge),
        _ => None,
    }
}

/// Stream a firmware binary to a target device via OTA.
///
/// Sends `OtaBegin`, streams 240-byte chunks with windowed ack flow control,
/// then sends `OtaFinish` and waits for `OtaComplete`.
///
/// `ack_interval` controls how many chunks the target buffers before sending
/// an ack. Use [`DEFAULT_ACK_INTERVAL`] (16) for typical use. Set to 1 for
/// stop-and-wait (slowest but most reliable over very lossy links).
pub async fn stream_firmware(
    firmware: &[u8],
    target: Role,
    ack_interval: u16,
    serial_tx: &mpsc::Sender<PcCommand>,
    serial_rx: &Arc<Mutex<mpsc::Receiver<DeviceEvent>>>,
    ws_tx: &broadcast::Sender<WsEvent>,
) -> Result<(), String> {
    let total_size = firmware.len() as u32;
    let total_chunks = (firmware.len() + OTA_CHUNK_SIZE - 1) / OTA_CHUNK_SIZE;
    let ack_interval = ack_interval.max(1) as usize;

    // Compute SHA-256 before streaming so the target can verify at the end.
    let sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(firmware);
        hasher.finalize().into()
    };

    // Lock serial_rx for the entire OTA session. This prevents other
    // operations (sweep, jog, status poll) from consuming our acks.
    let mut rx = serial_rx.lock().await;

    // ── Phase 1: OtaBegin ────────────────────────────────────────────────────

    serial_tx
        .send(PcCommand::OtaBegin {
            target,
            total_size,
            sha256,
            ack_interval: ack_interval as u16,
        })
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    wait_for(&mut rx, OTA_BEGIN_TIMEOUT, |e| {
        matches!(e, DeviceEvent::OtaReady)
    })
    .await
    .map_err(|e| format!("OtaBegin: {e}"))?;

    let _ = ws_tx.send(WsEvent::Log {
        message: format!(
            "OTA: device ready, streaming {} chunks ({} bytes, ack every {})",
            total_chunks, total_size, ack_interval
        ),
    });

    // ── Phase 2: Stream chunks in windows ────────────────────────────────────

    let mut next_to_send: usize = 0;
    let mut last_progress_pct: usize = 0;

    while next_to_send < total_chunks {
        // Determine the window: up to ack_interval chunks.
        let window_end = (next_to_send + ack_interval).min(total_chunks);
        let expected_ack_seq = (window_end - 1) as u16;

        // Retry loop for this window.
        let mut acked = false;
        for attempt in 0..=OTA_WINDOW_RETRIES {
            if attempt > 0 {
                let _ = ws_tx.send(WsEvent::Log {
                    message: format!(
                        "Retrying chunks {}-{}/{} (attempt {})",
                        next_to_send + 1,
                        window_end,
                        total_chunks,
                        attempt + 1,
                    ),
                });
            }

            // Send all chunks in the window.
            for i in next_to_send..window_end {
                let start = i * OTA_CHUNK_SIZE;
                let end = (start + OTA_CHUNK_SIZE).min(firmware.len());
                let mut data = heapless::Vec::<u8, 240>::new();
                data.extend_from_slice(&firmware[start..end])
                    .map_err(|_| "Chunk exceeds 240 bytes".to_string())?;

                serial_tx
                    .send(PcCommand::OtaData {
                        target,
                        seq: i as u16,
                        data,
                    })
                    .await
                    .map_err(|_| "Serial connection lost".to_string())?;
            }

            // Wait for the batch ack.
            match wait_for(&mut rx, OTA_WINDOW_TIMEOUT, |e| {
                matches!(e, DeviceEvent::OtaAck { seq } if *seq == expected_ack_seq)
            })
            .await
            {
                Ok(()) => {
                    acked = true;
                    break;
                }
                Err(WaitError::Timeout) if attempt < OTA_WINDOW_RETRIES => continue,
                Err(e) => {
                    return Err(format!(
                        "Chunks {}-{}/{}: {e}",
                        next_to_send + 1,
                        window_end,
                        total_chunks,
                    ));
                }
            }
        }
        if !acked {
            return Err(format!(
                "Chunks {}-{}/{}: Timeout (after {} retries)",
                next_to_send + 1,
                window_end,
                total_chunks,
                OTA_WINDOW_RETRIES
            ));
        }

        next_to_send = window_end;

        // Progress update — emit when the integer percentage changes.
        let pct = next_to_send * 100 / total_chunks;
        if pct > last_progress_pct || next_to_send == total_chunks {
            last_progress_pct = pct;
            let _ = ws_tx.send(WsEvent::OtaProgress {
                chunks_sent: next_to_send as u16,
                total_chunks: total_chunks as u16,
            });
        }
    }

    // ── Phase 3: OtaFinish ───────────────────────────────────────────────────

    serial_tx
        .send(PcCommand::OtaFinish { target })
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    wait_for(&mut rx, OTA_FINISH_TIMEOUT, |e| {
        matches!(e, DeviceEvent::OtaComplete)
    })
    .await
    .map_err(|e| format!("OtaFinish: {e}"))?;

    Ok(())
}

/// Wait for a `DeviceEvent` matching `predicate`.
///
/// Returns `Ok(())` on match, or a typed [`WaitError`] on timeout, device
/// error, or channel close.
async fn wait_for(
    rx: &mut mpsc::Receiver<DeviceEvent>,
    timeout: std::time::Duration,
    predicate: impl Fn(&DeviceEvent) -> bool,
) -> Result<(), WaitError> {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(ref event) if predicate(event) => return Ok(()),
                Some(DeviceEvent::OtaError { description }) => {
                    return Err(WaitError::Device(description.to_string()));
                }
                Some(_) => continue,
                None => return Err(WaitError::Disconnected),
            }
        }
    })
    .await
    .unwrap_or(Err(WaitError::Timeout))
}
