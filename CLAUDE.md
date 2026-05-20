# CLAUDE.md

Guidance for Claude Code (and other AI coding agents) when working in this repository.

## What this is

`go-modules` is a Rust CLI that runs on GOcontroll Moduline embedded controllers (aarch64 Linux) to scan, update, and overwrite firmware on plug-in modules over SPI. The whole app lives in `src/main.rs` (≈2800 lines, no submodules). `unsafe_code = "forbid"` is enforced via `Cargo.toml`.

It is one of three tightly coupled packages on the controller; understanding the boundaries matters:

| Package | Responsibility | Key persisted artifact |
|---|---|---|
| `go-modules` (this repo) | Scan SPI bus, identify modules, flash firmware, write `modules.json` | `/lib/firmware/gocontroll/modules.json` |
| `go-hardware-driver` | Read `modules.json` and run the per-slot module driver loop (100 Hz) | `/dev/shm/gocontroll/slot{N}/...` |
| `go-web-ui` | Browser-based config editor + status view | reads/writes `modules.json` |

`modules.json` is the single source of truth between all three. Schema is documented in `GOcontroll-Architecture/modules/configuration.md`; changes to its shape require coordinated changes across all three packages.

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

Drop `--release` for a debug build — `#[cfg(debug_assertions)]` blocks enable verbose firmware-upload error logging.

For quick controller-only testing without zigbuild, a plain `cargo build --release --target aarch64-unknown-linux-gnu` works on any glibc ≥ what the controller runs (trixie ships glibc 2.41 at the time of writing).

## Release flow

CI (`.github/workflows/build-package.yml`) triggers on `v*` tags only:
1. `cross build --release` for aarch64
2. Builds the .deb manually (not via `cargo deb` — embeds a postinst that runs `go-modules check`)
3. Creates a GitHub Release with the .deb attached
4. Dispatches `rebuild-index` to the GOcontroll apt repo so it picks up the package

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

### Module-type detection

`ModuleType::from_firmware()` maps the first three firmware-identifier bytes to a module type. Currently supported:

| Bytes | Article prefix | `module_type` (JSON) |
|---|---|---|
| `(20, 10, 1)` | 201001 | `input-6ch` |
| `(20, 10, 2)` | 201002 | `input-10ch` |
| `(20, 10, 3)` | 201003 | `input-4-20ma` |
| `(20, 20, 1)` | 202001 | `bridge-2ch` |
| `(20, 20, 2)` | 202002 | `output-6ch` |
| `(20, 20, 3)` | 202003 | `output-10ch` |
| `(20, 30, 3)` | 203003 | `ir-communication` |

When adding a new module type you must update **all four** of:

1. `ModuleType` enum + `from_firmware()` + `channel_count()` + `default_module()` + `default_channel()` (this repo).
2. `go-hardware-driver` registry entry — stub or full driver in `src/modules/*.c` plus an extern + table entry in `src/registry.c` plus the source file added to the Makefile.
3. `go-web-ui` MODULE_TYPE_NAMES + MODULE_SCHEMAS in `ModulineWebUI/handlers/modules.py`; regenerate `ModulineWebUI/data/module_pinning.json` from `pinning.md` using `scripts/generate_module_pinning.py`.
4. `GOcontroll-Architecture/modules/` — flip the status in `naming.md` + `configuration.md`, add a per-module spec markdown (use one of the existing specs as a template), regenerate `module_pinning.json` from the updated `pinning.md`.

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

### `modules.json` schema notes

Top-level `schema_version` is `"1.0.0"` (constant `MODULES_JSON_SCHEMA_VERSION`). Backwards-compatible additions inside slot entries are made by giving the field a `#[serde(default = "...")]` so older files load cleanly. Two examples already in the codebase:

- `enabled: bool` (per-slot, added v3.2.0). When `false`, `go-hardware-driver` (≥ 0.2.0) leaves the slot completely untouched: no reset, no bootloader skip, no init, no cyclic tick. Defaults to `true` via `default_enabled()` so older files load as "driver active" and the next save backfills the key.
- `name: ""` on each channel (added v3.1.0). Backfilled in `save_modules()` for older entries.

Conservative-defaults policy: `default_module()` and `default_channel()` return safe-by-default values (outputs disabled, supplies off, conservative current limits). Type-swap detection (`module_type` actually changed) wipes the slot config; same-type rescan preserves user edits via `merge_defaults_into` + `merge_channels`.

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

## Conventions / gotchas

- **Never break `modules.json` on a failed scan.** A failure to read a single slot must not result in writing `SlotEntry::empty()` everywhere — `SCAN_HAD_ERRORS` guards the write in `get_modules_and_save`. If you add a new error path that returns all-None modules, set this flag.
- **Schema fields are append-only.** Removing a field from `ModulesJson` / `SlotEntry` requires a `schema_version` major bump (which the other two packages need to be updated for in the same release window). Adding fields with a default is free.
- **Detection-derived fields are never user-editable.** `slot`, `module_type`, `article_number`, `hardware_version`, `firmware_version`, `firmware`, `manufacturer`, `qr_front`, `qr_back` are owned by the scanner. The Web UI's `READ_ONLY_SLOT_KEYS` mirrors this.
- **Don't introduce `unsafe` blocks.** `Cargo.toml` forbids them at the crate level; needing one is a signal that the design is wrong.
- **Controller-only.** Don't add functionality that only works on the dev host — there is no dev-host runtime for this CLI. CI cross-compiles only.

## Related architecture documentation

The authoritative module schemas live in the `GOcontroll-Architecture` repo under `modules/`:

- `configuration.md` — `modules.json` JSON schema and runtime contract
- `naming.md` — article-number format + module-type enumeration
- `pinning.md` — per-module pin assignments per controller slot
- `spi.md` — module SPI protocol reference
- `input-6ch.md`, `input-10ch.md`, `input-4-20ma.md`, `bridge-2ch.md`, `output-6ch.md`, `output-10ch.md`, `ir-communication.md` — per-module specs (JSON schema, wire-encoding, defaults, open questions)

When changing detection logic, defaults, or schema fields, update the matching architecture document(s) in the same change.
