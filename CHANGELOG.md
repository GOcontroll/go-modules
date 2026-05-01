v3.1.0 (unreleased)
 - TUI is now persistent: the tool no longer exits after a single action.
   The main menu redraws after each action so multiple operations can be
   performed in a single session.
 - New menu layout per apt look-and-feel spec § 1–§ 2: separator / prompt /
   separator / options / separator / footer. The prompt line ("Select what you
   want to do") is replaced by the chosen action's name during execution.
 - Key bindings revised: ← / q navigate one menu back; Esc / Ctrl-C / Ctrl-D
   close the tool. Enter still selects.
 - Modules are re-scanned at the top of every loop iteration so the overview
   table reflects post-update state.
 - CLI mode preserved (one-shot, exits at end). Exit codes on rare hard-error
   paths (corrupted firmware) are now identical to soft errors — see the PR
   description.

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