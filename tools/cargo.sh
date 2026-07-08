#!/usr/bin/env bash
# Run Cargo with the rustup stable toolchain and matching compiler tools.
#
# Some development machines have another cargo/rustc/rustdoc earlier on PATH.
# That can make firmware builds fail with "can't find crate for `core`" even
# when rustup has installed riscv32imc-unknown-none-elf. Force Cargo, rustc,
# and rustdoc to come from the same rustup toolchain.
set -euo pipefail

TOOLCHAIN="${RUSTUP_TOOLCHAIN:-stable}"
RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)"
RUSTDOC="$(rustup which --toolchain "$TOOLCHAIN" rustdoc)"
export RUSTC
export RUSTDOC

exec rustup run "$TOOLCHAIN" cargo "$@"
