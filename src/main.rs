use std::{
    env,
    fmt::{Display, Write},
    fs::{self, File},
    io::{self, IsTerminal, Write as _},
    mem,
    process::{exit, Command},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

static NODERED_WAS_RUNNING: AtomicBool = AtomicBool::new(false);
static SIMULINK_WAS_RUNNING: AtomicBool = AtomicBool::new(false);
static HARDWARE_DRIVER_WAS_RUNNING: AtomicBool = AtomicBool::new(false);

/// True when the process was invoked with a CLI command (scan/update/overwrite)
/// instead of dropping into the TUI. Read by `show_view` to skip the
/// wait-for-key step so scripted callers don't have to send an Esc to exit.
static STARTED_FROM_CLI: AtomicBool = AtomicBool::new(false);

/// Set by any hardware-error path during a scan (SPI device open / GPIO
/// chip+line open / SPI transfer). Checked by `get_modules_and_save` to
/// decide whether to persist scan results — a partially-failed scan must
/// not overwrite `modules.json`, because the all-`None` slots would be
/// indistinguishable from "modules physically removed" and would wipe
/// existing config.
static SCAN_HAD_ERRORS: AtomicBool = AtomicBool::new(false);

fn flag_scan_error() {
    SCAN_HAD_ERRORS.store(true, Ordering::Relaxed);
}

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
    terminal,
};

use futures::StreamExt;

use spidev::{SpiModeFlags, Spidev, SpidevOptions, SpidevTransfer};

use indicatif::{MultiProgress, ProgressBar, ProgressState, ProgressStyle};

use tokio::{task, task::JoinSet, time, time::timeout};

use gpio_cdev::{AsyncLineEventHandle, Chip, EventRequestFlags, LineRequestFlags};

use serde::{Deserialize, Serialize};

use serde_json::{json, Value};

use sha2::{Digest, Sha256};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_banner() {
    println!("\x1b[38;2;255;102;0m  GOcontroll Module Manager  v{}\x1b[0m", VERSION);
}

const SEP: &str = "  ------------------------------------";

#[derive(Clone, Copy, PartialEq, Eq)]
enum MenuMode {
    /// Top-level menu: q/Ctrl+C close the app, Esc/Left are ignored.
    Main,
    /// Sub-menu: Esc/Left/q/Ctrl+C all return to the parent screen.
    Sub,
}

enum SelectResult<T> {
    Selected(T),
    /// Esc/Left/q/Ctrl+C in a Sub menu.
    Back,
    /// q/Ctrl+C/Ctrl+D in the Main menu — used to exit the app.
    Quit,
}

fn draw_menu<T: Display>(options: &[T], selected: usize, first: bool, mode: MenuMode) {
    let mut stdout = io::stdout();
    let total_lines = options.len() as u16 + 3;
    if !first {
        queue!(stdout, cursor::MoveUp(total_lines)).unwrap();
    }
    queue!(
        stdout,
        terminal::Clear(terminal::ClearType::CurrentLine),
        Print(format!("{}\r\n", SEP)),
    )
    .unwrap();
    for (i, option) in options.iter().enumerate() {
        queue!(stdout, terminal::Clear(terminal::ClearType::CurrentLine)).unwrap();
        if i == selected {
            queue!(
                stdout,
                SetForegroundColor(Color::Cyan),
                Print(format!("  \u{25ba} {}\r\n", option)),
                ResetColor,
            )
            .unwrap();
        } else {
            queue!(stdout, Print(format!("    {}\r\n", option))).unwrap();
        }
    }
    let hint = match mode {
        MenuMode::Main => "  \u{2191}/\u{2193} navigate   Enter select   Esc quit\r\n",
        MenuMode::Sub => {
            "  \u{2191}/\u{2193} navigate   Enter select   \u{2190}/Esc back\r\n"
        }
    };
    queue!(
        stdout,
        terminal::Clear(terminal::ClearType::CurrentLine),
        Print(format!("{}\r\n", SEP)),
        terminal::Clear(terminal::ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        Print(hint),
        ResetColor,
    )
    .unwrap();
    stdout.flush().unwrap();
}

fn force_quit() -> ! {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(io::stdout(), cursor::Show);
    restart_services(
        NODERED_WAS_RUNNING.load(Ordering::Relaxed),
        SIMULINK_WAS_RUNNING.load(Ordering::Relaxed),
        HARDWARE_DRIVER_WAS_RUNNING.load(Ordering::Relaxed),
    );
    exit(-1);
}

fn run_select<T: Display>(prompt: &str, mut options: Vec<T>, mode: MenuMode) -> SelectResult<T> {
    if !io::stdin().is_terminal() {
        println!("{}", prompt);
        for (i, opt) in options.iter().enumerate() {
            println!("  {}. {}", i + 1, opt);
        }
        loop {
            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_err() {
                return match mode {
                    MenuMode::Main => SelectResult::Quit,
                    MenuMode::Sub => SelectResult::Back,
                };
            }
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= options.len() {
                    return SelectResult::Selected(options.remove(n - 1));
                }
            }
        }
    }
    let mut selected = 0usize;
    terminal::enable_raw_mode().unwrap();
    let _ = execute!(io::stdout(), cursor::Hide);
    draw_menu(&options, selected, true, mode);
    loop {
        let evt = event::read();
        let cleanup = || {
            let _ = terminal::disable_raw_mode();
            let _ = execute!(io::stdout(), cursor::Show);
        };
        match evt {
            Ok(Event::Key(KeyEvent { code: KeyCode::Up, .. })) => {
                if selected > 0 {
                    selected -= 1;
                }
                draw_menu(&options, selected, false, mode);
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Down, .. })) => {
                if selected + 1 < options.len() {
                    selected += 1;
                }
                draw_menu(&options, selected, false, mode);
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Enter, .. })) => {
                cleanup();
                return SelectResult::Selected(options.remove(selected));
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Esc, .. })) => {
                cleanup();
                return match mode {
                    MenuMode::Main => SelectResult::Quit,
                    MenuMode::Sub => SelectResult::Back,
                };
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Left, .. })) if mode == MenuMode::Sub => {
                cleanup();
                return SelectResult::Back;
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }))
            | Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })) => {
                cleanup();
                force_quit();
            }
            _ => {}
        }
    }
}

/// Render a static informational view inside the frame and wait for the user
/// to press a back key. Sub-menu semantics: never closes the app.
fn show_view(lines: &[String]) {
    let mut stdout = io::stdout();
    println!("{}", SEP);
    for line in lines {
        println!("  {}", line);
    }
    println!("{}", SEP);

    if !io::stdin().is_terminal() || STARTED_FROM_CLI.load(Ordering::Relaxed) {
        // Non-TTY or CLI-invoked single-shot: skip the "Esc back" hint and
        // return immediately instead of waiting for a keypress.
        let _ = stdout.flush();
        return;
    }

    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("  \u{2190}/Esc back\r\n"),
        ResetColor,
    )
    .unwrap();
    let _ = stdout.flush();

    terminal::enable_raw_mode().unwrap();
    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Esc | KeyCode::Left | KeyCode::Enter,
                ..
            })) => {
                let _ = terminal::disable_raw_mode();
                return;
            }
            Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c') | KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })) => {
                force_quit();
            }
            _ => {}
        }
    }
}

fn run_confirm(prompt: &str, default: bool) -> bool {
    if !io::stdin().is_terminal() {
        return default;
    }
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    print!("  {} {} ", prompt, hint);
    io::stdout().flush().unwrap();
    let mut input = String::new();
    match io::stdin().read_line(&mut input) {
        Ok(_) => {
            let t = input.trim().to_lowercase();
            if t.is_empty() {
                default
            } else {
                t == "y" || t == "yes"
            }
        }
        Err(_) => default,
    }
}

const DUMMY_MESSAGE: [u8; 5] = [0; 5];

const BOOTMESSAGE_LENGTH: usize = 46;
const BOOTMESSAGE_LENGTH_CHECK: usize = 61;

const SLOT_PROMPT: &str = "Which slot to overwrite?";

const FIRMWARE_DIR: &str = "/lib/firmware/gocontroll/";
const CLOUD_BASE_URL: &str = "https://firmware.gocontroll.com";

const USAGE: &str = "Usage:
go-modules <command> [subcommands]
or
go-modules

commands:
scan							Scan the modules in the controller
update <all/slot#>				In case of all, try to update all modules, in case of a slot number, try to update that slot specifically
overwrite <slot> <firmware>		Overwrite the firmware in <slot> with <firmware>
check [--verbose/-v]			Fetch latest firmware for all modules from the GOcontroll cloud.
								Downloads to /lib/firmware/gocontroll/ and validates checksums.
								Use --verbose or -v to show release dates and changelogs.

examples:
go-modules										Use with the tui (recommended)
go-modules scan									Scan all modules in the controller
go-modules update all							Try to update all modules in the controller
go-modules update 1								Try to update the module in slot 1
go-modules overwrite 1 20-10-1-5-0-0-9.srec		Forcefully overwrite the module in slot 1 with 20-10-1-5-0-0-9.srec (can be used to downgrade modules)
go-modules check								Fetch latest firmware files from the GOcontroll cloud
go-modules check --verbose						Fetch latest firmware files and show release dates and changelogs";

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct FirmwareVersion {
    firmware: [u8; 7],
}

impl FirmwareVersion {
    /// create a FirmwareVersion from a filename for example 20-10-1-5-0-0-9.srec
    fn from_filename(name: String) -> Option<Self> {
        let mut firmware: [u8; 7] = [0u8; 7];
        if let Some(no_extension) = name.split('.').next() {
            let numbers = no_extension.split('-');

            for (i, num) in numbers.enumerate() {
                let part = firmware.get_mut(i)?;
                if let Ok(file_part) = num.parse::<u8>() {
                    *part = file_part;
                } else {
                    return None;
                }
            }
        }
        Some(Self { firmware })
    }

    /// get the software part of the firmware version
    fn get_software(&self) -> &[u8] {
        self.firmware.get(4..7).unwrap()
    }

    /// get the hardware part of the firmware version
    fn get_hardware(&self) -> &[u8] {
        self.firmware.get(0..4).unwrap()
    }

    /// get a string version of the firmware version like 20-10-1-5-0-0-9
    fn as_string(&self) -> String {
        format!(
            "{}-{}-{}-{}-{}-{}-{}",
            self.firmware[0],
            self.firmware[1],
            self.firmware[2],
            self.firmware[3],
            self.firmware[4],
            self.firmware[5],
            self.firmware[6]
        )
    }

    /// get a filename version of the firmware version like 20-10-1-5-0-0-9.srec
    fn as_filename(&self) -> String {
        format!("{}.srec", self.as_string())
    }
}

impl Display for FirmwareVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_filename())
    }
}

enum CommandArg {
    Scan,
    Update,
    Overwrite,
    Check,
}

//impl display to make sure we don't have capital letters, as the don't match the commands
impl Display for CommandArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                Self::Scan => "scan",
                Self::Update => "update",
                Self::Overwrite => "overwrite",
                Self::Check => "check",
            }
        )
    }
}

enum UploadError {
    FirmwareCorrupted(u8),
    FirmwareUntouched(u8),
}

#[repr(usize)]
#[derive(Copy, Clone)]
enum ControllerTypes {
    ModulineIV = 9,
    ModulineMini = 5,
    ModulineDisplay = 3,
}

/// modules.json schema version (configuration.md §7).
const MODULES_JSON_SCHEMA_VERSION: &str = "1.0.0";

/// Module type identifiers from configuration.md §4. Mapped from the first
/// 3 firmware bytes — see `ModuleType::from_firmware`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum ModuleType {
    #[serde(rename = "input-6ch")]
    Input6Ch,
    #[serde(rename = "input-10ch")]
    Input10Ch,
    #[serde(rename = "input-4-20ma")]
    Input420Ma,
    #[serde(rename = "bridge-2ch")]
    Bridge2Ch,
    #[serde(rename = "output-6ch")]
    Output6Ch,
    #[serde(rename = "output-10ch")]
    Output10Ch,
    #[serde(rename = "ir-communication")]
    IrCommunication,
}

impl ModuleType {
    fn from_firmware(fw: &FirmwareVersion) -> Option<Self> {
        let hw = fw.get_hardware();
        match (hw[0], hw[1], hw[2]) {
            (20, 10, 1) => Some(Self::Input6Ch),
            (20, 10, 2) => Some(Self::Input10Ch),
            (20, 10, 3) => Some(Self::Input420Ma),
            (20, 20, 1) => Some(Self::Bridge2Ch),
            (20, 20, 2) => Some(Self::Output6Ch),
            (20, 20, 3) => Some(Self::Output10Ch),
            (20, 30, 3) => Some(Self::IrCommunication),
            _ => None,
        }
    }

    fn channel_count(self) -> usize {
        match self {
            Self::Input6Ch | Self::Output6Ch => 6,
            Self::Input10Ch | Self::Input420Ma | Self::Output10Ch => 10,
            Self::Bridge2Ch | Self::IrCommunication => 2,
        }
    }

    /// Conservative-default `module` object per configuration.md §5. None for
    /// types that have no module-level configuration.
    fn default_module(self) -> Option<Value> {
        match self {
            Self::Input6Ch => Some(json!({
                "sensor_supply_1": "off",
                "sensor_supply_2": "off",
                "sensor_supply_3": "off",
            })),
            Self::Input10Ch => Some(json!({
                "sensor_supply_1": "off",
                "sensor_supply_2": "off",
            })),
            Self::Input420Ma => Some(json!({
                "sensor_supply_1": "off",
                "sensor_supply_2": "off",
                "sensor_supply_3": "off",
                "sensor_supply_4": "off",
                "sensor_supply_5": "off",
            })),
            Self::Output6Ch => Some(json!({
                "frequency_pairs": ["100Hz", "100Hz", "100Hz"],
            })),
            Self::Output10Ch => Some(json!({
                "frequency_pairs": ["100Hz", "100Hz", "100Hz", "100Hz", "100Hz"],
            })),
            Self::Bridge2Ch => None,
            Self::IrCommunication => Some(json!({
                "ir_output_type":   "direct",
                "protocol_id":      "sae_j2799",
                "software_version": "1.01",
                "tank_volume":      400,
                "receptable_type":  "h35",
                "can_active":       false,
                "can_bitrate":      "250k",
                "frequency_pairs":  ["100Hz"],
            })),
        }
    }

    /// Conservative-default `channels[i]` entry per configuration.md §5. Each
    /// channel carries a `name` alias (empty by default) that integrators set
    /// to a human-readable identifier ("voorpomp", "throttle"…) used by
    /// downstream consumers.
    fn default_channel(self, channel: u8) -> Value {
        match self {
            Self::Input6Ch => json!({
                "channel": channel,
                "func": "mv_analog",
                "voltage_range": "5V",
                "pull_up": "none",
                "pull_down": "none",
                "pulses_per_rotation": 0,
                "analog_filter_samples": 0,
                "name": "",
            }),
            Self::Input10Ch => json!({
                "channel": channel,
                "func": "mv_analog",
                "pull_up": "none",
                "pull_down": "none",
                "name": "",
            }),
            Self::Input420Ma => json!({
                "channel": channel,
                "name": "",
            }),
            Self::Bridge2Ch => json!({
                "channel": channel,
                "func": "disabled",
                "freq": "100Hz",
                "name": "",
            }),
            Self::Output6Ch => json!({
                "channel": channel,
                "func": "disabled",
                "current_max": 4000,
                "peak_current": 1200,
                "peak_time": 1500,
                "fast_loop_module": 0,
                "fast_loop_channel": 0,
                "name": "",
            }),
            Self::Output10Ch => json!({
                "channel": channel,
                "func": "disabled",
                "name": "",
            }),
            Self::IrCommunication => json!({
                "channel": channel,
                "func": "disabled",
                "peak_duty": 1000,
                "peak_time": 500,
                "name": "",
            }),
        }
    }

    fn default_channels(self) -> Vec<Value> {
        (1..=self.channel_count() as u8)
            .map(|c| self.default_channel(c))
            .collect()
    }
}

/// Top-level shape of /lib/firmware/gocontroll/modules.json (configuration.md §2).
/// All fields are `serde(default)` so a partially-corrupted or older file does
/// not nuke the entire deserialization — `save_modules` re-stamps the top-level
/// fields anyway, and per-slot fields each carry their own defaults.
#[derive(Serialize, Deserialize)]
struct ModulesJson {
    #[serde(default)]
    schema_version: String,
    #[serde(default)]
    controller: String,
    #[serde(default)]
    slots: Vec<SlotEntry>,
}

/// One entry in `slots[]` (configuration.md §3). Empty slots carry only the
/// legacy detection/identification fields (slot, firmware, manufacturer, qr_*);
/// populated slots add the type-derived fields and the config block.
#[derive(Serialize, Deserialize)]
struct SlotEntry {
    slot: u8,
    /// Full 7-byte firmware identifier (e.g. `"20-20-2-6-2-2-0"`). `""` when empty.
    #[serde(default)]
    firmware: String,
    /// Manufacturer code reported by the module (bytes 13..17 of bootloader info).
    #[serde(default)]
    manufacturer: u32,
    /// QR code front (bytes 17..21 of bootloader info).
    #[serde(default)]
    qr_front: u32,
    /// QR code back (bytes 21..25 of bootloader info).
    #[serde(default)]
    qr_back: u32,
    /// Hardware-driver opt-out. When false `go-hardware-driver` leaves this
    /// slot completely untouched (no reset, no bootloader skip, no init, no
    /// cyclic tick). Defaults to true on missing-key so existing modules.json
    /// files written by go-modules <3.2.0 are migrated transparently.
    #[serde(default = "default_enabled")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module_type: Option<ModuleType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    article_number: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hardware_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    firmware_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    module: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    channels: Vec<Value>,
}

fn default_enabled() -> bool { true }

impl SlotEntry {
    fn empty(slot: u8) -> Self {
        Self {
            slot,
            firmware: String::new(),
            manufacturer: 0,
            qr_front: 0,
            qr_back: 0,
            enabled: true,
            module_type: None,
            article_number: None,
            hardware_version: None,
            firmware_version: None,
            label: None,
            module: None,
            channels: Vec::new(),
        }
    }
}

fn controller_schema_name(c: &ControllerTypes) -> &'static str {
    match c {
        ControllerTypes::ModulineIV => "moduline-l4",
        ControllerTypes::ModulineMini => "moduline-m1",
        ControllerTypes::ModulineDisplay => "moduline-hmi1",
    }
}

/// 8-digit article number encoded from firmware bytes 0..4
/// (e.g. bytes 20-10-1-5 → 20100105). See naming.md.
fn article_number_from_firmware(fw: &FirmwareVersion) -> u32 {
    let hw = fw.get_hardware();
    hw[0] as u32 * 1_000_000 + hw[1] as u32 * 10_000 + hw[2] as u32 * 100 + hw[3] as u32
}

/// Hardware version string `"1.{vv}"` derived from byte 3 of the firmware
/// version. Only byte 3 carries the hardware version per naming.md §3 — the
/// preceding bytes are part of the 6-digit module-type prefix.
fn hardware_version_string(fw: &FirmwareVersion) -> String {
    let hw = fw.get_hardware();
    format!("1.{:02}", hw[3])
}

/// SemVer firmware version from bytes 4,5,6.
fn firmware_version_string(fw: &FirmwareVersion) -> String {
    let sw = fw.get_software();
    format!("{}.{}.{}", sw[0], sw[1], sw[2])
}

struct Module {
    slot: u8,
    spidev: Spidev,
    interrupt: AsyncLineEventHandle,
    firmware: FirmwareVersion,
    manufacturer: u32,
    qr_front: u32,
    qr_back: u32,
}

/// Cloud manifest structs for firmware.gocontroll.com
#[derive(Deserialize)]
struct CloudMainManifest {
    updated: String,
    modules: Vec<CloudModuleEntry>,
}

#[derive(Deserialize)]
struct CloudModuleEntry {
    manifest: String,
}

#[derive(Deserialize)]
struct CloudModuleManifest {
    name: String,
    hardware_version: String,
    releases: Vec<CloudRelease>,
}

#[derive(Deserialize)]
struct CloudRelease {
    sw_version: String,
    file: String,
    date: String,
    sha256: String,
    changelog: String,
}

impl Module {
    /// construct a new module at the given slot for the given controller type
    async fn new(slot: u8, controller: &ControllerTypes) -> Option<Self> {
        //get the spidev
        let (mut spidev, interrupt) = match controller {
            //get the Interrupt GPIO and the spidev
            ControllerTypes::ModulineIV => match slot {
                1 => (
                    Spidev::new(
                        File::open("/dev/spidev1.0")
                            .map_err(|_| { eprintln!("Could not get slot 1 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 6, slot)?,
                ),
                2 => (
                    Spidev::new(
                        File::open("/dev/spidev1.1")
                            .map_err(|_| { eprintln!("Could not get slot 2 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 20, slot)?,
                ),
                3 => (
                    Spidev::new(
                        File::open("/dev/spidev2.0")
                            .map_err(|_| { eprintln!("Could not get slot 3 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 7, slot)?,
                ),
                4 => (
                    Spidev::new(
                        File::open("/dev/spidev2.1")
                            .map_err(|_| { eprintln!("Could not get slot 4 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 21, slot)?,
                ),
                5 => (
                    Spidev::new(
                        File::open("/dev/spidev2.2")
                            .map_err(|_| { eprintln!("Could not get slot 5 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 1, slot)?,
                ),
                6 => (
                    Spidev::new(
                        File::open("/dev/spidev2.3")
                            .map_err(|_| { eprintln!("Could not get slot 6 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip3", 26, slot)?,
                ),
                7 => (
                    Spidev::new(
                        File::open("/dev/spidev0.0")
                            .map_err(|_| { eprintln!("Could not get slot 7 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip2", 19, slot)?,
                ),
                8 => (
                    Spidev::new(
                        File::open("/dev/spidev0.1")
                            .map_err(|_| { eprintln!("Could not get slot 8 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip2", 22, slot)?,
                ),
                _ => {
                    eprintln!(
                        "For the Moduline L4, slot should be a value from 1-8 but it was {}",
                        slot
                    );
                    return None;
                }
            },
            ControllerTypes::ModulineMini => match slot {
                1 => (
                    Spidev::new(
                        File::open("/dev/spidev1.0")
                            .map_err(|_| { eprintln!("Could not get slot 1 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 10, slot)?,
                ),
                2 => (
                    Spidev::new(
                        File::open("/dev/spidev1.1")
                            .map_err(|_| { eprintln!("Could not get slot 2 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 5, slot)?,
                ),
                3 => (
                    Spidev::new(
                        File::open("/dev/spidev2.0")
                            .map_err(|_| { eprintln!("Could not get slot 3 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip3", 26, slot)?,
                ),
                4 => (
                    Spidev::new(
                        File::open("/dev/spidev2.1")
                            .map_err(|_| { eprintln!("Could not get slot 4 spidev"); flag_scan_error(); })
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip2", 19, slot)?,
                ),
                _ => {
                    eprintln!(
                        "For the Moduline M1, slot should be a value from 1-4 but it was {}",
                        slot
                    );
                    return None;
                }
            },
            ControllerTypes::ModulineDisplay => {
                match slot {
                    1 => (
                        Spidev::new(
                            File::open("/dev/spidev1.0")
                                .map_err(|_| { eprintln!("Could not get slot 1 spidev"); flag_scan_error(); })
                                .ok()?,
                        ),
                        get_interrupt("/dev/gpiochip3", 5, slot)?,
                    ),
                    2 => (
                        Spidev::new(
                            File::open("/dev/spidev1.1")
                                .map_err(|_| { eprintln!("Could not get slot 2 spidev"); flag_scan_error(); })
                                .ok()?,
                        ),
                        get_interrupt("/dev/gpiochip0", 0, slot)?,
                    ),
                    _ => {
                        eprintln!("For the Moduline HMI1, slot should be a value from 1-2 but it was {}",slot);
                        return None;
                    }
                }
            }
        };
        spidev
            .configure(
                &SpidevOptions::new()
                    .bits_per_word(8)
                    .max_speed_hz(2_000_000)
                    .mode(SpiModeFlags::SPI_MODE_0)
                    .build(),
            )
            .map_err(|_| eprintln!("Could not configure spidev for slot {}", slot))
            .ok()?;
        let module = Self {
            slot,
            spidev,
            interrupt,
            firmware: FirmwareVersion { firmware: [0; 7] },
            manufacturer: 0,
            qr_front: 0,
            qr_back: 0,
        };
        module.get_module_info().await
    }

    /// get information from the module like firmware, manufacture, qr codes
    async fn get_module_info(mut self) -> Option<Self> {
        let mut tx_buf = [0u8; BOOTMESSAGE_LENGTH + 1];
        let mut rx_buf = [0u8; BOOTMESSAGE_LENGTH + 1];

        match self
            .spidev
            .transfer(&mut SpidevTransfer::write(&DUMMY_MESSAGE))
        {
            Ok(()) => (),
            Err(_) => { flag_scan_error(); return None; }
        }

        self.reset_module(true);

        //give module time to reset
        time::sleep(Duration::from_millis(200)).await;

        self.reset_module(false);

        time::sleep(Duration::from_millis(200)).await;

        tx_buf[0] = 9;
        tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
        tx_buf[2] = 9;
        tx_buf[BOOTMESSAGE_LENGTH - 1] = calculate_checksum(&tx_buf, BOOTMESSAGE_LENGTH - 1);

        match self
            .spidev
            .transfer(&mut SpidevTransfer::read_write(&tx_buf, &mut rx_buf))
        {
            Ok(()) => (),
            Err(_) => { flag_scan_error(); return None; }
        }

        if rx_buf[BOOTMESSAGE_LENGTH - 1] != calculate_checksum(&rx_buf, BOOTMESSAGE_LENGTH - 1)
            || (rx_buf[0] != 9 && rx_buf[2] != 9)
        {
            return None;
        }

        self.firmware = FirmwareVersion {
            firmware: clone_into_array(rx_buf.get(6..13).unwrap()),
        };
        self.manufacturer = u32::from_be_bytes(clone_into_array(rx_buf.get(13..17).unwrap()));
        self.qr_front = u32::from_be_bytes(clone_into_array(rx_buf.get(17..21).unwrap()));
        self.qr_back = u32::from_be_bytes(clone_into_array(rx_buf.get(21..25).unwrap()));
        Some(self)
    }

    /// switch the reset gpio for the module to the given state
    fn reset_module(&self, state: bool) {
        if state {
            _ = std::fs::write(
                format!("/sys/class/leds/ResetM-{}/brightness", self.slot),
                "255",
            );
        } else {
            _ = std::fs::write(
                format!("/sys/class/leds/ResetM-{}/brightness", self.slot),
                "0",
            );
        }
    }

    async fn wipe_module_error(&mut self) {
        let mut tx_buf = [0u8; BOOTMESSAGE_LENGTH + 1];
        match self
            .spidev
            .transfer(&mut SpidevTransfer::write(&DUMMY_MESSAGE))
        {
            Ok(()) => (),
            Err(_) => return,
        }

        self.reset_module(true);

        //give module time to reset
        time::sleep(Duration::from_millis(200)).await;

        self.reset_module(false);

        time::sleep(Duration::from_millis(200)).await;

        //wipe the old firmware and set the new software version no err_n_restart_services from this point on, errors lead to corrupt firmware.
        tx_buf[0] = 29;
        tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
        tx_buf[2] = 29;
        tx_buf[6] = 255;
        tx_buf[7] = 255;
        tx_buf[8] = 255;
        tx_buf[BOOTMESSAGE_LENGTH - 1] = calculate_checksum(&tx_buf, BOOTMESSAGE_LENGTH - 1);

        //this is super scuffed but for some reason it queues up events, so when in earlier parts the interrupt happens it fills the queue, causing it to skip the memory wipe interrupt and fail
        while let Ok(_) = timeout(Duration::from_millis(1), self.interrupt.next()).await {
            ()
        }

        //register the interrupt waiter
        let interrupt = self.interrupt.next();
        match self.spidev.transfer(&mut SpidevTransfer::write(&tx_buf)) {
            Ok(()) => (),
            Err(err) => {
                eprintln!("Error: failed spi transfer {}", err);
                return;
            }
        }

        _ = timeout(Duration::from_millis(3500), interrupt).await;
    }

    /// Overwrite the firmware on a module \
    ///
    /// Firmware uploading mechanism \
    /// Because of the parallel spi communication, the feedback from the module is about the previous message that was sent. \
    /// So, after the first message you receive junk, after the second message you receive info if the first message was sent correctly. \
    /// Two ways to fix this: \
    /// The old, send a line of firmware, then send a status request to check if it was uploaded correctly, try again if not, move on to the next line if it was. \
    /// This requires at least two messages sent per line of firmware, theoretically doubling the time to upload one piece of firmware.
    ///
    /// The new fast but complex way, keep track of the line of which you will receive feedback while also keeping track of what you are currently sending, \
    /// this gets complicated once errors start happening. The diagrams below will explain what happens in which situation: \
    /// normal function: \
    /// ``` text
    /// | 0 /\  ||      | 1 /\  ||      | 2 /\  ||      | 3 /\  ||      | 4 /\  ||      | 5 /\  ||      | 6 /\  ||      | 7 /\  ||      | 8 /\  ||      |
    /// |   ||  \/ignore|   ||  \/ 0    |   ||  \/ 1    |   ||  \/ 2    |   ||  \/ 3    |   ||  \/ 4    |   ||  \/ 5    |   ||  \/ 6    |   ||  \/ 7    |
    /// | lineNum    0  | lineNum    1  | lineNum    2  | lineNum    3  | lineNum    4  | lineNum    5  | lineNum    6  | lineNum    7  | lineNum    8  |
    /// | lineCheck MAX | lineCheck  0  | lineCheck  1  | lineCheck  2  | lineCheck  3  | lineCheck  4  | lineCheck  5  | lineCheck  6  | lineCheck  7  |
    /// | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  |
    /// ```
    /// on error swap lineNum and lineCheck, on success after odd number of errors swap them and add one to lineNum \
    /// repeated single/odd number of errors
    /// ``` text
    /// | 0 /\  ||      | 1 /\  ||      | 2 /\  ||      | 3 /\  ||      | 2 /\  ||      | 4 /\  ||      | 2 /\  ||      | 5 /\  ||      | 6 /\  ||      |
    /// |   ||  \/ignore|   ||  \/ 0    |   ||  \/ 1    |   ||  \/ err  |   ||  \/ 3    |   ||  \/ err  |   ||  \/ 4    |   ||  \/ 2    |   ||  \/ 5    |
    /// | lineNum    0  | lineNum    1  | lineNum    2  | lineNum    3  | lineNum    2  | lineNum    4  | lineNum    2  | lineNum    5  | lineNum    6  |
    /// | lineCheck MAX | lineCheck  0  | lineCheck  1  | lineCheck  2  | lineCheck  3  | lineCheck  2  | lineCheck  4  | lineCheck  2  | lineCheck  5  |
    /// | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 1  | errorCount 0  | errorCount 1  | errorCount 0  | errorCount 0  | errorCount 0  |
    /// ```
    /// repeated even number of errors
    /// ``` text
    /// | 0 /\  ||      | 1 /\  ||      | 2 /\  ||      | 3 /\  ||      | 2 /\  ||      | 3 /\  ||      | 4 /\  ||      | 5 /\  ||      | 6 /\  ||      |
    /// |   ||  \/ignore|   ||  \/ 0    |   ||  \/ 1    |   ||  \/ err  |   ||  \/ err  |   ||  \/ 2    |   ||  \/ 3    |   ||  \/ 4    |   ||  \/ 5    |
    /// | lineNum    0  | lineNum    1  | lineNum    2  | lineNum    3  | lineNum    2  | lineNum    3  | lineNum    4  | lineNum    5  | lineNum    6  |
    /// | lineCheck MAX | lineCheck  0  | lineCheck  1  | lineCheck  2  | lineCheck  3  | lineCheck  2  | lineCheck  3  | lineCheck  4  | lineCheck  5  |
    /// | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 1  | errorCount 2  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  |
    /// ```
    /// end of firmware
    /// ``` text
    /// | n-1 /\  ||    | test/\  ||    | n /\  ||      | test/\  ||                    |
    /// |     ||  \/ n-2|     ||  \/ n-1|   ||  \/ n-1  |     ||  \/ firmware response  |
    /// | lineNum    n-1| lineNum    n  | lineNum    n  | lineNum    n                  |
    /// | lineCheck  n-2| lineCheck  n-1| lineCheck  n-1| lineCheck  n                  |
    /// | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0                  |
    /// ```
    /// end of firmware with error
    /// ``` text
    /// | n-1 /\  ||    | test/\  ||    | n-1 /\  ||    | test/\  ||    | n /\  ||      | test/\  ||    | n /\  ||      | test/\  ||                    |
    /// |     ||  \/ n-2|     ||  \/ err|     ||  \/junk|     ||  \/ n-1|   ||  \/ n-1  |     ||  \/ err|   ||  \/ junk |     ||  \/ firmware response  |
    /// | lineNum    n-1| lineNum    n  | lineNum    n-1| lineNum    n  | lineNum    n  | lineNum    n  | lineNum    n  | lineNum    n                  |
    /// | lineCheck  n-2| lineCheck  n-1| lineCheck  n  | lineCheck  n-1| lineCheck  n-1| lineCheck  n  | lineCheck  n  | lineCheck  n                  |
    /// | errorCount 0  | errorCount 1  | errorCount 2  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0  | errorCount 0                  |
    ///```
    async fn overwrite_module(
        &mut self,
        new_firmware: &FirmwareVersion,
        multi_progress: MultiProgress,
        style: ProgressStyle,
    ) -> Result<(), UploadError> {
        let mut tx_buf_escape = [0u8; BOOTMESSAGE_LENGTH_CHECK];
        let mut rx_buf_escape = [0u8; BOOTMESSAGE_LENGTH_CHECK];

        let mut tx_buf = [0u8; BOOTMESSAGE_LENGTH + 1];
        let mut rx_buf = [0u8; BOOTMESSAGE_LENGTH + 1];

        //open and read the firmware file
        let firmware_content_string = match fs::read_to_string(format!(
            "{}{}",
            FIRMWARE_DIR,
            new_firmware.as_filename()
        )) {
            Ok(file) => file,
            Err(err) => {
                eprintln!(
                    "Error: could not read {}\n{}",
                    new_firmware.as_filename(),
                    err
                );
                return Err(UploadError::FirmwareUntouched(self.slot));
            }
        };

        //upload
        let lines: Vec<&str> = firmware_content_string.split('\n').collect();

        if lines.len() <= 1 {
            eprintln!("Error: firmware file corrupt");
            return Err(UploadError::FirmwareUntouched(self.slot));
        }
        //wipe the old firmware and set the new software version no err_n_restart_services from this point on, errors lead to corrupt firmware.
        tx_buf[0] = 29;
        tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
        tx_buf[2] = 29;
        let sw = new_firmware.get_software();
        tx_buf[6] = sw[0];
        tx_buf[7] = sw[1];
        tx_buf[8] = sw[2];
        tx_buf[BOOTMESSAGE_LENGTH - 1] = calculate_checksum(&tx_buf, BOOTMESSAGE_LENGTH - 1);

        //this is super scuffed but for some reason it queues up events, so when in earlier parts the interrupt happens it fills the queue, causing it to skip the memory wipe interrupt and fail
        while let Ok(_) = timeout(Duration::from_millis(1), self.interrupt.next()).await {
            ()
        }

        //register the interrupt waiter
        let interrupt = self.interrupt.next();
        match self.spidev.transfer(&mut SpidevTransfer::write(&tx_buf)) {
            Ok(()) => (),
            Err(err) => {
                eprintln!("Error: failed spi transfer {}", err);
                return Err(UploadError::FirmwareUntouched(self.slot));
            }
        }

        let spinner = multi_progress.add(ProgressBar::new_spinner());
        spinner.set_message(format!("Wiping old firmware on slot {}", self.slot));
        spinner.enable_steady_tick(Duration::from_millis(100));
        //wait for interrupt to happen or 2.5 secondes to pass, wiping the memory takes some time.
        _ = timeout(Duration::from_millis(3500), interrupt).await;
        spinner.finish_and_clear();

        let progress = multi_progress.add(ProgressBar::new(lines.len() as u64));
        progress.set_style(style);
        progress.set_message(format!(
            "Uploading firmware {} to slot {}",
            new_firmware.as_string(),
            self.slot
        ));

        let mut line_number: usize = 0;
        #[allow(unused_assignments)]
        let mut send_buffer_pointer: usize = 0;
        #[allow(unused_assignments)]
        let mut message_pointer: usize = 0;
        let mut message_type: u8 = 0;
        let mut firmware_line_check: usize = usize::MAX; //set line check to usize::MAX for the first message so we know its the first message
        let mut firmware_error_counter: u8 = 0;

        while message_type != 7 {
            // 7 marks the last line of the .srec file
            message_type = u8::from_str_radix(lines[line_number].get(1..2).unwrap(), 16).unwrap();

            let line_length =
                u8::from_str_radix(lines[line_number].get(2..4).unwrap(), 16).unwrap();
            //first time the last line is reached, it is not allowed to send the last line, as it could cause the module to jump to the firmware, potentially leaving line n-1 with an error
            if message_type == 7 && firmware_line_check != line_number {
                //prepare dummy message to get feedback from the previous message
                tx_buf[0] = 49;
                tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
                tx_buf[2] = 49;
                tx_buf[BOOTMESSAGE_LENGTH - 1] =
                    calculate_checksum(&tx_buf, BOOTMESSAGE_LENGTH - 1);
                let interrupt = self.interrupt.next();
                match self
                    .spidev
                    .transfer(&mut SpidevTransfer::read_write(&tx_buf, &mut rx_buf))
                {
                    Ok(()) => {
                        if rx_buf[BOOTMESSAGE_LENGTH - 1]
                            == calculate_checksum(&rx_buf, BOOTMESSAGE_LENGTH - 1)
                            && firmware_line_check
                                == u16::from_be_bytes(clone_into_array(rx_buf.get(6..8).unwrap()))
                                    as usize
                            && rx_buf[8] == 1
                        {
                            _ = timeout(Duration::from_millis(5), interrupt).await;
                        } else {
                            firmware_error_counter += 1;
                            mem::swap(&mut line_number, &mut firmware_line_check);
                            message_type = 0; //last message failed, set the message type to not 7 again so we don't exit the while loop
                            _ = timeout(Duration::from_millis(5), interrupt).await;
                            continue;
                        }
                    }
                    Err(_) => {
                        firmware_error_counter += 1;
                        mem::swap(&mut line_number, &mut firmware_line_check);
                        message_type = 0; //last message failed, set the message type to not 7 again so we don't exit the while loop
                        _ = timeout(Duration::from_millis(5), interrupt).await;
                        continue;
                    }
                }
            }
            // prepare firmware message
            tx_buf[0] = 39;
            tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
            tx_buf[2] = 39;

            send_buffer_pointer = 6;
            tx_buf[send_buffer_pointer] = (line_number >> 8) as u8;
            send_buffer_pointer += 1;
            tx_buf[send_buffer_pointer] = line_number as u8;
            send_buffer_pointer += 1;
            tx_buf[send_buffer_pointer] = message_type;
            send_buffer_pointer += 1;

            message_pointer = 2;
            while message_pointer < ((line_length * 2) + 2) as usize {
                tx_buf[send_buffer_pointer] = u8::from_str_radix(
                    lines[line_number]
                        .get(message_pointer..message_pointer + 2)
                        .unwrap(),
                    16,
                )
                .unwrap();
                send_buffer_pointer += 1;
                message_pointer += 2;
            }
            tx_buf[send_buffer_pointer] = u8::from_str_radix(
                lines[line_number]
                    .get(message_pointer..message_pointer + 2)
                    .unwrap(),
                16,
            )
            .unwrap();

            tx_buf[BOOTMESSAGE_LENGTH - 1] = calculate_checksum(&tx_buf, BOOTMESSAGE_LENGTH - 1);
            let interrupt = self.interrupt.next();
            match self
                .spidev
                .transfer(&mut SpidevTransfer::read_write(&tx_buf, &mut rx_buf))
            {
                Ok(_) => {
                    // the first message will always receive junk, ignore this junk and continue to line 1
                    if firmware_line_check == usize::MAX {
                        line_number += 1;
                        firmware_line_check = 0; // no ; to exit the match statement
                        _ = timeout(Duration::from_micros(1000), interrupt).await;
                        continue;
                    }
                    let received_line =
                        u16::from_be_bytes(clone_into_array(rx_buf.get(6..8).unwrap()));
                    let local_checksum_match = rx_buf[BOOTMESSAGE_LENGTH - 1]
                        == calculate_checksum(&rx_buf, BOOTMESSAGE_LENGTH - 1);
                    let remote_checksum_match = rx_buf[8] == 1;
                    let received_line_match = received_line as usize == firmware_line_check;

                    if local_checksum_match && received_line_match && remote_checksum_match {
                        if firmware_error_counter & 0b1 > 0 {
                            // if the error counter is uneven swap line number and the line being checked
                            std::mem::swap(&mut line_number, &mut firmware_line_check);
                        } else {
                            // else set the check number to the line line number, line number will be incremented later if necessary
                            firmware_line_check = line_number;
                        }
                        // the last message needs to be handled differently as it will instantly jump to the firmware when this message is received correctly.
                        if message_type == 7 {
                            // prepare a dummy message to see if we get a response from the firmware or from the bootloader.
                            tx_buf_escape[0] = 49;
                            tx_buf_escape[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
                            tx_buf_escape[2] = 49;
                            tx_buf_escape[BOOTMESSAGE_LENGTH - 1] =
                                calculate_checksum(&tx_buf_escape, BOOTMESSAGE_LENGTH - 1);
                            time::sleep(Duration::from_millis(5)).await;
                            _ = self.spidev.transfer(&mut SpidevTransfer::read_write(
                                &tx_buf_escape,
                                &mut rx_buf_escape,
                            ));
                            if rx_buf_escape[rx_buf_escape[1] as usize]
                                == calculate_checksum(&rx_buf_escape, rx_buf_escape[1] as usize)
                                && rx_buf_escape[6] == 20
                            {
                                // received response from bootloader, finish the last line of the progress bar and let the while loop exit.
                                progress.inc(1);
                            } else {
                                // last message failed, set the message type to not 7 again so we don't exit the while loop and try again instead
                                message_type = 0;
                            }
                        } else {
                            // normal firmware message success
                            line_number += 1;
                            firmware_error_counter = 0;
                            progress.inc(1);
                        }
                    } else {
                        mem::swap(&mut line_number, &mut firmware_line_check);
                        message_type = 0;
                        firmware_error_counter += 1;

                        #[cfg(debug_assertions)]
                        {
                            progress.println(format!(
                                "error number {}, rx: {:?}",
                                firmware_error_counter, rx_buf
                            ));
                            if !local_checksum_match {
                                progress.println(format!(
									"Error slot {}: checksum from module: {} didn't match with the calculated one: {}",
									self.slot, rx_buf[BOOTMESSAGE_LENGTH-1], calculate_checksum(&rx_buf, BOOTMESSAGE_LENGTH-1)
								));
                            }

                            if !received_line_match {
                                // use line number as it has been mem::swapped just before with firmware line check, which is the on we want
                                progress.println(format!("Error slot {}: firmware line: {} didn't match with the reply from the module: {}",self.slot, line_number, received_line));
                            }

                            if !remote_checksum_match {
                                progress.println(format!(
									"Error slot {}: module did not receive the firmware line correctly",
									self.slot
								));
                            }
                        }
                        if firmware_error_counter > 10 {
                            if !local_checksum_match {
                                progress.abandon_with_message(
                                    "Error: upload failed, checksum didn't match",
                                );
                            } else if !received_line_match {
                                progress.abandon_with_message("Error: upload failed, firmware line didn't match with the reply from the module");
                            } else if !remote_checksum_match {
                                progress.abandon_with_message("Error: upload failed, module did not receive the firmware line correctly");
                            } else {
                                progress
                                    .abandon_with_message("Error: upload failed, no idea how\n");
                            }
                            return Err(UploadError::FirmwareCorrupted(self.slot));
                        }
                    }
                }
                Err(_) => {
                    mem::swap(&mut line_number, &mut firmware_line_check);
                    message_type = 0;
                    firmware_error_counter += 1;
                    progress.println(format!(
                        "Error slot {}: failed to transfer spi message",
                        self.slot
                    ));
                    if firmware_error_counter > 10 {
                        progress.abandon_with_message("Error: upload failed, spi transfer failed");
                        return Err(UploadError::FirmwareCorrupted(self.slot));
                    }
                }
            } //exit match
              //wait for interrupt to happen (or 1 millisecond to pass), then continue with the next line
            _ = timeout(Duration::from_micros(1000), interrupt).await;
        } //exit while
        progress.finish_with_message("Upload successful!");
        self.cancel_firmware_upload(&mut tx_buf);
        Ok(())
    }

    /// Update a module, checking for new matching firmwares in the firmwares parameter \
    /// The outer Result<Result, UploadError> indicates whether there was an error in the upload process \
    /// The inner Result<Module,Module> indicates whether there was an available update or not.
    async fn update_module(
        mut self,
        firmwares: &[FirmwareVersion],
        multi_progress: MultiProgress,
        style: ProgressStyle,
    ) -> Result<Result<Self, Self>, UploadError> {
        if let Some((index, _junk)) = firmwares
            .iter()
            .enumerate()
            .filter(|(_i, available)| available.get_hardware() == self.firmware.get_hardware()) //filter out incorrect hardware versions
            .filter(|(_i, available)| {
                (available.get_software() > self.firmware.get_software()
                    || self.firmware.get_software() == [255u8, 255, 255])
                    && available.get_software() != [255u8, 255, 255]
            }) //filter out wrong software versions
            .map(|(i, available)| (i, available.get_software())) //turn them all into software versions
            .reduce(|acc, (i, software)| if acc.1 < software { (i, software) } else { acc })
        //cant use min/max because of the tuple, have to manually compare it in a reduce function
        {
            println!(
                "updating slot {} from {} to {}",
                self.slot,
                self.firmware.as_string(),
                firmwares.get(index).unwrap().as_string()
            );
            match self
                .overwrite_module(firmwares.get(index).unwrap(), multi_progress, style)
                .await
            {
                Ok(()) => {
                    self.firmware = *firmwares.get(index).unwrap();
                    Ok(Ok(self)) //firmware updated successfully
                }
                Err(err) => {
                    if let UploadError::FirmwareCorrupted(slot) = err {
                        eprintln!(
                            "firmware upload critically failed on slot {}, wiping firmware...",
                            slot
                        );
                        self.wipe_module_error().await;
                    }
                    Err(err)
                } //error uploading the new firmware
            }
        } else {
            // no new firmware found to update the module with.
            Ok(Err(self))
        }
    }

    /// Cancel the firmware upload of the module bringing the module into operational state
    fn cancel_firmware_upload(&mut self, tx_buf: &mut [u8]) {
        tx_buf[0] = 19;
        tx_buf[1] = (BOOTMESSAGE_LENGTH - 1) as u8;
        tx_buf[2] = 19;
        tx_buf[BOOTMESSAGE_LENGTH - 1] = calculate_checksum(tx_buf, BOOTMESSAGE_LENGTH - 1);
        _ = self.spidev.transfer(&mut SpidevTransfer::write(tx_buf));
    }
}

impl Display for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let hardware = self.firmware.get_hardware();
        let software = self.firmware.get_software();
        write!(
            f,
            "{}",
            match hardware[1] {
                10 => match hardware[2] {
                    1 => format!(
                        "slot {}: 6 Channel Input module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    2 => format!(
                        "slot {}: 10 Channel Input module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    3 => format!(
                        "slot {}: 4-20mA Input module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    _ => format!("slot {}: unknown: {}", self.slot, self.firmware.as_string()),
                },
                20 => match hardware[2] {
                    1 => format!(
                        "slot {}: 2 Channel Output module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    2 => format!(
                        "slot {}: 6 Channel Output module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    3 => format!(
                        "slot {}: 10 Channel Output module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    _ => format!("slot {}: unknown: {}", self.slot, self.firmware.as_string()),
                },
                30 => match hardware[2] {
                    3 => format!(
                        "slot {}: IR communication module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    4 => format!(
                        "slot {}: Multibus module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    _ => format!("slot {}: unknown: {}", self.slot, self.firmware.as_string()),
                },
                40 => match hardware[2] {
                    1 => format!(
                        "slot {}: ANLEG RTC Control module version {} sw: {}.{}.{}",
                        self.slot, hardware[3], software[0], software[1], software[2]
                    ),
                    _ => format!("slot {}: unknown: {}", self.slot, self.firmware.as_string()),
                },
                _ => format!("slot {}: unknown: {}", self.slot, self.firmware.as_string()),
            }
        )
    }
}

impl Module {
    fn type_name(&self) -> &'static str {
        let hw = self.firmware.get_hardware();
        match hw[1] {
            10 => match hw[2] {
                1 => "6 Channel Input",
                2 => "10 Channel Input",
                3 => "4-20mA Input",
                _ => "Unknown",
            },
            20 => match hw[2] {
                1 => "2 Channel Output",
                2 => "6 Channel Output",
                3 => "10 Channel Output",
                _ => "Unknown",
            },
            30 => match hw[2] {
                3 => "IR communication",
                4 => "Multibus",
                _ => "Unknown",
            },
            40 => match hw[2] {
                1 => "ANLEG RTC Control",
                _ => "Unknown",
            },
            _ => "Unknown",
        }
    }
}

/// Among `available`, return the highest-versioned firmware whose hardware
/// matches `module.firmware` and whose software is strictly newer than the
/// module's current software. Returns None when no update applies. Also
/// treats current SW = `[255,255,255]` (sentinel for uninitialized) as
/// "anything available is an update".
fn latest_update_for(
    module: &Module,
    available: &[FirmwareVersion],
) -> Option<FirmwareVersion> {
    let current_sw = module.firmware.get_software();
    let current_uninit = current_sw == [255u8, 255, 255];
    available
        .iter()
        .copied()
        .filter(|f| f.get_hardware() == module.firmware.get_hardware())
        .filter(|f| f.get_software() != [255u8, 255, 255])
        .filter(|f| current_uninit || f.get_software() > current_sw)
        .max_by_key(|f| {
            let sw = f.get_software();
            (sw[0], sw[1], sw[2])
        })
}

/// Format the scanned modules into space-aligned columns:
/// header row above the values, no surrounding box. Returns one
/// String per output row, intended for `show_view`. The "Update" column
/// shows the highest locally-cached firmware that is newer than the
/// module's current software (empty when up to date or no firmware
/// cached — run `go-modules check` to refresh the local cache).
fn format_module_lines(modules: &[Module], available: &[FirmwareVersion]) -> Vec<String> {
    let headers = ["Slot", "Type", "HW", "SW Version", "Update"];

    let rows: Vec<[String; 5]> = modules
        .iter()
        .map(|m| {
            let hw = m.firmware.get_hardware();
            let sw = m.firmware.get_software();
            let update_cell = match latest_update_for(m, available) {
                Some(fw) => {
                    let nsw = fw.get_software();
                    format!("→ {}.{}.{}", nsw[0], nsw[1], nsw[2])
                }
                None => String::new(),
            };
            [
                m.slot.to_string(),
                m.type_name().to_string(),
                hw[3].to_string(),
                format!("{}.{}.{}", sw[0], sw[1], sw[2]),
                update_cell,
            ]
        })
        .collect();

    let mut widths: [usize; 5] = [0; 5];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let render = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, &cell) in cells.iter().enumerate() {
            let pad = widths[i].saturating_sub(cell.chars().count());
            s.push_str(cell);
            for _ in 0..pad {
                s.push(' ');
            }
            if i + 1 < cells.len() {
                s.push_str("  ");
            }
        }
        s
    };

    let mut out = Vec::with_capacity(rows.len() + 1);
    out.push(render(&headers));
    for row in &rows {
        let cells: [&str; 5] = [&row[0], &row[1], &row[2], &row[3], &row[4]];
        out.push(render(&cells));
    }
    out
}

/// Restart nodered, go-simulink, and go-hardware-driver if they were running
/// before the app started. Idempotent — safe to call from the ctrlc handler
/// and from the normal exit path. `go-hardware-driver` is absent on legacy
/// controllers; in that case the snapshot bool is simply false and we skip it.
fn restart_services(nodered: bool, simulink: bool, hardware_driver: bool) {
    if nodered {
        _ = Command::new("systemctl")
            .arg("start")
            .arg("nodered")
            .status();
    }

    if simulink {
        _ = Command::new("systemctl")
            .arg("start")
            .arg("go-simulink")
            .status();
    }

    if hardware_driver {
        _ = Command::new("systemctl")
            .arg("start")
            .arg("go-hardware-driver")
            .status();
    }
}

/// error out without restarting any services (used before services are stopped)
fn err_n_die(message: &str) -> ! {
    eprintln!("{}", message);
    exit(-1);
}

/// calculate an spi messages checksum
fn calculate_checksum(message: &[u8], length: usize) -> u8 {
    let mut checksum: u8 = 0;
    for val in message.get(0..length).unwrap() {
        checksum = checksum.wrapping_add(*val);
    }
    checksum
}

/// turn a slice into a sized array to perform ::from_bytes() operations on
fn clone_into_array<A, T>(slice: &[T]) -> A
where
    A: Default + AsMut<[T]>,
    T: Clone,
{
    let mut a = A::default();
    <A as AsMut<[T]>>::as_mut(&mut a).clone_from_slice(slice);
    a
}

/// verify the SHA256 checksum of a byte slice against an expected hex string
fn verify_sha256(data: &[u8], expected_hex: &str) -> bool {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize()) == expected_hex
}

/// get module interrupt pin
fn get_interrupt(chip: &str, line: u32, slot: u8) -> Option<AsyncLineEventHandle> {
    let mut chip = Chip::new(chip)
        .map_err(|_| { eprintln!("Could not get slot {slot} interrupt chip"); flag_scan_error(); })
        .ok()?;
    let line = chip
        .get_line(line)
        .map_err(|_| { eprintln!("Could not get slot {slot} interrupt line"); flag_scan_error(); })
        .ok()?;
    line.async_events(
        LineRequestFlags::INPUT,
        EventRequestFlags::FALLING_EDGE,
        format!("module {slot} interrupt").as_str(),
    )
    .map_err(|err| { eprintln!("Could not get slot {slot} interrupt line handle: {err}"); flag_scan_error(); })
    .ok()
}

/// One row's worth of `check` output, kept in struct form so the caller
/// can align the columns after every entry has been computed.
struct CheckEntry {
    name: String,
    hw: String,
    sw: String,
    status: String,
    released: Option<String>,
    changelog: Option<String>,
}

/// Fetch the latest firmware files from the GOcontroll cloud, save them locally
/// after SHA256 validation, and return the human-readable status lines (one per
/// module, plus optional Released/Changes detail lines when `verbose`).
///
/// All output is returned rather than printed so the caller can render it
/// inside the frame view.
async fn check_firmware(verbose: bool) -> Result<Vec<String>, String> {
    let client = reqwest::Client::new();

    // Fetch main manifest
    let main_manifest: CloudMainManifest = client
        .get(format!("{}/modules/manifest.json", CLOUD_BASE_URL))
        .send()
        .await
        .map_err(|e| format!("{e}"))?
        .json()
        .await
        .map_err(|e| format!("{e}"))?;

    // Ensure firmware directory exists
    fs::create_dir_all(FIRMWARE_DIR)
        .map_err(|e| format!("Could not create firmware directory {FIRMWARE_DIR}: {e}"))?;

    let mut entries: Vec<CheckEntry> = Vec::with_capacity(main_manifest.modules.len());

    for entry in &main_manifest.modules {
        // Fetch per-module sub-manifest
        let sub_manifest: CloudModuleManifest = match client
            .get(format!("{}/{}", CLOUD_BASE_URL, entry.manifest))
            .send()
            .await
        {
            Ok(resp) => match resp.json().await {
                Ok(m) => m,
                Err(e) => {
                    entries.push(CheckEntry {
                        name: format!("(manifest {})", entry.manifest),
                        hw: String::new(),
                        sw: String::new(),
                        status: format!("parse failed: {e}"),
                        released: None,
                        changelog: None,
                    });
                    continue;
                }
            },
            Err(e) => {
                entries.push(CheckEntry {
                    name: format!("(manifest {})", entry.manifest),
                    hw: String::new(),
                    sw: String::new(),
                    status: format!("fetch failed: {e}"),
                    released: None,
                    changelog: None,
                });
                continue;
            }
        };

        // The first entry in releases is the latest version
        let latest = match sub_manifest.releases.first() {
            Some(r) => r,
            None => {
                entries.push(CheckEntry {
                    name: sub_manifest.name,
                    hw: sub_manifest.hardware_version,
                    sw: String::new(),
                    status: "no releases found".into(),
                    released: None,
                    changelog: None,
                });
                continue;
            }
        };

        // Extract filename from the cloud file path
        let filename = match latest.file.split('/').last() {
            Some(f) if !f.is_empty() => f,
            _ => {
                entries.push(CheckEntry {
                    name: sub_manifest.name,
                    hw: sub_manifest.hardware_version,
                    sw: latest.sw_version.clone(),
                    status: format!("invalid file path: {}", latest.file),
                    released: None,
                    changelog: None,
                });
                continue;
            }
        };

        let local_path = format!("{}{}", FIRMWARE_DIR, filename);
        let mut status = String::new();
        let mut needs_download = true;

        if let Ok(existing_data) = fs::read(&local_path) {
            if verify_sha256(&existing_data, &latest.sha256) {
                status = "up to date".into();
                needs_download = false;
            } else {
                status = "local file corrupted, re-downloading...".into();
            }
        }

        if needs_download {
            match client
                .get(format!("{}/{}", CLOUD_BASE_URL, latest.file))
                .send()
                .await
            {
                Ok(resp) => match resp.bytes().await {
                    Ok(data) => {
                        if !verify_sha256(&data, &latest.sha256) {
                            status = "checksum verification failed".into();
                        } else if let Err(e) = fs::write(&local_path, &data) {
                            status = format!("could not save {filename}: {e}");
                        } else if status.is_empty() {
                            status = "downloaded".into();
                        } else {
                            status = "re-downloaded".into();
                        }
                    }
                    Err(e) => status = format!("download failed: {e}"),
                },
                Err(e) => status = format!("download failed: {e}"),
            }
        }

        entries.push(CheckEntry {
            name: sub_manifest.name,
            hw: sub_manifest.hardware_version,
            sw: latest.sw_version.clone(),
            status,
            released: Some(latest.date.clone()),
            changelog: Some(latest.changelog.clone()),
        });
    }

    // Compute column widths for space alignment.
    let name_w = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
    let hw_w = entries.iter().map(|e| e.hw.len()).max().unwrap_or(0);
    let sw_w = entries.iter().map(|e| e.sw.len()).max().unwrap_or(0);

    let mut out = Vec::with_capacity(entries.len() * if verbose { 3 } else { 1 } + 2);
    out.push(format!(
        "Cloud manifest last updated: {}",
        main_manifest.updated
    ));
    out.push(String::new());

    for e in &entries {
        let mut line = String::new();
        let _ = write!(line, "{:<name_w$}", e.name, name_w = name_w);
        if hw_w > 0 {
            let _ = write!(line, "  HW {:<hw_w$}", e.hw, hw_w = hw_w);
        }
        if sw_w > 0 {
            let _ = write!(
                line,
                "  v{:<sw_w$}",
                if e.sw.is_empty() { "" } else { &e.sw },
                sw_w = sw_w
            );
        }
        let _ = write!(line, "  {}", e.status);
        out.push(line);

        if verbose {
            if let Some(r) = &e.released {
                out.push(format!("  Released: {r}"));
            }
            if let Some(c) = &e.changelog {
                out.push(format!("  Changes:  {c}"));
            }
        }
    }

    Ok(out)
}

/// get the current modules in the controller
async fn get_modules(controller: &ControllerTypes) -> Vec<Module> {
    let mut modules = Vec::with_capacity(8);
    let mut set = JoinSet::new();
    let controller = *controller;
    for i in 1..controller as usize {
        set.spawn(async move { Module::new(i as u8, &controller).await });
    }
    for _ in 1..controller as usize {
        if let Some(Ok(Some(module))) = set.join_next().await {
            modules.push(module);
        }
    }
    modules
}

/// get the modules in the controller and save them
///
/// `modules.json` is only rewritten when the scan ran cleanly. If any
/// hardware error occurred (SPI device open, GPIO line open, SPI transfer),
/// `SCAN_HAD_ERRORS` is set by the failing path and we return without
/// touching the file — otherwise the all-`None` slots from a failed scan
/// would be written as `SlotEntry::empty()` and silently wipe user-edited
/// channel/module config.
async fn get_modules_and_save(controller: ControllerTypes) -> Vec<Module> {
    SCAN_HAD_ERRORS.store(false, Ordering::Relaxed);
    let modules = get_modules(&controller).await;
    if SCAN_HAD_ERRORS.load(Ordering::Relaxed) {
        eprintln!(
            "Scan encountered hardware errors; modules.json was not updated. \
             Resolve the SPI/GPIO conflict and rerun the scan."
        );
        return modules;
    }
    let mut modules_out: Vec<Option<Module>> = match &controller {
        ControllerTypes::ModulineDisplay => vec![None, None],
        ControllerTypes::ModulineIV => vec![None, None, None, None, None, None, None, None],
        ControllerTypes::ModulineMini => vec![None, None, None, None],
    };
    for module in modules {
        let slot = module.slot;
        modules_out[(slot - 1) as usize] = Some(module);
    }
    save_modules(modules_out, &controller)
}

/// Insert any keys from `defaults` that are not already present in `existing`.
/// Existing values are never overwritten. Both must be JSON objects; any other
/// shape is left untouched. Used by `save_modules` to honor user-edited config
/// (`name`, `func`, `pull_up`, `current_max`, etc.) while still backfilling
/// keys that newer schema versions added.
fn merge_defaults_into(existing: &mut Value, defaults: &Value) {
    if let (Some(e), Some(d)) = (existing.as_object_mut(), defaults.as_object()) {
        for (k, v) in d {
            e.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

/// Merge per-channel defaults for `ty` into `existing`. Channels are matched on
/// the `channel` field; an entry that exists gets missing keys filled in (other
/// keys preserved), an entry that's missing gets the full default appended.
/// Channels outside `1..=channel_count` are left in place untouched.
fn merge_channels(existing: &mut Vec<Value>, ty: ModuleType) {
    let count = ty.channel_count() as u8;
    for ch in 1..=count {
        let pos = existing
            .iter()
            .position(|v| v.get("channel").and_then(|c| c.as_u64()) == Some(ch as u64));
        let default = ty.default_channel(ch);
        match pos {
            Some(i) => merge_defaults_into(&mut existing[i], &default),
            None => existing.push(default),
        }
    }
}

/// Save all modules to /lib/firmware/gocontroll/modules.json (new schema per
/// configuration.md) and /usr/module-firmware/modules.txt (legacy 4-line format
/// kept alive for older Node-RED installs — see CLAUDE.md).
///
/// Merge semantics for modules.json: detection-derived fields
/// (`slot`, `module_type`, `article_number`, `hardware_version`, `firmware_version`)
/// are refreshed from the SPI scan; user-edited `module` and `channels` are
/// preserved across rescans via `merge_defaults_into` / `merge_channels` —
/// missing keys get conservative defaults, present keys keep their value.
/// Only when the detected `module_type` differs from a previously-recorded
/// `Some(other)` is the slot wiped and refilled with defaults for the new
/// type (the old keys no longer apply). A previously-absent `module_type`
/// (None) is treated as "unknown but assume compatible" to avoid clobbering
/// config from older schema versions.
///
/// Parse failures: the old `.ok()` chain silently dropped the entire doc on
/// any deserialization error, which would wipe user config. The current code
/// instead backs the file up to `modules.json.bak.<unix-ts>` and logs to
/// stderr before falling back to an empty doc.
///
/// Caller convention: `modules` may be a full slot-indexed vec
/// (`vec[i]` = slot `i+1`, `None` = empty slot) or a partial list of just
/// updated slots. `None` entries trigger slot-removal only when the vec
/// length matches the controller's slot count (i.e. the caller produced a
/// full scan); partial-update callers pass only `Some` entries.
fn save_modules(modules: Vec<Option<Module>>, controller: &ControllerTypes) -> Vec<Module> {
    let slot_count = match controller {
        ControllerTypes::ModulineIV => 8usize,
        ControllerTypes::ModulineMini => 4usize,
        ControllerTypes::ModulineDisplay => 2usize,
    };
    let full_scan = modules.len() == slot_count;

    let empty_doc = || ModulesJson {
        schema_version: MODULES_JSON_SCHEMA_VERSION.to_string(),
        controller: controller_schema_name(controller).to_string(),
        slots: Vec::new(),
    };
    let path = "/lib/firmware/gocontroll/modules.json";
    let mut doc: ModulesJson = match fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<ModulesJson>(&s) {
            Ok(d) => d,
            Err(e) => {
                let ts = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let bak = format!("{}.bak.{}", path, ts);
                eprintln!(
                    "modules.json failed to parse ({e}); backing up to {bak} \
                     and starting from an empty doc"
                );
                let _ = fs::copy(path, &bak);
                empty_doc()
            }
        },
        Err(_) => empty_doc(),
    };

    // Re-stamp top-level fields in case the file was hand-edited.
    doc.schema_version = MODULES_JSON_SCHEMA_VERSION.to_string();
    doc.controller = controller_schema_name(controller).to_string();

    // Backfill `name: ""` on any channel that lacks it (added in v3.1.0 — older
    // entries written by go-modules ≤3.0.x do not have it).
    for slot in doc.slots.iter_mut() {
        for ch in slot.channels.iter_mut() {
            if let Some(obj) = ch.as_object_mut() {
                obj.entry("name").or_insert_with(|| Value::String(String::new()));
            }
        }
    }

    // input-4-20ma migration (v3.1.x): the original draft used a `supply_16ch`
    // array of booleans and `func` on each channel. The current shape is named
    // `sensor_supply_1..5` strings ("on"/"off") and channels expose only
    // `channel` + `name` (per-channel func has no module-firmware meaning).
    for slot in doc.slots.iter_mut() {
        if matches!(slot.module_type, Some(ModuleType::Input420Ma)) {
            if let Some(obj) = slot.module.as_mut().and_then(|v| v.as_object_mut()) {
                if let Some(arr) = obj.remove("supply_16ch") {
                    if let Some(bools) = arr.as_array() {
                        for (i, v) in bools.iter().enumerate().take(5) {
                            let on = v.as_bool().unwrap_or(false);
                            let key = format!("sensor_supply_{}", i + 1);
                            obj.insert(
                                key,
                                Value::String(if on { "on" } else { "off" }.to_string()),
                            );
                        }
                    }
                }
                for i in 1..=5 {
                    let key = format!("sensor_supply_{}", i);
                    obj.entry(key)
                        .or_insert_with(|| Value::String("off".to_string()));
                }
            } else {
                slot.module = ModuleType::Input420Ma.default_module();
            }
            for ch in slot.channels.iter_mut() {
                if let Some(o) = ch.as_object_mut() {
                    o.remove("func");
                }
            }
        }
    }

    for (idx, m) in modules.iter().enumerate() {
        match m {
            Some(module) => {
                let detected_type = match ModuleType::from_firmware(&module.firmware) {
                    Some(t) => t,
                    None => {
                        eprintln!(
                            "Slot {}: unknown module type for firmware {}; \
                             not writing modules.json entry",
                            module.slot,
                            module.firmware.as_string()
                        );
                        continue;
                    }
                };
                let article_number = article_number_from_firmware(&module.firmware);
                let hardware_version = hardware_version_string(&module.firmware);
                let firmware_version = firmware_version_string(&module.firmware);

                let firmware = module.firmware.as_string();
                if let Some(existing) = doc.slots.iter_mut().find(|s| s.slot == module.slot) {
                    // Only treat as a real type-swap when the previous type was
                    // recorded AND differs. A previously-absent module_type
                    // (older schema, hand-edited file) is treated as compatible
                    // so user-edited config is not clobbered.
                    let type_changed = matches!(existing.module_type, Some(prev) if prev != detected_type);
                    existing.module_type = Some(detected_type);
                    existing.article_number = Some(article_number);
                    existing.hardware_version = Some(hardware_version);
                    existing.firmware_version = Some(firmware_version);
                    existing.firmware = firmware;
                    existing.manufacturer = module.manufacturer;
                    existing.qr_front = module.qr_front;
                    existing.qr_back = module.qr_back;
                    if type_changed {
                        existing.module = detected_type.default_module();
                        existing.channels = detected_type.default_channels();
                    } else {
                        // Same type (or previously unknown): preserve user-set
                        // values, only fill in keys the existing entry lacks.
                        if let Some(default_mod) = detected_type.default_module() {
                            match existing.module.as_mut() {
                                Some(m) => merge_defaults_into(m, &default_mod),
                                None => existing.module = Some(default_mod),
                            }
                        }
                        merge_channels(&mut existing.channels, detected_type);
                    }
                } else {
                    doc.slots.push(SlotEntry {
                        slot: module.slot,
                        firmware,
                        manufacturer: module.manufacturer,
                        qr_front: module.qr_front,
                        qr_back: module.qr_back,
                        enabled: true,
                        module_type: Some(detected_type),
                        article_number: Some(article_number),
                        hardware_version: Some(hardware_version),
                        firmware_version: Some(firmware_version),
                        label: None,
                        module: detected_type.default_module(),
                        channels: detected_type.default_channels(),
                    });
                }
            }
            None if full_scan => {
                // Slot is empty in this scan — replace any prior entry with a
                // bare placeholder. Drops module/channels config because the
                // module is no longer present.
                let slot_num = (idx + 1) as u8;
                if let Some(pos) = doc.slots.iter().position(|s| s.slot == slot_num) {
                    doc.slots[pos] = SlotEntry::empty(slot_num);
                } else {
                    doc.slots.push(SlotEntry::empty(slot_num));
                }
            }
            None => { /* partial update: skip None entries */ }
        }
    }

    doc.slots.sort_by_key(|s| s.slot);

    if fs::create_dir_all("/lib/firmware/gocontroll/").is_err() {
        eprintln!("Could not create /lib/firmware/gocontroll/");
    }
    match serde_json::to_string_pretty(&doc) {
        Ok(json) => {
            if fs::write("/lib/firmware/gocontroll/modules.json", json).is_err() {
                eprintln!(
                    "Could not save module layout to /lib/firmware/gocontroll/modules.json"
                );
            }
        }
        Err(e) => eprintln!("Could not serialize module layout: {}", e),
    }

    write_legacy_modules_txt(&modules, slot_count, full_scan);

    modules.into_iter().flatten().collect()
}

/// Write /usr/module-firmware/modules.txt (legacy 4-line `:`-separated format
/// kept for older Node-RED installs). Preserves untouched slots by reading the
/// current file first.
fn write_legacy_modules_txt(
    modules: &[Option<Module>],
    slot_count: usize,
    full_scan: bool,
) {
    let mut firmware = vec![String::new(); slot_count];
    let mut manufacturer = vec!["0".to_string(); slot_count];
    let mut qr_front = vec!["0".to_string(); slot_count];
    let mut qr_back = vec!["0".to_string(); slot_count];

    if let Ok(content) = fs::read_to_string("/usr/module-firmware/modules.txt") {
        let lines: Vec<&str> = content.lines().collect();
        let take = |line: &str, default: &str| -> Vec<String> {
            let parts: Vec<&str> = line.split(':').collect();
            (0..slot_count)
                .map(|i| parts.get(i).map(|s| s.to_string()).unwrap_or_else(|| default.to_string()))
                .collect()
        };
        if let Some(l) = lines.first() {
            firmware = take(l, "");
        }
        if let Some(l) = lines.get(1) {
            manufacturer = take(l, "0");
        }
        if let Some(l) = lines.get(2) {
            qr_front = take(l, "0");
        }
        if let Some(l) = lines.get(3) {
            qr_back = take(l, "0");
        }
    }

    for (idx, m) in modules.iter().enumerate() {
        match m {
            Some(module) => {
                let i = (module.slot as usize).saturating_sub(1);
                if i < slot_count {
                    firmware[i] = module.firmware.as_string();
                    manufacturer[i] = module.manufacturer.to_string();
                    qr_front[i] = module.qr_front.to_string();
                    qr_back[i] = module.qr_back.to_string();
                }
            }
            None if full_scan => {
                if idx < slot_count {
                    firmware[idx] = String::new();
                    manufacturer[idx] = "0".to_string();
                    qr_front[idx] = "0".to_string();
                    qr_back[idx] = "0".to_string();
                }
            }
            None => {}
        }
    }

    if fs::create_dir_all("/usr/module-firmware/").is_err() {
        eprintln!("Could not create /usr/module-firmware/");
    }
    let text_content = format!(
        "{}\n{}\n{}\n{}",
        firmware.join(":"),
        manufacturer.join(":"),
        qr_front.join(":"),
        qr_back.join(":"),
    );
    if fs::write("/usr/module-firmware/modules.txt", text_content).is_err() {
        eprintln!("Could not save module layout to /usr/module-firmware/modules.txt");
    }
}

/// Update a single module. Returns the (possibly-updated) module along with
/// human-readable status lines for the result view.
async fn update_one_module(
    module: Module,
    available_firmwares: &[FirmwareVersion],
    multi_progress: MultiProgress,
    style: ProgressStyle,
    controller: ControllerTypes,
) -> (Option<Module>, Vec<String>) {
    match module
        .update_module(available_firmwares, multi_progress, style)
        .await
    {
        Ok(Ok(module)) => {
            let line = format!(
                "Successfully updated slot {} to {}",
                module.slot,
                module.firmware.as_string()
            );
            save_modules(vec![Some(module)], &controller);
            (None, vec![line])
        }
        Err(UploadError::FirmwareCorrupted(slot)) => (
            None,
            vec![format!(
                "Update failed, firmware is corrupted on slot {slot}"
            )],
        ),
        Err(UploadError::FirmwareUntouched(slot)) => {
            (None, vec![format!("Update failed on slot {slot}")])
        }
        Ok(Err(module)) => {
            let line = format!(
                "Update failed, no update available for slot {}: {}",
                module.slot,
                module.firmware.as_string()
            );
            (Some(module), vec![line])
        }
    }
}

/// Update every module in parallel. Returns status lines for the result view.
async fn update_all_modules(
    modules: Vec<Module>,
    available_firmwares: &[FirmwareVersion],
    multi_progress: &MultiProgress,
    style: &ProgressStyle,
    controller: ControllerTypes,
) -> Vec<String> {
    let mut upload_results = Vec::with_capacity(modules.len());
    let mut new_modules = Vec::with_capacity(modules.len());
    let mut lines: Vec<String> = Vec::new();
    let mut set = JoinSet::new();
    let shared_firmwares: Arc<[FirmwareVersion]> = Arc::from(available_firmwares);
    for module in modules {
        let firmwares = Arc::clone(&shared_firmwares);
        let multi_progress = multi_progress.clone();
        let style = style.clone();
        set.spawn(
            async move { module.update_module(&firmwares, multi_progress, style).await },
        );
    }
    for _ in 0..set.len() {
        upload_results.push(set.join_next().await.unwrap().unwrap());
    }
    for result in upload_results {
        match result {
            Ok(Ok(module)) => new_modules.push(Some(module)),
            Err(UploadError::FirmwareCorrupted(slot)) => {
                lines.push(format!("Update failed, firmware is corrupted on slot {slot}"));
            }
            Err(UploadError::FirmwareUntouched(slot)) => {
                lines.push(format!("Update failed on slot {slot}"));
            }
            Ok(Err(_)) => (), //no new firmwares available
        }
    }
    if !new_modules.is_empty() {
        lines.push("Successfully updated:".into());
        for module in &new_modules {
            let m = module.as_ref().unwrap();
            lines.push(format!("slot {} to {}", m.slot, m.firmware.as_string()));
        }
    } else if lines.is_empty() {
        lines.push("No updates found for the modules in this controller.".into());
    }
    save_modules(new_modules, &controller);
    lines
}

/// Reset the screen so the next view starts at the top, then re-print the
/// banner and optional subtitle. Called between menu transitions so output
/// stacks predictably. Pass an empty string to skip the subtitle line.
fn redraw_chrome(subtitle: &str) {
    let _ = execute!(
        io::stdout(),
        terminal::Clear(terminal::ClearType::All),
        cursor::MoveTo(0, 0)
    );
    print_banner();
    if !subtitle.is_empty() {
        println!("{}", SEP);
        println!("  {}", subtitle);
    }
    #[cfg(debug_assertions)]
    println!("Debug version");
}

/// Detect the controller hardware. Exits the process on unsupported hardware
/// — runs before services are touched, so no restart needed.
fn detect_controller() -> ControllerTypes {
    let hardware_string = fs::read_to_string("/sys/firmware/devicetree/base/hardware")
        .unwrap_or_else(|_| {
            err_n_die("Could not find a hardware description file, this feature is not supported by your hardware.");
        });

    if hardware_string.contains("Moduline IV") || hardware_string.contains("Moduline L4") {
        ControllerTypes::ModulineIV
    } else if hardware_string.contains("Moduline Mini") || hardware_string.contains("Moduline M1") {
        ControllerTypes::ModulineMini
    } else if hardware_string.contains("Moduline Display")
        || hardware_string.contains("Moduline HMI1")
    {
        ControllerTypes::ModulineDisplay
    } else {
        err_n_die(
            format!(
                "{} is not a supported GOcontroll Moduline product. Can't proceed",
                hardware_string
            )
            .as_str(),
        );
    }
}

/// Returns whether the given systemd service is currently active.
fn is_service_active(name: &str) -> bool {
    let output = Command::new("systemctl")
        .arg("is-active")
        .arg(name)
        .output();
    match output {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).contains("in"),
        Err(_) => false,
    }
}

fn stop_service(name: &str) {
    _ = Command::new("systemctl").arg("stop").arg(name).status();
}

fn read_firmware_dir() -> Vec<FirmwareVersion> {
    let dir = match fs::read_dir(FIRMWARE_DIR) {
        Ok(d) => d,
        Err(_) => {
            eprintln!("Could not find the firmware folder");
            return Vec::new();
        }
    };
    dir.filter_map(|f| f.ok())
        .filter_map(|f| f.file_name().to_str().map(str::to_string))
        .filter(|n| n.ends_with(".srec"))
        .filter_map(FirmwareVersion::from_filename)
        .collect()
}

/// Sub-menu flow for the Update action. Returns the (possibly-unchanged)
/// modules vector along with status lines for the result view.
/// An empty `lines` vector signals that the user backed out of the sub-menus.
async fn run_update_flow(
    modules: Vec<Module>,
    available_firmwares: &[FirmwareVersion],
    multi_progress: &MultiProgress,
    style: &ProgressStyle,
    controller: ControllerTypes,
    cli_arg: Option<String>,
) -> (Vec<Module>, Vec<String>) {
    if let Some(arg) = cli_arg {
        return match arg.as_str() {
            "all" => {
                let lines = update_all_modules(
                    modules,
                    available_firmwares,
                    multi_progress,
                    style,
                    controller,
                )
                .await;
                (Vec::new(), lines)
            }
            other => {
                if let Ok(slot) = other.parse::<u8>() {
                    let mut remaining = modules;
                    if let Some(idx) = remaining.iter().position(|m| m.slot == slot) {
                        let module = remaining.remove(idx);
                        let (returned, lines) = update_one_module(
                            module,
                            available_firmwares,
                            multi_progress.clone(),
                            style.clone(),
                            controller,
                        )
                        .await;
                        if let Some(m) = returned {
                            remaining.push(m);
                        }
                        (remaining, lines)
                    } else {
                        (
                            remaining,
                            vec![format!("Couldn't find a module in slot {slot}")],
                        )
                    }
                } else {
                    (modules, vec![format!("Invalid slot: {other}")])
                }
            }
        };
    }

    redraw_chrome("Select your update method:");
    match run_select(
        "Update one module or all?",
        vec!["all", "one"],
        MenuMode::Sub,
    ) {
        SelectResult::Selected("all") => {
            let lines = update_all_modules(
                modules,
                available_firmwares,
                multi_progress,
                style,
                controller,
            )
            .await;
            (Vec::new(), lines)
        }
        SelectResult::Selected("one") => {
            if modules.is_empty() {
                return (modules, vec!["No modules found in the controller.".into()]);
            }
            redraw_chrome("Select module to update:");
            match run_select("Select a module to update", modules, MenuMode::Sub) {
                SelectResult::Selected(module) => {
                    let (returned, lines) = update_one_module(
                        module,
                        available_firmwares,
                        multi_progress.clone(),
                        style.clone(),
                        controller,
                    )
                    .await;
                    let remaining: Vec<Module> = returned.into_iter().collect();
                    (remaining, lines)
                }
                SelectResult::Back | SelectResult::Quit => (Vec::new(), Vec::new()),
            }
        }
        SelectResult::Selected(_) | SelectResult::Back | SelectResult::Quit => {
            (modules, Vec::new())
        }
    }
}

/// Sub-menu flow for the Overwrite action. Same return contract as
/// `run_update_flow`.
async fn run_overwrite_flow(
    modules: Vec<Module>,
    available_firmwares: &[FirmwareVersion],
    multi_progress: MultiProgress,
    style: ProgressStyle,
    controller: ControllerTypes,
    slot_arg: Option<String>,
    firmware_arg: Option<String>,
) -> (Vec<Module>, Vec<String>) {
    let mut remaining = modules;

    // Pick the module
    let mut module = if let Some(arg) = slot_arg {
        match arg.parse::<u8>() {
            Ok(slot) => match remaining.iter().position(|m| m.slot == slot) {
                Some(idx) => remaining.remove(idx),
                None => {
                    return (
                        remaining,
                        vec![format!("Couldn't find a module in slot {slot}")],
                    )
                }
            },
            Err(_) => return (remaining, vec![format!("Invalid slot entered: {arg}")]),
        }
    } else if remaining.is_empty() {
        return (remaining, vec!["No modules found in the controller.".into()]);
    } else {
        redraw_chrome("Select slot to overwrite:");
        match run_select(SLOT_PROMPT, remaining, MenuMode::Sub) {
            SelectResult::Selected(m) => {
                // Sub-menu consumed the Vec; we no longer have access to the
                // others. They will be repopulated by the caller's rescan.
                remaining = Vec::new();
                m
            }
            SelectResult::Back | SelectResult::Quit => return (Vec::new(), Vec::new()),
        }
    };

    // Pick the firmware
    let new_firmware = if let Some(arg) = firmware_arg {
        match FirmwareVersion::from_filename(arg.clone()) {
            Some(fw) if available_firmwares.contains(&fw) => fw,
            Some(_) => {
                remaining.push(module);
                return (
                    remaining,
                    vec![format!("{}{} does not exist", FIRMWARE_DIR, arg)],
                );
            }
            None => {
                remaining.push(module);
                return (remaining, vec![format!("Invalid firmware entered: {arg}")]);
            }
        }
    } else {
        let valid: Vec<&FirmwareVersion> = available_firmwares
            .iter()
            .filter(|f| f.get_hardware() == module.firmware.get_hardware())
            .collect();
        if valid.is_empty() {
            remaining.push(module);
            return (remaining, vec!["No firmware(s) found for this module.".into()]);
        }
        redraw_chrome("Select firmware to upload:");
        match run_select("Which firmware to upload?", valid, MenuMode::Sub) {
            SelectResult::Selected(fw) => *fw,
            SelectResult::Back | SelectResult::Quit => {
                remaining.push(module);
                return (remaining, Vec::new());
            }
        }
    };

    // Run the upload
    let original = module.firmware.as_string();
    match module
        .overwrite_module(&new_firmware, multi_progress, style)
        .await
    {
        Ok(()) => {
            let line = format!(
                "Successfully updated slot {} from {} to {}",
                module.slot,
                original,
                new_firmware.as_string()
            );
            module.firmware = new_firmware;
            save_modules(vec![Some(module)], &controller);
            (Vec::new(), vec![line])
        }
        Err(UploadError::FirmwareCorrupted(slot)) => {
            let mut lines = vec![format!(
                "firmware upload critically failed on slot {slot}, wiping firmware..."
            )];
            module.wipe_module_error().await;
            lines.push(format!("Update failed, firmware is corrupted on slot {slot}"));
            (Vec::new(), lines)
        }
        Err(UploadError::FirmwareUntouched(slot)) => {
            (Vec::new(), vec![format!("Update failed on slot {slot}")])
        }
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
async fn main() {
    redraw_chrome("");

    let cli_arg1 = env::args().nth(1);
    let cli_arg2 = env::args().nth(2);
    let cli_arg3 = env::args().nth(3);

    // Handle the check command early — before hardware detection, service
    // management, and module scanning. Allows `check` to run on any system
    // with network access, without requiring SPI hardware.
    if cli_arg1.as_deref() == Some("check") {
        let verbose = env::args().any(|a| a == "--verbose" || a == "-v");
        match check_firmware(verbose).await {
            Ok(lines) => {
                for line in &lines {
                    println!("{line}");
                }
                exit(0);
            }
            Err(e) => {
                eprintln!("Error checking for firmware updates: {e}");
                exit(1);
            }
        }
    }

    // Detect controller
    let controller = detect_controller();

    // Snapshot service state and stop services. `go-hardware-driver` is the
    // generic SPI/GPIO driver that talks to the same modules; on legacy
    // controllers without it `is_service_active` returns false and the rest
    // of the flow is a no-op.
    let nodered = is_service_active("nodered");
    let simulink = is_service_active("go-simulink");
    let hardware_driver = is_service_active("go-hardware-driver");
    NODERED_WAS_RUNNING.store(nodered, Ordering::Relaxed);
    SIMULINK_WAS_RUNNING.store(simulink, Ordering::Relaxed);
    HARDWARE_DRIVER_WAS_RUNNING.store(hardware_driver, Ordering::Relaxed);
    if nodered {
        stop_service("nodered");
    }
    if simulink {
        stop_service("go-simulink");
    }
    if hardware_driver {
        stop_service("go-hardware-driver");
    }

    // SIGINT handler (fires only outside crossterm raw mode — i.e. during
    // async firmware upload / network fetches). Restart services and exit.
    if let Err(err) = ctrlc::set_handler(move || {
        restart_services(nodered, simulink, hardware_driver);
        exit(-1);
    }) {
        eprintln!("couldn't set sigint handler: {}", err);
        restart_services(nodered, simulink, hardware_driver);
        exit(-1);
    }

    // Scan modules in parallel with the rest of init
    let modules_fut = task::spawn(get_modules_and_save(controller));

    // Resolve firmware directory; offer download if missing
    let mut available_firmwares: Vec<FirmwareVersion> = if fs::metadata(FIRMWARE_DIR).is_err() {
        println!("No firmware found on this controller.");
        if run_confirm("Do you want to download the latest firmware?", true) {
            match check_firmware(false).await {
                Ok(lines) => {
                    for line in &lines {
                        println!("{line}");
                    }
                }
                Err(e) => eprintln!("Error downloading firmware: {e}"),
            }
            read_firmware_dir()
        } else {
            Vec::new()
        }
    } else {
        read_firmware_dir()
    };

    // Progress bar style (multi_progress is created fresh per action to avoid stale bars)
    let style = ProgressStyle::with_template("{bar:40.cyan/blue} {pos:>7}/{len:7} ({eta}) {msg}")
        .unwrap()
        .progress_chars("##-")
        .with_key("eta", |state: &ProgressState, w: &mut dyn Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        });

    // Wait for the initial module scan
    let mut modules = match modules_fut.await {
        Ok(m) => m,
        Err(_) => {
            eprintln!("Could not get module information");
            restart_services(nodered, simulink, hardware_driver);
            exit(-1);
        }
    };

    // Initial action from CLI args (if any). Subsequent loop iterations come
    // from the main menu. Sub-actions never close the app.
    let mut next_action: Option<CommandArg> = match cli_arg1.as_deref() {
        Some("scan") => Some(CommandArg::Scan),
        Some("update") => Some(CommandArg::Update),
        Some("overwrite") => Some(CommandArg::Overwrite),
        None => None,
        Some(other) => {
            eprintln!("Invalid command entered {}\n{}", other, USAGE);
            restart_services(nodered, simulink, hardware_driver);
            exit(-1);
        }
    };

    // When invoked with a CLI command, run that single action and exit instead
    // of falling back into the interactive TUI menu. Required so scripts and
    // services (e.g. go-provision-server) can call `go-modules scan` and have
    // the process terminate cleanly. The atomic is also read by `show_view`
    // so it skips the wait-for-key step in TTY-mode CLI calls.
    let started_from_cli = next_action.is_some();
    STARTED_FROM_CLI.store(started_from_cli, Ordering::Relaxed);

    loop {
        let action = match next_action.take() {
            Some(a) => a,
            None => {
                redraw_chrome("Select your action:");
                match run_select(
                    "What do you want to do?",
                    vec![
                        CommandArg::Scan,
                        CommandArg::Update,
                        CommandArg::Overwrite,
                        CommandArg::Check,
                    ],
                    MenuMode::Main,
                ) {
                    SelectResult::Selected(a) => a,
                    SelectResult::Quit => break,
                    SelectResult::Back => continue, // unreachable on main menu
                }
            }
        };

        match action {
            CommandArg::Scan => {
                redraw_chrome("Result of scanned modules:");
                if modules.is_empty() {
                    show_view(&["No modules found".into()]);
                } else {
                    show_view(&format_module_lines(&modules, &available_firmwares));
                }
            }
            CommandArg::Check => {
                let (subtitle, lines) = match check_firmware(false).await {
                    Ok(mut l) => {
                        let date = l.first()
                            .and_then(|s| s.strip_prefix("Cloud manifest last updated: "))
                            .unwrap_or("")
                            .to_string();
                        let subtitle = if date.is_empty() {
                            "Latest firmware:".to_string()
                        } else {
                            format!("Latest firmware: ({date})")
                        };
                        if l.len() >= 2 { l.drain(0..2); }
                        (subtitle, l)
                    }
                    Err(e) => (
                        "Latest firmware:".to_string(),
                        vec![format!("Error checking for firmware updates: {e}")],
                    ),
                };
                redraw_chrome(&subtitle);
                available_firmwares = read_firmware_dir();
                show_view(&lines);
            }
            CommandArg::Update => {
                let owned = std::mem::take(&mut modules);
                let multi_progress = MultiProgress::new();
                let (returned, lines) = run_update_flow(
                    owned,
                    &available_firmwares,
                    &multi_progress,
                    &style,
                    controller,
                    cli_arg2.clone(),
                )
                .await;
                modules = if returned.is_empty() {
                    // Action ran or the sub-menu consumed the modules — rescan.
                    get_modules(&controller).await
                } else {
                    returned
                };
                if !lines.is_empty() {
                    redraw_chrome("Update result:");
                    show_view(&lines);
                }
            }
            CommandArg::Overwrite => {
                let owned = std::mem::take(&mut modules);
                let (returned, lines) = run_overwrite_flow(
                    owned,
                    &available_firmwares,
                    MultiProgress::new(),
                    style.clone(),
                    controller,
                    cli_arg2.clone(),
                    cli_arg3.clone(),
                )
                .await;
                modules = if returned.is_empty() {
                    get_modules(&controller).await
                } else {
                    returned
                };
                if !lines.is_empty() {
                    redraw_chrome("Overwrite result:");
                    show_view(&lines);
                }
            }
        }

        if started_from_cli {
            break;
        }
    }

    restart_services(nodered, simulink, hardware_driver);
    exit(0);
}
