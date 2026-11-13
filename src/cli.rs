//! Local automation and control commands, shared by the desktop app and the console companion.

use crate::{automation::ChatOrder, model::ChatKind, paths, single_instance};

#[derive(Debug, clap::Subcommand)]
pub enum Control {
    #[command(flatten)]
    Access(Automation),
    /// Private read/watch/send access to an explicitly opted-in running account.
    Automation {
        #[command(subcommand)]
        operation: Automation,
    },
    /// Reload palettes in an already-running Zap Fasterk without showing its window.
    ReloadThemes,
    /// Quit an already-running Zap Fasterk, which closing its window leaves in the tray.
    Quit,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Automation {
    /// List direct chats, most recent first. Groups and channels have their
    /// own commands; --kind all restores the combined list.
    Chats {
        #[command(flatten)]
        list: ChatList,
        /// Which conversations to list; direct chats by default.
        #[arg(long, value_enum, default_value = "direct")]
        kind: ChatKindFilter,
    },
    /// List group chats, most recent first.
    Groups {
        #[command(flatten)]
        list: ChatList,
    },
    /// List followed channels, most recent first.
    Channels {
        #[command(flatten)]
        list: ChatList,
    },
    /// List address-book contacts; --all includes unsaved contacts.
    Contacts {
        #[arg(long)]
        account: Option<String>,
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
        account: Option<String>,
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long)]
        before: Option<i64>,
    },
    /// Search archived message text and captions.
    Search {
        text: String,
        #[arg(long)]
        account: Option<String>,
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
        account: Option<String>,
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
        account: Option<String>,
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
        /// Chat id, phone number, or conversation name; numbers and names
        /// must single out one conversation.
        target: Option<String>,
        text: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, conflicts_with = "target")]
        chat: Option<String>,
        #[arg(long)]
        request_id: Option<String>,
        #[arg(long, conflicts_with = "text")]
        text_file: Option<std::path::PathBuf>,
        #[arg(long = "file")]
        files: Vec<std::path::PathBuf>,
    },
    /// Check a previous send without retrying it.
    Status {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        chat: String,
        #[arg(long)]
        request_id: String,
    },
}

impl Automation {
    fn account_mut(&mut self) -> &mut Option<String> {
        match self {
            Automation::Chats { list, .. }
            | Automation::Groups { list }
            | Automation::Channels { list } => &mut list.account,
            Automation::Contacts { account, .. }
            | Automation::Messages { account, .. }
            | Automation::Search { account, .. }
            | Automation::Read { account, .. }
            | Automation::Watch { account, .. }
            | Automation::Send { account, .. }
            | Automation::Status { account, .. } => account,
        }
    }
}

/// Options shared by the `chats`, `groups` and `channels` listings.
#[derive(Debug, Clone, clap::Args)]
pub struct ChatList {
    #[arg(long)]
    account: Option<String>,
    #[arg(long, default_value_t = 25)]
    limit: usize,
    /// Chat id after which to continue; its position resolves in the chosen sort.
    #[arg(long)]
    after: Option<String>,
    #[arg(long, value_enum, default_value = "activity")]
    sort: ChatOrder,
    /// Only chats with unread messages or a marked-unread dot.
    #[arg(long)]
    unread: bool,
    /// Include archived chats, hidden by default like in the app's list.
    #[arg(long)]
    archived: bool,
}

/// `--kind` for `chats`: direct chats by default, `all` for everything.
#[derive(Clone, Copy, Debug, Default, clap::ValueEnum)]
pub enum ChatKindFilter {
    #[default]
    Direct,
    Group,
    Broadcast,
    All,
}

impl ChatKindFilter {
    fn request_kind(self) -> Option<ChatKind> {
        match self {
            Self::Direct => Some(ChatKind::Direct),
            Self::Group => Some(ChatKind::Group),
            Self::Broadcast => Some(ChatKind::Broadcast),
            Self::All => None,
        }
    }
}

/// Which account an automation call runs as: the explicit `--account`, or
/// the only linked account when the flag is missing. Refuses rather than
/// guessing between several accounts or inventing one.
fn resolve_account(state: &std::path::Path, account: Option<&str>) -> Result<String, String> {
    if let Some(id) = account.filter(|id| !id.is_empty()) {
        return Ok(id.to_owned());
    }
    let mut ids = Vec::new();
    if let Ok(dir) = std::fs::read_dir(state.join("accounts")) {
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with('.')
                && entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
            {
                ids.push(name.into_owned());
            }
        }
    }
    ids.sort();
    match ids.as_slice() {
        [] => Err("no linked account found; link one first".to_owned()),
        [only] => Ok(only.clone()),
        _ => Err(format!(
            "several accounts exist; pass one with --account: {}",
            ids.join(", ")
        )),
    }
}

/// How a `send` target that is not a full chat id is looked up.
enum TargetQuery {
    /// Bare phone digits: match the address ending with them.
    Phone(String),
    /// Anything else: case-insensitive conversation-name substring.
    Name(String),
}

impl TargetQuery {
    fn classify(raw: &str) -> Self {
        let trimmed = raw.trim();
        let digits: String = trimmed
            .strip_prefix('+')
            .unwrap_or(trimmed)
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if !digits.is_empty()
            && trimmed
                .chars()
                .all(|c| c.is_ascii_digit() || c == '+' || c == ' ' || c == '-')
        {
            Self::Phone(digits)
        } else {
            Self::Name(trimmed.to_lowercase())
        }
    }

    /// Whether this conversation singles out the target. Ids are unique, so
    /// an exact digit match decides on the spot; anything broader collects a
    /// candidate for the caller to disambiguate.
    fn matches(&self, id: &str, name: &str) -> TargetMatch {
        match self {
            Self::Phone(digits) => {
                let local = id.split('@').next().unwrap_or_default();
                if local == digits {
                    TargetMatch::Exact
                } else if local.ends_with(digits.as_str()) {
                    TargetMatch::Candidate
                } else {
                    TargetMatch::No
                }
            }
            Self::Name(want) => {
                if !name.is_empty() && name.to_lowercase().contains(want.as_str()) {
                    TargetMatch::Candidate
                } else {
                    TargetMatch::No
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetMatch {
    No,
    Candidate,
    Exact,
}

/// Mints a fresh 32-digit hexadecimal send id, as `status` expects it.
fn new_request_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| "Could not generate a request id")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Resolves a `send` target that is not a full chat id against every known
/// conversation: a bare number matches the address ending with those digits,
/// any other text matches by conversation name. Both must single out exactly
/// one conversation; the error names the candidates. Locked conversations
/// never list, and sending to one is refused anyway.
fn resolve_chat_target(
    runtime: &std::path::Path,
    account: &str,
    raw: &str,
) -> Result<String, String> {
    use crate::automation::Request;
    let query = TargetQuery::classify(raw);
    let mut after: Option<String> = None;
    let mut candidates: Vec<(String, String)> = Vec::new();
    loop {
        let page = Request::Chats {
            account: account.to_owned(),
            after: after.clone(),
            limit: 25,
            kind: None,
            order: crate::automation::ChatOrder::Activity,
            unread_only: false,
            include_archived: true,
        };
        let reply = crate::automation::send(runtime, &page)
            .map_err(|_| "Could not reach opted-in local automation".to_owned())?;
        let value: serde_json::Value =
            serde_json::from_str(&reply).map_err(|_| "Invalid automation reply")?;
        if let Some(error) = value.get("error").and_then(|error| error.as_str()) {
            return Err(error.to_owned());
        }
        let rows = value
            .get("chats")
            .and_then(|rows| rows.as_array())
            .cloned()
            .unwrap_or_default();
        for row in &rows {
            let Some(id) = row.get("id").and_then(|id| id.as_str()) else {
                continue;
            };
            let name = row.get("name").and_then(|name| name.as_str()).unwrap_or("");
            match query.matches(id, name) {
                TargetMatch::No => {}
                TargetMatch::Exact => return Ok(id.to_owned()),
                TargetMatch::Candidate => candidates.push((name.to_owned(), id.to_owned())),
            }
        }
        let next = value
            .get("cursor")
            .and_then(|cursor| cursor.as_str())
            .map(str::to_owned);
        if next.as_deref() == after.as_deref() {
            return Err("Archive cursor made no progress".into());
        }
        let Some(next) = next else { break };
        after = Some(next);
    }
    match candidates.as_slice() {
        [] => Err(format!(
            "no conversation matches \"{raw}\"; list them with: chats --kind all"
        )),
        [(name, id)] => {
            let _ = name;
            Ok(id.clone())
        }
        _ => {
            let mut message = format!("\"{raw}\" matches {} conversations; repeat with the full id:", candidates.len());
            for (name, id) in candidates.iter().take(10) {
                message.push_str(&format!("\n{name} <{id}>"));
            }
            if candidates.len() > 10 {
                message.push_str(&format!("\nand {} more", candidates.len() - 10));
            }
            Err(message)
        }
    }
}

pub fn automation_command(
    operation: &Automation,
    runtime: &std::path::Path,
    state: &std::path::Path,
) -> Result<(), String> {
    use crate::automation::{Attachment, Request};
    use std::io::Read;
    let mut operation = operation.clone();
    let resolved = resolve_account(state, operation.account_mut().as_deref())?;
    *operation.account_mut() = Some(resolved.clone());
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
    let request = match &operation {
        Automation::Chats { list, kind } => {
            remaining = Some(list.limit);
            Request::Chats {
                account: resolved.clone(),
                after: list.after.clone(),
                limit: list.limit.min(25),
                kind: kind.request_kind(),
                order: list.sort,
                unread_only: list.unread,
                include_archived: list.archived,
            }
        }
        Automation::Groups { list } => {
            remaining = Some(list.limit);
            Request::Chats {
                account: resolved.clone(),
                after: list.after.clone(),
                limit: list.limit.min(25),
                kind: Some(ChatKind::Group),
                order: list.sort,
                unread_only: list.unread,
                include_archived: list.archived,
            }
        }
        Automation::Channels { list } => {
            remaining = Some(list.limit);
            Request::Chats {
                account: resolved.clone(),
                after: list.after.clone(),
                limit: list.limit.min(25),
                kind: Some(ChatKind::Broadcast),
                order: list.sort,
                unread_only: list.unread,
                include_archived: list.archived,
            }
        }
        Automation::Contacts {
            account: _,
            all,
            after,
            limit,
        } => {
            remaining = Some(*limit);
            Request::Contacts {
                account: resolved.clone(),
                all: *all,
                after: after.clone(),
                limit: (*limit).min(25),
            }
        }
        Automation::Messages {
            account: _,
            chat,
            before,
            limit,
        } => {
            remaining = Some(*limit);
            Request::Read {
                account: resolved.clone(),
                chat: chat.clone(),
                before: before.map(|time| (time, String::new())),
                limit: (*limit).min(25),
            }
        }
        Automation::Search {
            account: _,
            chat,
            text,
            before_row,
            limit,
        } => {
            remaining = Some(*limit);
            Request::Search {
                account: resolved.clone(),
                chat: chat.clone().unwrap_or_default(),
                text: text.clone(),
                before: *before_row,
                limit: (*limit).min(25),
            }
        }
        Automation::Read {
            account: _,
            chat,
            limit,
            before_time,
            before_id,
        } => Request::Read {
            account: resolved.clone(),
            chat: chat.clone(),
            before: before_time.zip(before_id.clone()),
            limit: *limit,
        },
        Automation::Watch {
            account: _,
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
                    account: resolved.clone(),
                    chat: chat.clone(),
                    after: cursor.clone(),
                    limit: *limit,
                }
            } else {
                Request::WatchAccount {
                    account: resolved.clone(),
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
            account: _,
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
            // A bare number or conversation name singles out the chat; a full
            // id passes through untouched.
            let chat = if chat.contains('@') {
                chat
            } else {
                resolve_chat_target(runtime, &resolved, &chat)?
            };
            // Without an explicit id the CLI mints one and reports it, so the
            // send stays checkable through `status`. Repeating a send must
            // reuse a printed id; a fresh run mints a fresh one.
            let request_id = match request_id {
                Some(id) => id.to_ascii_lowercase(),
                None => {
                    let generated = new_request_id()?;
                    eprintln!("Request id: {generated}");
                    generated
                }
            };
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
                    account: resolved.clone(),
                    chat,
                    request_id: request_id.clone(),
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
                    if !metadata.is_file() || metadata.len() > crate::automation::FILE_LIMIT {
                        return Err("Attachments must be files of at most 64 MiB".into());
                    }
                    let mut source = source.take(crate::automation::FILE_LIMIT + 1);
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
                    account: resolved.clone(),
                    chat,
                    request_id: request_id.clone(),
                    text,
                    files: attachments,
                }
            }
        }
        Automation::Status {
            account: _,
            chat,
            request_id,
        } => Request::Status {
            account: resolved.clone(),
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
                after: initial_after,
                limit,
                kind,
                order,
                unread_only,
                include_archived,
            } => Request::Chats {
                account: account.clone(),
                after: collection_cursor.clone().or_else(|| initial_after.clone()),
                limit: remaining.unwrap_or(*limit).min(page_limit),
                kind: *kind,
                order: *order,
                unread_only: *unread_only,
                include_archived: *include_archived,
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
                limit: remaining.unwrap_or(*limit).min(page_limit),
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
        let reply=crate::automation::send(runtime,&current).map_err(|_|"Could not reach opted-in local automation. A send may be Unconfirmed; query the same request id".to_owned())?;
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
        if matches!(request, Request::Chats { .. } | Request::Contacts { .. }) {
            if let Some(left) = remaining.as_mut() {
                *left = left.saturating_sub(rows.map_or(0, Vec::len));
                if *left == 0 {
                    break;
                }
            }
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

/// Runs a control command against the already-running app, returning the
/// process exit code. Automation subcommands go through
/// [`automation_command`]; anything else prints its failure and yields 1,
/// except quitting nothing running, which is already the asked state.
pub fn run_control(command: &Control) -> i32 {
    if let Control::Automation { operation } | Control::Access(operation) = command {
        let discovered = paths::AppDirs::discover();
        return match automation_command(operation, &discovered.runtime, &discovered.state) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("{error}");
                1
            }
        };
    }
    let (verb, not_running) = match command {
        Control::ReloadThemes => (
            "reload-themes",
            "Zap Fasterk is not running, so there are no themes to reload.",
        ),
        Control::Quit => ("quit", "Zap Fasterk is not running."),
        Control::Automation { .. } | Control::Access(_) => {
            unreachable!("automation subcommands run through automation_command")
        }
    };
    let discovered = paths::AppDirs::discover();
    if let Err(error) = single_instance::send(&discovered.runtime, verb) {
        use std::io::ErrorKind;
        if matches!(
            error.kind(),
            ErrorKind::NotFound | ErrorKind::ConnectionRefused
        ) {
            eprintln!("{not_running}");
            if matches!(command, Control::Quit) {
                return 0;
            }
        } else {
            eprintln!("Could not reach the running Zap Fasterk: {error}");
        }
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(accounts: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for id in accounts {
            std::fs::create_dir_all(dir.path().join("accounts").join(id)).unwrap();
        }
        dir
    }

    #[test]
    fn single_account_needs_no_flag() {
        let dir = state_with(&["1"]);
        assert_eq!(resolve_account(dir.path(), None).unwrap(), "1".to_owned());
        assert_eq!(
            resolve_account(dir.path(), Some("2")).unwrap(),
            "2".to_owned()
        );
    }

    #[test]
    fn missing_or_ambiguous_account_refuses() {
        let dir = state_with(&[]);
        assert!(resolve_account(dir.path(), None).is_err());
        let dir = state_with(&["1", "2"]);
        let error = resolve_account(dir.path(), None).unwrap_err();
        assert!(error.contains("--account"), "{error}");
    }

    #[test]
    fn minted_request_ids_are_valid_status_keys() {
        let id = new_request_id().unwrap();
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn send_targets_match_by_number_suffix_or_name() {        let phone = TargetQuery::classify("5491150531403");
        assert!(matches!(phone, TargetQuery::Phone(_)));
        assert_eq!(
            phone.matches("5491150531403@s.whatsapp.net", "Charly"),
            TargetMatch::Exact
        );
        let short = TargetQuery::classify("+54 911-5053-1403");
        assert_eq!(
            short.matches("5491150531403@s.whatsapp.net", "Charly"),
            TargetMatch::Exact
        );
        assert_eq!(
            short.matches("5491150539999@s.whatsapp.net", "Otro"),
            TargetMatch::No
        );
        let name = TargetQuery::classify("charly");
        assert!(matches!(name, TargetQuery::Name(_)));
        assert_eq!(
            name.matches("5491150531403@s.whatsapp.net", "Charly Artese"),
            TargetMatch::Candidate
        );
        assert_eq!(name.matches("1@s.whatsapp.net", ""), TargetMatch::No);
        assert_eq!(name.matches("2@s.whatsapp.net", "Otro"), TargetMatch::No);
    }
}
