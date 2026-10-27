//! Desktop entry point.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use zapfast::{app, backend, paths, settings, single_instance};

use clap::Parser;

/// A fast, native WhatsApp client.
#[derive(Debug, Parser)]
#[command(name = "zap-faster", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Control>,
    /// Log more from the WhatsApp library.
    #[arg(short, long)]
    verbose: bool,
    /// Start in the tray without opening a window, when the tray is available
    /// and Zap Faster keeps running in the background. For login autostart.
    #[arg(long)]
    start_hidden: bool,

    /// Open an international number or WhatsApp chat link, without sending its text.
    #[arg(value_name = "CHAT", conflicts_with = "start_hidden")]
    whatsapp_url: Option<String>,

    /// Start with offline sample chats.
    #[cfg(feature = "demo")]
    #[arg(long)]
    demo: bool,

    /// Prepare an offline, scripted tour. Press Space to play or replay it.
    #[cfg(feature = "demo")]
    #[arg(long, conflicts_with = "demo_page")]
    demo_tour: bool,

    /// Start the tour automatically after this many milliseconds.
    #[cfg(feature = "demo")]
    #[arg(long, requires = "demo_tour", value_name = "MS")]
    demo_tour_delay: Option<u64>,

    /// Save pointer and shortcut timing for video captions (demo tour only).
    #[cfg(feature = "demo")]
    #[arg(long, requires = "demo_tour", value_name = "PATH")]
    demo_tour_events: Option<std::path::PathBuf>,

    /// Which tour to play: `launch` (41 seconds) or `whats-new` (what 0.16
    /// added, 86 seconds).
    #[cfg(feature = "demo")]
    #[arg(
        long,
        requires = "demo_tour",
        value_name = "NAME",
        default_value = "launch",
        value_parser = clap::builder::PossibleValuesParser::new(zapfast::demo::tour::Script::NAMES),
    )]
    demo_tour_script: String,

    /// Play the tour at once on a virtual clock, save every frame as a PNG in
    /// this directory, and quit when it ends.
    #[cfg(feature = "demo")]
    #[arg(
        long,
        requires = "demo_tour",
        conflicts_with = "demo_tour_delay",
        value_name = "DIR"
    )]
    demo_tour_frames: Option<std::path::PathBuf>,

    /// Frames per second for `--demo-tour-frames` (default 30).
    #[cfg(feature = "demo")]
    #[arg(long, requires = "demo_tour_frames", value_name = "FPS", value_parser = clap::value_parser!(u32).range(1..=120))]
    demo_fps: Option<u32>,

    /// Preview macOS content layout on another platform (demo only).
    #[cfg(feature = "demo")]
    #[arg(long, requires = "demo")]
    demo_macos: bool,

    /// Demo view: `chat`, `phone-menu`, `empty`, `settings`, `login`,
    /// `pair`, `shortcuts`, `about`, `info`, `mention`, `light`, or a comma-separated
    /// mix such as `chat,light`.
    #[cfg(feature = "demo")]
    #[arg(long)]
    demo_page: Option<String>,

    /// Save the demo window as a PNG and exit. Implies `--demo`.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "PATH")]
    demo_shot: Option<std::path::PathBuf>,
    /// Screenshot window size as WxH logical points.
    #[arg(long, value_name = "WxH")]
    demo_size: Option<String>,

    /// Delay before taking the screenshot, in milliseconds.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "MS", default_value_t = 1500)]
    demo_shot_delay: u64,

    /// Hold a synthetic pointer at `X,Y` (logical points) to capture hover
    /// states in demo screenshots without moving the real cursor.
    #[cfg(feature = "demo")]
    #[arg(long, value_name = "X,Y")]
    demo_hover: Option<String>,
}

#[derive(Debug, clap::Subcommand)]
enum Control {
    #[command(flatten)]
    Access(Automation),
    /// Private read/watch/send access to an explicitly opted-in running account.
    Automation {
        #[command(subcommand)]
        operation: Automation,
    },
    /// Reload palettes in an already-running Zap Faster without showing its window.
    ReloadThemes,
    /// Quit an already-running Zap Faster, which closing its window leaves in the tray.
    Quit,
}

#[derive(Debug, clap::Subcommand)]
enum Automation {
    /// List conversations available to local automation.
    Chats {
        #[arg(long)]
        account: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        after: Option<String>,
    },
    /// List address-book contacts; --all includes unsaved contacts.
    Contacts {
        #[arg(long)]
        account: String,
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        after: Option<String>,
    },
    /// Read a conversation without sending read receipts.
    Messages {
        chat: String,
        #[arg(long)]
        account: String,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        before: Option<i64>,
    },
    /// Search archived message text and captions.
    Search {
        text: String,
        #[arg(long)]
        account: String,
        #[arg(long)]
        chat: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        before_row: Option<i64>,
    },
    /// Read one archive page (compatibility command).
    Read {
        #[arg(long)]
        account: String,
        #[arg(long)]
        chat: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, requires = "before_id")]
        before_time: Option<i64>,
        #[arg(long, requires = "before_time")]
        before_id: Option<String>,
    },
    /// Watch new incoming messages. Without --seconds, runs until interrupted.
    Watch {
        #[arg(long)]
        account: String,
        #[arg(long)]
        chat: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        seconds: Option<u32>,
        #[arg(long)]
        since: Option<i64>,
        #[arg(long, default_value_t = 1)]
        interval: u32,
        #[arg(long, requires_all = ["after_id", "chat"])]
        after_row: Option<i64>,
        #[arg(long, requires = "after_row")]
        after_id: Option<String>,
    },
    /// Send text, stdin (-), or repeated --file attachments under a stable request id.
    Send {
        target: Option<String>,
        text: Option<String>,
        #[arg(long)]
        account: String,
        #[arg(long, conflicts_with = "target")]
        chat: Option<String>,
        #[arg(long)]
        request_id: String,
        #[arg(long, conflicts_with = "text")]
        text_file: Option<std::path::PathBuf>,
        #[arg(long = "file")]
        files: Vec<std::path::PathBuf>,
    },
    /// Check a previous send without retrying it.
    Status {
        #[arg(long)]
        account: String,
        #[arg(long)]
        chat: String,
        #[arg(long)]
        request_id: String,
    },
}
fn automation_command(operation: &Automation, runtime: &std::path::Path) -> Result<(), String> {
    use std::io::Read;
    use zapfast::automation::{Attachment, Request};
    let mut cursor = None;
    let mut row_cursor = None;
    let mut collection_cursor = None;
    let mut message_cursor = None;
    let mut search_cursor = None;
    let mut remaining = None;
    let mut page_limit = 25;
    let mut interval = 1;
    let mut watching = false;
    let mut duration = None;
    let request = match operation {
        Automation::Chats {
            account,
            after,
            limit,
        } => Request::Chats {
            account: account.clone(),
            after: after.clone(),
            limit: *limit,
        },
        Automation::Contacts {
            account,
            all,
            after,
            limit,
        } => Request::Contacts {
            account: account.clone(),
            all: *all,
            after: after.clone(),
            limit: *limit,
        },
        Automation::Messages {
            account,
            chat,
            before,
            limit,
        } => {
            remaining = Some(*limit);
            Request::Read {
                account: account.clone(),
                chat: chat.clone(),
                before: before.map(|time| (time, String::new())),
                limit: (*limit).min(25),
            }
        }
        Automation::Search {
            account,
            chat,
            text,
            before_row,
            limit,
        } => {
            remaining = Some(*limit);
            Request::Search {
                account: account.clone(),
                chat: chat.clone().unwrap_or_default(),
                text: text.clone(),
                before: *before_row,
                limit: (*limit).min(25),
            }
        }
        Automation::Read {
            account,
            chat,
            limit,
            before_time,
            before_id,
        } => Request::Read {
            account: account.clone(),
            chat: chat.clone(),
            before: before_time.zip(before_id.clone()),
            limit: *limit,
        },
        Automation::Watch {
            account,
            chat,
            limit,
            seconds,
            since,
            interval: poll,
            after_row,
            after_id,
        } => {
            if !(1..=60).contains(poll)
                || seconds.is_some_and(|seconds| !(1..=3600).contains(&seconds))
            {
                return Err(
                    "Watch interval must be 1 through 60 seconds; duration 1 through 3600".into(),
                );
            }
            watching = true;
            interval = *poll;
            duration = *seconds;
            if let Some(chat) = chat {
                cursor = after_row.zip(after_id.clone());
                Request::Watch {
                    account: account.clone(),
                    chat: chat.clone(),
                    after: cursor.clone(),
                    limit: *limit,
                }
            } else {
                Request::WatchAccount {
                    account: account.clone(),
                    since: since.unwrap_or_else(|| {
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64
                    }),
                    after: None,
                    limit: *limit,
                }
            }
        }
        Automation::Send {
            account,
            target,
            chat,
            request_id,
            text,
            text_file,
            files,
        } => {
            let chat = chat
                .as_ref()
                .or(target.as_ref())
                .ok_or("Send requires a chat id")?
                .clone();
            let mut body = Vec::new();
            if let Some(path) = text_file {
                std::fs::File::open(path)
                    .map_err(|_| "Could not open text file")?
                    .take(4001)
                    .read_to_end(&mut body)
                    .map_err(|_| "Could not read text file")?;
            } else if text.as_deref() == Some("-") {
                std::io::stdin()
                    .take(4001)
                    .read_to_end(&mut body)
                    .map_err(|_| "Could not read stdin")?;
            } else if let Some(text) = text {
                body.extend_from_slice(text.as_bytes());
            }
            if body.len() > 4000 {
                return Err("Text must be at most 4000 UTF-8 bytes".into());
            }
            let text = String::from_utf8(body).map_err(|_| "Text must be UTF-8")?;
            if files.is_empty() {
                Request::Send {
                    account: account.clone(),
                    chat,
                    request_id: request_id.to_ascii_lowercase(),
                    text,
                }
            } else {
                if files.len() > 8 {
                    return Err("At most eight attachments per request".into());
                }
                use sha2::{Digest, Sha256};
                let mut attachments = Vec::new();
                for path in files {
                    let path = std::fs::canonicalize(path)
                        .map_err(|_| "Attachment must be an existing readable file")?;
                    let source =
                        std::fs::File::open(&path).map_err(|_| "Could not open attachment")?;
                    let metadata = source
                        .metadata()
                        .map_err(|_| "Could not inspect attachment")?;
                    if !metadata.is_file() || metadata.len() > zapfast::automation::FILE_LIMIT {
                        return Err("Attachments must be files of at most 64 MiB".into());
                    }
                    let mut source = source.take(zapfast::automation::FILE_LIMIT + 1);
                    let mut hash = Sha256::new();
                    let mut buffer = [0u8; 65536];
                    let mut size = 0u64;
                    loop {
                        let count = source
                            .read(&mut buffer)
                            .map_err(|_| "Could not read attachment")?;
                        if count == 0 {
                            break;
                        }
                        size += count as u64;
                        hash.update(&buffer[..count]);
                    }
                    if size != metadata.len() {
                        return Err("Attachment changed while being read".into());
                    }
                    let sha256 = hash
                        .finalize()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect();
                    attachments.push(Attachment { path, size, sha256 });
                }
                Request::SendFiles {
                    account: account.clone(),
                    chat,
                    request_id: request_id.to_ascii_lowercase(),
                    text,
                    files: attachments,
                }
            }
        }
        Automation::Status {
            account,
            chat,
            request_id,
        } => Request::Status {
            account: account.clone(),
            chat: chat.clone(),
            request_id: request_id.to_ascii_lowercase(),
        },
    };
    if !request.valid() {
        return Err("Invalid request; page size must be 1 through 25, and send needs a stable 32-digit hexadecimal request id and text or files".into());
    }
    let deadline = duration.map(|seconds| {
        std::time::Instant::now() + std::time::Duration::from_secs(u64::from(seconds))
    });
    loop {
        let current = match &request {
            Request::Watch {
                account,
                chat,
                limit,
                ..
            } => Request::Watch {
                account: account.clone(),
                chat: chat.clone(),
                limit: *limit,
                after: cursor.clone(),
            },
            Request::WatchAccount {
                account,
                since,
                limit,
                ..
            } => Request::WatchAccount {
                account: account.clone(),
                since: *since,
                limit: *limit,
                after: row_cursor,
            },
            Request::Chats {
                account,
                after,
                limit,
            } => Request::Chats {
                account: account.clone(),
                after: collection_cursor.clone().or_else(|| after.clone()),
                limit: *limit,
            },
            Request::Contacts {
                account,
                all,
                after,
                limit,
            } => Request::Contacts {
                account: account.clone(),
                all: *all,
                after: collection_cursor.clone().or_else(|| after.clone()),
                limit: *limit,
            },
            Request::Read {
                account,
                chat,
                before,
                limit,
            } if remaining.is_some() => Request::Read {
                account: account.clone(),
                chat: chat.clone(),
                before: message_cursor.clone().or_else(|| before.clone()),
                limit: remaining.unwrap_or(*limit).min(page_limit),
            },
            Request::Search {
                account,
                chat,
                text,
                before,
                limit,
            } => Request::Search {
                account: account.clone(),
                chat: chat.clone(),
                text: text.clone(),
                before: search_cursor.or(*before),
                limit: remaining.unwrap_or(*limit).min(page_limit),
            },
            _ => request.clone(),
        };
        // Reassemble authorized byte fragments before printing any row from this page.
        let reply=zapfast::automation::send(runtime,&current).map_err(|_|"Could not reach opted-in local automation. A send may be Unconfirmed; query the same request id".to_owned())?;
        let value: serde_json::Value =
            serde_json::from_str(&reply).map_err(|_| "Invalid automation reply")?;
        if let Some(error) = value.get("error") {
            if error.as_str() == Some("Response too large; use a smaller page")
                && remaining.is_some()
                && page_limit > 1
            {
                page_limit = 1;
                continue;
            }
            return Err(error
                .as_str()
                .unwrap_or("Automation request refused")
                .to_owned());
        }
        // Collection and message output is one object per row, suitable for jq and pipes.
        let page = value.get("page").unwrap_or(&value);
        let rows = page
            .get("chats")
            .or_else(|| page.get("contacts"))
            .or_else(|| page.get("messages"))
            .and_then(|rows| rows.as_array());
        if let Some(rows) = rows {
            for row in rows {
                println!("{row}");
            }
        } else {
            println!("{value}");
        }
        if let Some(left) = remaining.as_mut() {
            *left = left.saturating_sub(rows.map_or(0, Vec::len));
            if *left == 0 {
                break;
            }
            if matches!(request, Request::Search { .. }) {
                let next = value.get("cursor").and_then(|cursor| cursor.as_i64());
                if next.is_none() {
                    break;
                }
                if search_cursor == next {
                    return Err("Archive cursor made no progress".into());
                }
                search_cursor = next;
            } else {
                let next = value.get("next_before").filter(|cursor| !cursor.is_null());
                let Some(next) = next else {
                    break;
                };
                let next =
                    serde_json::from_value(next.clone()).map_err(|_| "Invalid message cursor")?;
                if message_cursor.as_ref() == Some(&next) {
                    return Err("Archive cursor made no progress".into());
                }
                message_cursor = Some(next);
            }
            continue;
        }
        if matches!(request, Request::Chats { .. } | Request::Contacts { .. }) {
            let next = value.get("cursor").and_then(|cursor| cursor.as_str());
            if let Some(next) = next {
                if collection_cursor.as_deref() == Some(next) {
                    return Err("Archive cursor made no progress".into());
                }
                collection_cursor = Some(next.to_owned());
                continue;
            }
            break;
        }
        if !watching {
            break;
        }
        if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
            break;
        }
        let next = value.get("cursor").filter(|next| !next.is_null());
        let mut advanced = false;
        if let Some(next) = next {
            if matches!(request, Request::WatchAccount { .. }) {
                let next = next.as_i64().ok_or("Invalid watch cursor")?;
                advanced = row_cursor != Some(next);
                row_cursor = Some(next);
            } else {
                let next =
                    serde_json::from_value(next.clone()).map_err(|_| "Invalid watch cursor")?;
                advanced = cursor.as_ref() != Some(&next);
                cursor = Some(next);
            }
        }
        // Drain bounded pages before waiting; filtering private rows must still advance.
        if !advanced {
            std::thread::sleep(std::time::Duration::from_secs(u64::from(interval)));
        }
    }
    Ok(())
}

/// Default log filter, used when `RUST_LOG` is unset.
///
/// `fastframe_fonts` logs, once at startup, which installed face draws each
/// script Inter lacks, which is what a report of odd Arabic or CJK text
/// needs first.
///
/// `arboard` warns on every clipboard open when a Wayland compositor has no
/// data-control protocol (GNOME, mutter) and it falls back to X11, which works
/// there. Quiet that one target so it does not fill the log file, without
/// hiding real clipboard failures (`arboard=error`) or any other warning.
fn default_log_filter(verbose: bool) -> &'static str {
    if verbose {
        "info,zapfast=debug,whatsapp_rust=debug,wacore=debug"
    } else {
        // env_logger treats '/' as a message-regex separator, so it cannot
        // select the literal target Client/AppState here. Enable Client INFO;
        // the protocol redactor reduces every such line to a fixed category.
        "warn,zapfast=info,fastframe_fonts=info,Client=info,arboard=error"
    }
}

fn main() -> eframe::Result<()> {
    let result = run();
    // A Windows release has no console, so an error that ends the start
    // would otherwise leave the reader nothing to see (#349). `run` has
    // already released the single-instance lock.
    #[cfg(windows)]
    if let Err(error) = &result {
        startup_failure_dialog(error);
    }
    result
}

fn run() -> eframe::Result<()> {
    // First, before parsing the command line or touching any state: run the
    // update helper when asked (`--apply-update <job>`, then exit), and take
    // `--update-receipt` and `--update-error` off the command line.
    let launch = fastframe_update::intercept(&zapfast::updates::CONFIG);
    if launch
        .arguments
        .iter()
        .any(|argument| argument == "--camera-list-helper")
    {
        zapfast::camera::run_list_helper();
        return Ok(());
    }
    if launch
        .arguments
        .iter()
        .any(|argument| argument == "--camera-helper")
    {
        zapfast::camera::run_helper();
        return Ok(());
    }
    let cli = Cli::parse_from(&launch.arguments);
    let group_invite = cli
        .whatsapp_url
        .as_deref()
        .and_then(zapfast::chat_link::GroupInviteLink::parse);
    let chat_link = cli
        .whatsapp_url
        .as_deref()
        .filter(|_| group_invite.is_none())
        .map(zapfast::chat_link::ChatLink::parse)
        .transpose()
        .map_err(|error| {
            eframe::Error::AppCreation(
                std::io::Error::new(std::io::ErrorKind::InvalidInput, error).into(),
            )
        })?;
    let discovered = paths::AppDirs::discover();
    if let Some(Control::Automation { operation } | Control::Access(operation)) = &cli.command {
        if let Err(error) = automation_command(operation, &discovered.runtime) {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(command) = &cli.command {
        let (verb, not_running) = match command {
            Control::ReloadThemes => (
                "reload-themes",
                "Zap Faster is not running, so there are no themes to reload.",
            ),
            Control::Quit => ("quit", "Zap Faster is not running."),
            Control::Automation { .. } | Control::Access(_) => unreachable!("handled above"),
        };
        if let Err(error) = single_instance::send(&discovered.runtime, verb) {
            use std::io::ErrorKind;
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) {
                eprintln!("{not_running}");
                // Nothing left to quit is what was asked for.
                if matches!(command, Control::Quit) {
                    return Ok(());
                }
            } else {
                eprintln!("Could not reach the running Zap Faster: {error}");
            }
            std::process::exit(1);
        }
        return Ok(());
    }
    let waker = backend::Waker::default();
    #[cfg(feature = "demo")]
    let demo = cli.demo || cli.demo_shot.is_some() || cli.demo_tour;
    #[cfg(not(feature = "demo"))]
    let demo = false;
    // Keep one linked instance. Demo runs do not participate.
    let instance = if demo {
        None
    } else {
        // A hidden start must not surface a copy that is already running.
        let request = chat_link
            .as_ref()
            .map(zapfast::chat_link::ChatLink::request)
            .or_else(|| {
                group_invite
                    .as_ref()
                    .map(zapfast::chat_link::GroupInviteLink::request)
            });
        let verb = request
            .as_deref()
            .unwrap_or(if cli.start_hidden { "ping" } else { "show" });
        match single_instance::acquire(&discovered.runtime, &waker, verb) {
            single_instance::Outcome::Only(guard) => Some(guard),
            single_instance::Outcome::Surfaced if cli.start_hidden => {
                eprintln!("Zap Faster is already running");
                return Ok(());
            }
            single_instance::Outcome::Surfaced => {
                eprintln!("Zap Faster or FastsApp is already running; asked it to show its window");
                return Ok(());
            }
            single_instance::Outcome::Unanswered => {
                if chat_link.is_some() || group_invite.is_some() {
                    eprintln!(
                        "The running Zap Faster cannot open this link. Quit it and open the link again."
                    );
                } else {
                    eprintln!("Zap Faster is already running but did not answer");
                }
                return Ok(());
            }
        }
    };
    let default_filter = default_log_filter(cli.verbose);
    // A demo must not create empty Zap Faster directories that would prevent a
    // later real launch from adopting the existing FastsApp session.
    let dirs = if demo {
        paths::AppDirs::under(&std::env::temp_dir().join(format!(
            "zapfast-demo-{}-{}",
            std::process::id(),
            jiff::Timestamp::now().as_millisecond(),
        )))
    } else {
        discovered
    };
    if !demo {
        dirs.adopt_previous_names()
            .map_err(|error| eframe::Error::AppCreation(error.into()))?;
    }
    // Do not open logs, settings, or either database unless their parent
    // directories have been created and secured successfully.
    dirs.ensure()
        .map_err(|error| eframe::Error::AppCreation(error.into()))?;
    let logging = fastframe_log::Logging::new("zapfast", env!("CARGO_PKG_VERSION"))
        .filter(default_filter)
        .panic_log(dirs.panic_log())
        .redact(redact_protocol);
    // Write desktop-session logs to disk. Demo runs use stderr so they do not
    // replace a live session's log.
    let logging = if demo {
        logging
    } else {
        logging.file(dirs.log_file())
    };
    logging
        .init()
        .map_err(|error| eframe::Error::AppCreation(error.into()))?;
    // Moving a single-account setup into its account folder needs the
    // keyring; when it cannot finish, the log says why.
    if !demo && let Err(error) = dirs.adopt_single_account() {
        log::error!("could not move the linked account into its folder: {error}");
        return Err(eframe::Error::AppCreation(error.into()));
    }
    let settings = settings::Settings::load(&dirs.settings_file());
    // Configure fastframe-emoji before any renderer wrapper can initialize it.
    zapfast::emoji::setup_with_font(
        settings.emoji_renderer,
        settings.custom_emoji_font.as_deref(),
    );
    let demo_persistence = demo.then(|| dirs.state.join("window.ron"));

    #[allow(unused_mut)]
    let mut app = if demo {
        app::App::headless(dirs, settings).0
    } else {
        app::App::new(&waker, dirs, settings, app::AppOptions { tray: true })
            .map_err(|error| eframe::Error::AppCreation(error.into()))?
    };
    if cli.verbose {
        app.update_arguments.push("--verbose".into());
    }
    if let Some(error) = launch.error {
        app.toast_error(error);
    }
    if let Some(guard) = &instance {
        app.set_remote_control(guard);
    }
    if let Some(link) = chat_link {
        app.open_chat_link(link);
    }
    if let Some(link) = group_invite {
        app.open_group_invite(link);
    }
    #[cfg(feature = "demo")]
    if demo {
        zapfast::demo::populate(&mut app);
        zapfast::demo::apply_flags(&mut app, cli.demo_page.as_deref());
        if cli.demo_tour {
            tour_script(&cli.demo_tour_script).prepare(&mut app);
        }
    }
    #[cfg(feature = "demo")]
    let shot = cli.demo_shot.clone().map(|path| Shot {
        path,
        due: std::time::Instant::now() + std::time::Duration::from_millis(cli.demo_shot_delay),
        asked: false,
    });
    #[cfg(feature = "demo")]
    let demo_hover = cli.demo_hover.as_deref().and_then(|value| {
        let (x, y) = value.split_once(',')?;
        Some(egui::pos2(x.trim().parse().ok()?, y.trim().parse().ok()?))
    });
    #[cfg(feature = "demo")]
    let demo_phone_menu = cli
        .demo_page
        .as_deref()
        .is_some_and(|page| page.split(',').any(|part| part.trim() == "phone-menu"));
    #[cfg(feature = "demo")]
    let demo_call_devices = cli
        .demo_page
        .as_deref()
        .is_some_and(|page| page.split(',').any(|part| part.trim() == "call-devices"));
    let mut update_receipt = launch.receipt;
    // The link, archive, and tray outlive windows. The shell recreates a
    // window when the tray, a notification, or another launch requests one;
    // without a tray a hidden start shows the window (App::start_hidden).
    let start_hidden = cli.start_hidden && !demo && update_receipt.is_none();
    fastframe_shell::Shell::new(app, &waker)
        .start_hidden(start_hidden)
        .idle(fastframe_tray::idle)
        .run(|lease| {
            let receipt = update_receipt.take();
            // The latest geometry lives in the app waiting in the shell, so
            // a reopened window (tray, notification, Wayland reopen) uses
            // what the last one remembered, not what the process started
            // with. Demo runs keep their fixed screenshot size.
            // Decided as each window is made: it cannot become transparent
            // later.
            let transparent = lease.peek(|app: &app::App| {
                zapfast::backdrop::transparent_window(
                    app.settings.theme == zapfast::settings::ThemeChoice::Messages,
                    app.palette.window.a() < 255,
                )
            });
            zapfast::backdrop::set_window_transparent(transparent);
            let geometry = if demo_persistence.is_some() {
                fastframe_shell::window::Geometry::default()
            } else {
                lease.peek(|app: &app::App| app.settings.window_geometry())
            };
            #[cfg(feature = "demo")]
            let shot = shot.clone();
            #[cfg(feature = "demo")]
            let tour = cli.demo_tour.then(|| {
                zapfast::demo::tour::Tour::scripted(
                    tour_script(&cli.demo_tour_script),
                    cli.demo_tour_delay.map(std::time::Duration::from_millis),
                    cli.demo_tour_events.clone(),
                    cli.demo_tour_frames
                        .clone()
                        .map(|dir| zapfast::demo::tour::Capture {
                            dir,
                            fps: cli.demo_fps.unwrap_or(30),
                        }),
                )
            });
            let options = native_options(demo_persistence.clone(), geometry, transparent);
            #[cfg(feature = "demo")]
            let options = if shot.is_some() {
                // Automated synthetic captures must not steal focus from the user's window.
                eframe::NativeOptions {
                    viewport: options.viewport.with_active(false),
                    ..options
                }
            } else {
                options
            };
            #[cfg(feature = "demo")]
            let demo_minimized = cli
                .demo_page
                .as_deref()
                .is_some_and(|page| page.split(',').any(|part| part == "call-minimized"));
            eframe::run_native(
                "Zap Faster",
                options,
                Box::new(move |cc| {
                    let mut app = lease.take(&cc.egui_ctx);
                    app.attach(&cc.egui_ctx);
                    #[cfg(feature = "demo")]
                    if demo_minimized && let Some(call) = &app.call {
                        cc.egui_ctx.data_mut(|data| {
                            data.insert_temp(
                                zapfast::ui::call::minimized_id(
                                    app.account().id.as_str(),
                                    call.generation,
                                ),
                                true,
                            )
                        });
                    }
                    #[cfg(feature = "demo")]
                    if cli.demo_macos {
                        zapfast::theme::preview_macos(&cc.egui_ctx);
                    }
                    Ok(Box::new(Shell {
                        app,
                        window_recovery_checked: false,
                        transparent,
                        update_receipt: receipt,
                        #[cfg(target_os = "windows")]
                        taskbar: Default::default(),
                        #[cfg(feature = "demo")]
                        shot,
                        #[cfg(feature = "demo")]
                        hover: demo_hover,
                        #[cfg(feature = "demo")]
                        open_phone_menu: demo_phone_menu,
                        #[cfg(feature = "demo")]
                        open_call_devices: demo_call_devices,
                        #[cfg(feature = "demo")]
                        tour,
                    }))
                }),
            )
        })?;
    drop(instance);
    Ok(())
}

/// Whether eframe stopped because the graphics driver gave it no usable
/// OpenGL: no pixel format, no context of any version it asks for, or a
/// context the renderer cannot use.
#[cfg(windows)]
fn is_graphics_failure(error: &eframe::Error) -> bool {
    matches!(
        error,
        eframe::Error::Glutin(_) | eframe::Error::NoGlutinConfigs(..) | eframe::Error::OpenGL(_)
    )
}

/// The text of the dialog shown when Zap Faster cannot start, kept apart from
/// the dialog so it is tested on every platform.
#[cfg(any(windows, test))]
fn startup_failure_text(graphics: bool, details: &str, log: &std::path::Path) -> String {
    let summary = if graphics {
        "Zap Faster could not start because the graphics driver does not offer \
         OpenGL 2.1 or newer, which Zap Faster needs to draw its window.\n\n\
         Install the current driver from the maker of the graphics chip \
         (Intel, AMD or NVIDIA). The Microsoft Basic Display Adapter, some \
         virtual machines and some remote desktop sessions offer no usable \
         OpenGL."
    } else {
        "Zap Faster could not start."
    };
    format!(
        "{summary}\n\nDetails: {details}\n\nThe log may say more: {}",
        log.display()
    )
}

/// Explains a failed start in a message box, the only thing a release
/// without a console can show before its window exists.
#[cfg(windows)]
fn startup_failure_dialog(error: &eframe::Error) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };
    use windows::core::PCWSTR;

    let text = startup_failure_text(
        is_graphics_failure(error),
        &error.to_string(),
        &paths::AppDirs::discover().log_file(),
    );
    let wide = |text: &str| text.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (text, caption) = (wide(&text), wide("Zap Faster"));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

/// Summarises the WhatsApp library's lines, which can quote protocol
/// payloads, into fixed categories.
fn redact_protocol(
    record: &log::Record<'_>,
    message: &str,
) -> Option<std::borrow::Cow<'static, str>> {
    use zapfast::diagnostics::{is_protocol_target, protocol_summary};
    (is_protocol_target(record.target())
        || is_protocol_target(record.module_path().unwrap_or_default()))
    .then(|| protocol_summary(message))
}

/// Parses `--demo-size WxH`.
fn demo_size_arg() -> Option<[f32; 2]> {
    let value = std::env::args()
        .skip_while(|arg| arg != "--demo-size")
        .nth(1)?;
    let (w, h) = value.split_once('x')?;
    Some([w.parse::<f32>().ok()?, h.parse::<f32>().ok()?])
}

/// The tour `--demo-tour-script` names; clap has already checked the name.
#[cfg(feature = "demo")]
fn tour_script(name: &str) -> zapfast::demo::tour::Script {
    zapfast::demo::tour::Script::from_name(name).unwrap_or_default()
}

fn native_options(
    demo_persistence: Option<std::path::PathBuf>,
    geometry: fastframe_shell::window::Geometry,
    transparent: bool,
) -> eframe::NativeOptions {
    let default_size = demo_size_arg().unwrap_or([1180.0, 780.0]);
    let demo = demo_persistence.is_some();
    let viewport = egui::ViewportBuilder::default()
        .with_title(if demo {
            "Zap Faster Demo"
        } else {
            "Zap Faster"
        })
        .with_app_id(if demo {
            "zap-faster-demo".to_owned()
        } else {
            fastframe_desktop::desktop_entry("zap-faster")
        });
    // The remembered size and position, from the settings, over the default
    // size. Demo runs keep their fixed screenshot size.
    let viewport = viewport.with_inner_size(default_size);
    let viewport = if demo {
        viewport
    } else {
        geometry.apply(viewport)
    };
    let viewport = viewport
        // Keep the floor small enough that Windows can still snap the window
        // into narrow Aero Snap and LG Screen Split zones (a 2560 px ultrawide
        // split four ways is about 640 px wide, which a 720 px minimum blocks).
        .with_min_inner_size([400.0, 300.0])
        .with_icon(app_icon())
        // macOS uses a full-size content view under the traffic lights.
        .with_fullsize_content_view(true)
        // Transparent where the Messages theme can show the desktop through
        // its sidebar; every other theme paints the window opaque.
        .with_transparent(transparent)
        .with_titlebar_shown(false)
        .with_title_shown(false);
    eframe::NativeOptions {
        viewport,
        persistence_path: demo_persistence.or_else(|| eframe::storage_dir("zapfast")),
        // The settings remember the window (fastframe_shell::window::Geometry),
        // in logical pixels. eframe's own copy is in points at the zoom of the
        // time, and opened smaller at every launch with the interface zoomed,
        // since its zoom comes back only once the window exists.
        persist_window: false,
        ..Default::default()
    }
}

/// eframe adapter holding the long-lived [`app::App`] for one window; it goes
/// back to the shell when the window closes.
struct Shell {
    /// Whether this window's first frame checked that a monitor shows it.
    window_recovery_checked: bool,
    /// Whether this window was made transparent, for the Messages theme.
    transparent: bool,
    update_receipt: Option<fastframe_update::Receipt>,
    app: fastframe_shell::Held<app::App>,
    /// This window's unread overlay on its taskbar button.
    #[cfg(target_os = "windows")]
    taskbar: zapfast::notify::Taskbar,
    #[cfg(feature = "demo")]
    shot: Option<Shot>,
    #[cfg(feature = "demo")]
    tour: Option<zapfast::demo::tour::Tour>,
    #[cfg(feature = "demo")]
    hover: Option<egui::Pos2>,
    #[cfg(feature = "demo")]
    open_phone_menu: bool,
    #[cfg(feature = "demo")]
    open_call_devices: bool,
}

/// Pending screenshot request.
#[cfg(feature = "demo")]
#[derive(Clone)]
struct Shot {
    path: std::path::PathBuf,
    due: std::time::Instant,
    asked: bool,
}

#[cfg(feature = "demo")]
impl Shell {
    fn drive_shot(&mut self, ctx: &egui::Context) {
        let Some(shot) = self.shot.as_mut() else {
            return;
        };
        ctx.request_repaint();
        if !shot.asked && std::time::Instant::now() >= shot.due {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            shot.asked = true;
        }
        let image = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        let Some(image) = image else {
            return;
        };
        let [width, height] = [image.size[0] as u32, image.size[1] as u32];
        let pixels: Vec<u8> = image
            .pixels
            .iter()
            .flat_map(|pixel| pixel.to_srgba_unmultiplied())
            .collect();
        match image::RgbaImage::from_raw(width, height, pixels) {
            Some(buffer) => match buffer.save(&shot.path) {
                Ok(()) => log::info!("wrote {}x{} to {}", width, height, shot.path.display()),
                Err(error) => log::error!("could not write {}: {error}", shot.path.display()),
            },
            None => log::error!("the frame buffer did not match {width}x{height}"),
        }
        self.shot = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for Shell {
    /// What shows where the interface paints nothing: the window colour, or
    /// nothing while the window has a material, so it shows.
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        if zapfast::backdrop::active() {
            [0.0; 4]
        } else {
            zapfast::backdrop::clear_color(self.app.palette.window).to_normalized_gamma_f32()
        }
    }

    #[cfg(feature = "demo")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(tour) = &mut self.tour {
            tour.input(&mut self.app, ctx, input);
        }
        // Moves the pointer only while it is elsewhere: each move restarts
        // egui's tooltip delay, so a fake pointer that kept moving in place
        // would never show one.
        if let Some(pos) = self.hover
            && ctx.input(|input| input.pointer.latest_pos()) != Some(pos)
        {
            input.events.push(egui::Event::PointerMoved(pos));
        }
    }

    /// Does not persist egui interaction state across windows. The zoom and
    /// the window's geometry live in the settings: egui's memory would bring
    /// back a zoom that scales the remembered geometry a second time.
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        if !std::mem::replace(&mut self.window_recovery_checked, true) {
            fastframe_shell::window::recover_offscreen(ctx, frame);
        }
        let app = &mut *self.app;
        #[cfg(feature = "demo")]
        if let Some(tour) = self.tour.as_mut() {
            tour.drive(app, ctx);
        }
        app.background_frame(ctx);
        app.window_logic(ctx);
        #[cfg(target_os = "windows")]
        if let (Some(window), Some(count)) = (frame.winit_window(), app.taskbar_badge_count())
            && let Some(at) = self.taskbar.show(window, count, app.locale)
        {
            ctx.request_repaint_after(at.saturating_duration_since(std::time::Instant::now()));
        }
        // The chat header is 60 points and zooms; the linking screen keeps
        // AppKit's own 28-point strip.
        let title_bar = if app.is_linked() && !app.app_lock.is_locked() {
            zapfast::theme::TOP_BAR_HEIGHT
        } else {
            28.0 / ctx.zoom_factor()
        };
        fastframe_macos::align_traffic_lights(frame, ctx, title_bar);
        zapfast::backdrop::apply(frame, &app.palette, self.transparent);
        #[cfg(feature = "demo")]
        {
            // Keep requesting the configured screenshot size until it is applied.
            if self.shot.is_some()
                && let Some([w, h]) = demo_size_arg()
            {
                let now = ctx.input(|input| input.raw.screen_rect.map(|rect| rect.size()));
                if now.is_none_or(|now| (now.x - w).abs() > 1.0 || (now.y - h).abs() > 1.0) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(w, h)));
                }
            }
            self.drive_shot(ctx);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let app = &mut *self.app;
        #[cfg(feature = "demo")]
        if std::mem::take(&mut self.open_phone_menu) {
            egui::Popup::open_id(ui.ctx(), zapfast::demo::phone_menu_popup_id());
        }
        #[cfg(feature = "demo")]
        if std::mem::take(&mut self.open_call_devices) {
            zapfast::demo::open_call_devices(app, ui.ctx());
        }
        app.frame_ui(ui);
        let startups: Vec<_> = app
            .accounts
            .iter_mut()
            .filter_map(|account| account.backend.take_startup())
            .collect();
        if let Some(receipt) = self.update_receipt.take() {
            std::thread::spawn(move || {
                if let Err(error) = receipt.acknowledge() {
                    log::warn!("could not acknowledge the update: {error:#}");
                    return;
                }
                for startup in startups {
                    let _ = startup.send(());
                }
            });
        } else {
            for startup in startups {
                let _ = startup.send(());
            }
        }
        #[cfg(feature = "demo")]
        match self.tour.as_mut() {
            Some(tour) => tour.observe(app, ui.ctx()),
            // A demo without a tour still answers the interface's commands
            // itself, since there is no backend to answer them.
            None => zapfast::demo::tour::respond(app),
        }
    }

    /// Saves essential state before the window closes.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.app.save_state();
    }
}

fn app_icon() -> egui::IconData {
    #[cfg(target_os = "macos")]
    {
        // eframe replaces the bundle's Dock icon with this viewport icon.
        let image = image::load_from_memory(include_bytes!("../packaging/macos/icon-1024.png"))
            .expect("bundled macOS icon")
            .into_rgba8();
        egui::IconData {
            width: image.width(),
            height: image.height(),
            rgba: image.into_raw(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        const SIZE: usize = 128;
        egui::IconData {
            rgba: zapfast::util::app_icon_rgba(SIZE),
            width: SIZE as u32,
            height: SIZE as u32,
        }
    }
}

#[cfg(test)]
mod chat_link_cli_tests {
    use super::*;
    #[test]
    fn original_cli_surface_accepts_collections_stdin_files_and_unbounded_watch() {
        for args in [
            vec!["chats"],
            vec!["contacts", "--all"],
            vec!["messages", "fixture", "--limit", "100"],
            vec!["search", "synthetic phrase"],
            vec!["watch", "--since", "100", "--interval", "2"],
        ] {
            let mut command = vec!["zap-faster"];
            command.extend(args);
            command.extend(["--account", "2"]);
            assert!(matches!(
                Cli::try_parse_from(command).unwrap().command,
                Some(Control::Access(_))
            ));
        }
        let cli = Cli::try_parse_from([
            "zap-faster",
            "send",
            "fixture",
            "-",
            "--account",
            "2",
            "--request-id",
            "0123456789abcdef0123456789abcdef",
            "--file",
            "synthetic-first.pdf",
            "--file",
            "synthetic-second.png",
        ])
        .unwrap();
        assert!(
            matches!(cli.command,Some(Control::Access(Automation::Send { text:Some(text), files,.. })) if text=="-" && files.len()==2)
        );
    }

    #[test]
    fn automation_cli_is_explicit_and_send_requires_a_file_and_request_id() {
        let cli = Cli::try_parse_from([
            "zap-faster",
            "automation",
            "read",
            "--account",
            "2",
            "--chat",
            "fixture",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Control::Automation {
                operation: Automation::Read { .. }
            })
        ));
        assert!(
            Cli::try_parse_from([
                "zap-faster",
                "automation",
                "send",
                "--account",
                "2",
                "--chat",
                "fixture"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "zap-faster",
                "automation",
                "read",
                "--account",
                "2",
                "--chat",
                "fixture",
                "--before-time",
                "100"
            ])
            .is_err()
        );
    }

    #[test]
    fn browser_urls_are_positional_and_control_commands_still_work() {
        let uri = "whatsapp://send?phone=15550100123&text=Hello";
        let cli = Cli::try_parse_from(["zapfast", uri]).unwrap();
        assert_eq!(cli.whatsapp_url.as_deref(), Some(uri));
        assert!(cli.command.is_none());
        let cli = Cli::try_parse_from(["zapfast", "reload-themes"]).unwrap();
        assert!(matches!(cli.command, Some(Control::ReloadThemes)));
        assert!(cli.whatsapp_url.is_none());
        assert!(Cli::try_parse_from(["zapfast", "--start-hidden", uri]).is_err());
        for target in [
            "15550100123",
            "+1 (555) 010-0123",
            "https://wa.me/15550100123",
        ] {
            let cli = Cli::try_parse_from(["zap-faster", target]).unwrap();
            assert!(cli.command.is_none());
            assert!(
                zapfast::chat_link::ChatLink::parse(cli.whatsapp_url.as_deref().unwrap()).is_ok()
            );
        }
    }
}

#[cfg(all(test, feature = "demo"))]
mod tests {
    use super::*;

    #[test]
    fn tour_cli_accepts_manual_and_delayed_starts() {
        let cli = Cli::try_parse_from(["zapfast", "--demo-tour"]).unwrap();
        assert!(cli.demo_tour);
        assert!(cli.demo_tour_delay.is_none());
        let cli =
            Cli::try_parse_from(["zapfast", "--demo-tour", "--demo-tour-delay", "5000"]).unwrap();
        assert_eq!(cli.demo_tour_delay, Some(5000));
        assert!(Cli::try_parse_from(["zapfast", "--demo-tour-delay", "5000"]).is_err());
        assert!(Cli::try_parse_from(["zapfast", "--demo-tour", "--demo-page", "login",]).is_err());
    }

    #[test]
    fn tour_cli_picks_a_script_and_a_frame_capture() {
        let cli = Cli::try_parse_from(["zapfast", "--demo-tour"]).unwrap();
        assert_eq!(
            tour_script(&cli.demo_tour_script),
            zapfast::demo::tour::Script::Launch
        );
        let cli = Cli::try_parse_from([
            "zapfast",
            "--demo-tour",
            "--demo-tour-script",
            "whats-new",
            "--demo-tour-frames",
            "frames",
            "--demo-fps",
            "60",
        ])
        .unwrap();
        assert_eq!(
            tour_script(&cli.demo_tour_script),
            zapfast::demo::tour::Script::WhatsNew
        );
        assert_eq!(
            cli.demo_tour_frames.as_deref(),
            Some(std::path::Path::new("frames"))
        );
        assert_eq!(cli.demo_fps, Some(60));
        assert!(
            Cli::try_parse_from(["zapfast", "--demo-tour", "--demo-tour-script", "other"]).is_err()
        );
        assert!(Cli::try_parse_from(["zapfast", "--demo-tour-script", "whats-new"]).is_err());
        assert!(Cli::try_parse_from(["zapfast", "--demo-tour", "--demo-fps", "30"]).is_err());
        assert!(
            Cli::try_parse_from([
                "zapfast",
                "--demo-tour",
                "--demo-tour-frames",
                "frames",
                "--demo-tour-delay",
                "5000",
            ])
            .is_err()
        );
    }
}

#[cfg(test)]
mod startup_failure_tests {
    use super::*;

    #[test]
    fn a_graphics_failure_names_the_driver_requirement_and_the_details() {
        let log = std::path::Path::new("C:/zapfast/zapfast.log");
        let text = startup_failure_text(
            true,
            "glutin error: extension to create ES context with wgl is not present",
            log,
        );
        assert!(text.contains("OpenGL 2.1 or newer"));
        assert!(text.contains("graphics driver"));
        assert!(text.contains("ES context with wgl is not present"));
        assert!(text.contains(&log.display().to_string()));
    }

    #[test]
    fn any_other_failure_shows_its_details_without_blaming_the_driver() {
        let text = startup_failure_text(
            false,
            "The directory is not writable",
            std::path::Path::new("zapfast.log"),
        );
        assert!(text.starts_with("Zap Faster could not start."));
        assert!(!text.contains("OpenGL"));
        assert!(text.contains("The directory is not writable"));
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::*;

    fn matches(filter: &str, level: log::Level, target: &str) -> bool {
        let logger = env_logger::Builder::new().parse_filters(filter).build();
        logger.matches(
            &log::Record::builder()
                .level(level)
                .target(target)
                .args(format_args!("fixture"))
                .build(),
        )
    }

    /// A compositor without data-control makes arboard fall back to X11 and
    /// warn. That is expected, so the default log must not record it, while a
    /// genuine arboard failure still must.
    #[test]
    fn the_default_log_drops_arboards_wayland_fallback_warning() {
        let filter = default_log_filter(false);
        assert!(!matches(
            filter,
            log::Level::Warn,
            "arboard::platform::linux"
        ));
        assert!(matches(
            filter,
            log::Level::Error,
            "arboard::platform::linux"
        ));
        assert!(matches(
            filter,
            log::Level::Warn,
            "zapfast::backend::worker"
        ));
    }

    /// Every log names the face chosen for each fallback script, without
    /// asking a reporter to start with `--verbose`.
    #[test]
    fn the_default_log_records_the_fallback_fonts() {
        assert!(matches(
            default_log_filter(false),
            log::Level::Info,
            "fastframe_fonts::system"
        ));
        assert!(!matches(
            default_log_filter(false),
            log::Level::Debug,
            "fastframe_fonts::system"
        ));
    }

    #[test]
    fn default_log_records_protocol_recovery_lifecycle_and_redacts_other_client_info() {
        let filter = default_log_filter(false);
        assert!(matches(filter, log::Level::Info, "Client/AppState"));
        assert!(!matches(filter, log::Level::Info, "AppState"));
        // env_logger cannot select a literal target containing '/'. The
        // broader Client INFO target must remain inside the redaction boundary.
        assert!(matches(filter, log::Level::Info, "Client/Message"));
        let record = log::Record::builder()
            .level(log::Level::Info)
            .target("Client/Message")
            .module_path(Some("whatsapp_rust::client::message"))
            .args(format_args!("private"))
            .build();
        assert_eq!(
            redact_protocol(&record, "private 12345@s.whatsapp.net"),
            Some(std::borrow::Cow::Borrowed(
                "protocol diagnostic (private details omitted)"
            ))
        );
    }

    #[test]
    fn app_state_info_is_redacted_even_with_a_custom_log_target() {
        let record = log::Record::builder()
            .args(format_args!("private 12345@s.whatsapp.net"))
            .level(log::Level::Info)
            .target("Client/AppState")
            .module_path(Some("whatsapp_rust::client::app_state"))
            .build();
        assert_eq!(
            redact_protocol(
                &record,
                "Recovered the regular_low collection from the primary device: 12345@s.whatsapp.net"
            ),
            Some(std::borrow::Cow::Borrowed(
                "app-state regular_low recovery reply applied by library"
            ))
        );
    }

    #[test]
    fn verbose_keeps_arboard_warnings() {
        assert!(matches(
            default_log_filter(true),
            log::Level::Warn,
            "arboard::platform::linux"
        ));
    }
}
