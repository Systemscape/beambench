# Beambench — top-level justfile
# Delegates to per-crate justfiles in software/

default:
    just --list

# Build unified firmware
build-firmware:
    just software/firmware/build

# Build PC application (backend + frontend)
build-pc:
    just software/pc/build

# Run all host-side tests (protocol + PC)
test:
    just software/protocol/test
    just software/pc/test-all

# Check all crates compile
check:
    just software/protocol/check
    just software/firmware/check
    just software/pc/check
