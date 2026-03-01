# Beambench — top-level justfile
# Delegates to per-crate justfiles in software/

default:
    just --list

# Build all firmware crates
build-firmware:
    just software/stepper/build
    just software/tx/build
    just software/rx/build

# Build PC application (backend + frontend)
build-pc:
    just software/pc/build

# Check all crates compile
check:
    just software/protocol/check
    just software/stepper/check
    just software/tx/check
    just software/rx/check
    just software/pc/check
