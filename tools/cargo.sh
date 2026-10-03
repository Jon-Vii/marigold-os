#!/usr/bin/env bash
# Run Cargo with the pinned rustup toolchain and matching compiler tools.
#
# Some development machines have another cargo/rustc/rustdoc earlier on PATH.
# That can make firmware builds fail with "can't find crate for `core`" even
# when rustup has installed riscv32imc-unknown-none-elf. Force Cargo, rustc,
# and rustdoc to come from the same rustup toolchain.
set -euo pipefail

# Default to the toolchain pinned in rust-toolchain.toml so wrapper builds and
# plain `cargo` inside the repo use the same compiler.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PINNED="$(sed -n 's/^channel *= *"\(.*\)"/\1/p' "$REPO_ROOT/rust-toolchain.toml")"
TOOLCHAIN="${RUSTUP_TOOLCHAIN:-${PINNED:-stable}}"

if ! command -v rustup >/dev/null 2>&1; then
  cat >&2 <<'EOF'
error: rustup is required to build MarigoldOS firmware.

Install Rust with rustup, then install this repo's firmware target:
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup target add riscv32imc-unknown-none-elf
EOF
  exit 127
fi

if ! CARGO="$(rustup which --toolchain "$TOOLCHAIN" cargo 2>/dev/null)"; then
  cat >&2 <<EOF
error: cargo is not installed for the '$TOOLCHAIN' rustup toolchain.

Install or repair the toolchain, then retry:
  rustup toolchain install $TOOLCHAIN --profile default
  rustup target add --toolchain $TOOLCHAIN riscv32imc-unknown-none-elf
EOF
  exit 127
fi

RUSTC="$(rustup which --toolchain "$TOOLCHAIN" rustc)"
RUSTDOC="$(rustup which --toolchain "$TOOLCHAIN" rustdoc)"
export RUSTC
export RUSTDOC

exec "$CARGO" "$@"
