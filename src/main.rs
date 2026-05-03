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
    queue!(
        stdout,
        SetForegroundColor(Color::DarkGrey),
        Print("  \u{2190}/Esc back\r\n"),
        ResetColor,
    )
    .unwrap();
    let _ = stdout.flush();

    if !io::stdin().is_terminal() {
        // Non-TTY: just print and return immediately.
        return;
    }

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

#[derive(Serialize, Deserialize)]
struct SlotInfo {
    slot: u8,
    firmware: String,
    manufacturer: u32,
    qr_front: u32,
    qr_back: u32,
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
                            .map_err(|_| eprintln!("Could not get slot 1 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 6, slot)?,
                ),
                2 => (
                    Spidev::new(
                        File::open("/dev/spidev1.1")
                            .map_err(|_| eprintln!("Could not get slot 2 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 20, slot)?,
                ),
                3 => (
                    Spidev::new(
                        File::open("/dev/spidev2.0")
                            .map_err(|_| eprintln!("Could not get slot 3 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 7, slot)?,
                ),
                4 => (
                    Spidev::new(
                        File::open("/dev/spidev2.1")
                            .map_err(|_| eprintln!("Could not get slot 4 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 21, slot)?,
                ),
                5 => (
                    Spidev::new(
                        File::open("/dev/spidev2.2")
                            .map_err(|_| eprintln!("Could not get slot 5 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip4", 1, slot)?,
                ),
                6 => (
                    Spidev::new(
                        File::open("/dev/spidev2.3")
                            .map_err(|_| eprintln!("Could not get slot 6 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip3", 26, slot)?,
                ),
                7 => (
                    Spidev::new(
                        File::open("/dev/spidev0.0")
                            .map_err(|_| eprintln!("Could not get slot 7 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip2", 19, slot)?,
                ),
                8 => (
                    Spidev::new(
                        File::open("/dev/spidev0.1")
                            .map_err(|_| eprintln!("Could not get slot 8 spidev"))
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
                            .map_err(|_| eprintln!("Could not get slot 1 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 10, slot)?,
                ),
                2 => (
                    Spidev::new(
                        File::open("/dev/spidev1.1")
                            .map_err(|_| eprintln!("Could not get slot 2 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip0", 5, slot)?,
                ),
                3 => (
                    Spidev::new(
                        File::open("/dev/spidev2.0")
                            .map_err(|_| eprintln!("Could not get slot 3 spidev"))
                            .ok()?,
                    ),
                    get_interrupt("/dev/gpiochip3", 26, slot)?,
                ),
                4 => (
                    Spidev::new(
                        File::open("/dev/spidev2.1")
                            .map_err(|_| eprintln!("Could not get slot 4 spidev"))
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
                                .map_err(|_| eprintln!("Could not get slot 1 spidev"))
                                .ok()?,
                        ),
                        get_interrupt("/dev/gpiochip3", 5, slot)?,
                    ),
                    2 => (
                        Spidev::new(
                            File::open("/dev/spidev1.1")
                                .map_err(|_| eprintln!("Could not get slot 2 spidev"))
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
            Err(_) => return None,
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
            Err(_) => return None,
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
                        "slot {}: ANLEG IR module version {} sw: {}.{}.{}",
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
                3 => "ANLEG IR",
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

/// Format the scanned modules into space-aligned columns:
/// header row above the values, no surrounding box. Returns one
/// String per output row, intended for `show_view`.
fn format_module_lines(modules: &[Module]) -> Vec<String> {
    let headers = ["Slot", "Type", "HW", "SW Version"];

    let rows: Vec<[String; 4]> = modules
        .iter()
        .map(|m| {
            let hw = m.firmware.get_hardware();
            let sw = m.firmware.get_software();
            [
                m.slot.to_string(),
                m.type_name().to_string(),
                hw[3].to_string(),
                format!("{}.{}.{}", sw[0], sw[1], sw[2]),
            ]
        })
        .collect();

    let mut widths: [usize; 4] = [0; 4];
    for (i, h) in headers.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let render = |cells: &[&str]| -> String {
        let mut s = String::new();
        for (i, &cell) in cells.iter().enumerate() {
            let _ = write!(s, "{:<width$}", cell, width = widths[i]);
            if i + 1 < cells.len() {
                s.push_str("  ");
            }
        }
        s
    };

    let mut out = Vec::with_capacity(rows.len() + 1);
    out.push(render(&headers));
    for row in &rows {
        let cells: [&str; 4] = [&row[0], &row[1], &row[2], &row[3]];
        out.push(render(&cells));
    }
    out
}

/// Restart nodered and go-simulink if they were running before the app started.
/// Idempotent — safe to call from the ctrlc handler and from the normal exit path.
fn restart_services(nodered: bool, simulink: bool) {
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
        .map_err(|_| eprintln!("Could not get slot {slot} interrupt chip"))
        .ok()?;
    let line = chip
        .get_line(line)
        .map_err(|_| eprintln!("Could not get slot {slot} interrupt line"))
        .ok()?;
    line.async_events(
        LineRequestFlags::INPUT,
        EventRequestFlags::FALLING_EDGE,
        format!("module {slot} interrupt").as_str(),
    )
    .map_err(|err| eprintln!("Could not get slot {slot} interrupt line handle: {err}"))
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
async fn get_modules_and_save(controller: ControllerTypes) -> Vec<Module> {
    let modules = get_modules(&controller).await;
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

/// save all modules to /lib/firmware/gocontroll/modules.json and /usr/module-firmware/modules.txt
fn save_modules(modules: Vec<Option<Module>>, controller: &ControllerTypes) -> Vec<Module> {
    let slot_count = match controller {
        ControllerTypes::ModulineIV => 8usize,
        ControllerTypes::ModulineMini => 4usize,
        ControllerTypes::ModulineDisplay => 2usize,
    };

    let mut slots: Vec<SlotInfo> = fs::read_to_string("/lib/firmware/gocontroll/modules.json")
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<SlotInfo>>(&s).ok())
        .filter(|v| v.len() == slot_count)
        .unwrap_or_else(|| {
            (1..=slot_count as u8)
                .map(|s| SlotInfo {
                    slot: s,
                    firmware: String::new(),
                    manufacturer: 0,
                    qr_front: 0,
                    qr_back: 0,
                })
                .collect()
        });

    for (i, module) in modules.iter().enumerate() {
        if let Some(module) = module {
            if let Some(info) = slots.iter_mut().find(|s| s.slot == module.slot) {
                info.firmware = module.firmware.as_string();
                info.manufacturer = module.manufacturer;
                info.qr_front = module.qr_front;
                info.qr_back = module.qr_back;
            }
        } else {
            let slot_num = (i + 1) as u8;
            if let Some(info) = slots.iter_mut().find(|s| s.slot == slot_num) {
                info.firmware = String::new();
                info.manufacturer = 0;
                info.qr_front = 0;
                info.qr_back = 0;
            }
        }
    }

    // Write JSON to /lib/firmware/gocontroll/modules.json
    if fs::create_dir_all("/lib/firmware/gocontroll/").is_err() {
        eprintln!("Could not create /lib/firmware/gocontroll/");
    }
    match serde_json::to_string_pretty(&slots) {
        Ok(json) => {
            if fs::write("/lib/firmware/gocontroll/modules.json", json).is_err() {
                eprintln!("Could not save module layout to /lib/firmware/gocontroll/modules.json");
            }
        }
        Err(e) => eprintln!("Could not serialize module layout: {}", e),
    }

    // Write legacy text format to /usr/module-firmware/modules.txt for older Node-RED
    if fs::create_dir_all("/usr/module-firmware/").is_err() {
        eprintln!("Could not create /usr/module-firmware/");
    }
    let text_content = format!(
        "{}\n{}\n{}\n{}",
        slots.iter().map(|s| s.firmware.as_str()).collect::<Vec<_>>().join(":"),
        slots.iter().map(|s| s.manufacturer.to_string()).collect::<Vec<_>>().join(":"),
        slots.iter().map(|s| s.qr_front.to_string()).collect::<Vec<_>>().join(":"),
        slots.iter().map(|s| s.qr_back.to_string()).collect::<Vec<_>>().join(":"),
    );
    if fs::write("/usr/module-firmware/modules.txt", text_content).is_err() {
        eprintln!("Could not save module layout to /usr/module-firmware/modules.txt");
    }

    modules.into_iter().flatten().collect()
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

    // Snapshot service state and stop services
    let nodered = is_service_active("nodered");
    let simulink = is_service_active("go-simulink");
    NODERED_WAS_RUNNING.store(nodered, Ordering::Relaxed);
    SIMULINK_WAS_RUNNING.store(simulink, Ordering::Relaxed);
    if nodered {
        stop_service("nodered");
    }
    if simulink {
        stop_service("go-simulink");
    }

    // SIGINT handler (fires only outside crossterm raw mode — i.e. during
    // async firmware upload / network fetches). Restart services and exit.
    if let Err(err) = ctrlc::set_handler(move || {
        restart_services(nodered, simulink);
        exit(-1);
    }) {
        eprintln!("couldn't set sigint handler: {}", err);
        restart_services(nodered, simulink);
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
            restart_services(nodered, simulink);
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
            restart_services(nodered, simulink);
            exit(-1);
        }
    };

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
                    show_view(&format_module_lines(&modules));
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
    }

    restart_services(nodered, simulink);
    exit(0);
}
