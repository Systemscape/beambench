//! OTA firmware update responder — shared by all field device roles.
//!
//! Receives OTA chunks via ESP-NOW, writes them to the inactive OTA partition,
//! verifies SHA-256, switches the boot slot, and reboots.

use defmt::info;
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, mutex::Mutex, signal::Signal};
use embedded_storage::Storage;
use esp_radio::esp_now::EspNowSender;
use sha2::{Digest, Sha256};

use beambench_protocol::{self as proto, EspnowMessage, OTA_CHUNK_SIZE};
use esp_bootloader_esp_idf::partitions;

use crate::common::{SharedFlash, LedState, COLOR_RED, COLOR_GREEN};

const CHUNK_SIZE: u32 = OTA_CHUNK_SIZE as u32;

/// Active OTA session state.
struct OtaSession {
    /// Absolute flash offset of the target OTA partition.
    partition_offset: u32,
    /// Size of the target OTA partition.
    partition_size: u32,
    /// Expected SHA-256 hash from OtaBegin.
    expected_sha256: [u8; 32],
    /// Running SHA-256 hasher.
    hasher: Sha256,
    /// How often to send OtaAck (every N chunks). 1 = every chunk.
    ack_interval: u16,
    /// Total number of expected chunks (for detecting the last chunk).
    total_chunks: u16,
}

/// OTA responder state — `None` when idle, `Some` during an active update.
pub struct OtaState {
    session: Option<OtaSession>,
}

impl OtaState {
    pub const fn new() -> Self {
        Self { session: None }
    }
}

/// Handle an OTA-related ESP-NOW message.
///
/// Returns an optional response message to send back to the Bridge.
/// Call this from each role's listener task when an OTA message is received.
pub async fn handle_ota_message(
    msg: &EspnowMessage,
    ota: &Mutex<NoopRawMutex, OtaState>,
    flash: &SharedFlash,
) -> Option<EspnowMessage> {
    match msg {
        EspnowMessage::OtaBegin {
            total_size,
            sha256,
            ack_interval,
        } => handle_begin(ota, flash, *total_size, *sha256, *ack_interval).await,

        EspnowMessage::OtaData { seq, data } => handle_data(ota, flash, *seq, data).await,

        EspnowMessage::OtaFinish => handle_finish(ota, flash).await,

        _ => None,
    }
}

/// Returns true if the message is an OTA message (regardless of whether we handle it).
pub fn is_ota_message(msg: &EspnowMessage) -> bool {
    matches!(
        msg,
        EspnowMessage::OtaBegin { .. }
            | EspnowMessage::OtaData { .. }
            | EspnowMessage::OtaFinish
    )
}

/// Handle an OTA message, send the response to the Bridge, and reboot if complete.
///
/// Call this from each role's listener task when `is_ota_message()` returns true.
/// Sets the LED to blinking red during OTA and restores solid green when done.
pub async fn process_and_respond(
    msg: &EspnowMessage,
    ota: &Mutex<NoopRawMutex, OtaState>,
    flash: &SharedFlash,
    sender: &Mutex<NoopRawMutex, EspNowSender<'static>>,
    bridge_mac: &[u8; 6],
    led_signal: &Signal<NoopRawMutex, LedState>,
) {
    // Start blinking red on OtaBegin.
    if matches!(msg, EspnowMessage::OtaBegin { .. }) {
        led_signal.signal(LedState::Blink { color: COLOR_RED, period_ms: 200 });
    }

    if let Some(resp) = handle_ota_message(msg, ota, flash).await {
        let is_done = matches!(resp, EspnowMessage::OtaComplete | EspnowMessage::OtaError { .. });
        send_ota_response(sender, bridge_mac, &resp).await;
        if matches!(resp, EspnowMessage::OtaComplete) {
            schedule_reboot().await;
        }
        if is_done {
            led_signal.signal(LedState::Solid(COLOR_GREEN));
        }
    }
}

/// Send an OTA response back to the Bridge.
async fn send_ota_response(
    sender: &Mutex<NoopRawMutex, EspNowSender<'static>>,
    bridge_mac: &[u8; 6],
    msg: &EspnowMessage,
) {
    let mut buf = [0u8; proto::MAX_MSG_SIZE];
    if let Ok(data) = proto::serialize(msg, &mut buf) {
        let mut s = sender.lock().await;
        let _ = s.send_async(bridge_mac, data).await;
    }
}

// ── OtaBegin ────────────────────────────────────────────────────────────────

async fn handle_begin(
    ota: &Mutex<NoopRawMutex, OtaState>,
    flash: &SharedFlash,
    total_size: u32,
    sha256: [u8; 32],
    ack_interval: u16,
) -> Option<EspnowMessage> {
    let total_chunks = ((total_size + CHUNK_SIZE - 1) / CHUNK_SIZE) as u16;
    let ack_interval = ack_interval.max(1); // Ensure at least 1.
    info!(
        "OTA begin: {} bytes, {} chunks, ack every {} chunks",
        total_size, total_chunks, ack_interval
    );

    let mut f = flash.lock().await;

    // Read partition table to find the inactive OTA slot.
    let mut pt_buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = match partitions::read_partition_table(&mut *f, &mut pt_buf) {
        Ok(pt) => pt,
        Err(e) => {
            defmt::error!("Failed to read partition table: {:?}", defmt::Debug2Format(&e));
            return Some(ota_error("partition table read failed"));
        }
    };

    // Find OTA slots.
    let ota0 = pt.find_partition(partitions::PartitionType::App(
        partitions::AppPartitionSubType::Ota0,
    ));
    let ota1 = pt.find_partition(partitions::PartitionType::App(
        partitions::AppPartitionSubType::Ota1,
    ));

    let (ota0, ota1) = match (ota0, ota1) {
        (Ok(Some(a)), Ok(Some(b))) => (a, b),
        _ => {
            defmt::error!("OTA partitions not found in partition table");
            return Some(ota_error("ota partitions not found"));
        }
    };

    // Determine which slot is currently booted (by checking otadata).
    // If we can't determine, default to writing to ota_0.
    let booted = pt.booted_partition();
    let (target_offset, target_size) = match booted {
        Ok(Some(entry))
            if entry.partition_type()
                == partitions::PartitionType::App(partitions::AppPartitionSubType::Ota0) =>
        {
            // Currently on ota_0, write to ota_1.
            (ota1.offset(), ota1.len())
        }
        _ => {
            // Currently on ota_1 or factory/unknown, write to ota_0.
            (ota0.offset(), ota0.len())
        }
    };

    if total_size > target_size {
        defmt::error!(
            "Firmware too large: {} > partition size {}",
            total_size,
            target_size
        );
        return Some(ota_error("firmware too large for partition"));
    }

    info!(
        "Writing to OTA slot at offset 0x{:X} ({} bytes available)",
        target_offset,
        target_size
    );

    // Drop flash lock before storing session state.
    drop(f);

    let mut state = ota.lock().await;
    state.session = Some(OtaSession {
        partition_offset: target_offset,
        partition_size: target_size,
        expected_sha256: sha256,
        hasher: Sha256::new(),
        ack_interval,
        total_chunks,
    });

    Some(EspnowMessage::OtaReady)
}

// ── OtaData ─────────────────────────────────────────────────────────────────

async fn handle_data(
    ota: &Mutex<NoopRawMutex, OtaState>,
    flash: &SharedFlash,
    seq: u16,
    data: &heapless::Vec<u8, 240>,
) -> Option<EspnowMessage> {
    let mut state = ota.lock().await;
    let session = match &mut state.session {
        Some(s) => s,
        None => {
            defmt::warn!("OtaData received but no session active");
            return Some(ota_error("no ota session"));
        }
    };

    let offset = seq as u32 * CHUNK_SIZE;
    if offset + data.len() as u32 > session.partition_size {
        defmt::error!("OTA chunk out of bounds: offset {} + len {}", offset, data.len());
        return Some(ota_error("chunk out of bounds"));
    }

    let flash_addr = session.partition_offset + offset;

    // Update hash with the chunk data.
    session.hasher.update(data.as_slice());

    // Write to flash.
    let mut f = flash.lock().await;
    if let Err(e) = f.write(flash_addr, data.as_slice()) {
        defmt::error!("Flash write failed at 0x{:X}: {:?}", flash_addr, defmt::Debug2Format(&e));
        drop(f);
        state.session = None;
        return Some(ota_error("flash write failed"));
    }

    // Windowed ack: only ack on interval boundaries and the last chunk.
    let chunk_num = seq + 1; // 1-based count
    let is_last = chunk_num >= session.total_chunks;
    let is_interval = chunk_num % session.ack_interval == 0;
    if is_interval || is_last {
        Some(EspnowMessage::OtaAck { seq })
    } else {
        None
    }
}

// ── OtaFinish ───────────────────────────────────────────────────────────────

async fn handle_finish(
    ota: &Mutex<NoopRawMutex, OtaState>,
    flash: &SharedFlash,
) -> Option<EspnowMessage> {
    let mut state = ota.lock().await;
    let session = match state.session.take() {
        Some(s) => s,
        None => {
            defmt::warn!("OtaFinish received but no session active");
            return Some(ota_error("no ota session"));
        }
    };

    // Verify SHA-256.
    let computed = session.hasher.finalize();
    if computed.as_slice() != session.expected_sha256 {
        defmt::error!("SHA-256 mismatch!");
        return Some(ota_error("sha256 mismatch"));
    }

    info!("SHA-256 verified, switching boot partition");

    // Switch the OTA slot.
    let mut f = flash.lock().await;
    let mut pt_buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let mut updater = match esp_bootloader_esp_idf::ota_updater::OtaUpdater::new(&mut *f, &mut pt_buf) {
        Ok(u) => u,
        Err(e) => {
            defmt::error!("OtaUpdater init failed: {:?}", defmt::Debug2Format(&e));
            return Some(ota_error("ota updater init failed"));
        }
    };

    if let Err(e) = updater.activate_next_partition() {
        defmt::error!("Failed to activate partition: {:?}", defmt::Debug2Format(&e));
        return Some(ota_error("partition activation failed"));
    }

    if let Err(e) = updater.set_current_ota_state(esp_bootloader_esp_idf::ota::OtaImageState::New) {
        defmt::error!("Failed to set OTA state: {:?}", defmt::Debug2Format(&e));
        return Some(ota_error("set ota state failed"));
    }

    drop(f);

    info!("OTA complete, rebooting in 2 seconds...");

    Some(EspnowMessage::OtaComplete)
}

/// After sending OtaComplete, the caller should schedule a reboot.
pub async fn schedule_reboot() {
    embassy_time::Timer::after(embassy_time::Duration::from_secs(2)).await;
    esp_hal::system::software_reset();
}

/// Mark the currently booted OTA slot as valid.
///
/// Call this once after successful boot (ESP-NOW up, discovery running) to
/// prevent the bootloader from rolling back to the previous slot.
pub fn mark_current_valid(flash: &mut esp_storage::FlashStorage<'_>) {
    let mut pt_buf = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let updater = esp_bootloader_esp_idf::ota_updater::OtaUpdater::new(flash, &mut pt_buf);
    match updater {
        Ok(mut u) => {
            if let Err(e) = u.set_current_ota_state(
                esp_bootloader_esp_idf::ota::OtaImageState::Valid,
            ) {
                // Not fatal — might be first boot from factory partition.
                defmt::debug!(
                    "Could not set OTA state to Valid: {:?}",
                    defmt::Debug2Format(&e)
                );
            } else {
                info!("OTA slot marked as valid");
            }
        }
        Err(_) => {
            // No OTA partition table yet — first flash, not an error.
            defmt::debug!("No OTA partitions found (first flash?)");
        }
    }
}

fn ota_error(msg: &str) -> EspnowMessage {
    let mut desc = heapless::String::new();
    let _ = desc.push_str(msg);
    EspnowMessage::OtaError { description: desc }
}
