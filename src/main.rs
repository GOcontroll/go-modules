use std::{
    env,
    fmt::{Display, Write},
    fs::{self, File},
    io::{self, IsTerminal, Write as _},
    mem,
    process::{exit, Command},
    time::Duration,
};

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

fn draw_menu<T: Display>(options: &[T], selected: usize, first: bool) {
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
    queue!(
        stdout,
        terminal::Clear(terminal::ClearType::CurrentLine),
        Print(format!("{}\r\n", SEP)),
        terminal::Clear(terminal::ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        Print("  \u{2191}/\u{2193} navigate   Enter select   q quit\r\n"),
        ResetColor,
    )
    .unwrap();
    stdout.flush().unwrap();
}

fn run_select<T: Display>(prompt: &str, mut options: Vec<T>, on_cancel: impl Fn()) -> T {
    if !io::stdin().is_terminal() {
        println!("{}", prompt);
        for (i, opt) in options.iter().enumerate() {
            println!("  {}. {}", i + 1, opt);
        }
        loop {
            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_err() {
                return options.remove(0);
            }
            if let Ok(n) = input.trim().parse::<usize>() {
                if n >= 1 && n <= options.len() {
                    return options.remove(n - 1);
                }
            }
        }
    }
    let mut selected = 0usize;
    terminal::enable_raw_mode().unwrap();
    let _ = execute!(io::stdout(), cursor::Hide);
    draw_menu(&options, selected, true);
    loop {
        match event::read() {
            Ok(Event::Key(KeyEvent { code: KeyCode::Up, .. })) => {
                if selected > 0 {
                    selected -= 1;
                }
                draw_menu(&options, selected, false);
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Down, .. })) => {
                if selected + 1 < options.len() {
                    selected += 1;
                }
                draw_menu(&options, selected, false);
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Enter, .. })) => {
                let _ = terminal::disable_raw_mode();
                let _ = execute!(io::stdout(), cursor::Show);
                return options.remove(selected);
            }
            Ok(Event::Key(KeyEvent { code: KeyCode::Char('q'), .. }))
            | Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }))
            | Ok(Event::Key(KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            })) => {
                let _ = terminal::disable_raw_mode();
                let _ = execute!(io::stdout(), cursor::Show);
                on_cancel();
                unreachable!()
            }
            _ => {}
        }
    }
}

fn run_confirm(prompt: &str, default: bool, on_cancel: impl Fn()) -> bool {
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
        Err(_) => {
            on_cancel();
            unreachable!()
        }
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

fn print_hline(widths: &[usize], l: &str, m: &str, r: &str) {
    let mut s = l.to_string();
    for (i, &w) in widths.iter().enumerate() {
        s.push_str(&"═".repeat(w + 2));
        if i + 1 < widths.len() {
            s.push_str(m);
        }
    }
    s.push_str(r);
    println!("{s}");
}

fn print_data_row(widths: &[usize], cells: &[String]) {
    let mut s = String::from("║");
    for (i, &w) in widths.iter().enumerate() {
        let cell = cells.get(i).map(String::as_str).unwrap_or("");
        let _ = write!(s, " {:<width$} ║", cell, width = w);
    }
    println!("{s}");
}

fn print_module_table(modules: &[Module]) {
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

    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    print_hline(&widths, "╔", "╦", "╗");
    print_data_row(&widths, &headers.map(str::to_string));
    print_hline(&widths, "╠", "╬", "╣");
    for row in &rows {
        print_data_row(&widths, row);
    }
    print_hline(&widths, "╚", "╩", "╝");
}

/// error out and restart nodered and go-simulink if required
fn err_n_restart_services(nodered: bool, simulink: bool) -> ! {
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
    exit(-1);
}

/// exit with a success code and restart the nodered and go-simulink services if required
fn success(nodered: bool, simulink: bool) -> ! {
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
    exit(0);
}

/// error out without restarting any services
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

/// Fetch the latest firmware files from the GOcontroll cloud and save them locally.
/// Validates SHA256 checksums. Prints status for each module.
/// With verbose=true, also prints release dates and changelogs.
async fn check_firmware(verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    println!("Checking for firmware updates...");

    let client = reqwest::Client::new();

    // Fetch main manifest
    let main_manifest: CloudMainManifest = client
        .get(format!("{}/modules/manifest.json", CLOUD_BASE_URL))
        .send()
        .await?
        .json()
        .await?;

    println!("Cloud manifest last updated: {}\n", main_manifest.updated);

    // Ensure firmware directory exists
    fs::create_dir_all(FIRMWARE_DIR)
        .map_err(|e| format!("Could not create firmware directory {FIRMWARE_DIR}: {e}"))?;

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
                    eprintln!("Could not parse manifest {}: {e}", entry.manifest);
                    continue;
                }
            },
            Err(e) => {
                eprintln!("Could not fetch manifest {}: {e}", entry.manifest);
                continue;
            }
        };

        // The first entry in releases is the latest version
        let latest = match sub_manifest.releases.first() {
            Some(r) => r,
            None => {
                eprintln!(
                    "{} (HW {}): no releases found",
                    sub_manifest.name, sub_manifest.hardware_version
                );
                continue;
            }
        };

        // Extract filename from the cloud file path (e.g. "modules/20100103/20-10-1-3-2-0-3.srec")
        let filename = match latest.file.split('/').last() {
            Some(f) if !f.is_empty() => f,
            _ => {
                eprintln!("Invalid file path in manifest: {}", latest.file);
                continue;
            }
        };

        let local_path = format!("{}{}", FIRMWARE_DIR, filename);

        // Check if the file already exists and has a valid checksum
        if let Ok(existing_data) = fs::read(&local_path) {
            if verify_sha256(&existing_data, &latest.sha256) {
                println!(
                    "{} (HW {}): v{} - already up to date",
                    sub_manifest.name, sub_manifest.hardware_version, latest.sw_version
                );
                if verbose {
                    println!("  Released: {}", latest.date);
                    println!("  Changes:  {}", latest.changelog);
                    println!();
                }
                continue;
            }
            // File exists but checksum is wrong — re-download
            println!(
                "{} (HW {}): v{} - local file corrupted, re-downloading...",
                sub_manifest.name, sub_manifest.hardware_version, latest.sw_version
            );
        }

        // Download the firmware file
        let data = match client
            .get(format!("{}/{}", CLOUD_BASE_URL, latest.file))
            .send()
            .await
        {
            Ok(resp) => match resp.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "{} (HW {}): download failed: {e}",
                        sub_manifest.name, sub_manifest.hardware_version
                    );
                    continue;
                }
            },
            Err(e) => {
                eprintln!(
                    "{} (HW {}): download failed: {e}",
                    sub_manifest.name, sub_manifest.hardware_version
                );
                continue;
            }
        };

        // Verify SHA256 checksum of downloaded data
        if !verify_sha256(&data, &latest.sha256) {
            eprintln!(
                "{} (HW {}): v{} - checksum verification failed! File not saved.",
                sub_manifest.name, sub_manifest.hardware_version, latest.sw_version
            );
            continue;
        }

        // Save to local firmware directory
        if let Err(e) = fs::write(&local_path, &data) {
            eprintln!(
                "{} (HW {}): could not save {}: {e}",
                sub_manifest.name, sub_manifest.hardware_version, filename
            );
            continue;
        }

        println!(
            "{} (HW {}): v{} - downloaded",
            sub_manifest.name, sub_manifest.hardware_version, latest.sw_version
        );
        if verbose {
            println!("  Released: {}", latest.date);
            println!("  Changes:  {}", latest.changelog);
            println!();
        }
    }

    Ok(())
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

async fn update_one_module(
    module: Module,
    available_firmwares: &[FirmwareVersion],
    multi_progress: MultiProgress,
    style: ProgressStyle,
    controller: ControllerTypes,
    nodered: bool,
    simulink: bool,
) -> ! {
    match module
        .update_module(available_firmwares, multi_progress, style)
        .await
    {
        Ok(Ok(module)) => {
            println!(
                "Successfully updated slot {} to {}",
                module.slot,
                module.firmware.as_string()
            );
            save_modules(vec![Some(module)], &controller);
            success(nodered, simulink);
        }
        Err(err) => match err {
            UploadError::FirmwareCorrupted(slot) => {
                err_n_die(
                    format!("Update failed, firmware is corrupted on slot {}", slot).as_str(),
                );
            }
            UploadError::FirmwareUntouched(slot) => {
                eprintln!("Update failed on slot {}", slot);
                err_n_restart_services(nodered, simulink);
            }
        },
        Ok(Err(module)) => {
            eprintln!(
                "Update failed, no update available for slot {}: {}",
                module.slot,
                module.firmware.as_string()
            );
            err_n_restart_services(nodered, simulink);
        }
    }
}

async fn update_all_modules(
    modules: Vec<Module>,
    available_firmwares: &[FirmwareVersion],
    multi_progress: &MultiProgress,
    style: &ProgressStyle,
    controller: ControllerTypes,
    nodered: bool,
    simulink: bool,
) -> ! {
    let mut upload_results = Vec::with_capacity(modules.len());
    let mut new_modules = Vec::with_capacity(modules.len());
    let mut firmware_corrupted = false;
    let mut set = JoinSet::new();
    for module in modules {
        let available_firmwares = available_firmwares.to_owned();
        let multi_progress = multi_progress.clone();
        let style = style.clone();
        set.spawn(async move {
            module
                .update_module(available_firmwares.as_slice(), multi_progress, style)
                .await
        });
    }
    for _ in 0..set.len() {
        upload_results.push(set.join_next().await.unwrap().unwrap());
    }
    for result in upload_results {
        match result {
            Ok(Ok(module)) => {
                //module updated
                new_modules.push(Some(module))
            }
            Err(err) => match err {
                UploadError::FirmwareCorrupted(slot) => {
                    eprintln!("Update failed, firmware is corrupted on slot {}", slot);
                    firmware_corrupted = true;
                }
                UploadError::FirmwareUntouched(slot) => {
                    eprintln!("Update failed on slot {}", slot);
                }
            },
            Ok(Err(_)) => (), //no new firmwares available
        }
    }
    if !new_modules.is_empty() {
        println!("Successfully updated:");
        for module in &new_modules {
            println!(
                "slot {} to {}",
                module.as_ref().unwrap().slot,
                module.as_ref().unwrap().firmware.as_string()
            );
        }
    } else if !firmware_corrupted {
        eprintln!("No updates found for the modules in this controller.");
    }
    save_modules(new_modules, &controller);
    if firmware_corrupted {
        err_n_die("could not restart nodered and go-simulink services due to corrupted firmware.");
    }

    success(nodered, simulink);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 3)]
async fn main() {
    let _ = execute!(io::stdout(), terminal::Clear(terminal::ClearType::All), cursor::MoveTo(0, 0));
    print_banner();
    #[cfg(debug_assertions)]
    println!("Debug version");

    // Handle the check command early — before hardware detection, service management, and module scanning.
    // This allows `check` to run on any system with network access, without requiring SPI hardware.
    if env::args().nth(1).as_deref() == Some("check") {
        let verbose = env::args().any(|a| a == "--verbose" || a == "-v");
        if let Err(e) = check_firmware(verbose).await {
            eprintln!("Error checking for firmware updates: {e}");
            exit(1);
        }
        exit(0);
    }

    //get the controller hardware
    let hardware_string= fs::read_to_string("/sys/firmware/devicetree/base/hardware").unwrap_or_else(|_|{
		err_n_die("Could not find a hardware description file, this feature is not supported by your hardware.");
	});

    let controller = if hardware_string.contains("Moduline IV") || hardware_string.contains("Moduline L4") {
        ControllerTypes::ModulineIV
    } else if hardware_string.contains("Moduline Mini") || hardware_string.contains("Moduline M1") {
        ControllerTypes::ModulineMini
    } else if hardware_string.contains("Moduline Display") || hardware_string.contains("Moduline HMI1") {
        ControllerTypes::ModulineDisplay
    } else {
        err_n_die(
            format!(
                "{} is not a supported GOcontroll Moduline product. Can't proceed",
                hardware_string
            )
            .as_str(),
        );
    };

    //stop services potentially trying to use the module
    let output = Command::new("systemctl")
        .arg("is-active")
        .arg("nodered")
        .output()
        .unwrap()
        .stdout;

    let nodered = !String::from_utf8_lossy(&output).into_owned().contains("in");

    let output = Command::new("systemctl")
        .arg("is-active")
        .arg("go-simulink")
        .output()
        .unwrap()
        .stdout;

    let simulink = !String::from_utf8_lossy(&output).into_owned().contains("in");

    if nodered {
        _ = Command::new("systemctl")
            .arg("stop")
            .arg("nodered")
            .status();
    }

    if simulink {
        _ = Command::new("systemctl")
            .arg("stop")
            .arg("go-simulink")
            .status();
    }

    match ctrlc::set_handler(move || { err_n_restart_services(nodered, simulink); }) {
        Ok(()) => (),
        Err(err) => {
            eprintln!("couldn't set sigint handler: {}", err);
            err_n_restart_services(nodered, simulink);
        }
    }

    //start getting module information in a seperate task while other init is happening
    let modules_fut = task::spawn(get_modules_and_save(controller));

    //get all the firmwares
    let read_firmware_dir = || -> Vec<FirmwareVersion> {
        fs::read_dir(FIRMWARE_DIR)
            .unwrap_or_else(|_| {
                eprintln!("Could not find the firmware folder");
                err_n_restart_services(nodered, simulink);
            })
            .map(|file| file.unwrap().file_name().to_str().unwrap().to_string())
            .filter(|file_name| file_name.ends_with(".srec"))
            .map(|firmware| FirmwareVersion::from_filename(firmware))
            .flatten()
            .collect()
    };

    let available_firmwares: Vec<FirmwareVersion> = if fs::metadata(FIRMWARE_DIR).is_err() {
        println!("No firmware found on this controller.");
        let download = run_confirm(
            "Do you want to download the latest firmware?",
            true,
            || { err_n_restart_services(nodered, simulink); },
        );
        if download {
            if let Err(e) = check_firmware(false).await {
                eprintln!("Error downloading firmware: {e}");
                err_n_restart_services(nodered, simulink);
            }
            read_firmware_dir()
        } else {
            vec![]
        }
    } else {
        read_firmware_dir()
    };

    //create the base for the progress bar(s)
    let multi_progress = MultiProgress::new();
    let style = ProgressStyle::with_template("{bar:40.cyan/blue} {pos:>7}/{len:7} ({eta}) {msg}")
        .unwrap()
        .progress_chars("##-")
        .with_key("eta", |state: &ProgressState, w: &mut dyn Write| {
            write!(w, "{:.1}s", state.eta().as_secs_f64()).unwrap()
        });

    let command = if let Some(arg) = env::args().nth(1) {
        match arg.as_str() {
            "scan" => CommandArg::Scan,
            "update" => CommandArg::Update,
            "overwrite" => CommandArg::Overwrite,
            _ => {
                eprintln!("Invalid command entered {}\n{}", arg, USAGE);
                err_n_restart_services(nodered, simulink);
            }
        }
    } else {
        run_select(
            "What do you want to do?",
            vec![
                CommandArg::Scan,
                CommandArg::Update,
                CommandArg::Overwrite,
                CommandArg::Check,
            ],
            || { err_n_restart_services(nodered, simulink); },
        )
    };

    // If the user selected Check from the TUI, run it and exit cleanly
    if let CommandArg::Check = command {
        // Services were stopped — restart them before running check (check doesn't need them stopped)
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
        if let Err(e) = check_firmware(false).await {
            eprintln!("Error checking for firmware updates: {e}");
            exit(1);
        }
        exit(0);
    }

    //get the modules from the previously started task
    let modules = modules_fut.await.unwrap_or_else(|_| {
        eprintln!("Could not get module information");
        err_n_restart_services(nodered, simulink);
    });

    match command {
        CommandArg::Scan => {
            if !modules.is_empty() {
                print_module_table(&modules);
            } else {
                println!("No modules found")
            }
            success(nodered, simulink);
        }

        CommandArg::Update => {
            //find the update type
            if let Some(arg) = env::args().nth(2) {
                match arg.as_str() {
                    "all" => {
                        update_all_modules(
                            modules,
                            &available_firmwares,
                            &multi_progress,
                            &style,
                            controller,
                            nodered,
                            simulink,
                        )
                        .await
                    }
                    _ => {
                        if let Ok(slot) = arg.parse::<u8>() {
                            let module = modules
                                .into_iter()
                                .find(|module| module.slot == slot)
                                .take()
                                .unwrap_or_else(|| {
                                    eprintln!("Couldn't find a module in slot {}", slot);
                                    err_n_restart_services(nodered, simulink);
                                });
                            update_one_module(
                                module,
                                &available_firmwares,
                                multi_progress,
                                style,
                                controller,
                                nodered,
                                simulink,
                            )
                            .await;
                        } else {
                            eprintln!("{}", USAGE);
                            err_n_restart_services(nodered, simulink);
                        }
                    }
                }
            } else {
                match run_select(
                    "Update one module or all?",
                    vec!["all", "one"],
                    || { err_n_restart_services(nodered, simulink); },
                ) {
                    "all" => {
                        update_all_modules(
                            modules,
                            &available_firmwares,
                            &multi_progress,
                            &style,
                            controller,
                            nodered,
                            simulink,
                        )
                        .await
                    }
                    "one" => {
                        if !modules.is_empty() {
                            let module = run_select(
                                "Select a module to update",
                                modules,
                                || { err_n_restart_services(nodered, simulink); },
                            );
                            update_one_module(
                                module,
                                &available_firmwares,
                                multi_progress,
                                style,
                                controller,
                                nodered,
                                simulink,
                            )
                            .await
                        } else {
                            eprintln!("No modules found in the controller.");
                            err_n_restart_services(nodered, simulink);
                        }
                    }
                    _ => {
                        eprintln!("You shouldn't be here, turn back to whence you came");
                        err_n_restart_services(nodered, simulink);
                    }
                }
            };
        }

        CommandArg::Overwrite => {
            let mut module = if let Some(arg) = env::args().nth(2) {
                if let Ok(slot) = arg.parse::<u8>() {
                    modules
                        .into_iter()
                        .find(|module| module.slot == slot)
                        .take()
                        .unwrap_or_else(|| {
                            eprintln!("Couldn't find a module in slot {}", slot);
                            err_n_restart_services(nodered, simulink);
                        })
                } else {
                    eprintln!("Invalid slot entered\n{}", USAGE);
                    err_n_restart_services(nodered, simulink);
                }
            } else if !modules.is_empty() {
                run_select(SLOT_PROMPT, modules, || { err_n_restart_services(nodered, simulink); })
            } else {
                eprintln!("No modules found in the controller.");
                err_n_restart_services(nodered, simulink);
            };

            let new_firmware = if let Some(arg) = env::args().nth(3) {
                if let Some(firmware) = FirmwareVersion::from_filename(arg.clone()) {
                    if available_firmwares.contains(&firmware) {
                        firmware
                    } else {
                        eprintln!("{}{} does not exist", FIRMWARE_DIR, arg);
                        err_n_restart_services(nodered, simulink);
                    }
                } else {
                    eprintln!("Invalid firmware entered\n{}", USAGE);
                    err_n_restart_services(nodered, simulink);
                }
            } else {
                let valid_firmwares: Vec<&FirmwareVersion> = available_firmwares
                    .iter()
                    .filter(|firmware| firmware.get_hardware() == module.firmware.get_hardware())
                    .collect();
                if !valid_firmwares.is_empty() {
                    *run_select(
                        "Which firmware to upload?",
                        valid_firmwares,
                        || { err_n_restart_services(nodered, simulink); },
                    )
                } else {
                    eprintln!("No firmware(s) found for this module.");
                    err_n_restart_services(nodered, simulink);
                }
            };
            match module
                .overwrite_module(&new_firmware, multi_progress, style)
                .await
            {
                Ok(()) => {
                    println!(
                        "Successfully updated slot {} from {} to {}",
                        module.slot,
                        module.firmware.as_string(),
                        new_firmware.as_string()
                    );
                    module.firmware = new_firmware;
                    save_modules(vec![Some(module)], &controller);
                    success(nodered, simulink);
                }
                Err(err) => match err {
                    UploadError::FirmwareCorrupted(slot) => {
                        eprintln!(
                            "firmware upload critically failed on slot {}, wiping firmware...",
                            slot
                        );
                        module.wipe_module_error().await;
                        err_n_die(
                            format!("Update failed, firmware is corrupted on slot {}", slot)
                                .as_str(),
                        );
                    }
                    UploadError::FirmwareUntouched(slot) => {
                        eprintln!("Update failed on slot {}", slot);
                        err_n_restart_services(nodered, simulink);
                    }
                },
            }
        }

        // Check is handled earlier in main before this match, this arm is unreachable
        CommandArg::Check => unreachable!(),
    }
}
