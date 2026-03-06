//! Role provisioning: read/write the device role from/to flash.
//!
//! Uses a fixed address in the NVS flash region. The role is stored as a
//! single byte with a 4-byte magic prefix for validation.

use beambench_protocol::Role;
use defmt::info;

/// Magic bytes to distinguish a provisioned role from erased flash (0xFF).
/// Used by the real flash implementation (not yet implemented).
#[allow(dead_code)]
const MAGIC: [u8; 4] = [0xBE, 0xA1, 0x01, 0x00]; // "bea1" + version 0

/// Determine the device role at boot.
///
/// - If a `role-*` cargo feature is active, write that role to flash and return it.
/// - Otherwise, read the role from flash.
pub fn resolve_role() -> Role {
    // Compile-time role from cargo feature (provisioning mode).
    let feature_role = feature_role();

    if let Some(role) = feature_role {
        info!("Provisioning role: {:?}", defmt::Debug2Format(&role));
        write_role(role);
        role
    } else {
        match read_role() {
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
//
// TODO: Replace with actual flash read/write using esp-storage or NVS when
// available for esp-hal. For now, we use a static mut as a placeholder that
// works for the provisioning flow (role is set at compile time via feature).
//
// The real implementation will:
// 1. Use `esp_storage::FlashStorage` to read/write a fixed offset in the NVS partition
// 2. Write: MAGIC ++ role_byte (5 bytes total)
// 3. Read: verify MAGIC, then parse role_byte

static STORED_ROLE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0xFF);
static STORED_MAGIC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn write_role(role: Role) {
    STORED_ROLE.store(role.to_byte(), core::sync::atomic::Ordering::SeqCst);
    STORED_MAGIC.store(true, core::sync::atomic::Ordering::SeqCst);
    info!("Role written to flash (placeholder)");
}

fn read_role() -> Option<Role> {
    if !STORED_MAGIC.load(core::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    Role::from_byte(STORED_ROLE.load(core::sync::atomic::Ordering::SeqCst))
}
