v3.1.4
 - Fixed: `go-modules <command>` (scan/update/overwrite) now exits after the
   action instead of falling back into the interactive TUI menu. Required so
   scripts and services (e.g. `go-provision-server`) can shell out to
   `go-modules scan` and have the process terminate cleanly. The result view
   also no longer blocks on an Esc keypress when invoked with a CLI arg
   (applies in TTY-mode too, e.g. interactive SSH). Invoking `go-modules`
   with no arguments keeps the existing TUI loop unchanged.

v3.1.3
 - Fixed: a scan that hits hardware errors (SPI device open, GPIO line open,
   SPI transfer) no longer overwrites `modules.json`. Previously a failed
   scan returned all `None` slots, which `save_modules` interpreted as
   "modules physically removed" and reset every slot to `SlotEntry::empty()`,
   wiping all user-edited channel/module config. Now error sites set a
   `SCAN_HAD_ERRORS` flag and `get_modules_and_save` skips the write,
   printing a stderr hint instead.

v3.1.2
 - Fixed: `modules.json` user-edited config no longer overwritten by defaults
   on rescan. Per-channel `name`, `func`, `pull_up`, `pull_down`, `voltage_range`,
   `pulses_per_rotation`, `analog_filter_samples` (inputs) and `current_max`,
   `peak_current`, `peak_time`, `fast_loop_*` (outputs) plus module-level
   `sensor_supply_*` and `frequency_pairs` are preserved; only missing keys are
   filled with conservative defaults. Full reset still happens when the detected
   module type genuinely differs from the previously-recorded one.
 - Fixed: a `modules.json` that fails to parse no longer silently wipes all
   slot config. The unparseable file is backed up to `modules.json.bak.<ts>`
   and a stderr message is printed before falling back to an empty doc.
 - Top-level `ModulesJson` fields are `serde(default)` so a missing
   `schema_version` / `controller` field does not fail the whole parse.

v3.0.0
 - Added `check` command: fetches the latest firmware for all module hardware versions
   from the GOcontroll cloud (firmware.gocontroll.com) into /lib/firmware/gocontroll/
 - SHA256 checksum validation for all downloaded firmware files
 - Use `check --verbose` (or `-v`) to display release dates and changelogs
 - Fixed typos in output messages

v2.2.0
 - Debian 11 compatibility improvements

v2.1.0
 - Added Multibus module support

v2.0.0
 - Firmware locations have been moved to /lib/firmware/gocontroll/
 - The modules file has been moved to /lib/gocontroll/modules
 - Now errors if it cannot find any firmwares instead of quietly failing

v1.1.0
 - Now wipes the module firmware if it is corrupted so it will try again with a `go-modules update all` call

v1.0.1
 - Fixed Moduline Display match string

v1.0.0
 - First release
 - Can work with the old slow bootloader aswell as the new fast one