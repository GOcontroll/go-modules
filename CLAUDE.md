# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`go-modules` is a Rust CLI that runs on GOcontroll Moduline embedded controllers (aarch64 Linux) to scan, update, and overwrite firmware on plug-in modules over SPI. The whole app lives in `src/main.rs` (~2200 lines, no submodules). `unsafe_code = "forbid"` is enforced via `Cargo.toml`.

## Build / package

Target is **always** `aarch64-unknown-linux-gnu`. Nothing is meant to run on the dev host.

```bash
# Standard build (needs aarch64-linux-gnu-gcc — see .cargo/config.toml)
RUSTFLAGS="-Zlocation-detail=none" cargo +nightly build \
  -Z build-std=std,panic_abort --target aarch64-unknown-linux-gnu --release

# Glibc-flexible build via zig (replace 2.31 with required glibc; 2.31 = bullseye)
RUSTFLAGS="-Zlocation-detail=none -C target-cpu=cortex-a53" cargo +nightly zigbuild \
  -Z build-std=std,panic_abort --target aarch64-unknown-linux-gnu.2.31 --release

# Compress (debug builds compress slowly)
upx --best --lzma target/aarch64-unknown-linux-gnu/release/go-modules

# .deb
cargo deb --no-build --target aarch64-unknown-linux-gnu --no-strip
dpkg-sig --sign builder target/aarch64-unknown-linux-gnu/debian/go-modules_*_arm64.deb
```

Drop `--release` for a debug build (verbose firmware-upload error logging, see `#[cfg(debug_assertions)]` blocks).

## Release flow

CI (`.github/workflows/build-package.yml`) triggers on `v*` tags only:
1. `cross build --release` for aarch64
2. Builds the .deb manually (not via `cargo deb` — embeds a postinst that runs `go-modules check`)
3. Creates a GitHub Release with the .deb attached
4. Dispatches `rebuild-index` to `GOcontroll/go-apt` so the apt repo picks it up

Bump `version` in `Cargo.toml` and tag `vX.Y.Z` to ship. The tag drives the package version — they must match.

## Runtime architecture

### Three controller types

Detected from `/sys/firmware/devicetree/base/hardware` in `detect_controller()`. **Both old and new names are accepted** (this is required for backwards compatibility — don't drop the old names):

| New name | Old name         | Slots | Enum variant              |
|----------|------------------|-------|---------------------------|
| L4       | Moduline IV      | 8     | `ControllerTypes::ModulineIV` |
| M1       | Moduline Mini    | 4     | `ControllerTypes::ModulineMini` |
| HMI1     | Moduline Display | 2     | `ControllerTypes::ModulineDisplay` |

The `ControllerTypes` repr is `slot_count + 1` so `1..controller as usize` iterates slots — keep this invariant if adding controllers.

### Per-slot SPI + GPIO wiring

Each slot maps to a hard-coded `(spidev, gpiochip, line)` triple inside `Module::new()`. The mapping differs per controller and slot — this table is the source of truth for hardware bring-up. Module reset goes through `/sys/class/leds/ResetM-{slot}/brightness`.

### Firmware upload protocol (`overwrite_module`)

The new pipelined protocol is the trickiest part of the codebase. Because SPI is parallel, the response to message N arrives during message N+1, so the code tracks `line_number` (currently sending) and `firmware_line_check` (line whose response is expected next), swapping them on error and using `firmware_error_counter` parity to decide when to swap back. The doc-comment on `overwrite_module` has ASCII diagrams for normal flow, single/odd errors, even errors, end-of-firmware, and end-of-firmware-with-error — read those before touching this function.

Protocol message-type bytes (first byte of `tx_buf`): `9` = info request, `19` = cancel upload, `29` = wipe + set new sw version, `39` = firmware line, `49` = status poll. Bootloader response with `rx_buf[6] == 20` distinguishes "still in bootloader" from "jumped to firmware".

Firmware filenames encode the version: `20-10-1-5-0-0-9.srec` = 4 hardware bytes (`20-10-1-5`) + 3 software bytes (`0-0-9`). See `FirmwareVersion` in `main.rs`.

### Cloud firmware (`check` command)

Fetches `https://firmware.gocontroll.com/modules/manifest.json` → per-module sub-manifests → downloads `.srec` files into `/lib/firmware/gocontroll/`, validating SHA256 against the manifest. `check` runs **before** `detect_controller()` and service shutdown in `main()`, so it works on any host with network access — preserve that ordering.

### Service management

Snapshots the active state of `nodered` and `go-simulink` systemd services at startup, stops them, and restarts whatever was running on exit. Both the normal exit path and the `ctrlc` handler (and `force_quit()` for crossterm raw-mode escapes) call `restart_services` — keep that property when adding exit paths.

### Persisted state

- `/lib/firmware/gocontroll/modules.json` — JSON layout (current format)
- `/usr/module-firmware/modules.txt` — legacy colon-separated format kept alive for older Node-RED installs

`save_modules()` writes both. Don't drop the legacy file without coordinating with Node-RED.

### TUI

Built on `crossterm` + `indicatif`. `MenuMode::Main` (q/Esc/Ctrl+C quit the app) vs `MenuMode::Sub` (Esc/Left/q go back to parent). Sub-actions never close the app — the main loop returns to the menu. Non-TTY stdin falls back to a numbered-prompt mode automatically (used when invoked from scripts/postinst).

## CLI surface

```
go-modules                                # interactive TUI (recommended)
go-modules scan                           # list modules
go-modules update all | <slot>            # update all or one slot
go-modules overwrite <slot> <fw.srec>     # force-flash specific firmware (downgrade path)
go-modules check [-v | --verbose]         # fetch latest firmware from cloud
```
