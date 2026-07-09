# Visual & Layout Changes Verification

Golden frames are the fast oracle for visual, layout, rendering, and typography
changes: they run on a plain host, cover both boards, and catch regressions no
unit test will. Work through the steps below before trusting such a change.

## Host target is mandatory

`.cargo/config.toml` pins `[build] target = "riscv32imc-unknown-none-elf"`, so
every host-side `cargo` command needs an explicit `--target`. Omit it and the
emulator builds for the `no_std` firmware target and fails deep in `serde` with
thousands of "cannot find `Some` in this scope" errors.

```sh
HOST="$(rustc -vV | sed -n 's/^host: //p')"   # aarch64-apple-darwin on an Apple-silicon Mac
```

CI passes the same thing as `$HOST_TARGET` (`x86_64-unknown-linux-gnu`).

## Format and lint

```sh
cargo fmt --all
cargo clippy -p app-core -p display -p ui -p proto --all-targets --target "$HOST" -- -D warnings
tools/cargo.sh clippy -p fw -- -D warnings
tools/cargo.sh clippy -p fw --features device-x3 -- -D warnings
```

The host clippy set is exactly CI's `--workspace --exclude hal-ext --exclude fw`.
The firmware runs lint `hal-ext` and the shared crates on the RISC-V target as a
side effect of `-p fw`, and the `device-x3` run is what catches breakage in
board-gated panel and geometry code — always run both boards.

`cargo fmt --all` and `cargo clippy --workspace` only reach the six root-workspace
crates. `tools/emulator`, `tools/preview`, and `tools/web-emulator` each declare
their own `[workspace]`, sit outside CI's fmt and clippy jobs, and are not
fmt-clean today. Match local style when editing them; do not run `cargo fmt`
across those trees or you will bury the real change in reformatting noise.

## Golden frames

X4 and X3 frames share `fixtures/golden/`; X3 files carry an `-x3` suffix. Check
both boards:

```sh
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features -- --scenario fixtures/scenarios --check fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features --features device-x3 -- --scenario fixtures/scenarios --check fixtures/golden
```

When a frame changes on purpose, point `--dump` straight at the golden directory
for each board — it names every file from the scenario stem plus the board
suffix, and rewrites unchanged frames byte-for-byte, so `git diff` shows exactly
the frames the change touched. Review those images, then re-run the two `--check`
commands above.

```sh
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features -- --scenario fixtures/scenarios --dump fixtures/golden
cargo run --manifest-path tools/emulator/Cargo.toml --target "$HOST" \
  --no-default-features --features device-x3 -- --scenario fixtures/scenarios --dump fixtures/golden
```

`--check` is an exact PNG byte comparison against the emulator's own encoder, so
goldens must come from `--dump`. Never hand-edit one or round-trip it through an
image editor. `--present-dump` writes the panel-presented image instead of the
framebuffer — useful when debugging refresh and ghosting, never a golden.

To eyeball a single scenario, pass a `.toml` file to `--scenario` and a `.png` to
`--dump`; `--gui` needs `--features gui`.

## Reading-page goldens

Typography and reading-layout changes also need the reading-page goldens, which
`--scenario` does not cover. **No CI job runs these** — the root `cargo test
--workspace` never reaches `tools/emulator`, and the golden-frames job only runs
scenarios — so this check exists only if you run it.

```sh
cargo test --manifest-path tools/emulator/Cargo.toml --target "$HOST" --test reading_golden
cargo test --manifest-path tools/emulator/Cargo.toml --target "$HOST" --test reading_golden \
  --no-default-features --features device-x3
```

Regenerate after an intentional typography change by prefixing each with
`REGEN_READING_GOLDEN=1`, then re-run both without it to confirm they pass.

## Browser emulator

`ui` and `display` changes also feed the wasm emulator, and the device chrome
(bezel, rocker buttons) lives in `web/`. If the change is visible there, build
and look at both boards:

```sh
rustup target add wasm32-unknown-unknown
tools/build-web.sh _site
python3 -m http.server -d _site 8000    # then open http://localhost:8000/?board=x3
```

## Hardware

Golden frames prove the render, not the panel. After display flush, refresh-plan,
or sleep-screen changes, follow up with the short hardware benches in
`docs/agents/bench.md`.
