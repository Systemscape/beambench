//! Role provisioning: read/write the device role from/to flash.
//!
//! Uses a fixed offset in the `role` flash partition (0xF000). The role is
//! stored as 8 bytes: 4-byte magic prefix + 1 role byte + 3 padding bytes.
//! Flash reads/writes must be 4-byte aligned.

use beambench_protocol::Role;
use defmt::info;
use embedded_storage::{ReadStorage, Storage};
use esp_hal::peripherals::FLASH;
use esp_storage::FlashStorage;

/// Magic bytes to distinguish a provisioned role from erased flash (0xFF).
const MAGIC: [u8; 4] = [0xBE, 0xA1, 0x01, 0x00]; // "bea1" + version 0

/// Flash offset for the role partition (matches partition table in architecture-v2.md).
const ROLE_OFFSET: u32 = 0xF000;

/// Total size of the role record in flash (must be 4-byte aligned).
const ROLE_RECORD_SIZE: usize = 8; // 4 magic + 1 role + 3 padding

/// Determine the device role at boot.
///
/// - If a `role-*` cargo feature is active, write that role to flash and return it.
/// - Otherwise, read the role from flash.
pub fn resolve_role(flash: FLASH) -> Role {
    let mut storage = FlashStorage::new(flash);

    // Compile-time role from cargo feature (provisioning mode).
    let feature_role = feature_role();

    if let Some(role) = feature_role {
        info!("Provisioning role: {:?}", defmt::Debug2Format(&role));
        write_role(&mut storage, role);
        role
    } else {
        match read_role(&mut storage) {
            Some(role) => {
                info!("Role from flash: {:?}", defmt::Debug2Format(&role));
                role
            }
            None => {
                defmt::panic!(
                    "No role provisioned! Flash with --features role-<rx|tx|stepper|bridge>"
                );
            }
        }
    }
}

/// Returns the role specified by a cargo feature, if any.
fn feature_role() -> Option<Role> {
    // Compile-time check: at most one role feature may be active.
    #[cfg(any(
        all(feature = "role-rx", feature = "role-tx"),
        all(feature = "role-rx", feature = "role-stepper"),
        all(feature = "role-rx", feature = "role-bridge"),
        all(feature = "role-tx", feature = "role-stepper"),
        all(feature = "role-tx", feature = "role-bridge"),
        all(feature = "role-stepper", feature = "role-bridge"),
    ))]
    compile_error!("At most one role-* feature may be active");

    #[cfg(feature = "role-rx")]
    return Some(Role::Rx);
    #[cfg(feature = "role-tx")]
    return Some(Role::Tx);
    #[cfg(feature = "role-stepper")]
    return Some(Role::Turntable);
    #[cfg(feature = "role-bridge")]
    return Some(Role::Bridge);

    #[allow(unreachable_code)]
    None
}

// ── Flash read/write ────────────────────────────────────────────────────────

fn write_role(storage: &mut FlashStorage<'_>, role: Role) {
    let mut buf = [0u8; ROLE_RECORD_SIZE];
    buf[..4].copy_from_slice(&MAGIC);
    buf[4] = role.to_byte();
    // buf[5..8] stays zero (padding)

    // Erase the sector before writing (flash can only clear bits, not set them).
    if let Err(e) = storage.write(ROLE_OFFSET, &buf) {
        defmt::error!("Failed to write role to flash: {:?}", defmt::Debug2Format(&e));
        return;
    }
    info!("Role written to flash at 0x{:X}", ROLE_OFFSET);
}

fn read_role(storage: &mut FlashStorage<'_>) -> Option<Role> {
    let mut buf = [0u8; ROLE_RECORD_SIZE];
    if let Err(e) = storage.read(ROLE_OFFSET, &mut buf) {
        defmt::error!("Failed to read role from flash: {:?}", defmt::Debug2Format(&e));
        return None;
    }

    if buf[..4] != MAGIC {
        info!("No valid role magic at 0x{:X}", ROLE_OFFSET);
        return None;
    }

    Role::from_byte(buf[4])
}
