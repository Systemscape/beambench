//! OTA firmware streaming over the serial protocol.
//!
//! Reads a firmware binary, computes SHA-256, and streams 240-byte chunks
//! to the target device with per-chunk ack flow control.

use std::sync::Arc;

use beambench_protocol::{DeviceEvent, PcCommand, Role};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::WsEvent;

const OTA_CHUNK_SIZE: usize = 240;
const OTA_BEGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const OTA_CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const OTA_FINISH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

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
/// Sends `OtaBegin`, streams 240-byte chunks with ack flow control,
/// then sends `OtaFinish` and waits for `OtaComplete`.
/// Progress updates are broadcast via `ws_tx`.
pub async fn stream_firmware(
    firmware: &[u8],
    target: Role,
    serial_tx: &mpsc::Sender<PcCommand>,
    serial_rx: &Arc<Mutex<mpsc::Receiver<DeviceEvent>>>,
    ws_tx: &broadcast::Sender<WsEvent>,
) -> Result<(), String> {
    let total_size = firmware.len() as u32;
    let total_chunks = (firmware.len() + OTA_CHUNK_SIZE - 1) / OTA_CHUNK_SIZE;

    // Compute SHA-256.
    let sha256: [u8; 32] = {
        let mut hasher = Sha256::new();
        hasher.update(firmware);
        hasher.finalize().into()
    };

    // Lock serial_rx for the entire OTA session (exclusive operation).
    let mut rx = serial_rx.lock().await;

    // ── OtaBegin ────────────────────────────────────────────────────────────
    serial_tx
        .send(PcCommand::OtaBegin {
            target,
            total_size,
            sha256,
        })
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    wait_for(&mut rx, OTA_BEGIN_TIMEOUT, |e| {
        matches!(e, DeviceEvent::OtaReady)
    })
    .await
    .map_err(|e| format!("OtaBegin: {}", e))?;

    let _ = ws_tx.send(WsEvent::Log {
        message: format!(
            "OTA: device ready, streaming {} chunks ({} bytes)",
            total_chunks, total_size
        ),
    });

    // ── Stream chunks ───────────────────────────────────────────────────────
    for (i, chunk) in firmware.chunks(OTA_CHUNK_SIZE).enumerate() {
        let seq = i as u16;
        let mut data = heapless::Vec::<u8, 240>::new();
        data.extend_from_slice(chunk)
            .map_err(|_| "Chunk exceeds 240 bytes".to_string())?;

        serial_tx
            .send(PcCommand::OtaData { target, seq, data })
            .await
            .map_err(|_| "Serial connection lost".to_string())?;

        wait_for(&mut rx, OTA_CHUNK_TIMEOUT, |e| {
            matches!(e, DeviceEvent::OtaAck { seq: s } if *s == seq)
        })
        .await
        .map_err(|e| format!("Chunk {}/{}: {}", seq + 1, total_chunks, e))?;

        // Progress update every ~1% + on the last chunk.
        let progress_interval = (total_chunks / 100).max(1);
        if i % progress_interval == 0 || i == total_chunks - 1 {
            let _ = ws_tx.send(WsEvent::OtaProgress {
                chunks_sent: seq + 1,
                total_chunks: total_chunks as u16,
            });
        }
    }

    // ── OtaFinish ───────────────────────────────────────────────────────────
    serial_tx
        .send(PcCommand::OtaFinish { target })
        .await
        .map_err(|_| "Serial connection lost".to_string())?;

    wait_for(&mut rx, OTA_FINISH_TIMEOUT, |e| {
        matches!(e, DeviceEvent::OtaComplete)
    })
    .await
    .map_err(|e| format!("OtaFinish: {}", e))?;

    Ok(())
}

/// Wait for a `DeviceEvent` matching `predicate`. Returns error on timeout,
/// `OtaError` response, or channel close.
async fn wait_for(
    rx: &mut mpsc::Receiver<DeviceEvent>,
    timeout: std::time::Duration,
    predicate: impl Fn(&DeviceEvent) -> bool,
) -> Result<(), String> {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Some(ref event) if predicate(event) => return Ok(()),
                Some(DeviceEvent::OtaError { description }) => {
                    return Err(format!("Device: {}", description));
                }
                Some(_) => continue,
                None => return Err("Serial connection lost".to_string()),
            }
        }
    })
    .await
    .unwrap_or(Err("Timeout".to_string()))
}
