//! Application state and the frame loop.
//!
//! Views queue [`Action`]s while drawing. The app applies them after the frame
//! and processes backend events.

use crate::backend::{EditDraft, EditFailure};
use crate::model::ComposerMention;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::account::Account;
use crate::audio::{Player, Recorder};
use crate::backend::{Backend, Command, Event, LinkStatus, Refusal, Unsent, Waker};
use crate::i18n::Locale;
use crate::image_preview::PreviewState;
use crate::model::{
    AccountId, Action, Chat, ChatFilter, ChatId, Contact, Content, Delivery, Dialog, Gif, GifError,
    Label, Media, MediaState, Message, Page, PickerTab, Scroll, SidebarDisplayMode, StickerPack,
    StickerShelf, Toast, ToastKind,
};
use crate::nav::{History, Location};
use crate::paths::AppDirs;
use crate::settings::{AccountRoster, NotificationSound, Settings, ThemeChoice};
use crate::single_instance::{ControlCommand, Guard};
use crate::theme::{self, Palette};

/// Initial and incremental message-page size.
pub const PAGE: usize = 60;

/// How long a reading of the archive's downloaded size stays fresh. The
/// reading itself is bounded by the `messages_downloaded` index, so this only
/// decides how often a page left open follows a download.
const STORAGE_STATS_TTL: Duration = Duration::from_secs(60);
/// Minimum delay between phone history requests.
const PHONE_COOLDOWN: Duration = Duration::from_secs(6);
/// WhatsApp message-edit window.
/// WhatsApp revoke-for-everyone window.
pub const REVOKE_WINDOW: Duration = Duration::from_secs(2 * 24 * 60 * 60);

/// Pause, in seconds, after which a trackpad gesture ends and a new one
/// picks its own pane, as fastframe-scroll ends one. Measured on the frame's
/// input clock (`InputState::time`), not on the wall clock, so a slow frame
/// is not taken for a pause and tests can run it at their pace.
const SCROLL_GESTURE_GAP: f64 = 0.15;

/// A pane that scrolls on its own, which a scroll gesture stays with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollPane {
    /// The chat list, or its collapsed avatar column.
    Chats,
    /// The open chat's messages.
    Messages,
}

/// Keeps a scroll gesture, glide included, with the pane it began over
/// (#274). egui scrolls whichever scroll area is under the pointer, so a
/// gesture that drifted over the other pane moved that one instead, and so
/// did the rest of its glide. The panes say where they are each frame; while
/// a gesture lasts, vertical scrolling with the pointer away from its pane is
/// taken from egui and handed to that pane, which applies it itself.
#[derive(Default)]
pub struct ScrollRoute {
    /// Where each pane was drawn in the last frame.
    placed: Vec<(ScrollPane, egui::Rect)>,
    /// Where each pane is drawn in this frame. Behind a lock so a view can
    /// record it while other parts of the app are borrowed.
    placing: std::sync::Mutex<Vec<(ScrollPane, egui::Rect)>>,
    /// The pane the gesture under way began over.
    owner: Option<ScrollPane>,
    /// When the gesture last had input, in egui input time.
    last_input: Option<f64>,
    /// Whether the gesture's fingers lifted: new input starts another.
    lifted: bool,
    /// Scrolling taken for the owner this frame.
    carry: Option<(ScrollPane, f32)>,
}

impl ScrollRoute {
    /// Records where `pane` is drawn this frame.
    pub fn place(&self, pane: ScrollPane, rect: egui::Rect) {
        self.placing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push((pane, rect));
    }

    /// The vertical scrolling taken for `pane` from over another place this
    /// frame, for the pane to apply with `Ui::scroll_with_delta`.
    pub fn take(&mut self, pane: ScrollPane) -> f32 {
        match self.carry {
            Some((to, delta)) if to == pane => {
                self.carry = None;
                delta
            }
            _ => 0.0,
        }
    }

    /// Picks the gesture's pane and takes its scrolling from elsewhere.
    /// `moved` is whether wheel input arrived this frame, `lifted` whether
    /// the fingers left the trackpad, and `gliding` whether fastframe-scroll's
    /// glide is still adding to the scroll.
    fn route(&mut self, ctx: &egui::Context, moved: bool, lifted: bool, gliding: bool) {
        self.placed = std::mem::take(self.placing.get_mut().unwrap_or_else(|p| p.into_inner()));
        self.carry = None;
        let now = ctx.input(|input| input.time);
        // Floating layers, such as menus and dialogs, are no pane.
        let under = ctx
            .input(|input| input.pointer.hover_pos())
            .filter(|pos| {
                ctx.layer_id_at(*pos)
                    .is_none_or(|layer| layer.order == egui::Order::Background)
            })
            .and_then(|pos| {
                self.placed
                    .iter()
                    .find(|(_, rect)| rect.contains(pos))
                    .map(|(pane, _)| *pane)
            });
        let recent = self
            .last_input
            .is_some_and(|at| now - at < SCROLL_GESTURE_GAP);
        if moved {
            if self.lifted || !recent {
                self.owner = under;
            }
            self.last_input = Some(now);
            self.lifted = false;
        }
        self.lifted |= lifted;
        let settling = ctx.input(|input| input.smooth_scroll_delta != egui::Vec2::ZERO);
        if !moved && !gliding && !settling && !recent {
            self.owner = None;
        }
        let Some(owner) = self.owner else {
            return;
        };
        if under != Some(owner) && self.placed.iter().any(|(pane, _)| *pane == owner) {
            let delta = ctx.input_mut(|input| std::mem::take(&mut input.smooth_scroll_delta.y));
            if delta != 0.0 {
                self.carry = Some((owner, delta));
            }
        }
    }
}
/// Delay after the last keystroke before clearing typing state.
const COMPOSING_TIMEOUT: Duration = Duration::from_secs(4);
/// How long an info toast stays, including its fade.
pub const INFO_TOAST_LIFETIME: Duration = Duration::from_millis(3200);
/// Error toasts kept on screen at once; older ones give way to newer ones.
const MAX_ERROR_TOASTS: usize = 3;
/// Typing-state timeout when no stop event arrives.
const TYPING_TIMEOUT: Duration = Duration::from_secs(12);
/// How long other apps' media stays paused while the next voice message of a
/// run downloads. A stalled download must not keep music paused for good.
const VOICE_FETCH_HOLD: Duration = Duration::from_secs(10);

/// Loaded chat history and paging state.
#[derive(Default)]
pub struct Conversation {
    pub messages: Vec<Message>,
    /// Transcript text cached in the archive, by source message id.
    pub transcripts: HashMap<String, String>,
    /// Whether the local archive has no earlier messages.
    pub complete: bool,
    pub loading_older: bool,
    /// Whether the initial page was requested.
    pub requested: bool,
    /// Whether a phone history request is active.
    pub fetching_phone: bool,
    /// Whether the active phone request is the reader's own (see
    /// [`Command::FetchOlder`]).
    pub phone_explicit: bool,
    /// Whether phone history is exhausted or unavailable.
    pub phone_exhausted: bool,
    /// Last phone response time for request throttling.
    pub phone_answered: Option<Instant>,
    /// Consecutive empty phone responses used for backoff.
    pub phone_misses: u32,
    /// Whether messages arrived after the latest phone request.
    pub phone_delivered: bool,
    /// The height each row last took, keyed by message id, so the transcript
    /// can skip rows far from the viewport instead of laying them out.
    pub(crate) rows: HashMap<String, RowHeight>,
    /// The keyboard page, Home, or End scroll under way in the message list.
    pub(crate) key_scroll: Option<KeyScroll>,
}

/// A keyboard scroll the message list eases through, one instant step per
/// frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyScroll {
    pub kind: Scroll,
    /// For PgUp/PgDn, the signed `scroll_with_delta` distance still to cover.
    pub remaining: f32,
    /// `ui.input().time` when it started.
    start: f64,
    duration: f32,
    /// The eased progress already applied, from 0 to 1.
    progress: f32,
}

impl KeyScroll {
    pub fn new(kind: Scroll, remaining: f32, start: f64, duration: f32) -> Self {
        Self {
            kind,
            remaining,
            start,
            duration,
            progress: 0.0,
        }
    }

    /// Advances the easing to `now` and returns the share of the distance
    /// still to cover that this frame moves: all of it once time is up. A
    /// pass redone in the same frame gets 0.
    pub fn advance(&mut self, now: f64) -> f32 {
        let t = ((now - self.start) / f64::from(self.duration)).clamp(0.0, 1.0) as f32;
        if t >= 1.0 || self.progress >= 1.0 {
            self.progress = 1.0;
            return 1.0;
        }
        // Ease out: a scroll a held key restarts still moves at once.
        let eased = egui::emath::easing::cubic_out(t);
        let fraction = (eased - self.progress) / (1.0 - self.progress);
        self.progress = eased;
        fraction
    }

    pub fn done(&self) -> bool {
        self.progress >= 1.0
    }
}

/// A transcript row's height as last laid out or estimated.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RowHeight {
    pub height: f32,
    /// The egui pass the row was last laid out in; `None` for an estimate.
    pub pass: Option<u64>,
}

impl Conversation {
    fn merge(&mut self, incoming: Vec<Message>, older: bool) {
        // Prepend new older rows, but refresh duplicates too: on-demand
        // history can supply edits and phone ordering absent from our archive.
        let old_len = self.messages.len();
        let mut positions: HashMap<String, usize> = self
            .messages
            .iter()
            .enumerate()
            .map(|(index, message)| (message.id.clone(), index))
            .collect();
        for message in incoming {
            match positions.get(&message.id) {
                Some(&index) => {
                    let existing = &mut self.messages[index];
                    let media = existing
                        .content
                        .media()
                        .map(|media| (media.state.clone(), media.path.clone()));
                    *existing = message;
                    // Keep downloads in flight and files absent from a freshly
                    // classified copy. An incoming path takes precedence.
                    if let (Some((state, path)), Some(media)) =
                        (media, existing.content.media_mut())
                        && media.path.is_none()
                    {
                        media.state = state;
                        media.path = path;
                    }
                }
                None => {
                    positions.insert(message.id.clone(), self.messages.len());
                    self.messages.push(message);
                }
            }
        }
        if older {
            let added = self.messages.len() - old_len;
            self.messages.rotate_right(added);
        }
        self.sort();
    }

    /// Chat order: by time, with messages waiting to send last.
    fn sort(&mut self) {
        self.messages.sort_by_key(|message| {
            (
                message.waiting_to_send(),
                message.timestamp,
                message.history_order.unwrap_or(i64::MAX),
            )
        });
    }

    /// Moves one row whose time or waiting state changed to its place in
    /// chat order, after the rows it ties with, or before them when it is
    /// `first`. The queue changes rows one at a time in sending order, so a
    /// burst sent within one second keeps its order when it starts waiting
    /// and when it goes out. A send refused again is the first to go out.
    fn reposition(&mut self, id: &str, first: bool) {
        let Some(index) = self.messages.iter().position(|row| row.id == id) else {
            return;
        };
        let row = self.messages.remove(index);
        let key = (
            row.waiting_to_send(),
            row.timestamp,
            row.history_order.unwrap_or(i64::MAX),
        );
        let at = self.messages.partition_point(|other| {
            let other = (
                other.waiting_to_send(),
                other.timestamp,
                other.history_order.unwrap_or(i64::MAX),
            );
            if first { other < key } else { other <= key }
        });
        self.messages.insert(at, row);
    }

    pub fn message_mut(&mut self, id: &str) -> Option<&mut Message> {
        self.messages.iter_mut().find(|message| message.id == id)
    }

    pub fn message(&self, id: &str) -> Option<&Message> {
        self.messages.iter().find(|message| message.id == id)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Presence {
    pub online: bool,
    pub last_seen: Option<i64>,
}

/// Messages swept by dragging over them (#246): everything from the row
/// the drag began on to the row under the pointer joins what was selected
/// before, by the chat's order, so rows the list skipped count too.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sweep {
    pub chat: ChatId,
    pub anchor: String,
    /// The row under the pointer when the sweep last moved.
    pub to: String,
    base: Vec<String>,
}

/// Whether a message can join a selection, matching `Worker::forward_job`.
pub(crate) fn can_select(content: &Content) -> bool {
    !matches!(
        content,
        Content::Revoked
            | Content::Call { .. }
            | Content::PhoneOnly { .. }
            | Content::Unsupported { .. }
            | Content::Poll { .. }
            | Content::Interactive { .. }
    )
}

/// Adds the messages from `anchor` to `to` to a selection, in either
/// direction, keeping the chat's order and skipping content that cannot
/// be forwarded.
fn add_range(messages: &[Message], ids: &mut Vec<String>, anchor: &str, to: &str) {
    let position = |id: &str| messages.iter().position(|message| message.id == id);
    let (Some(from), Some(to)) = (position(anchor), position(to)) else {
        return;
    };
    let (from, to) = (from.min(to), from.max(to));
    for message in &messages[from..=to] {
        if can_select(&message.content) && !ids.contains(&message.id) {
            ids.push(message.id.clone());
        }
    }
    ids.sort_by_key(|id| position(id).unwrap_or(usize::MAX));
}

/// The "unread messages" divider of the open chat. It stays until another
/// chat opens, like on the phone.
#[derive(Clone, Debug, PartialEq)]
pub struct UnreadDivider {
    pub chat: ChatId,
    /// Unread incoming messages when the chat was opened.
    pub count: u32,
    /// The transcript has scrolled to it once.
    pub placed: bool,
}

struct ArchiveRestoreTask {
    origin: AccountId,
    destination: AccountId,
    receiver:
        std::sync::mpsc::Receiver<Result<Option<crate::archive::backup::BackupSummary>, String>>,
}

pub struct App {
    pub dirs: AppDirs,
    pub settings: Settings,
    /// Resolved interface language, from the setting or the system locale.
    pub locale: Locale,
    settings_dirty: bool,
    last_settings_save: Instant,
    pub roster: AccountRoster,
    pub accounts: Vec<Account>,
    pub active: usize,
    pub adding_account: bool,
    /// The account on screen when "Add account" was chosen, for Cancel.
    account_before_adding: Option<AccountId>,
    /// Whether the account switcher under our own avatar is open.
    pub account_menu: bool,
    pub palette: Palette,
    pub custom_themes: theme::Catalog,
    applied_dark: Option<bool>,
    /// Reveals a new palette from the middle of the window outwards.
    theme_transition: fastframe_theme::Transition,
    /// Whether palette changes are revealed. Off in tests, whose frames send
    /// no screenshots and would hold the old palette.
    pub reveal_theme_changes: bool,
    zoom_applied: bool,
    /// The wallpaper image named in the settings, decoded off this thread.
    pub wallpaper_image: crate::wallpaper::CustomImage,
    custom_font_request: u64,
    custom_font_pending: Option<crate::custom_font::PendingPick>,
    custom_emoji_request: u64,
    custom_emoji_pending: Option<crate::custom_emoji::PendingPick>,

    /// A sent message moves its chat up, so the chat list goes to the top.
    pub scroll_chats_to_top: bool,
    pub composer: String,
    pub(crate) emoticon_undo: Option<EmoticonUndo>,
    composer_mentions: Vec<ComposerMention>,
    /// Byte offset of the `:` starting the active emoji query.
    pub emoji_start: Option<usize>,
    /// Keyboard-highlighted emoji in suggestions or the full picker.
    pub emoji_selected: usize,
    /// Byte offset of the `@` starting the active mention query.
    pub mention_start: Option<usize>,
    /// Keyboard-highlighted member in the mention suggestions.
    pub mention_selected: usize,
    /// Private quotes kept with their direct chat while switching conversations.
    /// Deletion tombstones prevent refused private sends from reviving a replacement chat.
    /// Account baseline, advanced on unlink without reusing an old destination lifetime.
    /// Reply target in the open chat.
    pub reply_to: Option<crate::model::ReplyTarget>,
    /// Outgoing message being edited.
    pub editing: Option<String>,
    composing: bool,
    last_keystroke: Option<Instant>,
    /// The search pane beside the open chat: its query, day filter and the
    /// matches it lists.
    pub chat_search_open: bool,
    pub chat_search: String,
    /// Matches in the open chat, newest first.
    pub chat_search_hits: Vec<Message>,
    /// Whether the archive held more matches than the pane lists.
    pub chat_search_truncated: bool,
    /// Whether an answer for the current query and day is still due.
    pub chat_search_pending: bool,
    /// The result the arrow keys have reached, if any.
    pub chat_search_selected: Option<usize>,
    /// Local day the in-chat search is limited to, if any.
    pub chat_search_day: Option<jiff::civil::Date>,
    /// Month the day filter shows.
    pub chat_search_month: jiff::civil::Date,
    pub chat_search_calendar: bool,
    /// Whether the pane's field should take focus.
    pub focus_chat_search: bool,
    /// Counts and sizes of the downloaded attachments, for Settings. `None`
    /// until a reading succeeds, so a failed one leaves the row blank instead
    /// Whether the locked-chats folder is open.
    pub locked_folder: bool,
    /// The verifier authenticated for this window session, never the code.
    chat_lock_session: Option<String>,
    pub chat_lock_entry: String,
    pub chat_lock_confirm: String,
    pub chat_lock_error: bool,
    pub new_chat_search: String,
    /// Last answer from the slow code verifier, keyed by what was checked.
    chat_lock_check: std::cell::RefCell<Option<(String, Option<String>, bool)>>,
    /// Receipts of the message whose "Message info" is open.
    pub message_receipts: Option<crate::model::MessageReceipts>,
    /// The group message the backend is following receipts for.
    pub(crate) receipts_watch: Option<(ChatId, String)>,
    /// The group invite link being previewed or joined.
    pub invite: Option<crate::model::GroupInvite>,
    /// Where the unread messages began when the open chat was opened.
    pub unread_divider: Option<UnreadDivider>,
    /// Messages selected in a chat, in the chat's order.
    pub selection: Option<(ChatId, Vec<String>)>,
    /// The message a Shift-click range starts from.
    selection_anchor: Option<String>,
    /// Messages being swept with the mouse held down, if any.
    pub(crate) sweep: Option<Sweep>,
    /// Whether files are being dragged over the window.
    pub dropping: bool,
    /// A text paste already handled the clipboard before the shortcut release.
    paste_before_release: bool,
    /// Last observed command state, distinguishing a new shortcut from V-up.
    paste_command_down: bool,
    /// A V press delivered to us, unlike the press swallowed by native paste.
    ordinary_v_down: bool,
    saw_paste_command: bool,
    paste_was_focused: bool,
    /// Open emoji, GIF, or sticker picker tab.
    pub picker: Option<PickerTab>,
    /// Picker anchor at the composer button.
    pub picker_anchor: Option<egui::Rect>,
    pub picker_search: String,
    /// Recent emoji as they stood when the emoji picker opened. The row keeps
    /// that order while the picker is open, so the same emoji can be sent
    /// again from where it was; it catches up the next time it opens (#294).
    pub picker_recent: Option<Vec<String>>,
    /// Whether the newly opened picker should focus search.
    pub picker_focus: bool,
    /// Message the full emoji reaction picker is targeting.
    pub reaction_target: Option<(ChatId, String)>,
    /// Control that opened the reaction picker.
    pub reaction_anchor: Option<egui::Rect>,
    /// The reaction picker came from a message's context menu, which stays
    /// open beside it.
    pub reaction_beside_menu: bool,
    /// Demo/test: keep this message's context menu open.
    pub open_message_menu: Option<String>,
    #[cfg(any(test, feature = "demo"))]
    pub(crate) queued_demo: Option<crate::demo::queued::QueueDemo>,
    /// Demo/test: keep this chat row's context menu open.
    #[cfg(any(test, feature = "demo"))]
    pub open_chat_menu: Option<ChatId>,
    /// Demo/test: keep the open chat's header menu open.
    #[cfg(any(test, feature = "demo"))]
    pub open_header_menu: Option<ChatId>,
    /// Emoji-grid header to scroll into view.
    pub emoji_jump: Option<&'static str>,
    /// Attachments pending in the composer.
    pub pending: Vec<Pending>,
    /// Whether the composer plus menu is open or closing.
    pub composer_tools_open: bool,
    /// In-chat audio player.
    pub player: Player,
    /// In-chat video player.
    pub video: crate::video::Player,
    /// Chat of the loaded video; leaving it stops the video.
    video_chat: Option<ChatId>,
    /// Video to play once its download finishes.
    video_wanted: Option<(ChatId, String)>,
    /// Chat whose voice messages carry on into the next unheard one when a
    /// clip ends. Leaving the chat, or playing a video, ends the run.
    voice_chat: Option<ChatId>,
    /// Next voice message of a run, playing once its download finishes, and
    /// when the download began. Media stays paused for a short while meanwhile.
    voice_wanted: Option<(ChatId, String, Instant)>,
    /// Active voice recorder.
    pub recording: Option<Recorder>,
    /// A voice message the worker refused, kept with its chat so it can be
    /// sent again from that chat or discarded.
    /// Keeps other apps' music paused while recording or playing audio.
    media_hold: Option<crate::media_pause::Hold>,
    /// Only the real app pauses other apps' media, never tests or demos.
    pauses_media: bool,
    /// Image currently shown in the native preview.
    pub image_preview: Option<PreviewState>,
    pub sticker_view: Option<crate::image_preview::StickerPreview>,
    /// Whether the loaded video covers the window instead of its bubble.
    pub video_expanded: bool,
    /// The motion photo whose clip plays over it: its chat and message.
    pub motion_playing: Option<(ChatId, String)>,
    /// Message bodies registered for transcript copy formatting.
    pub copy_rows: std::sync::Arc<std::sync::Mutex<Vec<crate::transcript::Row>>>,
    /// Previous message-list rect used by the selection hook.
    pub selection_view: std::sync::Arc<std::sync::Mutex<Option<egui::Rect>>>,
    pub gif_query: String,
    pub gif_results: Vec<Gif>,
    /// Whether a GIF search is active.
    pub gif_pending: bool,
    pub gif_error: Option<GifError>,
    /// The list the sticker tab shows.
    pub sticker_shelf: StickerShelf,
    /// Text in the sticker search field.
    pub sticker_search: String,
    /// Emojis each listed sticker is tagged with.
    pub sticker_emojis: std::collections::HashMap<PathBuf, Vec<String>>,
    /// Text in the "New pack" field.
    pub sticker_pack_name: String,
    /// The picture being made into a sticker.
    pub sticker_draft: Option<crate::model::StickerDraft>,
    /// The staged picture being cropped, while the cropper is open.
    pub picture_edit: Option<crate::model::PictureEdit>,
    /// Where the picture being written came from, for when it lands.
    pub picture_applying: Option<crate::model::PictureOrigin>,
    /// Where each cropped staged file came from, by the path it was written to.
    pub picture_origins: std::collections::HashMap<std::path::PathBuf, crate::model::PictureOrigin>,
    /// The pixels of a pasted picture being cropped again, keyed by the buffer
    /// they came from. Cropping a pasted picture leaves a file where the strip
    /// had the pixels, so the cropper has to draw from the buffer itself.
    pub picture_texture: Option<(usize, egui::TextureHandle)>,
    /// A pack shared in a chat, being viewed: the pack and its publisher.
    pub sticker_preview: Option<(StickerPack, String)>,
    /// Whether the viewed pack is still downloading.
    pub sticker_preview_pending: bool,
    /// A pack just created here, selected once the backend lists it.
    sticker_pack_created: Option<String>,
    /// The wheel step, and on Linux the touchpad's scale, glide and axis
    /// lock.
    pub(crate) scrolling: fastframe_scroll::Scrolling,
    /// Keeps a scroll gesture with the pane it began over.
    pub scroll_route: ScrollRoute,

    pub page: Page,
    pub dialog: Option<Dialog>,
    pub backup_draft: crate::ui::archive_backup::BackupDraft,
    archive_restore: Option<ArchiveRestoreTask>,
    /// Places visited in this window, for the mouse's back and forward
    /// buttons. Cleared when the account on screen changes.
    nav_history: History,
    /// Chat filter in the forwarding destination dialog.
    pub forward_search: String,
    /// Selected destinations in the active account forwarding dialog.
    pub forward_recipients: Vec<ChatId>,
    pub forward_reviewing: bool,

    pub location_draft: String,
    /// The group name being typed in the group info dialog.
    pub group_name_edit: Option<String>,

    /// The new-contact dialog's "Save to phone" box.
    pub new_contact_to_phone: bool,
    pub sidebar_visible: bool,
    /// Name typed in the label manager.
    pub label_name: String,
    /// Colour the manager will use for the next label.
    pub label_color: String,
    /// Label being renamed, with the name being typed.
    pub label_editing: Option<(String, String)>,
    /// Label the chat list shows; `None` shows every chat.
    /// Chats opened from the Unread list, kept there until the filter changes.
    unread_kept: HashSet<ChatId>,
    pub toasts: Vec<Toast>,
    pub(crate) undo: crate::undo::Queue,
    pub(crate) accessibility_announcements: Vec<String>,
    pub actions: Vec<Action>,
    /// Actions queued by a backend event, applied on that account after the frame.
    deferred_account_actions: Vec<(AccountId, Action)>,
    /// Set while the events of an account that is not on screen are being
    /// applied: they update that account only, never the window's composer,
    /// dialogs, playback, or read state.
    events_hidden: bool,
    /// A newer release than this build, once GitHub has said so.
    pub update: Option<crate::updates::Release>,
    last_update_check: Option<Instant>,
    pub show_update: bool,
    pub update_download: crate::updates::DownloadState,
    pub update_support: Option<Result<crate::updates::Installation, String>>,
    update_inspecting: bool,
    pub update_arguments: Vec<String>,
    /// Whether to scroll the conversation to its newest message.
    pub scroll_to_bottom: bool,
    /// One-shot: an explicit jump (Ctrl+End, or the return-to-bottom button)
    /// must reach the bottom even while a message bubble retains keyboard
    /// focus, unlike `scroll_to_bottom` set for other reasons (opening a
    /// chat, sending a message), which still defers to that focus so it is
    /// not pulled out from under a keyboard-navigating reader.
    pub scroll_to_bottom_forced: bool,
    /// A pending page-relative scroll for the open chat, consumed by its
    /// message list on the next frame.
    pub scroll_page: Option<Scroll>,
    /// Whether the conversation was at the bottom last frame.
    pub at_bottom: bool,
    /// Message id to scroll into view.
    pub scroll_anchor: Option<String>,
    /// A message reached from a quote or a search result, which flashes
    /// once it is in view so the eye finds it.
    pub jump_highlight: Option<JumpHighlight>,
    pub focus_composer: bool,
    pub focus_search: bool,
    /// What the Settings page is filtered by.
    pub settings_search: String,
    pub focus_settings_search: bool,
    pub quit_requested: bool,
    pub window_focused: bool,
    /// A notification went out this frame: highlight the taskbar entry of a
    /// window that is open but not focused.
    wants_attention: bool,
    /// Whether Zap Faster starts at login, when this installation supports it.
    pub start_with_system: Option<bool>,
    /// Cross-thread window repaint handle.
    waker: Waker,
    tray: Option<fastframe_tray::Tray>,
    /// Sleep and wake as the system reports them. Tests and demo runs get a
    /// fixed one that reads nothing from the machine.
    power: fastframe_power::Power,
    /// Stands in for whether a panel shows the tray item, which tests cannot
    /// spawn.
    #[cfg(test)]
    test_tray_shown: Option<bool>,
    /// Whether the tray menu offers "Lock Zap Faster": whether an app lock
    /// password was set when it last changed.
    tray_lockable: bool,
    /// Whether the app is running without a window.
    pub window_hidden: bool,
    tray_start_deadline: Option<std::time::Instant>,
    /// Whether window close should keep the process running.
    pub hide_intent: bool,
    /// Whether a headless app should create a window.
    pub wants_show: bool,
    /// Whether this session draws through Wayland, where a compositor ignores
    /// a programmatic focus or unminimize request and the window is closed and
    /// reopened instead.
    wayland: bool,
    niri_focus_pending: Option<std::sync::mpsc::Receiver<bool>>,
    /// Whether the current window close should reopen a fresh one at once
    /// (`Closed::Reopen`) rather than stop drawing.
    reopen: bool,
    /// Requests received from later launches.
    automation: Option<crate::automation::Server>,
    control_commands: Option<std::sync::Arc<std::sync::Mutex<Vec<ControlCommand>>>>,
    /// Browser links wait for this account's archive and the app's unlock.
    pending_chat_links: Vec<(AccountId, crate::chat_link::ChatLink)>,
    pending_group_invites: Vec<(AccountId, crate::chat_link::GroupInviteLink)>,
    /// Chats and messages from clicked notifications.
    notification_opens: std::sync::Arc<std::sync::Mutex<Vec<crate::notify::NotificationTarget>>>,
    notifications: crate::notify::Notifications,
    pub(crate) in_app_notifications: Vec<crate::ui::notification_overlay::Notification>,
    #[cfg(target_os = "windows")]
    pub(crate) native_notifications: Option<crate::notify::NativePopups>,
    #[cfg(target_os = "windows")]
    native_notifications_tried: bool,
    next_in_app_notification: u64,
    /// Unread count on the taskbar icon, where the desktop reads it. `None`
    /// for demo and test runs, which must not touch the real taskbar.
    badge: Option<crate::notify::Badge>,
    /// The app lock: whether the window shows only the lock screen, the
    /// inactivity count, and the Settings password form.
    pub app_lock: crate::app_lock::AppLock,
}

/// A message that flashes after a jump to it, as WhatsApp does.
#[derive(Clone, Debug, PartialEq)]
pub struct JumpHighlight {
    pub chat: ChatId,
    pub message: String,
    /// When the message came into view, in egui's input time; `None` until
    /// then.
    pub since: Option<f64>,
}

impl JumpHighlight {
    /// How long the flash lasts, in seconds.
    pub const DURATION: f64 = 2.0;

    pub fn new(chat: ChatId, message: String) -> Self {
        Self {
            chat,
            message,
            since: None,
        }
    }

    /// The flash's strength `elapsed` seconds after the message came into
    /// view: a quick rise, a hold, then a fade to nothing.
    pub fn strength(elapsed: f64) -> f32 {
        const RISE: f64 = 0.15;
        const HOLD: f64 = 0.9;
        let strength = if elapsed < 0.0 {
            0.0
        } else if elapsed < RISE {
            elapsed / RISE
        } else if elapsed < HOLD {
            1.0
        } else {
            1.0 - (elapsed - HOLD) / (Self::DURATION - HOLD)
        };
        strength.clamp(0.0, 1.0) as f32
    }
}

/// Localizes known number-lookup failures while preserving unknown failure details.
fn contact_failure_message(locale: crate::i18n::Locale, error: &str) -> String {
    match error {
        "Could not check the number. Please try again." => {
            crate::i18n::gettext(locale, "Could not check the number. Please try again.")
                .into_owned()
        }
        "This number is not on WhatsApp" => {
            crate::i18n::gettext(locale, "This number is not on WhatsApp").into_owned()
        }
        "Not connected to WhatsApp" => {
            crate::i18n::gettext(locale, "Not connected to WhatsApp").into_owned()
        }
        "Enter a valid international phone number" => {
            crate::i18n::gettext(locale, "Enter a valid international phone number").into_owned()
        }
        _ => error.to_owned(),
    }
}

/// Attachments and their quote belong to one chat in one account.
#[derive(Default)]
pub(crate) struct AttachmentDraft {
    pub(crate) pending: Vec<Pending>,
    quoting: Option<crate::model::ReplyTarget>,
}

/// Attachment pending in the composer.
pub enum Pending {
    /// Clipboard image as straight-alpha RGBA and optional preview.
    Picture {
        width: usize,
        height: usize,
        rgba: std::sync::Arc<Vec<u8>>,
        texture: Option<egui::TextureHandle>,
    },
    File(PathBuf),
    /// Original file bytes or pasted pixels sent as a document.
    Document(Box<Pending>),
    /// A pre-transmission refusal retains its own caption and quote beside newer drafts.
    Recovered {
        item: Box<Pending>,
        caption: Option<String>,
        quoting: Option<crate::model::ReplyTarget>,
        mentions: Vec<String>,
        composer_owned: bool,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct EmoticonUndo {
    pub(crate) start_byte: usize,
    pub(crate) source: String,
    pub(crate) emoji: String,
    pub(crate) cursor_after: usize,
}

impl Pending {
    /// The media underneath the document send choice.
    pub fn media(&self) -> &Self {
        match self {
            Self::Document(item) | Self::Recovered { item, .. } => item.media(),
            item => item,
        }
    }

    /// Whether the composer can preview the file as an image.
    pub fn is_picture_file(path: &std::path::Path) -> bool {
        mime_guess2::from_path(path)
            .first()
            .is_some_and(|mime| mime.type_() == "image")
    }
}

/// Whether this session draws through Wayland, where a compositor ignores an
/// app's request to focus or unminimize one of its own windows: winit's
/// Wayland `focus_window` is a no-op and `set_minimized(false)` only warns.
/// Mirrors winit's own choice: Wayland when either variable is non-empty.
#[cfg(target_os = "linux")]
fn wayland_session() -> bool {
    ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .into_iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

/// Wayland is Linux-only; elsewhere a window focuses and restores normally.
#[cfg(not(target_os = "linux"))]
fn wayland_session() -> bool {
    false
}

/// The app outlives its window: closing it with "keep running" on hides
/// Zap Faster, and the tray, a notification or another launch brings it back.
impl fastframe_shell::Resident for App {
    fn closed(&self) -> fastframe_shell::Closed {
        if self.quit_requested {
            fastframe_shell::Closed::Quit
        } else if self.reopen {
            // The window was closed only to make a fresh one, which is how a
            // Wayland session brings a minimized or covered window forward.
            fastframe_shell::Closed::Reopen
        } else if self.hide_intent {
            fastframe_shell::Closed::Hide
        } else {
            fastframe_shell::Closed::Quit
        }
    }

    fn window_gone(&mut self) {
        App::window_gone(self);
    }

    fn headless_frame(&mut self, ctx: &egui::Context) -> fastframe_shell::Headless {
        self.background_frame(ctx);
        if self.quit_requested {
            fastframe_shell::Headless::Quit
        } else if self.wants_show {
            fastframe_shell::Headless::Show
        } else {
            fastframe_shell::Headless::Wait
        }
    }

    /// Without a tray there is no way back to a hidden window, so show it.
    fn start_hidden(&mut self) -> bool {
        if !self.hides_to_tray() {
            #[cfg(target_os = "linux")]
            if self.tray.is_some() && self.settings.keep_running_in_background {
                self.tray_start_deadline =
                    Some(std::time::Instant::now() + std::time::Duration::from_secs(15));
            } else {
                return false;
            }
            #[cfg(not(target_os = "linux"))]
            return false;
        }
        App::start_hidden(self);
        true
    }

    fn shutdown(&mut self) {
        App::shutdown(self);
    }
}

const TRAY_SHOW: &str = "show";
const TRAY_LOCK: &str = "lock";
const TRAY_QUIT: &str = "quit";

/// What a tray click asks for: a left click on Linux and macOS, or the menu's
/// first entry, toggles the window; a left click on Windows and a Dock click
/// on macOS show it.
fn tray_action(event: fastframe_tray::Event, window_hidden: bool) -> Option<Action> {
    use fastframe_tray::Event;
    Some(match event {
        Event::Show => Action::ShowWindow,
        Event::Toggle | Event::Menu(TRAY_SHOW) if window_hidden => Action::ShowWindow,
        Event::Toggle | Event::Menu(TRAY_SHOW) => Action::HideWindow,
        Event::Menu(TRAY_LOCK) => Action::LockApp,
        Event::Menu(TRAY_QUIT) => Action::Quit,
        Event::Menu(_) => return None,
    })
}

/// The tray item: Zap Faster's icon, and a menu to show or hide the window,
/// to lock it while an app lock password is set, and to quit. The title and
/// the menu never name a chat, so they are safe while locked.
///
/// "Lock Zap Faster" is always in the menu, hidden without a password;
/// `App::sync_tray` shows or hides it as the password is set or removed.
fn tray_config(lockable: bool) -> fastframe_tray::Config {
    use fastframe_tray::MenuItem;
    fastframe_tray::Config {
        id: "zapfast",
        title: "Zap Faster".into(),
        icon: crate::util::app_icon_rgba,
        template_icon: Some(crate::util::tray_template_rgba),
        // Use the app's pixel icon consistently so optional unread indicators can
        // change at runtime without recreating the tray.
        // Hosts prefer a named theme icon over pixels, which would hide unread badges.
        themed_icon: false,
        // A left click on macOS toggles the window, as on Linux.
        menu_on_click: false,
        menu: vec![
            MenuItem::action(TRAY_SHOW, "Show or hide Zap Faster"),
            MenuItem::action(TRAY_LOCK, "Lock Zap Faster").visible(lockable),
            MenuItem::Separator,
            MenuItem::action(TRAY_QUIT, "Quit"),
        ],
    }
}

/// Process-level app services.
#[derive(Clone, Copy, Debug)]
pub struct AppOptions {
    /// Registers the system-tray item.
    pub tray: bool,
}

impl Default for AppOptions {
    fn default() -> Self {
        Self { tray: true }
    }
}

impl std::ops::Deref for App {
    type Target = Account;

    fn deref(&self) -> &Account {
        &self.accounts[self.active]
    }
}

impl std::ops::DerefMut for App {
    fn deref_mut(&mut self) -> &mut Account {
        &mut self.accounts[self.active]
    }
}

impl App {
    pub fn new(
        waker: &Waker,
        dirs: AppDirs,
        settings: Settings,
        options: AppOptions,
    ) -> std::io::Result<Self> {
        crate::proxy::configure(&settings.proxy);
        let mut roster = AccountRoster::load(&dirs.accounts_file())?;
        crate::archive::backup::reconcile_restores(&dirs, &roster.order)
            .map_err(std::io::Error::other)?;
        if roster.order.is_empty() {
            roster = AccountRoster::default();
        }
        let mut accounts = Vec::new();
        for id in &roster.order {
            let Some(id) = AccountId::parse(id) else {
                continue;
            };
            accounts.push(Account::spawn(&dirs, id, &settings, waker)?);
        }
        if accounts.is_empty() {
            accounts.push(Account::spawn(&dirs, AccountId::first(), &settings, waker)?);
            roster = AccountRoster::default();
        }
        let active = roster
            .order
            .iter()
            .position(|id| id == &roster.active)
            .unwrap_or(0)
            .min(accounts.len() - 1);
        let mut app = Self::with_accounts(dirs, settings, roster, accounts, active, waker.clone());
        app.pauses_media = true;
        app.badge = Some(Default::default());
        app.custom_themes
            .enable_desktop_themes(crate::theme::DESKTOP_THEMES);
        app.load_custom_themes();
        // Reading the desktop's font settings may wait on D-Bus; keep it off
        // the first frame.
        let text_waker = waker.clone();
        std::thread::Builder::new()
            .name("text-rendering".into())
            .spawn(move || crate::theme::follow_text_rendering(move || text_waker.wake()))
            .ok();
        let power_waker = waker.clone();
        app.power = fastframe_power::Power::spawn(move || power_waker.wake());
        if options.tray {
            let waker = waker.clone();
            app.tray =
                fastframe_tray::Tray::spawn(tray_config(app.tray_lockable), move || waker.wake());
        }
        // The clock preference may wait on the desktop portal on Linux; keep
        // it off the first frame.
        std::thread::Builder::new()
            .name("clock-format".into())
            .spawn(|| {
                crate::util::twelve_hour_clock();
            })
            .ok();
        let folder = app.settings.download_folder.clone();
        for account in &app.accounts {
            account.backend.send(Command::SetDownloadOrganization {
                organization: app.settings.download_organization,
                descriptive_names: app.settings.descriptive_download_names,
            });
            account
                .backend
                .send(Command::SetDownloadFolder(folder.clone()));
            account.backend.send(Command::SetKeepChatsArchived(
                app.settings.keep_chats_archived,
            ));
        }
        if crate::autostart::supported() {
            app.start_with_system = Some(crate::autostart::enabled());
        }
        let _ = app.roster.save(&app.dirs.accounts_file());
        Ok(app)
    }

    /// Single-instance guard used by later launches.
    pub fn set_remote_control(&mut self, guard: &Guard) {
        self.control_commands = Some(guard.commands());
    }

    /// Creates a disconnected app and event sender for demos and tests.
    pub fn headless(dirs: AppDirs, settings: Settings) -> (Self, std::sync::mpsc::Sender<Event>) {
        let (account, events) = Account::detached(
            &dirs,
            AccountId::first(),
            crate::settings::AccountSettings::from_legacy(&settings),
        )
        .expect("test account folders");
        let roster = AccountRoster {
            active: account.id.0.clone(),
            ..AccountRoster::default()
        };
        let app = Self::with_accounts(dirs, settings, roster, vec![account], 0, Waker::default());
        (app, events)
    }

    /// Starts without a window (`--start-hidden`). The link and the archive
    /// start now: they otherwise wait for a first frame, which a hidden start
    /// only draws once the tray or another launch shows the window.
    pub fn start_hidden(&mut self) {
        self.hide_intent = true;
        // No window will attach the tray, which makes the macOS item; make
        // it now without bringing Zap Faster forward.
        if let Some(tray) = &mut self.tray {
            tray.create_item();
        }
        for account in &mut self.accounts {
            if let Some(startup) = account.backend.take_startup() {
                let _ = startup.send(());
            }
        }
    }

    /// Constructs application state around the supplied backend and persisted settings.
    #[cfg_attr(not(test), expect(dead_code))]
    fn with_backend(dirs: AppDirs, settings: Settings, backend: Backend, waker: Waker) -> Self {
        let (account, _) = {
            let id = AccountId::first();
            let account_dirs = dirs.account(&id);
            let _ = account_dirs.ensure();
            (
                Account::new(
                    id,
                    account_dirs,
                    crate::settings::AccountSettings::from_legacy(&settings),
                    backend,
                ),
                (),
            )
        };
        let roster = AccountRoster {
            active: account.id.0.clone(),
            ..AccountRoster::default()
        };
        Self::with_accounts(dirs, settings, roster, vec![account], 0, waker)
    }
    /// Builds the window state with the supplied accounts and selected account.
    fn with_accounts(
        dirs: AppDirs,
        settings: Settings,
        roster: AccountRoster,
        mut accounts: Vec<Account>,
        active: usize,
        waker: Waker,
    ) -> Self {
        if !settings.restore_last_chat {
            // Only the startup selection changes. Keep the saved chat for a
            // later launch, and leave account switching within this run alone.
            accounts[active].open_chat = None;
        }
        let palette = settings
            .cached_palette()
            .unwrap_or_else(|| match settings.theme {
                ThemeChoice::Light => Palette::light(),
                ThemeChoice::Messages => Palette::messages(),
                _ => Palette::dark(),
            });
        let locale = crate::i18n::resolve(settings.interface_language);
        // With a password set, Zap Faster starts locked.
        let app_lock = crate::app_lock::AppLock::new(settings.app_lock_hash.is_some());
        let tray_lockable = settings.app_lock_hash.is_some();
        let mut app = Self {
            dirs,
            settings,
            locale,
            settings_dirty: false,
            last_settings_save: Instant::now(),
            roster,
            accounts,
            active,
            adding_account: false,
            account_before_adding: None,
            account_menu: false,
            palette,
            custom_themes: theme::Catalog::default(),
            applied_dark: None,
            theme_transition: fastframe_theme::Transition::default(),
            reveal_theme_changes: !cfg!(test),
            zoom_applied: false,
            wallpaper_image: crate::wallpaper::CustomImage::default(),
            custom_font_request: 0,
            custom_font_pending: None,
            custom_emoji_request: 0,
            custom_emoji_pending: None,
            scroll_chats_to_top: false,
            composer: String::new(),
            emoticon_undo: None,
            composer_mentions: Vec::new(),
            emoji_start: None,
            emoji_selected: 0,
            mention_start: None,
            mention_selected: 0,
            reply_to: None,
            editing: None,
            composing: false,
            last_keystroke: None,
            chat_search_open: false,
            chat_search: String::new(),
            chat_search_hits: Vec::new(),
            chat_search_truncated: false,
            chat_search_pending: false,
            chat_search_selected: None,
            chat_search_day: None,
            chat_search_month: crate::util::today(),
            chat_search_calendar: false,
            focus_chat_search: false,
            locked_folder: false,
            chat_lock_session: None,
            chat_lock_entry: String::new(),
            chat_lock_confirm: String::new(),
            chat_lock_error: false,
            new_chat_search: String::new(),
            chat_lock_check: Default::default(),
            message_receipts: None,
            receipts_watch: None,
            invite: None,
            unread_divider: None,
            selection: None,
            selection_anchor: None,
            sweep: None,
            dropping: false,
            paste_before_release: false,
            paste_command_down: false,
            ordinary_v_down: false,
            saw_paste_command: false,
            paste_was_focused: false,
            picker: None,
            picker_anchor: None,
            picker_search: String::new(),
            picker_recent: None,
            picker_focus: false,
            reaction_target: None,
            reaction_anchor: None,
            reaction_beside_menu: false,
            open_message_menu: None,
            #[cfg(any(test, feature = "demo"))]
            queued_demo: None,
            #[cfg(any(test, feature = "demo"))]
            open_chat_menu: None,
            #[cfg(any(test, feature = "demo"))]
            open_header_menu: None,
            emoji_jump: None,
            pending: Vec::new(),
            composer_tools_open: false,
            player: Player::new(waker.clone()),
            video: crate::video::Player::new(waker.clone()),
            video_chat: None,
            video_wanted: None,
            voice_chat: None,
            voice_wanted: None,
            recording: None,
            media_hold: None,
            pauses_media: false,
            image_preview: None,
            sticker_view: None,
            video_expanded: false,
            motion_playing: None,
            copy_rows: Default::default(),
            selection_view: Default::default(),
            gif_query: String::new(),
            gif_results: Vec::new(),
            gif_pending: false,
            gif_error: None,
            sticker_shelf: StickerShelf::default(),
            sticker_search: String::new(),
            sticker_emojis: std::collections::HashMap::new(),
            sticker_pack_name: String::new(),
            sticker_pack_created: None,
            sticker_preview: None,
            sticker_preview_pending: false,
            sticker_draft: None,
            picture_edit: None,
            picture_applying: None,
            picture_origins: Default::default(),
            picture_texture: None,
            scrolling: fastframe_scroll::Scrolling::default(),
            scroll_route: ScrollRoute::default(),
            page: Page::Chats,
            dialog: None,
            backup_draft: crate::ui::archive_backup::BackupDraft::default(),
            archive_restore: None,
            nav_history: History::default(),
            forward_search: String::new(),
            forward_recipients: Vec::new(),
            forward_reviewing: false,

            location_draft: String::new(),
            group_name_edit: None,

            new_contact_to_phone: true,
            sidebar_visible: true,
            label_name: String::new(),
            label_color: crate::archive::DEFAULT_COLOR.to_owned(),
            label_editing: None,
            unread_kept: HashSet::new(),
            toasts: Vec::new(),
            undo: Default::default(),
            accessibility_announcements: Vec::new(),
            actions: Vec::new(),
            deferred_account_actions: Vec::new(),
            events_hidden: false,
            update: None,
            last_update_check: None,
            show_update: false,
            update_download: Default::default(),
            update_support: None,
            update_inspecting: false,
            update_arguments: Vec::new(),
            scroll_to_bottom: true,
            scroll_to_bottom_forced: false,
            scroll_page: None,
            at_bottom: true,
            scroll_anchor: None,
            jump_highlight: None,
            focus_composer: false,
            focus_search: false,
            settings_search: String::new(),
            focus_settings_search: false,
            quit_requested: false,
            window_focused: false,
            wants_attention: false,
            start_with_system: None,
            waker,
            tray: None,
            power: fastframe_power::Power::fixed(Default::default()),
            #[cfg(test)]
            test_tray_shown: None,
            tray_lockable,
            window_hidden: false,
            tray_start_deadline: None,
            hide_intent: false,
            wants_show: false,
            wayland: wayland_session(),
            niri_focus_pending: None,
            reopen: false,
            automation: None,
            control_commands: None,
            pending_chat_links: Vec::new(),
            pending_group_invites: Vec::new(),
            notification_opens: Default::default(),
            notifications: Default::default(),
            in_app_notifications: Vec::new(),
            #[cfg(target_os = "windows")]
            native_notifications: None,
            #[cfg(target_os = "windows")]
            native_notifications_tried: false,
            next_in_app_notification: 0,
            badge: None,
            app_lock,
        };
        // A hand-edited speed snaps to a supported one, so a speed control
        // always shows the speed that plays.
        app.settings.voice_speed = app.player.set_speed(app.settings.voice_speed);
        app
    }

    pub fn account(&self) -> &Account {
        &self.accounts[self.active]
    }

    pub fn account_mut(&mut self) -> &mut Account {
        &mut self.accounts[self.active]
    }

    pub fn any_linked(&self) -> bool {
        self.accounts.iter().any(Account::is_linked)
    }

    /// Whether more than one number is (or is being) linked here, so the
    /// window names which one it shows.
    pub fn has_several_accounts(&self) -> bool {
        self.accounts.len() >= 2
    }

    /// Unread chats across every account, for the taskbar and the tray.
    pub fn unread_chat_count_everywhere(&self) -> u32 {
        self.accounts
            .iter()
            .map(Account::unread_chat_count)
            .fold(0, u32::saturating_add)
    }

    /// Unread chats in the accounts that are not on screen, for the mark on
    /// the account switcher.
    pub fn unread_chat_count_elsewhere(&self) -> u32 {
        self.accounts
            .iter()
            .enumerate()
            .filter(|(index, _)| *index != self.active)
            .map(|(_, account)| account.unread_chat_count())
            .fold(0, u32::saturating_add)
    }

    fn save_roster(&mut self) {
        self.roster.active = self.account().id.0.clone();
        self.roster.order = self
            .accounts
            .iter()
            .map(|account| account.id.0.clone())
            .collect();
        if let Err(error) = self.roster.save(&self.dirs.accounts_file()) {
            log::warn!("could not save accounts.json: {error}");
        }
    }

    pub(crate) fn take_composer_text(&mut self) -> String {
        self.emoticon_undo = None;
        std::mem::take(&mut self.composer)
    }

    fn replace_composer_text(&mut self, text: String) {
        self.composer = text;
        self.emoticon_undo = None;
    }

    fn clear_composer_text(&mut self) {
        self.composer.clear();
        self.emoticon_undo = None;
    }

    /// Parks the current account's text, mentions and attachments before switching accounts.
    fn park_composer(&mut self) {
        if let Some(previous) = self.open_chat.clone() {
            self.save_attachment_draft(&previous);
            if let Some(target) = self.reply_to.take().filter(|target| target.chat.is_some()) {
                self.private_reply_drafts.insert(previous.clone(), target);
            }
            let draft = self.take_composer_text();
            if self.editing.take().is_some() || draft.trim().is_empty() {
                self.drafts.remove(&previous);
                self.draft_mentions.remove(&previous);
                self.composer_mentions.clear();
            } else {
                let mentions = std::mem::take(&mut self.composer_mentions);
                self.drafts.insert(previous.clone(), draft);
                self.draft_mentions.insert(previous.clone(), mentions);
            }
            self.stop_composing(&previous);
            let text = self.drafts.get(&previous).map_or("", String::as_str);
            self.store_draft(&previous, text);
        }
        self.picker = None;
        self.pending.clear();
        self.reply_to = None;
        self.editing = None;
        self.recording = None;
        self.emoji_start = None;
        self.mention_start = None;
    }
    /// Restores the active account's last conversation draft after changing accounts.
    fn restore_composer(&mut self) {
        if let Some(id) = self.open_chat.clone() {
            let text = self.drafts.remove(&id).unwrap_or_default();
            self.replace_composer_text(text);
            self.composer_mentions = self.draft_mentions.remove(&id).unwrap_or_default();
            let attachments = self.attachment_drafts.remove(&id).unwrap_or_default();
            self.pending = attachments.pending;
            self.reply_to = attachments
                .quoting
                .or_else(|| self.private_reply_drafts.remove(&id));
        } else {
            self.clear_composer_text();
            self.composer_mentions.clear();
        }
        self.focus_composer = self.open_chat.is_some();
        self.restore_refused_edit();
    }

    fn switch_account(&mut self, id: &AccountId) {
        let Some(index) = self.accounts.iter().position(|account| &account.id == id) else {
            return;
        };
        if index == self.active {
            return;
        }
        // An account left before it was linked was never really added.
        let abandoned =
            (self.adding_account && !self.is_linked()).then(|| self.account().id.clone());
        self.park_composer();
        self.clear_account_ui();
        self.accessibility_announcements.clear();
        self.active = index;
        self.adding_account = false;
        if let Some(abandoned) = abandoned {
            self.remove_account(abandoned);
        }
        self.restore_composer();
        self.page = Page::Chats;
        self.save_roster();
        self.wallpaper_image.reload();
        self.report_presence();
        self.report_background_history();
        // Chats belong to the account that owns them; another account's chat
        // cannot be restored into this one.
        self.nav_history.clear();
    }

    /// Drops App-owned pointers into the previous account's chats and media.
    fn clear_account_ui(&mut self) {
        self.forward_recipients.clear();
        self.forward_reviewing = false;
        // The locked folder and the picker show the previous account's chats
        // and stickers.
        if self.locked_folder {
            self.close_locked_folder();
        }
        self.sticker_shelf = StickerShelf::default();
        self.sticker_emojis.clear();
        self.sticker_pack_created = None;
        self.account_menu = false;
        self.chat_search_open = false;
        self.chat_search.clear();
        self.chat_search_hits.clear();
        self.chat_search_selected = None;
        self.chat_search_pending = false;
        self.selection = None;
        self.selection_anchor = None;
        self.sweep = None;
        self.reply_to = None;
        self.editing = None;
        self.reaction_target = None;
        self.reaction_anchor = None;
        self.picker = None;
        self.pending.clear();
        self.invite = None;
        self.message_receipts = None;
        self.receipts_watch = None;
        self.unread_divider = None;
        self.scroll_anchor = None;
        self.dialog = None;
        self.player.stop();
        self.video.stop();
        self.video_chat = None;
        self.video_wanted = None;
        self.motion_playing = None;
        self.voice_chat = None;
        self.voice_wanted = None;
        self.recording = None;
        self.image_preview = None;
        self.sticker_view = None;
        self.video_expanded = false;
    }

    fn begin_archive_restore(&mut self, passphrase: crate::archive::backup::BackupPassphrase) {
        if self.archive_restore.is_some() || self.backup_busy {
            return;
        }
        let origin = self.account().id.clone();
        let destination = loop {
            let mut reserved = self.roster.clone();
            let id = reserved.allocate();
            self.roster.next_id = reserved.next_id;
            let dirs = self.dirs.account(&id);
            if !dirs.state.exists() && !dirs.cache.exists() {
                break id;
            }
        };
        let dirs = self.dirs.account(&destination);
        let staging = self.dirs.state.join("backup-staging");
        let waker = self.waker.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        self.backup_busy = true;
        self.backup_status =
            Some(crate::i18n::gettext(self.locale, "Restoring encrypted archive…").into_owned());
        let spawned = std::thread::Builder::new().name("archive-restore".into()).spawn(move || {
            let result = (|| -> anyhow::Result<Option<crate::archive::backup::BackupSummary>> {
                let Some(source) = rfd::FileDialog::new()
                    .add_filter("Encrypted Zap Faster archive", &["zfbackup"])
                    .pick_file() else { return Ok(None); };
                let prepared = crate::archive::backup::prepare_restore(&source, passphrase.expose(), &staging)?;
                let has_session = prepared.has_session();
                let summary = prepared.install_into_empty(&dirs)?;
                let mut settings = if has_session {
                    crate::settings::AccountSettings::load(&dirs.settings_file())
                } else { Default::default() };
                // Retain the recovered identity and preferences without opening
                // a second connection to the same linked device automatically.
                settings.archive_only = true;
                settings.cli_access = false;
                if !has_session {
                    settings.background_history = false;
                    settings.archive_media = false;
                    settings.notifications = false;
                    settings.send_read_receipts = false;
                    settings.send_typing = false;
                    settings.auto_download = false;
                }
                if let Err(error) = settings.save(&dirs.settings_file()) {
                    if let Err(cleanup) = discard_unregistered_archive_restore(&dirs) {
                        anyhow::bail!("Could not save restored settings: {error}; cleanup failed: {cleanup}");
                    }
                    return Err(error.into());
                }
                Ok(Some(summary))
            })().map_err(|error| error.to_string());
            let _ = sender.send(result);
            waker.wake();
        });
        match spawned {
            Ok(_) => {
                self.archive_restore = Some(ArchiveRestoreTask {
                    origin,
                    destination,
                    receiver,
                })
            }
            Err(_) => {
                self.backup_busy = false;
                self.backup_status = Some(
                    crate::i18n::gettext(self.locale, "Could not start archive restore")
                        .into_owned(),
                );
                self.toast_error("Could not start archive restore");
            }
        }
    }

    fn poll_archive_restore(&mut self) {
        let Some(task) = &self.archive_restore else {
            return;
        };
        let result = match task.receiver.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Archive restore worker stopped unexpectedly".into())
            }
        };
        let task = self
            .archive_restore
            .take()
            .expect("restore task was present");
        let status = match result {
            Ok(Some(_summary)) => {
                let mut account = match Account::spawn(
                    &self.dirs,
                    task.destination.clone(),
                    &self.settings,
                    &self.waker,
                ) {
                    Ok(account) => account,
                    Err(error) => {
                        let cleanup = discard_unregistered_archive_restore(
                            &self.dirs.account(&task.destination),
                        )
                        .err();
                        let detail = cleanup
                            .map(|error| format!("; cleanup failed: {error}"))
                            .unwrap_or_default();
                        self.finish_archive_restore(
                            &task.origin,
                            format!("Could not open restored archive: {error}{detail}"),
                            true,
                        );
                        return;
                    }
                };
                let mut roster = self.roster.clone();
                roster.order = self
                    .accounts
                    .iter()
                    .map(|account| account.id.as_str().to_owned())
                    .collect();
                roster.order.push(task.destination.as_str().to_owned());
                roster.active = self.account().id.as_str().to_owned();
                if let Err(error) = roster.save(&self.dirs.accounts_file()) {
                    account.backend.shutdown();
                    let cleanup = discard_unregistered_archive_restore(&account.dirs).err();
                    let detail = cleanup
                        .map(|error| format!("; cleanup failed: {error}"))
                        .unwrap_or_default();
                    self.finish_archive_restore(
                        &task.origin,
                        format!("Could not register restored archive: {error}{detail}"),
                        true,
                    );
                    return;
                }
                if let Err(error) = crate::archive::backup::finish_restore(&account.dirs) {
                    log::warn!(
                        "restored account is registered; ownership-marker cleanup will retry: {error}"
                    );
                }
                self.roster = roster;
                self.accounts.push(account);
                self.report_background_history();
                (
                    crate::i18n::gettext(
                        self.locale,
                        "Archive restored. Open the account switcher to view the offline archive.",
                    )
                    .into_owned(),
                    false,
                )
            }
            Ok(None) => (
                crate::i18n::gettext(self.locale, "Archive restore cancelled").into_owned(),
                false,
            ),
            Err(error) => (
                crate::i18n::gettext(self.locale, "Archive restore failed: {error}")
                    .replace("{error}", &error),
                true,
            ),
        };
        self.finish_archive_restore(&task.origin, status.0, status.1);
    }

    fn finish_archive_restore(&mut self, origin: &AccountId, message: String, failed: bool) {
        if let Some(account) = self
            .accounts
            .iter_mut()
            .find(|account| &account.id == origin)
        {
            account.backup_busy = false;
            account.backup_status = Some(message.clone());
        }
        if failed {
            self.toast_error(message);
        } else {
            self.toast(message);
        }
    }

    fn add_account(&mut self) {
        let id = self.roster.allocate();
        let account = if self
            .accounts
            .iter()
            .all(|account| account.backend.is_offline())
        {
            match Account::detached(&self.dirs, id, crate::settings::AccountSettings::default()) {
                Ok((account, _)) => account,
                Err(error) => {
                    self.toast_error(format!("Could not add an account: {error}"));
                    return;
                }
            }
        } else {
            match Account::spawn(&self.dirs, id, &self.settings, &self.waker) {
                Ok(account) => account,
                Err(error) => {
                    self.toast_error(format!("Could not add an account: {error}"));
                    return;
                }
            }
        };
        account.backend.send(Command::SetDownloadOrganization {
            organization: self.settings.download_organization,
            descriptive_names: self.settings.descriptive_download_names,
        });
        account.backend.send(Command::SetDownloadFolder(
            self.settings.download_folder.clone(),
        ));
        account.backend.send(Command::SetKeepChatsArchived(
            self.settings.keep_chats_archived,
        ));
        self.account_before_adding = Some(self.account().id.clone());
        self.park_composer();
        self.clear_account_ui();
        self.accounts.push(account);
        self.active = self.accounts.len() - 1;
        self.adding_account = true;
        self.page = Page::Chats;
        self.restore_composer();
        self.save_roster();
        self.report_presence();
        self.report_background_history();
        self.nav_history.clear();
    }

    /// Unlinks an account and, once its backend has stopped, deletes its
    /// folders. The last account is only unlinked: the window always shows
    /// one, even if it is waiting to be linked.
    fn remove_account(&mut self, id: AccountId) {
        self.undo.forget_account(&id);
        let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
            return;
        };
        if self.accounts.len() <= 1 {
            self.accounts[index].backend.send(Command::Unlink);
            return;
        }
        self.accounts[index].backend.send(Command::RemoveAccount);
    }

    fn finish_removed(&mut self, index: usize) {
        if self.accounts.len() <= 1 {
            // Another removal got there first. Keep this one listed and
            // linkable instead of leaving the window on a stopped backend.
            self.accounts[index].backend.send(Command::Unlink);
            return;
        }
        let removing_active = index == self.active;
        if removing_active {
            self.park_composer();
            self.clear_account_ui();
        }
        self.accounts[index].backend.shutdown();
        let dirs = self.accounts[index].dirs.clone();
        // The key outlives the archive only by mistake: forget it once the
        // archive is gone, never before.
        let key = crate::archive::archive_key_identity(&dirs.archive_db()).ok();
        let state_error = std::fs::remove_dir_all(&dirs.state).err();
        let cache_error = std::fs::remove_dir_all(&dirs.cache)
            .err()
            .filter(|error| error.kind() != std::io::ErrorKind::NotFound);
        if let Some(identity) = key
            && !dirs.archive_db().exists()
            && cfg!(not(test))
            && let Err(error) = crate::archive::forget_archive_key(&identity)
        {
            log::warn!("could not delete a removed account's archive key: {error:#}");
        }
        self.accounts.remove(index);
        if index < self.active {
            self.active -= 1;
        } else if self.active >= self.accounts.len() {
            self.active = self.accounts.len() - 1;
        }
        if removing_active {
            self.adding_account = false;
            self.restore_composer();
            self.page = Page::Chats;
            self.wallpaper_image.reload();
            self.nav_history.clear();
        }
        self.save_roster();
        self.report_presence();
        self.report_background_history();
        if let Some(error) = state_error.or(cache_error) {
            let message =
                crate::i18n::gettext(self.locale, "Could not delete the removed account's files");
            self.toast_error(format!("{message}: {error}"));
        }
    }

    /// Updates the linked app while no window exists.
    pub fn window_gone(&mut self) {
        self.flush_open_draft();
        self.clear_chat_lock_entry();
        if self.dialog == Some(Dialog::UnlockLockedChats) {
            self.dialog = None;
        }
        if self.locked_folder || self.secret_code_matched() {
            self.close_locked_folder();
            self.search.clear();
            self.search_hits.clear();
        }
        self.window_hidden = true;
        self.window_focused = false;
        self.hide_intent = false;
        self.wants_show = false;
    }

    /// What the conversation shows behind its bubbles, for the chat and the
    /// wallpaper preview alike.
    pub fn wallpaper(&self) -> crate::wallpaper::Look {
        let mut color = self.settings.wallpaper_background(&self.palette);
        if self.palette.vibrant() {
            color = self.palette.vibrant_chat(color);
        }
        crate::wallpaper::Look {
            color,
            // Messages draws its conversations on a plain background.
            doodles: self.settings.show_wallpaper
                && self.palette.bubbles != crate::theme::BubbleStyle::Messages,
            image: self
                .account()
                .settings
                .wallpaper_image
                .is_some()
                .then(|| self.wallpaper_image.ready())
                .flatten(),
        }
    }

    /// Whether window close keeps the app in the tray.
    pub fn hides_to_tray(&self) -> bool {
        self.tray_shown() && self.settings.keep_running_in_background
    }

    /// Whether a panel shows the tray item now, so a hidden window can be
    /// brought back from it. On Linux the item exists before a panel shows
    /// it (Zap Faster started at login before the panel), and on a desktop
    /// without one it never is.
    fn tray_shown(&self) -> bool {
        #[cfg(test)]
        if let Some(shown) = self.test_tray_shown {
            return shown;
        }
        self.tray
            .as_ref()
            .is_some_and(fastframe_tray::Tray::is_shown)
    }

    fn handle_tray(&mut self) {
        self.sync_tray();
        let Some(events) = self.tray.as_ref().map(fastframe_tray::Tray::events) else {
            return;
        };
        let hidden = self.window_hidden;
        self.actions.extend(
            events
                .into_iter()
                .filter_map(|event| tray_action(event, hidden)),
        );
    }

    /// Offers "Lock Zap Faster" in the tray exactly while an app lock password
    /// is set, so setting or removing one changes the menu at once.
    fn sync_tray(&mut self) {
        let lockable = self.settings.app_lock_hash.is_some();
        if lockable == self.tray_lockable {
            return;
        }
        self.tray_lockable = lockable;
        if let Some(tray) = &mut self.tray {
            tray.set_visible(TRAY_LOCK, lockable);
        }
    }

    pub(crate) fn sync_automation_authority(&self) {
        let Some(server) = &self.automation else {
            return;
        };
        let locked = self.app_lock.is_locked();
        server
            .locked
            .store(locked, std::sync::atomic::Ordering::SeqCst);
        server.permits.sync(|account, chat| {
            !locked
                && self.accounts.iter().any(|known| {
                    known.id.as_str() == account
                        && known.settings.cli_access
                        && (chat.is_empty()
                            || known.chats.iter().any(|known| {
                                known.id == chat
                                    && !known.locked
                                    && known.disappearing_timer().is_none()
                            }))
                })
        });
        server.permits.sync_collection_scopes(|id| {
            (!locked).then_some(())?;
            let account = self
                .accounts
                .iter()
                .find(|account| account.id.as_str() == id && account.settings.cli_access)?;
            Some(Self::automation_scope(account))
        });
    }

    fn automation_scope(account: &Account) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut chats: Vec<_> = account
            .chats
            .iter()
            .map(|chat| (&chat.id, chat.locked, chat.disappearing_timer()))
            .collect();
        chats.sort_unstable_by(|a, b| a.0.cmp(b.0));
        let mut scope = std::collections::hash_map::DefaultHasher::new();
        chats.hash(&mut scope);
        scope.finish()
    }

    fn handle_automation(&mut self) {
        if self.automation.is_none()
            && self
                .accounts
                .iter()
                .any(|account| account.settings.cli_access)
        {
            match crate::automation::Server::start(&self.dirs.runtime, self.waker.clone()) {
                Ok(server) => self.automation = Some(server),
                Err(error) => {
                    log::warn!("could not start local automation: {error}");
                    return;
                }
            }
        }
        self.sync_automation_authority();
        let Some(server) = &self.automation else {
            return;
        };
        server.locked.store(
            self.app_lock.is_locked(),
            std::sync::atomic::Ordering::SeqCst,
        );
        let requests = std::mem::take(&mut *server.queue.lock().unwrap_or_else(|p| p.into_inner()));
        for request in requests {
            if request.expired() {
                request.answer(crate::automation::error(
                    "Application locked or request expired",
                ));
                continue;
            }
            let Some(account) = self
                .accounts
                .iter()
                .find(|account| account.id.as_str() == request.request.account())
            else {
                request.answer(crate::automation::error("Unknown account"));
                continue;
            };
            if !account.settings.cli_access
                || account
                    .chats
                    .iter()
                    .any(|chat| chat.id == request.request.chat() && chat.locked)
            {
                request.answer(crate::automation::error(
                    "Access disabled or conversation locked",
                ));
                continue;
            }
            if request.request.is_send()
                && (account.settings.archive_only || !matches!(account.link, LinkStatus::Connected))
            {
                request.answer(crate::automation::error("Account is offline"));
                continue;
            }
            let authorized = if request.request.chat().is_empty() {
                server
                    .permits
                    .authorize_collection(&request, Self::automation_scope(account))
            } else {
                server.permits.authorize(&request)
            };
            if !authorized {
                request.answer(crate::automation::error(
                    "Request expired or authorization revoked",
                ));
                continue;
            }
            account.backend.send(Command::Automation(request));
        }
    }

    fn handle_control_commands(&mut self) {
        self.handle_automation();
        let Some(queue) = &self.control_commands else {
            return;
        };
        let commands: Vec<ControlCommand> =
            std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner()));
        for command in commands {
            match command {
                ControlCommand::Show => self.actions.push(Action::ShowWindow),
                ControlCommand::ReloadThemes => self.actions.push(Action::ReloadThemes),
                ControlCommand::Quit => self.actions.push(Action::Quit),
                ControlCommand::Ping => {}
                ControlCommand::OpenChatLink(link) => self.open_chat_link(link),
                ControlCommand::OpenGroupInvite(link) => self.open_group_invite(link),
            }
        }
    }

    /// A launch or browser click opens on the account that was active then.
    pub fn open_chat_link(&mut self, link: crate::chat_link::ChatLink) {
        self.pending_chat_links
            .push((self.account().id.clone(), link));
        self.actions.push(Action::ShowWindow);
    }

    pub fn open_group_invite(&mut self, link: crate::chat_link::GroupInviteLink) {
        self.pending_group_invites
            .push((self.account().id.clone(), link));
        self.actions.push(Action::ShowWindow);
    }

    fn handle_chat_links(&mut self, ctx: &egui::Context) {
        if self.app_lock.is_locked() {
            return;
        }
        for (account, link) in std::mem::take(&mut self.pending_group_invites) {
            let Some(target) = self
                .accounts
                .iter()
                .find(|candidate| candidate.id == account)
            else {
                continue;
            };
            if !target.chats_loaded || !matches!(target.link, LinkStatus::Connected) {
                self.pending_group_invites.push((account, link));
                continue;
            }
            self.switch_account(&account);
            self.apply(Action::OpenUrl(link.url()), ctx);
        }
        let pending = std::mem::take(&mut self.pending_chat_links);
        for (account, link) in pending {
            let Some(target) = self
                .accounts
                .iter()
                .find(|candidate| candidate.id == account)
            else {
                continue;
            };
            if !target.chats_loaded {
                self.pending_chat_links.push((account, link));
                continue;
            }
            self.switch_account(&account);
            let id = link.chat_id();
            if self.chat(&id).is_some_and(|chat| chat.locked) && !self.locked_folder_open() {
                continue;
            }
            let name = self.display_name(&id);
            self.apply(
                Action::StartChat {
                    dismiss_dialog: false,
                    id: id.clone(),
                    name,
                },
                ctx,
            );
            // A browser link cannot bypass the phone's locked-chat folder.
            if self.open_chat.as_deref() != Some(id.as_str()) {
                continue;
            }
            self.page = Page::Chats;
            if let Some(text) = link.text.filter(|text| !text.is_empty()) {
                self.apply(Action::CancelEdit, ctx);
                self.reply_to = None;
                // Keep text already drafted for this recipient.
                if !self.composer.is_empty() {
                    self.composer.push('\n');
                }
                self.composer.push_str(&text);
                self.emoji_start = None;
                self.mention_start = None;
                self.store_draft(&id, &self.composer);
            }
            self.refocus_composer(ctx);
        }
    }

    /// Opens the messages announced by clicked notifications, creating a
    /// window when needed. The click carries the message, so the reader lands
    /// on what the notification showed, not on the end of the chat.
    fn handle_notification_opens(&mut self) {
        let opened: Vec<crate::notify::NotificationTarget> = std::mem::take(
            &mut *self
                .notification_opens
                .lock()
                .unwrap_or_else(|p| p.into_inner()),
        );
        for target in opened {
            // Behind the app lock nothing changes yet: the message, with its
            // account, opens once unlocked.
            if self.app_lock.is_locked() {
                self.app_lock.deferred = Some(target);
                self.actions.push(Action::ShowWindow);
                continue;
            }
            self.switch_account(&target.account);
            self.actions.push(Action::OpenMessage {
                chat: target.chat,
                message: target.message,
            });
            self.actions.push(Action::ShowWindow);
        }
    }

    fn clear_chat_notifications(&mut self, chat: &str) {
        let account = self.account().id.clone();
        self.notifications.clear(&account, chat);
        self.in_app_notifications.retain(|notification| {
            notification.target.account != account || notification.target.chat != chat
        });
    }

    /// The chat was looked at, so its next message may notify at once.
    fn reset_notification_throttle(&mut self, chat: &str) {
        let account = self.account().id.clone();
        self.notifications.reset_throttle(&account, chat);
    }

    /// Sends a desktop notification for an unseen incoming message.
    fn maybe_notify(&mut self, chat_id: &str, message: &Message) {
        if !self.account().settings.notifications {
            return;
        }
        let Some(chat) = self.chat(chat_id) else {
            return;
        };
        let now = crate::util::now();
        if !notification_eligible(chat, now, message.timestamp) {
            return;
        }
        let reading = !self.events_hidden
            && !self.window_hidden
            && self.window_focused
            && self.page == Page::Chats
            && self.open_chat.as_deref() == Some(chat_id);
        if reading {
            return;
        }
        // A limited chat notifies once, then waits; opening it ends the wait.
        // A mention of us or a reply to us always notifies and leaves the wait alone.
        let for_us = chat.is_group() && self.addresses_us(message);
        if !self.focus_allows_chat(chat) && !(for_us && self.account().settings.focus_mentions) {
            return;
        }
        let limited = !for_us
            && self
                .account()
                .settings
                .limit_notifications
                .applies(chat.is_group());
        let account = self.account().id.clone();
        if !self
            .notifications
            .allow(&account, chat_id, std::time::Instant::now(), limited)
        {
            return;
        }
        let Some(chat) = self.chat(chat_id) else {
            return;
        };
        if self.app_lock.is_locked() {
            self.notify_while_locked(chat_id, chat.is_group(), &message.id, limited);
            return;
        }
        let (name, is_group) = (self.chat_title(chat), chat.is_group());
        let chat_sound = chat.notification_sound.clone();
        let label_sound = self.chat_label_sound(chat);
        let sender = self.display_name_or(&message.sender, message.sender_name.as_deref());
        let (title, body) = crate::notify::preview_lines(
            self.locale,
            self.account().settings.notification_sender,
            self.account().settings.notification_content,
            &name,
            is_group,
            &sender,
            &self.message_text(message),
        );
        // Prefer the chat picture, then the sender picture. Cached files work
        // before the chat list loads; new requests help later notifications.
        let sender = message.sender.clone();
        let picture = self
            .avatar(chat_id)
            .or_else(|| self.cached_avatar(chat_id))
            .or_else(|| self.avatar(&sender))
            .or_else(|| self.cached_avatar(&sender));
        let picture = picture.filter(|_| self.account().settings.notification_sender);
        let waker = self.waker.clone();
        let identity_visible = self.account().settings.notification_sender;
        let sound = notification_sound(
            &self.settings,
            chat_sound
                .or_else(|| (!for_us).then_some(label_sound).flatten())
                .filter(|_| identity_visible),
            is_group,
            for_us && identity_visible,
        );
        if self.account().settings.in_app_notifications {
            let now = std::time::Instant::now();
            self.in_app_notifications
                .retain(|notification| notification.expires > now);
            self.next_in_app_notification = self.next_in_app_notification.wrapping_add(1);
            self.in_app_notifications
                .push(crate::ui::notification_overlay::Notification {
                    id: self.next_in_app_notification,
                    target: crate::notify::NotificationTarget {
                        account: self.account().id.clone(),
                        chat: chat_id.to_owned(),
                        message: message.id.clone(),
                    },
                    title: title.chars().take(96).collect(),
                    body: body.chars().take(160).collect(),
                    expires: now + crate::ui::notification_overlay::LIFETIME,
                });
            if self.in_app_notifications.len() > crate::ui::notification_overlay::LIMIT {
                self.in_app_notifications.remove(0);
            }
        }
        self.notifications.show_on(
            title,
            body,
            picture,
            sound,
            self.account().settings.call_speaker.clone(),
            crate::notify::NotificationTarget {
                account: self.account().id.clone(),
                chat: chat_id.to_owned(),
                message: message.id.clone(),
            },
            std::sync::Arc::clone(&self.notification_opens),
            move || waker.wake(),
            limited,
        );
        self.wants_attention = true;
    }

    /// Announces a message while the app lock is on: "New message" from
    /// Zap Faster, without the chat, the sender, the text or a picture, so the
    /// desktop shows nothing the lock screen hides. A chat's own sound and
    /// the mention sound would tell who wrote, so only the message sound
    /// plays, still silent for groups when group sounds are off. The click
    /// target stays inside Zap Faster: it opens the message once unlocked.
    fn notify_while_locked(&mut self, chat_id: &str, is_group: bool, message: &str, limited: bool) {
        self.wants_attention = true;
        let (title, body) = crate::notify::locked_lines(self.locale);
        let sound = notification_sound(&self.settings, None, is_group, false);
        let waker = self.waker.clone();
        self.notifications.show_on(
            title,
            body,
            None,
            sound,
            self.account().settings.call_speaker.clone(),
            crate::notify::NotificationTarget {
                account: self.account().id.clone(),
                chat: chat_id.to_owned(),
                message: message.to_owned(),
            },
            std::sync::Arc::clone(&self.notification_opens),
            move || waker.wake(),
            limited,
        );
    }

    /// Every id a member list may name us by: the phone number and, before
    /// the worker knows the pair, the privacy id.
    pub fn our_ids(&self) -> Vec<&str> {
        [self.me.as_deref(), self.me_lid.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Whether a message mentions us or replies to one of our messages. A
    /// mention may name us by phone number or by privacy id; the worker files
    /// both under the phone number once it knows the pair, and the raw token
    /// keeps the privacy id recognisable before then.
    fn addresses_us(&self, message: &Message) -> bool {
        let ours: Vec<&str> = [self.me.as_deref(), self.me_lid.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let is_us = |id: &str| ours.contains(&id);
        let mentioned = message.mentions.iter().any(|mention| {
            is_us(&mention.id)
                || ours
                    .iter()
                    .any(|id| id.split('@').next() == Some(mention.user.as_str()))
        });
        mentioned
            || message
                .quoted
                .as_ref()
                .is_some_and(|quoted| is_us(&quoted.sender))
    }

    /// Initializes a newly created window.
    pub fn attach(&mut self, ctx: &egui::Context) {
        #[cfg(target_os = "linux")]
        if !cfg!(test) && self.pauses_media {
            ctx.add_plugin(crate::opener::BrowserLauncher);
        }
        // Register transcript copy formatting once per egui context.
        ctx.add_plugin(crate::transcript::CopyAnnotator {
            rows: std::sync::Arc::clone(&self.copy_rows),
        });
        ctx.data_mut(|data| {
            data.insert_temp(
                egui::Id::new("copy-rows"),
                std::sync::Arc::clone(&self.copy_rows),
            );
        });
        ctx.add_plugin(crate::ui::conversation::SelectionLeash::new(
            std::sync::Arc::clone(&self.selection_view),
        ));
        // Colour emoji in labels, menus, tooltips and text fields; message
        // bodies paint their own over placeholders, which it leaves alone.
        ctx.add_plugin(crate::emoji::plugin());
        self.apply_font(ctx);
        crate::theme::install(ctx);
        // Zoom stays in the settings, so egui must not change it behind the
        // app's back: the shortcuts below go through `Action::ZoomBy`.
        ctx.options_mut(|options| options.zoom_with_keyboard = false);
        // A keystroke that wraps the draft is applied in one pass, and the
        // bottom panel holding the composer only takes the new height in
        // the next: three passes keep it from showing a frame out of place.
        ctx.options_mut(|options| options.max_passes = std::num::NonZeroUsize::new(3).unwrap());
        // Load and index the color emoji font outside the frame loop.
        std::thread::Builder::new()
            .name("emoji-font".into())
            .spawn(crate::emoji::warm_up)
            .ok();
        self.applied_dark = None;
        self.theme_transition = fastframe_theme::Transition::default();
        self.zoom_applied = false;
        self.window_hidden = false;
        self.paste_before_release = false;
        self.paste_command_down = false;
        self.ordinary_v_down = false;
        self.saw_paste_command = false;
        self.paste_was_focused = false;
        self.hide_intent = false;
        self.wants_show = false;
        self.reopen = false;
        self.refocus_composer(ctx);
        // Before the tray: muda keeps the first menu handler it is given, and
        // the tray installs one when it makes its item on the first window.
        // Ours hands the tray its own events (#215).
        #[cfg(target_os = "macos")]
        crate::macos::attach(ctx);
        if let Some(tray) = &mut self.tray {
            tray.attach();
        }
    }

    pub fn is_connected(&self) -> bool {
        self.link.is_connected()
    }

    /// How the chat list is drawn right now. Hiding it leaves a column of
    /// avatars with unread badges.
    pub fn sidebar_mode(&self) -> SidebarDisplayMode {
        if self.sidebar_visible {
            SidebarDisplayMode::Expanded
        } else {
            SidebarDisplayMode::CollapsedIconsOnly
        }
    }

    /// Whether the device has linked data, including while offline.
    pub fn is_linked(&self) -> bool {
        self.account().settings.archive_only
            || matches!(
                self.link,
                LinkStatus::Connected | LinkStatus::Connecting | LinkStatus::Disconnected { .. }
            )
            || (!self.chats.is_empty() && !matches!(self.link, LinkStatus::LoggedOut))
    }

    pub fn chat(&self, id: &str) -> Option<&Chat> {
        self.chats.iter().find(|chat| chat.id == id)
    }

    pub fn chat_mut(&mut self, id: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|chat| chat.id == id)
    }

    /// Drops every trace of a chat that no longer exists. Unlike hiding a
    /// locked chat this discards the draft, because there is nothing left to
    /// send it to, and it clears `last_chat` so a restart does not reopen it.
    fn forget_chat(&mut self, id: &str) {
        let generation = self
            .private_reply_generations
            .get(id)
            .copied()
            .unwrap_or(self.private_reply_generation);
        self.private_reply_generations.insert(
            id.to_owned(),
            generation
                .checked_add(1)
                .expect("private reply destination generation exhausted"),
        );
        self.leave_chat(id);
        self.chats.retain(|chat| chat.id != id);
        self.conversations.remove(id);
        // The recorded places belong to the account on screen, and chat ids
        // repeat across accounts.
        if !self.events_hidden {
            self.nav_history.forget(id);
        }
        self.drafts.remove(id);
        self.private_reply_drafts.remove(id);
        self.draft_mentions.remove(id);
        self.attachment_drafts.remove(id);
        self.typing.remove(id);
        self.unread_kept.remove(id);
        if self.scroll_chat_into_view.as_deref() == Some(id) {
            self.scroll_chat_into_view = None;
        }
        if self.settings.last_chat.as_deref() == Some(id) {
            self.settings.last_chat = None;
        }
        if self.account().settings.last_chat.as_deref() == Some(id) {
            self.account_mut().settings.last_chat = None;
            self.account_mut().mark_settings_dirty();
        }
    }

    /// Takes everything on screen away from a chat the user can no longer
    /// reach, whether it was locked or deleted: its notifications, search
    /// hits and dialogs, and, when it is open, the conversation with its
    /// composer, recording and playback. Keeping a draft is up to the caller.
    fn leave_chat(&mut self, id: &str) {
        if self.gallery.chat.as_deref() == Some(id) {
            self.gallery.listing = None;
            self.gallery.pending = false;
            self.gallery.token = self.gallery.token.wrapping_add(1);
        }
        self.clear_chat_notifications(id);
        self.search_hits.retain(|message| message.chat != id);
        // A locked or deleted chat keeps none of its pinned words on screen,
        // and none of its starred message text in the open list.
        self.pins.remove(id);
        self.chat_pins.remove(id);
        self.pin_notices.remove(id);
        self.starred.retain(|entry| entry.message.chat != id);
        if self.events_hidden {
            if self.open_chat.as_deref() == Some(id) {
                self.open_chat = None;
            }
            return;
        }
        if self
            .sticker_view
            .as_ref()
            .and_then(crate::image_preview::StickerPreview::chat)
            == Some(id)
        {
            self.sticker_view = None;
        }
        if self.image_preview.as_ref().and_then(PreviewState::chat) == Some(id) {
            self.image_preview = None;
        }
        if matches!(
            &self.dialog,
            Some(
                Dialog::ChatInfo(chat)
                    | Dialog::CreatePoll(chat)
                    | Dialog::SendLocation(chat)
                    | Dialog::ConfirmDeleteChat(chat)
                    | Dialog::ConfirmClearChat(chat)
            ) if chat == id
        ) || matches!(
            &self.dialog,
            Some(Dialog::Forward { chat, .. } | Dialog::ConfirmDeleteMessage { chat, .. } | Dialog::ConfirmDeleteSelection { chat, .. })
                if chat == id
        ) {
            self.dialog = None;
            self.poll_creating = false;
        }
        if self.open_chat.as_deref() == Some(id) {
            self.stop_composing(id);
            self.open_chat = None;
            self.clear_composer_text();
            self.composer_mentions.clear();
            self.pending.clear();
            self.private_reply_drafts.remove(id);
            self.reply_to = None;
            self.editing = None;
            self.picker = None;
            self.reaction_target = None;
            self.reaction_anchor = None;
            self.emoji_start = None;
            self.mention_start = None;
            self.emoji_jump = None;
            self.dialog = None;
            self.recording = None;
            self.player.stop();
        }
    }

    pub fn current_chat(&self) -> Option<&Chat> {
        self.open_chat
            .as_deref()
            .and_then(|id| self.chat(id))
            .filter(|chat| !chat.locked || self.locked_folder_open())
    }

    /// The label with this id, when it still exists.
    pub fn label(&self, id: &str) -> Option<&Label> {
        self.labels.iter().find(|label| label.id == id)
    }

    /// Whether a chat wears a label id.
    pub fn chat_wears(&self, chat: &Chat, label: &str) -> bool {
        chat.labels.iter().any(|worn| worn == label)
    }

    /// Unread chats wearing a label, counted the way the other chips count.
    ///
    /// Archived and locked chats are left out, because the label's chip does
    /// not list them.
    pub fn label_unread(&self, id: &str) -> usize {
        self.chats
            .iter()
            .filter(|chat| {
                !chat.archived && !chat.locked && chat.unread > 0 && self.chat_wears(chat, id)
            })
            .count()
    }

    /// Drops filters, edits and chat labels that point at labels which are
    /// gone, so a deleted label leaves the list before the chats reload.
    fn prune_labels(&mut self) {
        let known: Vec<String> = self.labels.iter().map(|label| label.id.clone()).collect();
        let settings = &mut self.account_mut().settings;
        settings.focus_labels.retain(|id| known.contains(id));
        settings.label_sounds.retain(|id, _| known.contains(id));
        if settings.focus_labels.is_empty() {
            settings.focus_enabled = false;
        }
        self.account_mut().mark_settings_dirty();
        for chat in &mut self.chats {
            chat.labels.retain(|id| known.contains(id));
        }
        if self
            .label_filter
            .as_ref()
            .is_some_and(|id| !known.contains(id))
        {
            self.label_filter = None;
            self.account_mut().settings.last_label = None;
            self.account_mut().mark_settings_dirty();
        }
        if self
            .label_editing
            .as_ref()
            .is_some_and(|(id, _)| !known.contains(id))
        {
            self.label_editing = None;
        }
    }

    /// Picks the label the chat list shows. A label is one more chip in the
    /// filter row, so it replaces the chip filter, and leaves the archive and
    /// the locked folder the way the other chips do.
    fn select_label(&mut self, label: Option<String>) {
        let label = label.filter(|id| self.labels.iter().any(|known| &known.id == id));
        if self.locked_folder {
            self.close_locked_folder();
            self.search.clear();
            self.search_hits.clear();
        }
        self.account_mut().settings.last_label = label.clone();
        self.account_mut().settings.last_filter = ChatFilter::All;
        self.account_mut().mark_settings_dirty();
        self.label_filter = label;
        self.chat_filter = ChatFilter::All;
        self.show_archived = false;
        self.unread_kept.clear();
    }

    /// Why a label name cannot be used, if it cannot. `except` is the label
    /// being renamed, which may keep its own name.
    fn label_name_refusal(&self, name: &str, except: Option<&str>) -> Option<String> {
        let taken = self.labels.iter().any(|label| {
            Some(label.id.as_str()) != except && label.name.to_lowercase() == name.to_lowercase()
        });
        taken.then(|| {
            crate::i18n::gettext(self.locale, "A label named “{name}” already exists.")
                .replace("{name}", name)
        })
    }

    /// Resolves an address-book, push, phone-number, or fallback name.
    pub fn display_name(&self, id: &str) -> String {
        self.display_name_or(id, None)
    }

    /// Resolves a consistent display name using settings and an optional
    /// message-provided fallback. Our own id becomes "You".
    /// The short name WhatsApp shows where space is short (a group's member
    /// line, the sender before a group's last message) for `id`, whose full
    /// display name is `name`: the first name saved with the contact, whole,
    /// as it can hold several words. Without one (a profile name, or a
    /// contact the phone has not sent since first names were kept) the first
    /// word of the name; a phone number and our own "You" stay whole.
    pub fn short_name<'a>(&'a self, id: &str, name: &'a str) -> &'a str {
        if self.me.as_deref() == Some(id) || name.starts_with('+') {
            return name;
        }
        if let Some(first) = self.contacts.get(id).and_then(Contact::first_name) {
            return first;
        }
        name.split_whitespace().next().unwrap_or(name)
    }

    pub fn display_name_or(&self, id: &str, hint: Option<&str>) -> String {
        if self.me.as_deref() == Some(id) {
            return "You".to_owned();
        }
        self.person_name(id, hint)
    }

    /// Resolves a mention name without replacing our own name with "You".
    pub fn mention_name(&self, id: &str) -> String {
        if self.me.as_deref() == Some(id) {
            return self
                .me_name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "You".to_owned());
        }
        self.person_name(id, None)
    }

    /// Resolves the chat-list title.
    pub fn chat_title(&self, chat: &Chat) -> String {
        if chat.is_group()
            && (chat.name.trim().is_empty() || (chat.name == "Group" && !chat.group_subject_known))
        {
            let participants = self.participant_names(chat);
            return if participants.is_empty() {
                "Group".to_owned()
            } else {
                participants
            };
        }
        if self.me.as_deref() == Some(chat.id.as_str()) {
            return self.self_title();
        }
        if chat.is_group() {
            return chat.name.clone();
        }
        self.person_name(&chat.id, None)
    }

    /// Our own chat's title, "Name (You)" as on the phone, so searching for
    /// our own name finds it.
    pub fn self_title(&self) -> String {
        match self
            .me_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            Some(name) => crate::i18n::gettext(self.locale, "{name} (You)").replace("{name}", name),
            None => crate::i18n::gettext(self.locale, "You").into_owned(),
        }
    }

    /// Whether a contact search for `needle`, already a search key, should
    /// offer the chat with ourselves: by our name, our number, "You" or
    /// "Message yourself", in English or the interface language.
    pub fn offers_self(&self, needle: &str) -> bool {
        let Some(me) = self.me.as_deref() else {
            return false;
        };
        // A locked chat stays out of every list but the locked one.
        if self.chat(me).is_some_and(|chat| chat.locked) {
            return false;
        }
        if needle.is_empty() {
            return true;
        }
        let yourself = crate::i18n::gettext(self.locale, "Message yourself");
        let you = crate::i18n::gettext(self.locale, "You");
        [
            self.self_title().as_str(),
            yourself.as_ref(),
            you.as_ref(),
            "Message yourself",
        ]
        .iter()
        .any(|text| crate::util::search_key(text).contains(needle))
            || crate::model::phone_of(me).is_some_and(|phone| phone.contains(needle))
    }

    fn person_name(&self, id: &str, hint: Option<&str>) -> String {
        let contact = self.contacts.get(id);
        let present = |name: Option<&str>| name.filter(|name| !name.is_empty()).map(str::to_owned);
        let saved = present(contact.and_then(|contact| contact.full_name.as_deref()));
        let called = present(contact.and_then(|contact| contact.push_name.as_deref()))
            .or_else(|| present(hint));
        // Saved names first, as WhatsApp does; a profile name wears a tilde.
        if let Some(name) = saved.or_else(|| called.map(|name| format!("~{name}"))) {
            return name;
        }
        if let Some(chat) = self.chat(id)
            && !chat.name.is_empty()
            && !chat.name.chars().all(|c| c.is_ascii_digit())
        {
            return chat.name.clone();
        }
        match crate::model::phone_of(id) {
            Some(digits) => crate::util::phone(digits),
            None => "Unknown".to_owned(),
        }
    }

    /// Resolves message mentions for markup.
    pub fn mention_list(&self, message: &Message) -> Vec<crate::markup::Mention> {
        message
            .mentions
            .iter()
            .map(|mention| crate::markup::Mention {
                user: mention.user.clone(),
                name: self.mention_name(&mention.id),
            })
            .collect()
    }

    /// Resolves `@user` tokens in previews without mention metadata.
    pub fn resolve_mention_tokens(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find('@') {
            out.push_str(&rest[..at]);
            out.push('@');
            let after = &rest[at + 1..];
            let digits = after
                .char_indices()
                .find(|(_, c)| !c.is_ascii_digit())
                .map_or(after.len(), |(index, _)| index);
            let id = format!("{}@s.whatsapp.net", &after[..digits]);
            let known = digits >= 5
                && (self.me.as_deref() == Some(id.as_str())
                    || self.contacts.contains_key(&id)
                    || self.chat(&id).is_some());
            if known {
                out.push_str(&self.mention_name(&id));
                rest = &after[digits..];
            } else {
                rest = after;
            }
        }
        out.push_str(rest);
        out
    }

    /// One-line plain-text message summary with resolved mentions.
    pub fn message_text(&self, message: &Message) -> String {
        match &message.content {
            Content::Text { text, .. } | Content::Interactive { text, .. } => {
                crate::markup::plain(text, &self.mention_list(message))
            }
            _ => self.preview_line(&message.summary(), message),
        }
    }

    /// Plain text taken from `message` (a caption summary, a matched line)
    /// with its mentions named as its body names them, including privacy-id
    /// mentions the contact list cannot resolve on its own; any other
    /// `@number` token of a known person is named too.
    pub fn preview_line(&self, text: &str, message: &Message) -> String {
        self.resolve_mention_tokens(&crate::markup::plain(text, &self.mention_list(message)))
    }

    /// Whether a direct chat uses a saved address-book name.
    pub fn is_saved_contact(&self, id: &str) -> bool {
        self.contacts.get(id).is_some_and(|contact| {
            contact
                .full_name
                .as_deref()
                .is_some_and(|name| !name.is_empty())
        })
    }

    /// Group members sorted by name, then phone number, with our id last.
    pub fn participant_list(&self, chat: &Chat) -> Vec<(String, String)> {
        let me = self.me.as_deref();
        let mut named = Vec::new();
        let mut numbers = Vec::new();
        for id in chat
            .participants
            .iter()
            .filter(|id| Some(id.as_str()) != me)
        {
            let name = self.display_name(id);
            if name.starts_with('+') || name == "Unknown" {
                numbers.push((id.clone(), name));
            } else {
                named.push((id.clone(), name));
            }
        }
        named.sort_by_key(|(_, name)| name.trim_start_matches('~').to_lowercase());
        numbers.sort_by(|a, b| a.1.cmp(&b.1));
        named.extend(numbers);
        if let Some(me) = me
            && chat.participants.iter().any(|id| id == me)
        {
            named.push((me.to_owned(), "You".to_owned()));
        }
        named
    }

    /// Group members matching the active composer mention query.
    pub fn mention_candidates(&self, chat: &Chat, query: &str) -> Vec<(String, String)> {
        if !chat.is_group() {
            return Vec::new();
        }
        let needle = query.trim().to_lowercase();
        let digits: String = query.chars().filter(char::is_ascii_digit).collect();
        self.participant_list(chat)
            .into_iter()
            .filter(|(id, name)| {
                if self.me.as_deref() == Some(id) {
                    return false;
                }
                if needle.is_empty() {
                    return true;
                }
                name.trim_start_matches('~')
                    .to_lowercase()
                    .contains(&needle)
                    || (!digits.is_empty()
                        && id
                            .split('@')
                            .next()
                            .is_some_and(|user| user.contains(&digits)))
            })
            .collect()
    }

    pub fn participant_names(&self, chat: &Chat) -> String {
        let me = self.me.as_deref();
        let mut names = Vec::new();
        let mut numbers = Vec::new();
        let mut seen = HashSet::new();
        for id in chat
            .participants
            .iter()
            .filter(|id| Some(id.as_str()) != me && seen.insert(id.as_str()))
        {
            let name = self.display_name(id);
            if name.starts_with('+') || name == "Unknown" {
                numbers.push(name);
            } else {
                names.push(self.short_name(id, name.trim_start_matches('~')).to_owned());
            }
        }
        names.sort_by_key(|name| name.to_lowercase());
        let mut counted = Vec::new();
        let mut iter = names.into_iter().peekable();
        while let Some(name) = iter.next() {
            let mut count = 1;
            while iter
                .peek()
                .is_some_and(|next| next.to_lowercase() == name.to_lowercase())
            {
                iter.next();
                count += 1;
            }
            counted.push(if count == 1 {
                name
            } else {
                format!("{name} x{count}")
            });
        }
        numbers.sort();
        counted.extend(numbers);
        if chat.participants.iter().any(|id| Some(id.as_str()) == me) {
            counted.push("You".to_owned());
        }
        counted.join(", ")
    }

    /// Whether the typed search text is the secret code that reveals the
    /// locked-chats folder.
    pub fn secret_code_matched(&self) -> bool {
        // Verifying runs a slow KDF, and this is read every frame, so the
        // answer is kept until the typed text or the stored verifier changes.
        let code = self.search.trim();
        let stored = &self.settings.chat_lock_code_hash;
        let mut cached = self.chat_lock_check.borrow_mut();
        if let Some((checked, against, matched)) = cached.as_ref()
            && checked == code
            && against == stored
        {
            return *matched;
        }
        let matched = self.settings.verifies_chat_lock_code(code);
        *cached = Some((code.to_owned(), stored.clone(), matched));
        matched
    }

    fn chat_lock_authenticated(&self) -> bool {
        self.chat_lock_session.is_some()
            && self.chat_lock_session == self.settings.chat_lock_code_hash
    }

    /// Search-code entry is retained for compatibility; the Locked tab uses
    /// a window-scoped session so the search field remains useful.
    pub fn locked_folder_open(&self) -> bool {
        self.locked_folder && (self.chat_lock_authenticated() || self.secret_code_matched())
    }

    pub fn locked_count(&self) -> usize {
        self.chats.iter().filter(|chat| chat.locked).count()
    }

    pub fn should_show_chat_lock_hint(&self) -> bool {
        self.locked_count() > 0
            && self.settings.chat_lock_code_hash.is_none()
            && !self.settings.chat_lock_hint_dismissed
    }

    /// Visible chats filtered by search, archive state, and the chat filter,
    /// with drafts first, then pinned chats.
    /// Locked chats only appear inside the locked folder.
    pub fn focus_allows_chat(&self, chat: &Chat) -> bool {
        !self.account().settings.focus_enabled
            || self
                .account()
                .settings
                .focus_labels
                .iter()
                .any(|id| self.chat_wears(chat, id))
    }

    fn chat_label_sound(&self, chat: &Chat) -> Option<NotificationSound> {
        // Label manager order gives overlapping labels a stable precedence.
        self.labels.iter().find_map(|label| {
            self.chat_wears(chat, &label.id)
                .then(|| self.account().settings.label_sounds.get(&label.id))
                .flatten()
                .cloned()
        })
    }

    pub fn visible_chats(&self) -> Vec<&Chat> {
        let needle = crate::util::search_key(self.search.trim());
        let locked = self.locked_folder_open();
        let filtering = !locked && needle.is_empty() && !self.show_archived;
        let mut chats: Vec<&Chat> = self
            .chats
            .iter()
            .filter(|chat| chat.locked == locked)
            .filter(|chat| self.focus_allows_chat(chat))
            .filter(|chat| locked || chat.archived == self.show_archived || !needle.is_empty())
            .filter(|chat| match &self.label_filter {
                // A label lists every chat wearing it, channels included,
                // because someone put each of them there.
                _ if !filtering => true,
                Some(label) => self.chat_wears(chat, label),
                None => {
                    self.chat_filter.matches(chat)
                        || (self.chat_filter == ChatFilter::Unread
                            && self.unread_kept.contains(&chat.id))
                }
            })
            .filter(|chat| {
                // Inside the locked folder the typed text is the secret code,
                // not a query to match.
                (self.locked_folder && !self.chat_lock_authenticated())
                    || needle.is_empty()
                    || crate::util::search_key(&self.chat_title(chat)).contains(&needle)
                    || chat.phone().is_some_and(|phone| phone.contains(&needle))
                    || chat.last.as_ref().is_some_and(|last| {
                        crate::util::search_key(&last.summary).contains(&needle)
                    })
            })
            .collect();
        // Within each draft group, preserve pin and phone Favorites order.
        let favorites_order = self.favorites_order();
        chats.sort_by(|a, b| {
            self.has_draft(&b.id)
                .cmp(&self.has_draft(&a.id))
                .then_with(|| b.pinned.cmp(&a.pinned))
                .then_with(|| {
                    if a.pinned && b.pinned {
                        b.pinned_at.cmp(&a.pinned_at).then(a.id.cmp(&b.id))
                    } else if favorites_order {
                        a.favorite_position
                            .cmp(&b.favorite_position)
                            .then(a.id.cmp(&b.id))
                    } else {
                        b.last_activity.cmp(&a.last_activity).then(a.id.cmp(&b.id))
                    }
                })
        });
        chats
    }

    /// Whether the chat list keeps the phone's favorites order instead of the
    /// latest activity, so a sent message does not move its chat up.
    fn favorites_order(&self) -> bool {
        self.label_filter.is_none()
            && self.chat_filter == ChatFilter::Favorites
            && !self.show_archived
            && crate::util::search_key(self.search.trim()).is_empty()
            && !self.locked_folder_open()
    }

    /// Matching individual contacts without an existing chat, sorted by name.
    pub fn matching_contacts(&self) -> Vec<&Contact> {
        let needle = crate::util::search_key(self.search.trim());
        if needle.is_empty() {
            return Vec::new();
        }
        let mut contacts: Vec<&Contact> = self
            .contacts
            .values()
            .filter(|contact| crate::model::phone_of(&contact.id).is_some())
            .filter(|contact| self.me.as_deref() != Some(contact.id.as_str()))
            .filter(|contact| !self.chats.iter().any(|chat| chat.id == contact.id))
            .filter(|contact| {
                contact
                    .display_name()
                    .is_some_and(|name| crate::util::search_key(name).contains(&needle))
                    || contact
                        .id
                        .split('@')
                        .next()
                        .is_some_and(|phone| phone.contains(&needle))
            })
            .collect();
        contacts
            .sort_by_key(|contact| contact.display_name().unwrap_or(&contact.id).to_lowercase());
        contacts.truncate(15);
        contacts
    }

    /// Archived chats with unread messages, for the Archived chip.
    pub fn archived_unread(&self) -> usize {
        self.chats
            .iter()
            .filter(|chat| chat.archived && !chat.locked && chat.unread > 0)
            .count()
    }

    pub fn archived_count(&self) -> usize {
        self.chats
            .iter()
            .filter(|chat| chat.archived && !chat.locked)
            .count()
    }

    /// Unarchived chats with unread messages that a filter would list.
    pub fn unread_chats(&self, filter: ChatFilter) -> usize {
        self.chats
            .iter()
            .filter(|chat| {
                !chat.archived && !chat.locked && chat.looks_unread() && filter.matches(chat)
            })
            .count()
    }

    /// The taskbar count: unarchived, unmuted, unlocked chats that look
    /// unread. WhatsApp counts chats here, not the messages inside them.
    pub fn unread_chat_count(&self) -> u32 {
        self.account().unread_chat_count()
    }

    /// Returns or requests a cached profile picture.
    fn cached_avatar(&self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self
            .account()
            .local_avatars
            .get(&crate::local_avatars::key(id))
        {
            return Some(path.clone());
        }
        let path = self.account().dirs.avatar_file(id, false);
        path.metadata()
            .ok()
            .filter(|metadata| metadata.len() > 0)
            .map(|_| path)
    }

    /// Registers an existing profile picture, used by demo data.
    pub fn adopt_avatar(&mut self, id: &str, path: PathBuf) {
        self.avatars.insert(id.to_owned(), Some(path));
    }

    pub fn avatar(&mut self, id: &str) -> Option<PathBuf> {
        self.account_mut().avatar(id)
    }

    /// Returns or requests a full-size profile picture.
    pub fn avatar_full(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self
            .account()
            .local_avatars
            .get(&crate::local_avatars::key(id))
        {
            return Some(path.clone());
        }
        if let Some(known) = self.avatars_full.get(id) {
            return known.clone();
        }
        if self.avatar_full_requests.insert(id.to_owned()) {
            self.backend.send(Command::FetchAvatar {
                id: id.to_owned(),
                full: true,
            });
        }
        None
    }

    /// Whether an outgoing message is still editable.
    pub fn can_edit(&self, message: &Message) -> bool {
        message.editable_at(crate::util::now())
    }

    /// Whether an outgoing message can still be revoked for everyone.
    pub fn can_revoke(&self, message: &Message) -> bool {
        message.from_me
            && !matches!(message.content, Content::Revoked)
            && crate::util::now() - message.timestamp <= REVOKE_WINDOW.as_secs() as i64
    }

    /// The open composer's current contents take precedence over its saved draft.
    fn draft_contents(&self, chat: &str) -> (&str, &[Pending]) {
        if self.open_chat.as_deref() == Some(chat) {
            let text = if self.editing.is_none() {
                self.composer.as_str()
            } else {
                ""
            };
            (text, &self.pending)
        } else {
            (
                self.drafts.get(chat).map_or("", String::as_str),
                self.attachment_drafts
                    .get(chat)
                    .map_or(&[], |draft| draft.pending.as_slice()),
            )
        }
    }

    /// Unsent text or attachments make a draft; editing a sent message does not.
    pub fn has_draft(&self, chat: &str) -> bool {
        let (text, pending) = self.draft_contents(chat);
        !text.trim().is_empty() || !pending.is_empty()
    }

    /// A chat's unsent text or first attachment on one line for its list row.
    /// Active text stays in the composer; attachment drafts also show in the row.
    pub fn draft_preview(&self, chat: &str) -> Option<String> {
        let (text, pending) = self.draft_contents(chat);
        if self.open_chat.as_deref() == Some(chat) && pending.is_empty() {
            return None;
        }
        let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            return Some(line);
        }
        pending.first().map(|attachment| match attachment.media() {
            Pending::File(path) => path.file_name().map_or_else(
                || "📎".to_owned(),
                |name| {
                    name.to_string_lossy()
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                },
            ),
            Pending::Picture { .. } => crate::i18n::gettext(self.locale, "Image").into_owned(),
            Pending::Document(_) | Pending::Recovered { .. } => {
                unreachable!("media unwraps documents")
            }
        })
    }

    /// Active typers in a chat as id and display name.
    pub fn typing_in(&self, chat: &str) -> Vec<(String, String)> {
        self.typing
            .get(chat)
            .map(|typers| {
                typers
                    .iter()
                    .map(|(sender, _)| (sender.clone(), self.display_name(sender)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Applies a changed row. A requeued one is a send refused again: it
    /// goes first among the waiting rows it ties with.
    fn message_updated(&mut self, message: Message, requeued: bool) {
        for entry in &mut self.starred {
            if entry.message.chat == message.chat && entry.message.id == message.id {
                entry.message = message.clone();
            }
        }
        if matches!(message.content, Content::Revoked) {
            if let Some(ids) = self.stars.get_mut(&message.chat) {
                ids.remove(&message.id);
            }
            if let Some(ids) = self.pins.get_mut(&message.chat) {
                ids.remove(&message.id);
            }
            if let Some(rows) = self.chat_pins.get_mut(&message.chat) {
                rows.retain(|row| row.id != message.id);
            }
            self.starred.retain(|entry| {
                entry.message.chat != message.chat || entry.message.id != message.id
            });
        }
        if let Some(listing) = self
            .gallery
            .listing
            .as_mut()
            .filter(|listing| listing.chat == message.chat)
        {
            for rows in [&mut listing.media, &mut listing.docs] {
                if let Some(existing) = rows.iter_mut().find(|row| row.id == message.id) {
                    *existing = message.clone();
                }
                rows.retain(|row| row.content.listable());
            }
            if listing.links.iter().any(|link| link.message == message.id) {
                listing.links.retain(|link| link.message != message.id);
                let updated = crate::model::ChatMedia::collect(
                    message.chat.clone(),
                    vec![message.clone()],
                    crate::model::MEDIA_LIST_LIMIT,
                );
                listing.links.extend(updated.links);
                listing
                    .links
                    .sort_by_key(|link| std::cmp::Reverse(link.timestamp));
                listing.links_truncated |= listing.links.len() > crate::model::MEDIA_LIST_LIMIT;
                listing.links.truncate(crate::model::MEDIA_LIST_LIMIT);
            }
        }
        if !matches!(
            message.content,
            Content::Image { .. } | Content::Video { .. }
        ) && !self.events_hidden
            && self.image_preview.as_ref().is_some_and(|p| {
                p.chat() == Some(message.chat.as_str()) && p.message() == Some(message.id.as_str())
            })
        {
            self.image_preview = None;
            self.video.stop();
        }
        let id = message.id.clone();
        let keep_transcript = matches!(
            message.content,
            Content::Audio {
                voice_note: true,
                ..
            }
        );
        if let Some(conversation) = self.conversations.get_mut(&message.chat)
            && let Some(existing) = conversation.message_mut(&message.id)
        {
            let moved = (requeued
                || existing.timestamp != message.timestamp
                || existing.waiting_to_send() != message.waiting_to_send())
            .then(|| message.id.clone());
            let state = existing.content.media().map(|media| media.state.clone());
            let motion_state = match &existing.content {
                Content::Image {
                    motion: Some(motion),
                    ..
                } => Some(motion.state.clone()),
                _ => None,
            };
            let carousel_states = match &existing.content {
                Content::Interactive {
                    card: Some(card), ..
                } => card
                    .carousel
                    .iter()
                    .map(|card| card.image.as_ref().map(|media| media.state.clone()))
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            };
            *existing = message;
            for (index, state) in carousel_states.into_iter().enumerate() {
                if let (Some(state), Some(media)) =
                    (state, existing.content.media_at_mut(Some(index)))
                {
                    media.state = state;
                }
            }
            if let (Some(state), Some(media)) = (state, existing.content.media_mut()) {
                media.state = state;
            }
            if let (
                Some(state),
                Content::Image {
                    motion: Some(motion),
                    ..
                },
            ) = (motion_state, &mut existing.content)
            {
                motion.state = state;
            }
            if !keep_transcript {
                conversation.transcripts.remove(&id);
            }
            if let Some(id) = moved {
                conversation.reposition(&id, requeued);
            }
        }
    }

    /// Drains backend events into interface state and queues follow-up actions.
    fn handle_events(&mut self) {
        self.poll_archive_restore();
        let mut removed = Vec::new();
        let mut deferred = Vec::new();
        let real_active = self.active;
        for index in 0..self.accounts.len() {
            let events = self.accounts[index].backend.poll();
            if events.is_empty() {
                continue;
            }
            let origin = self.accounts[index].id.clone();
            let live = index == real_active;
            let pending_before = self.actions.len();
            self.active = index;
            for event in events {
                if matches!(event, Event::AccountRemoved) {
                    removed.push(index);
                    continue;
                }
                self.events_hidden = !live;
                self.apply_backend_event(event, live);
                self.events_hidden = false;
            }
            let queued: Vec<_> = self.actions.drain(pending_before..).collect();
            deferred.extend(queued.into_iter().map(|action| (origin.clone(), action)));
            self.active = real_active;
        }
        self.deferred_account_actions.extend(deferred);
        for index in removed.into_iter().rev() {
            self.finish_removed(index);
        }
    }
    /// Applies an event to its owning account while protecting other accounts' visible state.
    fn apply_backend_event(&mut self, event: Event, live: bool) {
        self.apply_backend_event_inner(event, live);
        self.sync_automation_authority();
    }

    fn apply_backend_event_inner(&mut self, event: Event, live: bool) {
        match event {
            Event::Call(update) => {
                let fresh_incoming = update.phase == crate::calls::CallPhase::Incoming
                    && self
                        .call
                        .as_ref()
                        .is_none_or(|old| old.generation != update.generation);
                let chat = update.chat.clone();
                let finished = !update.phase.is_live();
                self.call = Some(*update);
                if finished {
                    self.backend.send(Command::LoadCallLog);
                }
                if fresh_incoming
                    && self.account().settings.notifications
                    && self
                        .chat(&chat)
                        .is_some_and(|known| self.focus_allows_chat(known))
                    && !self.app_lock.is_locked()
                {
                    let title = if self.account().settings.notification_sender {
                        self.chat(&chat)
                            .map(|chat| self.chat_title(chat))
                            .unwrap_or_else(|| "Zap Faster".into())
                    } else {
                        "Zap Faster".into()
                    };
                    let account = self.account().id.clone();
                    let waker = self.waker.clone();
                    self.notifications.show_on(
                        title,
                        "Incoming voice call".into(),
                        None,
                        crate::settings::NotificationSound::System,
                        self.account().settings.call_speaker.clone(),
                        crate::notify::NotificationTarget {
                            account,
                            chat,
                            message: String::new(),
                        },
                        self.notification_opens.clone(),
                        move || waker.wake(),
                        false,
                    );
                }
            }
            Event::CallDismissed => self.call = None,
            Event::CallPreparing(preparing) => {
                self.call_preparing = preparing
                    && !self
                        .pending_call_request
                        .as_ref()
                        .is_some_and(|request| request.is_cancelled());
                if !preparing {
                    self.pending_call_request = None;
                }
            }
            Event::CallDevices(devices) => self.call_devices = *devices,
            Event::CallLog(records) => self.call_records = records,
            Event::Statuses(rows) => self.statuses = rows,

            Event::Link(status) => {
                if live && status != self.link {
                    let text = match &status {
                        LinkStatus::Connected => "Connected to WhatsApp",
                        LinkStatus::Disconnected { .. } => "WhatsApp connection lost, reconnecting",
                        LinkStatus::LoggedOut => "WhatsApp device unlinked",
                        LinkStatus::Failed(_) => "Could not connect to WhatsApp",
                        LinkStatus::Connecting | LinkStatus::Starting => "Connecting to WhatsApp",
                        LinkStatus::Unlinked { .. } => "Waiting for device linking",
                    };
                    self.announce_accessibility(text);
                }
                self.handle_link(status, live);
            }
            Event::Me {
                id,
                lid,
                name,
                about,
            } => {
                self.me = Some(id);
                self.me_lid = lid;
                self.me_name = name;
                self.me_about = about;
            }
            Event::Labels(labels) => {
                self.labels = labels;
                self.prune_labels();
            }
            Event::Drafts(drafts) => {
                // Unsent text stored by an earlier session. Text typed in
                // this session wins over the stored copy.
                for (chat, text) in drafts {
                    // The chat reopened at startup shows its draft at once.
                    if live
                        && self.open_chat.as_deref() == Some(chat.as_str())
                        && self.editing.is_none()
                        && self.composer.is_empty()
                    {
                        self.replace_composer_text(text);
                    } else {
                        self.drafts.entry(chat).or_insert(text);
                    }
                }
            }
            Event::Stars { chat, ids } => {
                self.stars.insert(chat, ids.into_iter().collect());
            }
            Event::StarChanged {
                chat,
                message,
                starred,
            } => {
                let ids = self.stars.entry(chat).or_default();
                if starred {
                    ids.insert(message);
                } else {
                    ids.remove(&message);
                }
                // The list shows a star as soon as WhatsApp confirmed it,
                // so a refused one never appears there.

                // The open list would otherwise show the old state.
                if self.show_starred {
                    self.backend.send(Command::LoadStarred { offset: 0 });
                }
            }
            Event::StarredList { offset, list, more } => {
                self.starred_more = more;
                if offset == 0 {
                    self.starred = list;
                } else {
                    for entry in list {
                        if !self.starred.iter().any(|known| {
                            known.message.chat == entry.message.chat
                                && known.message.id == entry.message.id
                        }) {
                            self.starred.push(entry);
                        }
                    }
                }
            }
            Event::Chats(chats) => {
                self.chats_loaded = true;
                for chat in &chats {
                    if chat.unread == 0 {
                        self.clear_chat_notifications(&chat.id);
                    }
                }
                self.chats = chats;
                if !self.events_hidden
                    && self
                        .image_preview
                        .as_ref()
                        .and_then(PreviewState::chat)
                        .is_some_and(|id| {
                            self.chat(id)
                                .is_none_or(|chat| chat.locked && !self.locked_folder_open())
                        })
                {
                    self.image_preview = None;
                }
                if let Some(open) = self.open_chat.clone() {
                    if self.chat(&open).is_none_or(|chat| chat.locked) {
                        self.open_chat = None;
                    } else {
                        // Show archived messages immediately, including offline.
                        self.ensure_loaded(&open);
                    }
                }
            }
            Event::ChatUpdated(chat) => self.handle_chat_updated(*chat, live),
            Event::Pins {
                chat,
                items,
                notices,
            } => {
                // A pin carries the message's own words, so a locked chat
                // keeps them off the window until its folder is open, the
                // way its messages do.
                if self.locked_folder_open() || !self.chat(&chat).is_some_and(|known| known.locked)
                {
                    self.pins.insert(
                        chat.clone(),
                        items.iter().map(|item| item.id.clone()).collect(),
                    );
                    self.pin_notices.insert(chat.clone(), notices);
                    self.chat_pins.insert(chat, items);
                }
            }
            Event::PinChanged {
                chat,
                message,
                pinned,
            } => {
                let ids = self.pins.entry(chat.clone()).or_default();
                if pinned {
                    ids.insert(message);
                } else {
                    ids.remove(&message);
                }
            }
            Event::Messages {
                chat,
                messages,
                older,
                complete,
            } => {
                let conversation = self.conversations.entry(chat.clone()).or_default();
                let was_empty = conversation.messages.is_empty();
                if older && !messages.is_empty() {
                    conversation.phone_delivered = true;
                }
                conversation.merge(messages, older);
                if older {
                    conversation.loading_older = false;
                    conversation.complete = complete;
                } else if was_empty {
                    conversation.complete = complete;
                }
                // Request phone history when sync created a chat without messages.
                let bare = !older && complete && conversation.messages.is_empty();
                if live && self.open_chat.as_deref() == Some(chat.as_str()) {
                    if !older && (self.at_bottom || was_empty) {
                        self.scroll_to_bottom = true;
                    }
                    if bare {
                        self.fetch_older(&chat, false);
                    }
                    // After the first page, load toward a pending search anchor once.
                    if !older && let Some(anchor) = self.scroll_anchor.clone() {
                        let load = self.conversations.get_mut(&chat).and_then(|conversation| {
                            if conversation.message(&anchor).is_none()
                                && !conversation.loading_older
                            {
                                conversation.messages.first().map(|oldest| {
                                    conversation.loading_older = true;
                                    (oldest.timestamp, oldest.id.clone())
                                })
                            } else {
                                None
                            }
                        });
                        if let Some(before) = load {
                            self.backend.send(Command::LoadUntil {
                                chat,
                                id: anchor,
                                before,
                            });
                        }
                    }
                }
            }
            Event::ChatHits {
                chat,
                query,
                from,
                until,
                messages,
                truncated,
            } => {
                // Hits for another chat, for a query the user has already
                // replaced, or for a day they have already moved off,
                // arrive too late to matter.
                let (want_from, want_until) = self.chat_search_range();
                if live
                    && self.open_chat.as_deref() == Some(chat.as_str())
                    && query == self.chat_search.trim()
                    && from == want_from
                    && until == want_until
                {
                    self.chat_search_hits = messages;
                    self.chat_search_truncated = truncated;
                    self.chat_search_pending = false;
                    self.chat_search_selected = None;
                }
            }
            Event::SearchHits { query, messages } => {
                if query == self.search.trim() {
                    // Locked chats' messages stay out of plain search.
                    self.search_hits = messages
                        .into_iter()
                        .filter(|message| self.chat(&message.chat).is_none_or(|chat| !chat.locked))
                        .collect();
                }
            }
            Event::StorageStats {
                stats,
                counted,
                token,
            } => {
                // An answer to an older ask would put its numbers over
                // the newer ones and date them now, so it is dropped.
                if token == self.storage_stats_token {
                    // A failed read answers nothing and leaves the last
                    // good numbers, or the blank row, in place. The
                    // attempt still counts as one, so a broken archive is
                    // retried on the same cadence instead of on every
                    // frame.
                    if let Some(mut stats) = stats {
                        if counted {
                            self.storage_stats_counted = true;
                        } else {
                            // A sizes-only refresh carries a zero message
                            // total. Keep the total from the counted read.
                            stats.messages = self
                                .storage_stats
                                .map_or(stats.messages, |last| last.messages);
                        }
                        self.storage_stats = Some(stats);
                    }
                    self.storage_stats_at = Some(Instant::now());
                    self.storage_stats_asked = false;
                }
            }
            Event::Incoming { chat, message } => {
                if live && !message.from_me && self.open_chat.as_deref() == Some(chat.as_str()) {
                    self.announce_accessibility("New message in the open conversation");
                }
                self.maybe_notify(&chat, &message);
            }
            Event::Picked { chat, paths } => {
                if live && self.open_chat.as_deref() == Some(chat.as_str()) {
                    self.stage_files(paths);
                }
            }
            Event::InteractiveReplyState {
                chat,
                message,
                pending,
            } => {
                if pending {
                    self.interactive_sending.insert((chat, message));
                } else {
                    self.interactive_sending.remove(&(chat, message));
                }
            }
            Event::PollCreated { chat, error } => {
                self.poll_creating = false;
                if let Some(error) = error {
                    self.toast_error(error);
                } else if live && self.dialog == Some(Dialog::CreatePoll(chat)) {
                    self.dialog = None;
                    self.poll_draft = Default::default();
                }
            }
            Event::PollVoted {
                chat,
                message,
                error,
            } => {
                self.poll_voting.remove(&(chat, message));
                if let Some(error) = error {
                    self.toast_error(error);
                }
            }
            Event::MessageUpdated(message) => self.message_updated(*message, false),
            Event::MessageRequeued(message) => self.message_updated(*message, true),
            Event::Contacts(contacts) => {
                for contact in contacts {
                    self.contacts.insert(contact.id.clone(), contact);
                }
            }
            Event::Typing {
                chat,
                sender,
                composing,
            } => {
                if live
                    && composing
                    && self.open_chat.as_deref() == Some(chat.as_str())
                    && !self
                        .typing
                        .get(&chat)
                        .is_some_and(|typers| typers.iter().any(|(who, _)| *who == sender))
                {
                    self.announce_accessibility("Someone is typing in the open conversation");
                }
                let typers = self.typing.entry(chat).or_default();
                typers.retain(|(who, _)| *who != sender);
                if composing {
                    typers.push((sender, Instant::now()));
                }
            }
            Event::Presence {
                id,
                online,
                last_seen,
            } => {
                self.presence.insert(id, Presence { online, last_seen });
            }
            Event::Avatar { id, full, path } => {
                if full {
                    self.avatar_full_requests.remove(&id);
                    self.avatars_full.insert(id, path);
                } else {
                    self.avatar_requests.remove(&id);
                    self.avatars.insert(id, path);
                }
            }
            Event::Gifs { query, results } => {
                if query == self.gif_query {
                    self.gif_pending = false;
                    match results {
                        Ok(results) => {
                            self.gif_results = results;
                            self.gif_error = None;
                        }
                        Err(error) => {
                            self.gif_results.clear();
                            self.gif_error = Some(error);
                        }
                    }
                }
            }
            Event::Stickers {
                favorites,
                packs,
                recent,
                received,
                emojis,
            } => {
                self.stickers_saved = favorites;
                self.sticker_packs = packs;
                self.stickers = recent;
                self.stickers_received = received;
                self.stickers_pending = false;
                self.sticker_import_pending = false;
                // The picker belongs to the account on screen.
                if !live {
                    return;
                }
                self.sticker_emojis = emojis;
                // Show a pack made here as soon as it exists. Packs list
                // newest first, so the first match is the new one.
                if let Some(name) = self.sticker_pack_created.take() {
                    match self
                        .sticker_packs
                        .iter()
                        .find(|pack| pack.local && pack.name == name)
                    {
                        Some(pack) => self.sticker_shelf = StickerShelf::Pack(pack.dir.clone()),
                        None => self.sticker_pack_created = Some(name),
                    }
                }
                // A pack deleted on another surface cannot stay selected.
                if matches!(self.sticker_shelf, StickerShelf::Pack(_))
                    && self.selected_pack().is_none()
                {
                    self.sticker_shelf = StickerShelf::Recent;
                }
            }
            Event::MessageDeleted { chat, id } => {
                if let Some(ids) = self.stars.get_mut(&chat) {
                    ids.remove(&id);
                }
                if let Some(ids) = self.pins.get_mut(&chat) {
                    ids.remove(&id);
                }
                if let Some(rows) = self.pin_notices.get_mut(&chat) {
                    rows.retain(|row| row.id != id);
                }
                if let Some(rows) = self.chat_pins.get_mut(&chat) {
                    rows.retain(|row| row.id != id);
                }
                self.starred
                    .retain(|entry| entry.message.chat != chat || entry.message.id != id);
                if let Some(listing) = self
                    .gallery
                    .listing
                    .as_mut()
                    .filter(|listing| listing.chat == chat)
                {
                    listing.media.retain(|row| row.id != id);
                    listing.docs.retain(|row| row.id != id);
                    listing.links.retain(|row| row.message != id);
                }
                if live
                    && self.image_preview.as_ref().is_some_and(|p| {
                        p.chat() == Some(chat.as_str()) && p.message() == Some(id.as_str())
                    })
                {
                    self.image_preview = None;
                    self.video.stop();
                }
                if live
                    && self.sticker_view.as_ref().is_some_and(|preview| {
                        preview.chat() == Some(chat.as_str())
                            && preview.message() == Some(id.as_str())
                    })
                {
                    self.sticker_view = None;
                }
                if let Some(conversation) = self.conversations.get_mut(&chat) {
                    conversation.messages.retain(|message| message.id != id);
                    conversation.transcripts.remove(&id);
                }
                if live && self.editing.as_deref() == Some(id.as_str()) {
                    self.editing = None;
                    self.clear_composer_text();
                }
            }
            Event::ChatRemoved { chat } => self.forget_chat(&chat),
            Event::ChatCleared { chat, through } => {
                self.stars.remove(&chat);
                self.pins.remove(&chat);
                self.chat_pins.remove(&chat);
                self.pin_notices.remove(&chat);
                self.starred.retain(|entry| {
                    entry.message.chat != chat || entry.message.timestamp > through
                });
                self.handle_chat_cleared(&chat, through, live);
            }
            Event::UploadProgress {
                token,
                chat,
                bytes,
                total,
            } => {
                self.uploads.insert(token, (chat, bytes, total));
            }
            Event::UploadFinished(token) => {
                self.uploads.remove(&token);
            }
            Event::TransferProgress {
                chat,
                message,
                card,
                bytes,
                total,
            } => {
                if card.is_none()
                    && let Some(listing) = self
                        .gallery
                        .listing
                        .as_mut()
                        .filter(|listing| listing.chat == chat)
                    && let Some(row) = listing
                        .media
                        .iter_mut()
                        .chain(&mut listing.docs)
                        .find(|row| row.id == message)
                    && let Some(media) = row.content.media_at_mut(None)
                {
                    media.state = MediaState::Transferring { bytes, total };
                }
                if let Some(media) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|c| c.message_mut(&message))
                    .and_then(|m| m.content.media_at_mut(card))
                    && matches!(
                        media.state,
                        MediaState::Downloading | MediaState::Transferring { .. }
                    )
                {
                    media.state = MediaState::Transferring { bytes, total };
                }
            }
            Event::Gallery {
                chat,
                token,
                listing,
            } => {
                if self.gallery.chat.as_deref() == Some(chat.as_str())
                    && self.gallery.token == token
                {
                    self.gallery.pending = false;
                    if self.gallery_accessible(&chat) {
                        self.gallery.failed = listing.is_none();
                        self.gallery.listing = listing;
                    } else {
                        self.gallery.listing = None;
                    }
                }
            }
            Event::Media {
                card,
                chat,
                message,
                result,
            } => self.handle_media(&chat, &message, card, result),
            Event::Transcripts { chat, transcripts } => {
                let conversation = self.conversations.entry(chat).or_default();
                conversation.transcripts.extend(transcripts);
            }
            Event::MotionTransferProgress {
                chat,
                message,
                bytes,
                total,
            } => {
                if let Some(motion) = self.motion_mut(&chat, &message) {
                    motion.state = MediaState::Transferring { bytes, total };
                }
            }
            Event::Motion {
                chat,
                message,
                result,
            } => {
                // The preview asked for the clip to play it.
                let previewed = !self.events_hidden
                    && self.open_chat.as_deref() == Some(chat.as_str())
                    && self.image_preview.as_ref().is_some_and(|preview| {
                        self.conversations
                            .get(&chat)
                            .and_then(|conversation| conversation.message(&message))
                            .and_then(|message| message.content.media())
                            .and_then(|media| media.path.as_deref())
                            == Some(preview.path())
                    });
                if let Some(motion) = self.motion_mut(&chat, &message) {
                    match result {
                        Ok(path) => {
                            motion.path = Some(path.clone());
                            motion.state = MediaState::Idle;
                            if previewed {
                                self.actions.push(Action::CloseImagePreview);
                                self.actions.push(Action::ExpandVideo { message, path });
                            }
                        }
                        Err(error) => {
                            motion.state = MediaState::Failed(error);
                            if self
                                .motion_playing
                                .as_ref()
                                .is_some_and(|(c, m)| *c == chat && *m == message)
                            {
                                self.motion_playing = None;
                            }
                        }
                    }
                }
            }
            Event::Syncing(syncing) => {
                if self.syncing && !syncing {
                    self.toast("History loaded");
                }
                self.syncing = syncing;
                if !syncing {
                    self.sync_percent = None;
                }
            }
            Event::SyncProgress(percent) => self.sync_percent = Some(percent),
            Event::OlderFetched { chat, more } => {
                let conversation = self.conversations.entry(chat).or_default();
                conversation.fetching_phone = false;
                conversation.phone_explicit = false;
                conversation.phone_exhausted = !more;
                conversation.phone_answered = Some(Instant::now());
                if conversation.phone_delivered {
                    conversation.phone_misses = 0;
                } else {
                    conversation.phone_misses = (conversation.phone_misses + 1).min(7);
                }
                conversation.phone_delivered = false;
                // Page the archive again after phone history arrives.
                conversation.complete = false;
            }
            Event::ReceiptsPrivacy { disabled } => self.account_receipts_off = disabled,
            Event::AccountPrivacy { values, failed } => {
                self.account_privacy.apply_fetch(values, failed);
                // The account value wins over the local switch: it is what
                // the phone and the other linked devices enforce.
                if let Some(choice) = self
                    .account_privacy
                    .get(crate::privacy::PrivacyKind::ReadReceipts)
                {
                    self.account_receipts_off = choice != crate::privacy::PrivacyChoice::Everyone;
                }
            }
            Event::AccountPrivacySaved { kind } => {
                // A confirmation nothing waits for belongs to an account
                // that has since been unlinked.
                if self.account_privacy.finish_set(kind)
                    && kind == crate::privacy::PrivacyKind::ReadReceipts
                {
                    self.account_receipts_off = self.account_privacy.get(kind)
                        != Some(crate::privacy::PrivacyChoice::Everyone);
                }
            }
            Event::AccountPrivacyFailed { kind } => self.account_privacy.fail_set(kind),
            Event::PinLimit(limit) => self.pin_limit = limit,
            Event::Receipts(receipts) => {
                // A late answer for a dialog that has since closed is stale.
                if live
                    && self.receipts_watch.as_ref().is_some_and(|(chat, message)| {
                        *chat == receipts.chat && *message == receipts.message
                    })
                {
                    self.message_receipts = Some(receipts);
                }
            }
            Event::ChatSoundPicked { chat, path } => {
                if live {
                    crate::notify::play_sound_on(
                        crate::settings::NotificationSound::Custom(path.clone()),
                        self.account().settings.call_speaker.clone(),
                    );
                }
                self.actions.push(Action::SetChatSound {
                    chat,
                    sound: Some(crate::settings::NotificationSound::Custom(path)),
                });
            }
            Event::DownloadFolderPicked(path) => {
                if live {
                    self.actions.push(Action::SetDownloadFolder(Some(path)));
                }
            }
            Event::LocalAvatarChanged { id, result } => match result {
                Ok(Some(path)) => {
                    self.account_mut()
                        .local_avatars
                        .insert(crate::local_avatars::key(&id), path);
                }
                Ok(None) => {
                    self.account_mut()
                        .local_avatars
                        .remove(&crate::local_avatars::key(&id));
                }
                Err(error) if live => self.toast_error(error),
                Err(_) => log::warn!("could not change a hidden account's local contact picture"),
            },
            Event::WallpaperImagePicked(Ok(path)) => {
                // The copy may keep the earlier one's name: decode it anew.
                self.account_mut().settings.wallpaper_image = Some(path);
                self.account_mut().mark_settings_dirty();
                if live {
                    self.wallpaper_image.reload();
                }
            }
            Event::WallpaperImagePicked(Err(error)) => {
                let message = crate::i18n::gettext(self.locale, "Could not use this image");
                self.toast_error(format!("{message}: {error}"));
            }
            Event::NotificationSoundPicked { mention, path } => {
                if live {
                    crate::notify::play_sound_on(
                        crate::settings::NotificationSound::Custom(path.clone()),
                        self.account().settings.call_speaker.clone(),
                    );
                    self.actions.push(Action::SetNotificationSound {
                        mention,
                        sound: crate::settings::NotificationSound::Custom(path),
                    });
                }
            }
            Event::InvitePreview { code, result } => {
                use crate::model::InviteState;
                if let Some(invite) = self.invite.as_mut().filter(|invite| invite.code == code) {
                    invite.state = match result {
                        Ok(info) => InviteState::Ready(info),
                        Err(error) => InviteState::Failed(error),
                    };
                }
            }
            Event::InviteJoined { code, result } => {
                if self
                    .invite
                    .as_ref()
                    .is_some_and(|invite| invite.code == code)
                {
                    match result {
                        Ok((id, pending)) => {
                            self.invite = None;
                            if live && self.dialog == Some(Dialog::JoinGroup) {
                                self.dialog = None;
                            }
                            if pending {
                                self.toast(
                                    "Request sent. An admin must approve it before you join.",
                                );
                            } else if live {
                                self.actions.push(Action::OpenChat(id));
                            }
                        }
                        Err(error) => {
                            if let Some(invite) = self.invite.as_mut() {
                                invite.state = crate::model::InviteState::Failed(error);
                            }
                        }
                    }
                }
            }
            Event::ContactFailed { request, error } => {
                if self.new_contact_request == Some(request) {
                    self.new_contact_request = None;
                    self.new_contact_dialog_request = None;
                    self.new_contact_pending = false;
                    if live {
                        self.toast_error(contact_failure_message(self.locale, &error));
                    }
                }
            }
            Event::ContactReady { request, id, name } => {
                if self.new_contact_request != Some(request) {
                    return;
                }
                let owned_by_dialog = self.new_contact_dialog_request.take() == Some(request);
                self.new_contact_request = None;
                self.new_contact_pending = false;
                if live {
                    if owned_by_dialog
                        && matches!(
                            self.dialog,
                            Some(Dialog::NewContact | Dialog::MessageNumber)
                        )
                    {
                        self.dialog = None;
                    }
                    let name = name
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| crate::util::phone(&id));
                    self.actions.push(Action::StartChat {
                        id,
                        name,
                        dismiss_dialog: false,
                    });
                }
            }
            Event::ArchiveProgress(progress) => {
                self.archive_progress = Some(progress);
            }
            Event::BackupMergePreview(result) => {
                self.backup_busy = false;
                match result {
                    Ok(preview) => {
                        self.backup_merge_preview = preview;
                        if self.backup_merge_preview.is_some() && !self.events_hidden {
                            self.actions
                                .push(Action::ShowDialog(Dialog::ConfirmBackupMerge));
                        }
                    }
                    Err(error) => self.toast_error(error),
                }
            }
            Event::BackupMergeFinished(result) => {
                self.backup_busy = false;
                self.backup_merge_preview = None;
                match result {
                    Ok(preview) => {
                        self.backup_status = Some(crate::i18n::gettext(self.locale,
                            "Backup merged: {added} messages added, {edited} edited snapshots updated, {removed} confirmed deletions applied, {conflicts} alternatives retained for review.")
                            .replace("{added}",&preview.added.to_string()).replace("{edited}",&preview.edited.to_string())
                            .replace("{removed}",&preview.removed.to_string()).replace("{conflicts}",&preview.conflicts.to_string()));
                        self.reload_lists();
                        if !self.events_hidden
                            && let Some(chat) = self.open_chat.clone()
                        {
                            self.backend.send(Command::LoadChat { chat, before: None });
                        }
                    }
                    Err(error) => self.toast_error(error),
                }
            }
            Event::ArchiveBackupFinished(result) => {
                self.backup_busy = false;
                let (status, failed) = match result {
                    Ok(Some(summary)) => (
                        crate::i18n::ngettext(
                            self.locale,
                            "Archive backup saved ({count} file, {size})",
                            "Archive backup saved ({count} files, {size})",
                            u32::try_from(summary.files).unwrap_or(u32::MAX),
                        )
                        .replace("{count}", &summary.files.to_string())
                        .replace("{size}", &crate::util::bytes(summary.bytes)),
                        false,
                    ),
                    Ok(None) => (
                        crate::i18n::gettext(self.locale, "Archive backup cancelled").into_owned(),
                        false,
                    ),
                    Err(error) => (
                        crate::i18n::gettext(self.locale, "Archive backup failed: {error}")
                            .replace("{error}", &error),
                        true,
                    ),
                };
                self.backup_status = Some(status.clone());
                if !self.events_hidden {
                    if failed {
                        self.toast_error(status);
                    } else {
                        self.toast(status);
                    }
                }
            }
            Event::Info(message) => self.toast(message),
            Event::ClipboardImage(result) => {
                self.handle_clipboard_image(result, write_clipboard_image);
            }
            Event::StickerPicture {
                path,
                width,
                height,
                transparent,
            } => {
                self.sticker_draft = Some(crate::model::StickerDraft {
                    source: path,
                    width,
                    height,
                    transparent,
                    crop: crate::model::StickerCrop::centered(width, height),
                    keep_transparent: transparent,
                    emojis: String::new(),
                });
                if live {
                    self.picker = None;
                    self.dialog = Some(Dialog::StickerMaker);
                }
            }
            Event::PictureInspected { index, result } => match result {
                Ok((width, height)) => {
                    // The attachment may have been removed while the size was
                    // being read, so it is looked up again rather than assumed.
                    let source = self.pending.get(index).and_then(|item| match item {
                        Pending::File(path) => Some(path.clone()),
                        Pending::Picture { .. }
                        | Pending::Document(_)
                        | Pending::Recovered { .. } => None,
                    });
                    if let Some(path) = source {
                        self.picture_edit = Some(crate::model::PictureEdit::new(
                            index,
                            crate::model::PictureSource::File(path),
                            width,
                            height,
                        ));
                    }
                }
                Err(error) => self.toast_error(error),
            },
            Event::PictureEdited { index, result } => {
                self.picture_edit = None;
                match result {
                    Ok(path) => {
                        if index < self.pending.len() {
                            self.pending[index] = Pending::File(path.clone());
                        }
                        if let Some(origin) = self.picture_applying.take() {
                            self.picture_origins.insert(path, origin);
                        }
                    }
                    Err(error) => {
                        self.picture_applying = None;
                        self.toast_error(error);
                    }
                }
            }
            Event::StickerPackPreview(result) => {
                self.sticker_preview_pending = false;
                match result {
                    Ok(preview) => self.sticker_preview = Some(preview),
                    Err(error) => {
                        if live && self.dialog == Some(Dialog::StickerPack) {
                            self.dialog = None;
                        }
                        self.toast_error(error);
                    }
                }
            }
            Event::SendFailed { connection } => {
                let message = if connection {
                    crate::i18n::gettext(
                        self.locale,
                        "This message could not be sent. Check your connection before sending it again.",
                    )
                } else {
                    crate::i18n::gettext(
                        self.locale,
                        "This message could not be sent. Try again later.",
                    )
                };
                self.toast_error(message.into_owned());
            }
            Event::UpdateAvailable { version, url } => {
                let notice = crate::updates::Release { version, url };
                if self.update.as_ref() != Some(&notice) {
                    self.toast(format!("Zap Faster {} is available", notice.version));
                }
                self.update = Some(notice);
            }
            Event::UpdateSupport(result) => {
                self.update_support = Some(result);
                self.update_inspecting = false;
                self.maybe_download_update();
            }
            Event::UpdateProgress { received, total } => {
                self.update_download =
                    crate::updates::DownloadState::Downloading { received, total };
            }
            Event::UpdateDownloaded(result) => {
                self.update_download = match result {
                    Ok(prepared) => crate::updates::DownloadState::Ready(prepared),
                    Err(error) => crate::updates::DownloadState::Failed(error),
                };
            }
            Event::UpdateInstalling(result) => match result {
                Ok(()) => self.actions.push(Action::Quit),
                Err(error) => self.update_download = crate::updates::DownloadState::Failed(error),
            },
            Event::SendRefused {
                chat,
                quoting,
                unsent,
                reason,
                mentions,
            } => self.send_refused_with_mentions(chat, quoting, unsent, reason, mentions),
            Event::GroupSaving { chat, saving } => {
                if saving {
                    self.group_saving.insert(chat);
                } else {
                    self.group_saving.remove(&chat);
                }
            }
            Event::EditRefused {
                chat,
                id,
                draft,
                error,
            } => {
                if !matches!(error, EditFailure::Save) {
                    self.refused_edits.retain(|(target_chat, target_id, _)| {
                        target_chat != &chat || target_id != &id
                    });
                    self.refused_edits.push((chat, id, draft));
                }
                self.toast_error(edit_failure_message(self.locale, &error));
            }
            Event::Error(message) => {
                self.sticker_import_pending = false;
                let localized = match message.as_str() {
                    "Could not save the archive change. Try again." => crate::i18n::gettext(
                        self.locale,
                        "Could not save the archive change. Try again.",
                    ),
                    "This chat already has three pinned messages" => crate::i18n::gettext(
                        self.locale,
                        "This chat already has three pinned messages",
                    ),
                    "Could not save the star change" => {
                        crate::i18n::gettext(self.locale, "Could not save the star change")
                    }
                    "Could not read starred messages" => {
                        crate::i18n::gettext(self.locale, "Could not read starred messages")
                    }
                    "Wait for the previous pin request to finish" => crate::i18n::gettext(
                        self.locale,
                        "Wait for the previous pin request to finish",
                    ),
                    "Not connected to WhatsApp" => {
                        crate::i18n::gettext(self.locale, "Not connected to WhatsApp")
                    }
                    "This message is not stored on this computer" => crate::i18n::gettext(
                        self.locale,
                        "This message is not stored on this computer",
                    ),
                    "Invalid pin message reference" => {
                        crate::i18n::gettext(self.locale, "Could not pin this message")
                    }
                    "Could not save the pin change" => {
                        crate::i18n::gettext(self.locale, "Could not save the pin change")
                    }
                    "Pin request timed out; confirm its state on your phone before retrying" => {
                        crate::i18n::gettext(
                            self.locale,
                            "Pin request timed out; confirm its state on your phone before retrying",
                        )
                    }
                    "Could not confirm the pin request; check your phone before retrying" => {
                        crate::i18n::gettext(
                            self.locale,
                            "Could not confirm the pin request; check your phone before retrying",
                        )
                    }
                    _ => message.into(),
                };
                self.toast_error(localized);
            }
            Event::AccountRemoved => {}
        }
        self.restore_refused_edit();
        self.prune_selection();
    }

    fn prune_selection(&mut self) {
        let Some((chat, ids)) = self.selection.clone() else {
            return;
        };
        let conversation = self.conversations.get(chat.as_str());
        let selectable = |id: &str| {
            conversation
                .and_then(|conversation| conversation.message(id))
                .is_some_and(|message| can_select(&message.content))
        };
        let ids: Vec<String> = ids.into_iter().filter(|id| selectable(id)).collect();
        let anchor_gone = self
            .selection_anchor
            .as_deref()
            .is_some_and(|id| !selectable(id));
        let base = self
            .sweep
            .as_ref()
            .filter(|sweep| sweep.chat == chat)
            .map(|sweep| {
                sweep
                    .base
                    .iter()
                    .filter(|id| selectable(id))
                    .cloned()
                    .collect::<Vec<_>>()
            });
        if anchor_gone {
            self.selection_anchor = ids.last().cloned();
        }
        if let (Some(base), Some(sweep)) = (base, self.sweep.as_mut()) {
            sweep.base = base;
        }
        self.selection = Some((chat, ids));
    }

    /// Restores an idle composer's refused correction, using a normal draft
    /// when the original is still present but can no longer be edited.
    fn restore_refused_edit(&mut self) {
        if self.events_hidden {
            return;
        }
        if !self.composer.trim().is_empty()
            || self.editing.is_some()
            || self.reply_to.is_some()
            || !self.pending.is_empty()
        {
            return;
        }
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        let recovered = {
            let account = self.account_mut();
            let Some(conversation) = account.conversations.get(&chat) else {
                return;
            };
            account.refused_edits.retain(|(target_chat, id, _)| {
                target_chat != &chat
                    || conversation.message(id).is_some_and(|message| {
                        message.from_me && matches!(message.content, Content::Text { .. })
                    })
            });
            account
                .refused_edits
                .iter()
                .position(|(target_chat, _, _)| target_chat == &chat)
                .map(|index| account.refused_edits.remove(index))
        };
        if let Some((_, id, draft)) = recovered {
            self.editing = self
                .conversations
                .get(&chat)
                .and_then(|conversation| conversation.message(&id))
                .filter(|message| self.can_edit(message))
                .map(|_| id);
            self.reply_to = None;
            self.composer_tools_open = false;
            self.composer = draft.text;
            self.composer_mentions = draft.mentions;
            self.focus_composer = true;
        }
    }

    /// Applies link transitions and clears account-owned state when the device is unlinked.
    fn handle_link(&mut self, status: LinkStatus, live: bool) {
        match &status {
            LinkStatus::Connected => {
                for conversation in self.conversations.values_mut() {
                    for message in &mut conversation.messages {
                        if let Content::Poll { state, .. } = &mut message.content {
                            state.refresh_needed = true;
                            state.refreshing = false;
                        }
                    }
                }
                if matches!(self.link, LinkStatus::Disconnected { .. }) {
                    self.toast("Back online");
                }
                if live {
                    self.dialog = match self.dialog.take() {
                        Some(Dialog::PairWithPhone) => None,
                        other => other,
                    };
                }
                if let Some(open) = self.open_chat.clone() {
                    self.ensure_loaded(&open);
                }
            }
            LinkStatus::LoggedOut => {
                self.statuses.clear();
                // Account-scoped lookup results are discarded by the worker.
                // End their UI requests too, so a later account can retry.
                self.new_contact_request = None;
                self.new_contact_dialog_request = None;
                self.new_contact_pending = false;
                let account = self.account().id.clone();

                self.poll_voting.clear();
                self.interactive_sending.clear();
                self.poll_creating = false;
                self.poll_draft = Default::default();
                self.notifications.clear_account(&account);
                self.in_app_notifications
                    .retain(|notification| notification.target.account != account);
                self.chats.clear();
                self.chats_loaded = false;
                self.conversations.clear();
                self.pins.clear();
                self.chat_pins.clear();
                self.pin_notices.clear();
                self.contacts.clear();
                self.avatars.clear();
                self.starred.clear();
                self.show_starred = false;
                self.stars.clear();
                self.account_privacy = crate::privacy::Snapshot::default();
                self.account_receipts_off = false;
                self.open_chat = None;
                // Every recorded place named a chat of the account that just
                // went away, but only when that account is the one on screen.
                if !self.events_hidden {
                    self.nav_history.clear();
                }
                // Unsent text belongs to the account that was unlinked.
                self.drafts.clear();
                self.refused_edits.clear();
                self.private_reply_drafts.clear();
                self.private_reply_generation = self
                    .private_reply_generations
                    .values()
                    .copied()
                    .fold(self.private_reply_generation, u64::max)
                    .checked_add(1)
                    .expect("private reply account generation exhausted");
                self.private_reply_generations.clear();
                self.draft_mentions.clear();
                self.attachment_drafts.clear();
                if live {
                    self.pending.clear();
                    self.reply_to = None;
                    self.clear_composer_text();
                    self.composer_mentions.clear();
                }
                self.link = status;
                // The password guarded chats that are gone now; a forgotten
                // one is recovered exactly this way, once no number remains.
                if !self.any_linked() {
                    self.forget_app_lock();
                }
                self.toast_error("This device was unlinked from your phone");
                return;
            }
            LinkStatus::Failed(message) => self.toast_error(message.clone()),
            _ => {}
        }
        self.link = status;
    }

    fn handle_chat_updated(&mut self, chat: Chat, live: bool) {
        // A hidden account's remembered chat is not being read: marking it
        // read would send receipts for messages nobody has seen.
        let remembered = self.open_chat.as_deref() == Some(chat.id.as_str());
        let is_open = live && remembered && self.page == Page::Chats;
        let mut chat = chat;
        if chat.unread == 0 {
            self.clear_chat_notifications(&chat.id);
        }
        if is_open
            && (!chat.locked || self.locked_folder_open())
            && chat.unread > 0
            && self.window_focused
            && !self.window_hidden
        {
            chat.unread = 0;
            self.mark_read(&chat.id);
        }
        // Inside the authenticated folder the chat stays open; otherwise a
        // lock closes and clears everything it left behind.
        if chat.locked && !self.locked_folder_open() {
            self.hide_locked_chat(&chat.id);
        }
        // Leaving with "and archive" archives the chat once the phone agreed,
        // so this is where the open conversation closes, not before. Only a
        // chat that just became archived: a message arriving in one that was
        // already archived does not close it.
        if (is_open || (!live && remembered))
            && chat.archived
            && self.chat(&chat.id).is_none_or(|known| !known.archived)
        {
            if live {
                self.actions.push(Action::CloseChat);
            } else {
                self.open_chat = None;
            }
        }
        match self.chats.iter_mut().find(|known| known.id == chat.id) {
            Some(existing) => *existing = chat,
            None => self.chats.push(chat),
        }
        self.chats
            .sort_by_key(|chat| std::cmp::Reverse(chat.last_activity));
    }

    /// Rechecks source-chat authorization for every retained private quote.
    fn reply_source_visible(&self, target: &crate::model::ReplyTarget, destination: &str) -> bool {
        target.chat.is_none()
            || self
                .chat(target.source_chat(destination))
                .is_some_and(|chat| !chat.locked || self.locked_folder_open())
    }

    /// Resolves a quote preview only while its source chat is visible to the user.
    pub(crate) fn reply_preview(
        &self,
        target: &crate::model::ReplyTarget,
        destination: &str,
    ) -> Option<&Message> {
        let source = target.source_chat(destination);
        if !self.reply_source_visible(target, destination) {
            return None;
        }
        self.conversations.get(source)?.message(target)
    }

    fn close_locked_folder(&mut self) {
        self.locked_folder = false;
        self.chat_lock_session = None;
        self.clear_chat_lock_entry();
        self.chat_lock_check.borrow_mut().take();
        if let Some(id) = self.open_chat.clone()
            && self.chat(&id).is_some_and(|chat| chat.locked)
        {
            self.hide_locked_chat(&id);
        }
    }

    fn clear_chat_lock_entry(&mut self) {
        self.chat_lock_entry.clear();
        self.chat_lock_confirm.clear();
        self.chat_lock_error = false;
    }

    fn enter_locked_folder(&mut self) {
        self.chat_lock_session = self.settings.chat_lock_code_hash.clone();
        self.locked_folder = true;
        self.search.clear();
        self.search_hits.clear();
        self.chat_lock_check.borrow_mut().take();
        self.clear_chat_lock_entry();
        self.dialog = None;
        self.chat_filter = ChatFilter::All;
        self.show_archived = false;
        self.page = Page::Chats;
        self.sidebar_visible = true;
    }

    /// Empties a chat that stays listed. Search hits and anything pointing at
    /// one of its messages would otherwise refer to rows that are gone, and a
    /// pending edit would send `EditText` for a message that no longer exists.
    fn handle_chat_cleared(&mut self, id: &str, through: i64, live: bool) {
        if let Some(listing) = self
            .gallery
            .listing
            .as_mut()
            .filter(|listing| listing.chat == id)
        {
            listing.media.retain(|row| row.timestamp > through);
            listing.docs.retain(|row| row.timestamp > through);
            listing.links.retain(|row| row.timestamp > through);
        }
        // A confirmation that is open for this chat is about messages that are
        // already gone: clearing again would take what arrived since.
        if matches!(&self.dialog, Some(Dialog::ConfirmClearChat(chat)) if chat == id) {
            self.dialog = None;
        }
        self.clear_chat_notifications(id);
        // Clearing a chat also removes its stored draft.
        self.drafts.remove(id);
        self.private_reply_drafts.remove(id);
        self.draft_mentions.remove(id);
        self.attachment_drafts.remove(id);
        if live && self.open_chat.as_deref() == Some(id) {
            if !self.pending.is_empty() {
                self.pending.clear();
                self.composer.clear();
                self.composer_mentions.clear();
                self.reply_to = None;
            } else if self.editing.is_none() {
                self.composer.clear();
                self.composer_mentions.clear();
            }
        }
        self.search_hits
            .retain(|message| message.chat != id || message.timestamp > through);
        // Nothing earlier is left here, and the phone no longer has it either.
        let open = self.open_chat.as_deref() == Some(id);
        let editing = self.editing.clone();
        let reply = self.reply_to.clone();
        let reaction = self.reaction_target.clone();
        let (editing_gone, reply_gone, reaction_gone) = {
            let conversation = self.conversations.entry(id.to_owned()).or_default();
            conversation
                .messages
                .retain(|message| message.timestamp > through);
            let remaining: HashSet<_> = conversation
                .messages
                .iter()
                .map(|message| message.id.clone())
                .collect();
            conversation
                .transcripts
                .retain(|id, _| remaining.contains(id));
            conversation.requested = true;
            conversation.complete = true;
            conversation.phone_exhausted = true;
            conversation.loading_older = false;
            (
                editing
                    .as_ref()
                    .is_some_and(|edit| conversation.message(edit).is_none()),
                reply.as_ref().is_some_and(|reply| {
                    reply.source_chat(id) == id && conversation.message(&reply.id).is_none()
                }),
                reaction
                    .as_ref()
                    .is_some_and(|(_, target)| conversation.message(target).is_none()),
            )
        };
        if open && live {
            if editing_gone {
                self.editing = None;
                self.clear_composer_text();
                self.composer_mentions.clear();
            }
            if reply_gone {
                self.reply_to = None;
            }
            if reaction_gone {
                self.reaction_target = None;
                self.reaction_anchor = None;
                self.picker = None;
            }
        }
    }

    /// The pack the sticker tab shows, when it still exists.
    pub fn selected_pack(&self) -> Option<&StickerPack> {
        let StickerShelf::Pack(dir) = &self.sticker_shelf else {
            return None;
        };
        self.sticker_packs.iter().find(|pack| pack.dir == *dir)
    }

    /// Whether the search pane is on screen: it belongs to the open chat and
    /// only the chat view shows it.
    pub fn chat_search_visible(&self) -> bool {
        self.chat_search_open && self.page == Page::Chats && self.open_chat.is_some()
    }

    /// Opens the search pane beside the open chat, or focuses its field when
    /// it is already open.
    fn open_chat_search(&mut self) {
        if self.open_chat.is_none() || self.page != Page::Chats {
            return;
        }
        if !self.chat_search_open {
            self.chat_search_open = true;
            self.chat_search_month = crate::util::today();
        }
        self.focus_chat_search = true;
        self.focus_search = false;
        self.focus_composer = false;
    }

    fn close_chat_search(&mut self) {
        self.chat_search_open = false;
        self.chat_search.clear();
        self.chat_search_hits.clear();
        self.chat_search_truncated = false;
        self.chat_search_pending = false;
        self.chat_search_selected = None;
        self.chat_search_day = None;
        self.chat_search_calendar = false;
        self.focus_chat_search = false;
    }

    /// The Unix-second range the day filter stands for, if a day is picked.
    fn chat_search_range(&self) -> (Option<i64>, Option<i64>) {
        match self.chat_search_day.and_then(crate::util::day_bounds) {
            Some((from, until)) => (Some(from), Some(until)),
            None => (None, None),
        }
    }

    /// Asks for the open chat's matches, for the query and day in force.
    fn request_chat_search(&mut self) {
        self.chat_search_selected = None;
        let query = self.chat_search.trim().to_owned();
        let (from, until) = self.chat_search_range();
        let Some(chat) = self
            .open_chat
            .clone()
            .filter(|_| !query.is_empty() || from.is_some())
        else {
            self.chat_search_hits.clear();
            self.chat_search_truncated = false;
            self.chat_search_pending = false;
            return;
        };
        // The earlier matches stay listed until the answer replaces them, so
        // the list does not blink empty on every keystroke.
        self.chat_search_pending = true;
        self.backend.send(Command::SearchChatMessages {
            chat,
            query,
            from,
            until,
        });
    }

    /// Parks the open locked chat's draft before hiding its conversation and controls.
    fn hide_locked_chat(&mut self, id: &str) {
        // A locked chat still exists, so its unsent text waits as a draft.
        // Text emptied in the composer clears the stored copy too. A hidden
        // account's composer text is already parked among its drafts.
        if self.events_hidden {
            self.leave_chat(id);
            return;
        }
        if self.open_chat.as_deref() == Some(id) {
            self.save_attachment_draft(id);
        }
        if self.open_chat.as_deref() == Some(id)
            && self.editing.is_none()
            && self.composer.is_empty()
        {
            self.store_draft(id, "");
        }
        if self.open_chat.as_deref() == Some(id)
            && self.editing.is_none()
            && !self.composer.is_empty()
        {
            let draft = self.take_composer_text();
            let mentions = std::mem::take(&mut self.composer_mentions);
            self.drafts.insert(id.to_owned(), draft);
            self.draft_mentions.insert(id.to_owned(), mentions);
            self.store_draft(
                id,
                self.drafts.get(id).map(String::as_str).unwrap_or_default(),
            );
        }
        self.leave_chat(id);
    }

    /// Takes back a send the worker refused. Nothing typed or recorded is
    /// lost, and a refused reply is never sent again without its quote unless
    /// the user cancels the reply first.
    fn send_refused(
        &mut self,
        chat: ChatId,
        quoting: Option<crate::model::ReplyTarget>,
        unsent: Unsent,
        reason: Refusal,
    ) {
        self.send_refused_with_mentions(chat, quoting, unsent, reason, Vec::new());
    }

    fn send_refused_with_mentions(
        &mut self,
        chat: ChatId,
        quoting: Option<crate::model::ReplyTarget>,
        unsent: Unsent,
        reason: Refusal,
        mentions: Vec<String>,
    ) {
        // Removing the destination also discards recovery of its private replies.
        if quoting.as_ref().is_some_and(|target| {
            target.chat.is_some()
                && (self.chat(&chat).is_none()
                    || target.destination_generation
                        != self
                            .private_reply_generations
                            .get(&chat)
                            .copied()
                            .unwrap_or(self.private_reply_generation))
        }) {
            return;
        }
        let open = !self.events_hidden && self.open_chat.as_deref() == Some(chat.as_str());
        // Re-arm the reply banner, unless the user has moved on to another
        // reply or an edit since.
        if let Some(target) = quoting.clone() {
            if open && self.reply_to.is_none() && self.editing.is_none() {
                self.reply_to = Some(target);
                self.focus_composer = true;
            } else if !open && target.chat.is_some() {
                self.private_reply_drafts
                    .entry(chat.clone())
                    .or_insert(target);
            }
        }
        match unsent {
            Unsent::Text(text) => self.restore_text(&chat, text),
            // The composer cannot hold a spot, so the dialog takes it back.
            Unsent::Location {
                latitude,
                longitude,
            } => {
                if open {
                    self.location_draft = crate::geo::Spot {
                        latitude,
                        longitude,
                    }
                    .text();
                    self.dialog = Some(Dialog::SendLocation(chat));
                }
            }
            Unsent::Voice(samples) => {
                self.unsent_voice = Some((chat, samples));
                self.focus_composer = open;
            }
            Unsent::Files { paths, caption } => {
                let restored_caption = self.restore_attachments(
                    &chat,
                    paths.into_iter().map(Pending::File),
                    quoting,
                    caption.clone(),
                    mentions,
                );
                if restored_caption {
                    self.restore_text(&chat, caption.unwrap_or_default());
                }
            }
            Unsent::Image {
                width,
                height,
                rgba,
                caption,
            } => {
                let restored_caption = self.restore_attachments(
                    &chat,
                    [Pending::Picture {
                        width: width as usize,
                        height: height as usize,
                        rgba: std::sync::Arc::new(rgba),
                        texture: None,
                    }],
                    quoting,
                    caption.clone(),
                    mentions,
                );
                if restored_caption {
                    self.restore_text(&chat, caption.unwrap_or_default());
                }
            }
            Unsent::Document { source, caption } => {
                let item = match source {
                    crate::model::DocumentSource::File(path) => Pending::File(path),
                    crate::model::DocumentSource::Pixels {
                        width,
                        height,
                        rgba,
                    } => Pending::Picture {
                        width: width as usize,
                        height: height as usize,
                        rgba: std::sync::Arc::new(rgba),
                        texture: None,
                    },
                };
                let restored_caption = self.restore_attachments(
                    &chat,
                    [Pending::Document(Box::new(item))],
                    quoting,
                    caption.clone(),
                    mentions,
                );
                if restored_caption {
                    self.restore_text(&chat, caption.unwrap_or_default());
                }
            }
            Unsent::Sticker | Unsent::Gif => {}
        }
        let message = match reason {
            Refusal::Offline => crate::i18n::gettext(
                self.locale,
                "Not sent: Zap Faster is not connected to WhatsApp.",
            ),
            Refusal::QuoteUnavailable => crate::i18n::gettext(
                self.locale,
                "Not sent: the message you’re replying to isn’t available on this computer. Cancel the reply to send without a quote.",
            ),
        };
        self.toast_error(message.into_owned());
    }

    /// Returns refused attachments to the live composer or their owning account's chat draft.
    fn restore_attachments(
        &mut self,
        chat: &str,
        pending: impl IntoIterator<Item = Pending>,
        quoting: Option<crate::model::ReplyTarget>,
        caption: Option<String>,
        mentions: Vec<String>,
    ) -> bool {
        let live = !self.events_hidden && self.open_chat.as_deref() == Some(chat);
        let composer_owned = if live {
            self.pending.is_empty() && self.composer.trim().is_empty() && self.editing.is_none()
        } else {
            self.attachment_drafts
                .get(chat)
                .is_none_or(|draft| draft.pending.is_empty())
                && self
                    .drafts
                    .get(chat)
                    .is_none_or(|text| text.trim().is_empty())
        };
        let draft_quote = quoting.clone();
        let mut mentions = mentions;
        let mut first = true;
        let mut caption = caption;
        let mut quoting = quoting;
        let recovered: Vec<_> = pending
            .into_iter()
            .map(|item| {
                let owned = first && composer_owned;
                first = false;
                Pending::Recovered {
                    item: Box::new(item),
                    caption: caption.take(),
                    quoting: quoting.take(),
                    mentions: std::mem::take(&mut mentions),
                    composer_owned: owned,
                }
            })
            .collect();
        let pending = if !self.events_hidden && self.open_chat.as_deref() == Some(chat) {
            &mut self.pending
        } else if self.chat(chat).is_some() {
            let draft = self.attachment_drafts.entry(chat.to_owned()).or_default();
            if draft.quoting.is_none() {
                draft.quoting = draft_quote;
            }
            &mut draft.pending
        } else {
            return false;
        };
        // Keep refused groups in callback order, ahead of attachments staged since the send.
        let at = pending
            .iter()
            .take_while(|item| matches!(item, Pending::Recovered { .. }))
            .count();
        pending.splice(at..at, recovered);
        composer_owned
    }

    /// Returns refused text to its chat's composer, or to its stored draft
    /// when another chat is open. Text typed since the send is kept.
    fn restore_text(&mut self, chat: &str, text: String) {
        if text.trim().is_empty() {
            return;
        }
        if !self.events_hidden && self.open_chat.as_deref() == Some(chat) {
            if self.composer.trim().is_empty() && self.editing.is_none() {
                self.replace_composer_text(text);
                self.composer_mentions.clear();
                self.emoji_start = None;
                self.mention_start = None;
                self.focus_composer = true;
                self.store_draft(chat, &self.composer);
            }
        } else if self
            .drafts
            .get(chat)
            .is_none_or(|draft| draft.trim().is_empty())
        {
            self.store_draft(chat, &text);
            self.drafts.insert(chat.to_owned(), text);
        }
    }

    /// Mirrors a chat's draft into the encrypted archive, so unsent text
    /// survives a restart. An empty text clears the stored row.
    fn store_draft(&self, chat: &str, text: &str) {
        self.backend.send(Command::SaveDraft {
            chat: chat.to_owned(),
            text: text.to_owned(),
        });
    }

    fn motion_mut(&mut self, chat: &str, id: &str) -> Option<&mut crate::model::Motion> {
        match &mut self.conversations.get_mut(chat)?.message_mut(id)?.content {
            Content::Image { motion, .. } => motion.as_mut(),
            _ => None,
        }
    }

    pub(crate) fn gallery_accessible(&self, chat: &str) -> bool {
        !self.app_lock.is_locked()
            && self
                .chat(chat)
                .is_some_and(|row| !row.locked || self.locked_folder_open())
    }
    pub(crate) fn request_gallery(&mut self, chat: &str, refresh: bool) {
        if !self.gallery_accessible(chat) {
            return;
        }
        if !refresh
            && self.gallery.chat.as_deref() == Some(chat)
            && (self.gallery.pending || self.gallery.listing.is_some())
        {
            return;
        }
        if self.gallery.chat.as_deref() != Some(chat) {
            self.gallery.listing = None;
        }
        self.gallery.chat = Some(chat.into());
        self.gallery.token = self.gallery.token.wrapping_add(1);
        self.gallery.pending = true;
        self.gallery.failed = false;
        let anchor = self
            .image_preview
            .as_ref()
            .filter(|preview| preview.chat() == Some(chat))
            .and_then(|preview| preview.message())
            .map(str::to_owned);
        self.backend.send(Command::LoadGallery {
            chat: chat.into(),
            token: self.gallery.token,
            anchor,
        });
    }
    fn hydrate_gallery_message(&mut self, chat: &str, id: &str) {
        if !self.gallery_accessible(chat)
            || self
                .conversations
                .get(chat)
                .is_some_and(|c| c.message(id).is_some())
        {
            return;
        }
        let row = self
            .gallery
            .listing
            .as_ref()
            .filter(|listing| listing.chat == chat)
            .and_then(|listing| {
                listing
                    .media
                    .iter()
                    .chain(&listing.docs)
                    .find(|row| row.id == id)
            })
            .cloned();
        if let Some(row) = row {
            self.conversations
                .entry(chat.into())
                .or_default()
                .merge(vec![row], true);
        }
    }
    fn preview_media(&mut self, chat: String, id: String, ctx: &egui::Context) {
        if !self.gallery_accessible(&chat) {
            return;
        }
        self.hydrate_gallery_message(&chat, &id);
        let Some(row) = self
            .conversations
            .get(&chat)
            .and_then(|c| c.message(&id))
            .cloned()
        else {
            return;
        };
        if !matches!(row.content, Content::Image { .. } | Content::Video { .. }) {
            return;
        }
        let path = row
            .content
            .media()
            .and_then(|media| media.path.clone())
            .filter(|path| path.is_file());
        self.video.stop();
        let mut preview = PreviewState::in_conversation(PathBuf::new(), Some(chat.clone()));
        preview.select_message(id, path);
        self.image_preview = Some(preview);
        self.dialog = None;
        self.picker = None;
        self.request_gallery(&chat, false);
        ctx.memory_mut(|memory| {
            if let Some(focused) = memory.focused() {
                memory.surrender_focus(focused);
            }
        });
    }

    fn handle_media(
        &mut self,
        chat: &str,
        id: &str,
        card: Option<usize>,
        result: Result<PathBuf, String>,
    ) {
        if card.is_none()
            && let Some(listing) = self
                .gallery
                .listing
                .as_mut()
                .filter(|listing| listing.chat == chat)
            && let Some(row) = listing
                .media
                .iter_mut()
                .chain(&mut listing.docs)
                .find(|row| row.id == id)
            && let Some(media) = row.content.media_at_mut(None)
        {
            match &result {
                Ok(path) => {
                    media.path = Some(path.clone());
                    media.state = MediaState::Idle;
                }
                Err(reason) => media.state = MediaState::Failed(reason.clone()),
            }
        }
        if card.is_none()
            && let Ok(path) = &result
            && !self.events_hidden
            && let Some(preview) = self
                .image_preview
                .as_mut()
                .filter(|p| p.chat() == Some(chat) && p.message() == Some(id))
        {
            preview.select_message(id.into(), Some(path.clone()));
        }
        let want_video = self
            .video_wanted
            .as_ref()
            .is_some_and(|(wanted_chat, wanted)| wanted_chat == chat && wanted == id);
        let want_voice = self
            .voice_wanted
            .as_ref()
            .is_some_and(|(wanted_chat, wanted, _)| wanted_chat == chat && wanted == id);
        let open = self.open_chat.as_deref() == Some(chat);
        // Playback waits belong to the account on screen.
        let want_video = want_video && !self.events_hidden;
        let want_voice = want_voice && !self.events_hidden;
        let applied = {
            let Some(message) = self
                .conversations
                .get_mut(chat)
                .and_then(|conversation| conversation.message_mut(id))
            else {
                return;
            };
            let Some(media) = message.content.media_at_mut(card) else {
                return;
            };
            match result {
                Ok(path) => {
                    media.path = Some(path.clone());
                    media.state = MediaState::Idle;
                    Ok(path)
                }
                Err(error) => {
                    let notice = if error.contains("403") || error.contains("404") {
                        "No longer available on WhatsApp's servers".to_owned()
                    } else {
                        error
                    };
                    log::warn!("attachment download failed; details are shown in the bubble");
                    media.state = MediaState::Failed(notice);
                    Err(())
                }
            }
        };
        match applied {
            Ok(path) => {
                if want_video {
                    self.video_wanted = None;
                    self.actions.push(Action::PlayVideo {
                        message: id.to_owned(),
                        path,
                    });
                } else if want_voice {
                    self.voice_wanted = None;
                    if open {
                        self.actions.push(Action::PlayVoice {
                            message: id.to_owned(),
                            path,
                        });
                    }
                }
            }
            Err(()) => {
                if want_voice {
                    self.voice_wanted = None;
                }
            }
        }
    }

    fn ensure_loaded(&mut self, chat: &str) {
        let request = {
            let conversation = self.conversations.entry(chat.to_owned()).or_default();
            if conversation.requested {
                false
            } else {
                conversation.requested = true;
                true
            }
        };
        if request {
            self.backend.send(Command::LoadChat {
                chat: chat.to_owned(),
                before: None,
            });
            self.backend.send(Command::LoadPins(chat.to_owned()));
        }
    }

    pub fn load_older(&mut self, chat: &str, explicit: bool) {
        // The account's fields directly, so the window's scroll anchor can
        // change while its conversation is borrowed.
        let account = &mut self.accounts[self.active];
        let Some(conversation) = account.conversations.get_mut(chat) else {
            return;
        };
        if conversation.loading_older {
            return;
        }
        let Some(oldest) = conversation.messages.first() else {
            return;
        };
        if conversation.complete {
            self.fetch_older(chat, explicit);
            return;
        }
        conversation.loading_older = true;
        let before = (oldest.timestamp, oldest.id.clone());
        self.scroll_anchor = Some(oldest.id.clone());
        account.backend.send(Command::LoadChat {
            chat: chat.to_owned(),
            before: Some(before),
        });
    }

    /// Requests older phone history when available and outside the cooldown.
    /// `explicit` when the reader asked by scrolling to the top; automatic
    /// requests never report a silent phone.
    pub fn fetch_older(&mut self, chat: &str, explicit: bool) {
        let account = &mut self.accounts[self.active];
        let Some(conversation) = account.conversations.get_mut(chat) else {
            return;
        };
        if conversation.phone_exhausted {
            return;
        }
        if conversation.fetching_phone {
            if explicit && !conversation.phone_explicit {
                // The reader scrolled up while an automatic request waits:
                // the worker makes that request theirs instead of asking twice.
                conversation.phone_explicit = true;
                account.backend.send(Command::FetchOlder {
                    chat: chat.to_owned(),
                    explicit,
                });
            }
            return;
        }
        // Back off after empty responses. Only a connected phone can answer.
        if !matches!(account.link, LinkStatus::Connected) {
            return;
        }
        let cooldown =
            (PHONE_COOLDOWN * 2u32.pow(conversation.phone_misses)).min(Duration::from_secs(600));
        if conversation
            .phone_answered
            .is_some_and(|answered| answered.elapsed() < cooldown)
        {
            return;
        }
        conversation.fetching_phone = true;
        conversation.phone_explicit = explicit;
        self.scroll_anchor = conversation
            .messages
            .first()
            .map(|oldest| oldest.id.clone());
        account.backend.send(Command::FetchOlder {
            chat: chat.to_owned(),
            explicit,
        });
    }

    fn mark_read(&mut self, chat: &str) {
        self.clear_chat_notifications(chat);
        self.reset_notification_throttle(chat);
        if let Some(known) = self.chat_mut(chat) {
            known.unread = 0;
            known.marked_unread = false;
        }
        // Clear local unread state regardless of receipt settings.
        self.backend.send(Command::MarkRead {
            chat: chat.to_owned(),
            receipts: self.account().settings.send_read_receipts,
        });
    }

    /// Reminds the reader about a chat with nothing pending. A chat that
    /// already counts unread messages keeps its number instead.
    fn mark_unread(&mut self, chat: &str) {
        let Some(known) = self.chat_mut(chat) else {
            return;
        };
        if known.unread > 0 {
            return;
        }
        known.marked_unread = true;
        self.backend.send(Command::MarkUnread(chat.to_owned()));
    }

    /// Cancels dialog-owned lookups while preserving shared-contact requests.
    fn cancel_contact_dialog_request(&mut self) {
        if self.new_contact_dialog_request.take().is_some() {
            self.new_contact_request = None;
            self.new_contact_pending = false;
        }
    }

    /// Records where the window now is, for the back and forward buttons. The
    /// place is built only when it changed, so an idle frame allocates
    /// nothing.
    fn record_location(&mut self) {
        if self
            .nav_history
            .is_current(&self.page, self.open_chat.as_deref())
        {
            return;
        }
        self.nav_history.visit(Location {
            page: self.page.clone(),
            chat: self.open_chat.clone(),
        });
    }

    /// Steps to the place visited before this one, as the mouse's back button
    /// does.
    fn navigate_back(&mut self, ctx: &egui::Context) {
        // A place the window cannot show is forgotten by `show_location`, and
        // the step moves on to the one before it, so one press never lands on
        // a place that shows nothing. Each call takes a place off the stack,
        // so this ends at the oldest.
        while let Some(location) = self.nav_history.back() {
            if self.show_location(location, ctx) {
                break;
            }
        }
    }

    /// Steps forward again after a back step, as the mouse's forward button
    /// does.
    fn navigate_forward(&mut self, ctx: &egui::Context) {
        while let Some(location) = self.nav_history.forward() {
            if self.show_location(location, ctx) {
                break;
            }
        }
    }

    /// Restores a place through the same actions the interface uses, so a
    /// draft is saved and a locked chat stays shut. Reports whether the window
    /// could show it; a place naming a chat it cannot open is forgotten.
    fn show_location(&mut self, location: Location, ctx: &egui::Context) -> bool {
        let wanted = location.chat.clone();
        // The chat first: opening one lands on the chat list, and the page
        // below puts the reader back on the page the place recorded.
        if self.open_chat != location.chat {
            // A place whose chat is gone closes the current chat instead.
            match location.chat {
                Some(chat) if self.chat(&chat).is_some() => self.apply(Action::OpenChat(chat), ctx),
                _ => self.apply(Action::CloseChat, ctx),
            }
        }
        if self.page != location.page {
            self.apply(Action::Open(location.page), ctx);
        }
        // A chat that is gone, or locked outside its folder, cannot be opened.
        // Keeping its place would make every back step land on it again and
        // replace the forward history.
        if let Some(chat) = wanted
            && self.open_chat.as_deref() != Some(chat.as_str())
        {
            self.nav_history.forget(&chat);
            return false;
        }
        true
    }

    fn open_chat(&mut self, id: ChatId) {
        if self.open_chat.as_ref() != Some(&id) {
            self.accessibility_announcements.clear();
        }
        // Notifications and stale actions must not open a locked chat from
        // outside the authenticated folder.
        if self.chat(&id).is_some_and(|chat| chat.locked) && !self.locked_folder_open() {
            return;
        }
        if let Some(crate::model::CommunityRole::Member { parent, .. }) =
            self.chat(&id).and_then(|chat| chat.community.clone())
        {
            self.collapsed_communities.remove(&parent);
        }
        if self.locked_folder && self.chat(&id).is_some_and(|chat| !chat.locked) {
            self.close_locked_folder();
            self.search.clear();
            self.search_hits.clear();
        }
        self.reset_notification_throttle(&id);
        if self.open_chat.as_deref() != Some(id.as_str()) {
            self.reaction_target = None;
            self.reaction_anchor = None;
            self.composer_tools_open = false;
            self.emoji_jump = None;
            self.jump_highlight = None;
            if let Some(previous) = self.open_chat.take() {
                self.save_attachment_draft(&previous);
                let draft = self.take_composer_text();
                // Discard an unfinished edit instead of keeping it as a draft.
                if self.editing.take().is_some() || draft.trim().is_empty() {
                    self.drafts.remove(&previous);
                    self.draft_mentions.remove(&previous);
                    self.composer_mentions.clear();
                } else {
                    self.drafts.insert(previous.clone(), draft);
                    let mentions = std::mem::take(&mut self.composer_mentions);
                    self.draft_mentions.insert(previous.clone(), mentions);
                }
                if let Some(target) = self.reply_to.take().filter(|target| target.chat.is_some()) {
                    self.private_reply_drafts.insert(previous.clone(), target);
                } else {
                    self.private_reply_drafts.remove(&previous);
                }
                self.stop_composing(&previous);
                let draft = self.drafts.get(&previous).cloned().unwrap_or_default();
                self.store_draft(&previous, &draft);
            }
            self.selection = None;
            self.unread_divider =
                self.chat(&id)
                    .filter(|chat| chat.unread > 0)
                    .map(|chat| UnreadDivider {
                        chat: id.clone(),
                        count: chat.unread,
                        placed: false,
                    });
            let text = self.drafts.remove(&id).unwrap_or_default();
            self.replace_composer_text(text);
            self.composer_mentions = self.draft_mentions.remove(&id).unwrap_or_default();
            let attachments = self.attachment_drafts.remove(&id).unwrap_or_default();
            self.pending = attachments.pending;
            // A search belongs to the chat it was typed in.
            self.close_chat_search();
            self.reply_to = attachments
                .quoting
                .or_else(|| self.private_reply_drafts.remove(&id));
            self.editing = None;
            // A starred row's pending quote belongs to its own chat too.
            if self
                .reply_when_loaded
                .as_ref()
                .is_some_and(|(chat, _)| chat != &id)
            {
                self.reply_when_loaded = None;
            }
            // A run of voice messages belongs to the chat it started in.
            self.voice_chat = None;
            self.voice_wanted = None;
            // A page request belongs to the chat it was pressed in.
            self.scroll_page = None;
            if let Some(conversation) = self.conversations.get_mut(&id) {
                conversation.key_scroll = None;
            }
        }
        self.emoji_start = None;
        self.mention_start = None;
        self.open_chat = Some(id.clone());
        self.page = Page::Chats;
        self.scroll_to_bottom = true;
        self.at_bottom = true;
        self.focus_composer = true;
        self.ensure_loaded(&id);
        // Read again on every open: a pin that expired while the chat was
        // closed leaves the line under the header.
        self.backend.send(Command::LoadPins(id.clone()));
        if self
            .conversations
            .get(&id)
            .is_some_and(|conversation| conversation.complete && conversation.messages.is_empty())
        {
            self.fetch_older(&id, false);
        }
        if self
            .chat(&id)
            .is_some_and(|chat| chat.unread > 0 || chat.marked_unread)
        {
            self.mark_read(&id);
        }
        if self.settings.last_chat.as_deref() != Some(id.as_str()) {
            self.settings.last_chat = Some(id.clone());
            self.mark_settings_dirty();
        }
        if self.account().settings.last_chat.as_deref() != Some(id.as_str()) {
            self.account_mut().settings.last_chat = Some(id);
            self.account_mut().mark_settings_dirty();
        }
    }

    /// Returns keyboard focus to the open conversation when no search or
    /// overlay is active.
    fn refocus_composer(&mut self, ctx: &egui::Context) {
        let search_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("chat-search")));
        if self.page == Page::Chats
            && self.dialog.is_none()
            && self.picker.is_none()
            && self.reaction_target.is_none()
            && self.recording.is_none()
            && self.open_chat.is_some()
            && self.search.trim().is_empty()
            && !self.focus_search
            && !search_focused
        {
            self.focus_composer = true;
        }
    }

    /// The most recent own text message in the open chat, for Arrow-Up
    /// editing. Non-text and revoked messages cannot be edited and are
    /// skipped.
    pub(crate) fn previous_own_editable(&self) -> Option<String> {
        let conversation = self.conversations.get(self.open_chat.as_deref()?)?;
        conversation
            .messages
            .iter()
            .rev()
            .find(|message| self.can_edit(message))
            .map(|message| message.id.clone())
    }

    /// Updates typing state after composer changes.
    pub fn note_keystroke(&mut self) {
        self.last_keystroke = Some(Instant::now());
        if !self.composing
            && self.account().settings.send_typing
            && let Some(chat) = self.open_chat.clone()
        {
            self.composing = true;
            self.backend.send(Command::Composing {
                chat,
                composing: true,
            });
        }
    }

    fn stop_composing(&mut self, chat: &str) {
        if self.composing {
            self.composing = false;
            self.backend.send(Command::Composing {
                chat: chat.to_owned(),
                composing: false,
            });
        }
        self.last_keystroke = None;
    }

    /// Keeps following outgoing messages only when the reader was already at
    /// the newest edge. Sending from older history must not lose their place.
    fn follow_outgoing(&mut self) {
        self.follow_sent_chat();
        if self.at_bottom {
            self.scroll_to_bottom = true;
        }
    }

    /// Sends the spot the dialog read. A refusal brings the dialog back with
    /// the spot in it, since the composer cannot hold one.
    fn send_location(
        &mut self,
        chat: ChatId,
        latitude: f64,
        longitude: f64,
        quoting: Option<crate::model::ReplyTarget>,
    ) {
        self.backend.send(Command::SendLocation {
            chat,
            latitude,
            longitude,
            quoting,
        });
        self.follow_outgoing();
    }

    /// Scrolls the chat list to the top after sending unless the reader opted
    /// to keep their position or the favorites order keeps the chat in place.
    fn follow_sent_chat(&mut self) {
        if !self.settings.keep_chat_list_position && !self.favorites_order() {
            self.scroll_chats_to_top = true;
        }
    }

    /// Dispatches composed text with its explicit quote target and restores it on refusal.
    fn send_text(
        &mut self,
        chat: ChatId,
        text: String,
        quoting: Option<crate::model::ReplyTarget>,
    ) {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        if let Some(id) = self.editing.as_ref() {
            let eligible = self.open_chat.as_ref() == Some(&chat)
                && self
                    .conversations
                    .get(&chat)
                    .and_then(|conversation| conversation.message(id))
                    .is_some_and(|message| self.can_edit(message));
            if !eligible {
                self.editing = None;
                self.composer = text;
                self.toast_error(
                    crate::i18n::gettext(self.locale, "This message can no longer be edited")
                        .into_owned(),
                );
                self.focus_composer = true;
                return;
            }
        }
        let draft = EditDraft {
            text: text.clone(),
            mentions: self.composer_mentions.clone(),
        };
        let (text, mentions) = self.encode_composer_mentions(&chat, text);
        self.emoji_start = None;
        self.mention_start = None;
        self.stop_composing(&chat);
        if let Some(id) = self.editing.take() {
            self.refused_edits
                .retain(|(target_chat, target_id, _)| target_chat != &chat || target_id != &id);
            // The worker publishes the updated row only after the send succeeds.
            self.backend.send(Command::EditText {
                chat,
                id,
                text,
                mentions,
                draft,
            });
            return;
        }
        // The text is on its way, so there is nothing left to restore.
        self.store_draft(&chat, "");
        self.backend.send(Command::SendText {
            chat,
            text,
            quoting,
            mentions,
        });
        self.follow_outgoing();
    }

    /// Snapshots selected mentions before the view queues an attachment send.
    pub(crate) fn composer_mentions(&self) -> Vec<ComposerMention> {
        self.composer_mentions.clone()
    }

    /// Replaces selected display-name mentions with WhatsApp's `@user`
    /// tokens and returns the JIDs for message context.
    fn encode_composer_mentions(&mut self, chat: &str, mut text: String) -> (String, Vec<String>) {
        let participants = self
            .chat(chat)
            .map(|chat| chat.participants.clone())
            .unwrap_or_default();
        let selected = std::mem::take(&mut self.composer_mentions);
        let mut mentions = Vec::new();
        for mention in selected {
            if !participants.iter().any(|id| id == &mention.id) {
                continue;
            }
            let Some(user) = mention.id.split('@').next().filter(|user| !user.is_empty()) else {
                continue;
            };
            let shown = format!("@{}", mention.name);
            if let Some(at) = find_named_mention(&text, &shown) {
                text.replace_range(at..at + shown.len(), &format!("@{user}"));
                if !mentions.iter().any(|id| id == &mention.id) {
                    mentions.push(mention.id);
                }
            }
        }
        // Preserve mentions in an edited draft that already contains wire
        // tokens, even when it did not originate in this composer session.
        for id in participants {
            let Some(user) = id.split('@').next().filter(|user| !user.is_empty()) else {
                continue;
            };
            if contains_mention_token(&text, user) && !mentions.iter().any(|known| known == &id) {
                mentions.push(id);
            }
        }
        (text, mentions)
    }

    /// Parks attachments and reply quotes before leaving a chat or account.
    fn save_attachment_draft(&mut self, chat: &str) {
        if self.pending.is_empty() && self.reply_to.is_none() {
            self.attachment_drafts.remove(chat);
        } else {
            let draft = AttachmentDraft {
                pending: std::mem::take(&mut self.pending),
                quoting: self.reply_to.take(),
            };
            self.attachment_drafts.insert(chat.to_owned(), draft);
        }
    }

    /// Adds files to the open chat's composer.
    fn stage_files(&mut self, paths: Vec<PathBuf>) {
        if self.open_chat.is_none() {
            self.toast_error("Open a chat first");
            return;
        }
        for path in paths {
            self.pending.push(Pending::File(path));
        }
        self.focus_composer = true;
    }

    /// Sends pending files, attaching the caption to the first.
    fn send_pending(
        &mut self,
        account: AccountId,
        chat: ChatId,
        caption: String,
        selected: Vec<ComposerMention>,
    ) {
        // A queued action cannot consume another chat's or account's draft.
        let active = self.account().id.clone();
        let Some(target) = self.accounts.iter_mut().find(|known| known.id == account) else {
            return;
        };
        if account != active || target.open_chat.as_deref() != Some(chat.as_str()) {
            if target.attachment_drafts.contains_key(&chat)
                && !caption.trim().is_empty()
                && target
                    .drafts
                    .get(&chat)
                    .is_none_or(|text| text.trim().is_empty())
            {
                target.backend.send(Command::SaveDraft {
                    chat: chat.clone(),
                    text: caption.clone(),
                });
                target.drafts.insert(chat.clone(), caption);
                target.draft_mentions.insert(chat, selected);
            }
            return;
        }
        if self.pending.is_empty() {
            return;
        }
        self.drafts.remove(&chat);
        self.draft_mentions.remove(&chat);
        self.store_draft(&chat, "");
        let original_caption = caption.clone();
        let original_mentions = selected.clone();
        self.composer_mentions = selected;
        // The reply travels with the first attachment, like the caption.
        let mut quoting = self.reply_to.take();
        let caption = caption.trim().to_owned();
        let (caption, mentions) = self.encode_composer_mentions(&chat, caption);
        let caption = Some(caption).filter(|text| !text.is_empty());
        let mut caption = caption;
        let mut mentions = mentions;
        self.emoji_start = None;
        self.mention_start = None;
        // Dispatch in composer order. The worker's upload queue preserves this order.
        for item in std::mem::take(&mut self.pending) {
            let (item, item_caption, item_quote, item_mentions) = match item {
                Pending::Recovered {
                    item,
                    caption: restored,
                    quoting: restored_quote,
                    mentions: restored_mentions,
                    composer_owned,
                } => {
                    if composer_owned {
                        let changed = caption != restored;
                        let outgoing_mentions = if changed {
                            std::mem::take(&mut mentions)
                        } else {
                            restored_mentions
                        };
                        (*item, caption.take(), quoting.take(), outgoing_mentions)
                    } else {
                        if caption == restored {
                            caption = None;
                        }
                        if quoting == restored_quote {
                            quoting = None;
                        }
                        (*item, restored, restored_quote, restored_mentions)
                    }
                }
                item => (
                    item,
                    caption.take(),
                    quoting.take(),
                    std::mem::take(&mut mentions),
                ),
            };
            let command = match item {
                Pending::Picture {
                    width,
                    height,
                    rgba,
                    ..
                } => Command::SendImage {
                    chat: chat.clone(),
                    width: width as u32,
                    height: height as u32,
                    rgba: std::sync::Arc::try_unwrap(rgba).unwrap_or_else(|arc| (*arc).clone()),
                    caption: item_caption,
                    mentions: item_mentions,
                    quoting: item_quote,
                },
                Pending::File(path) => Command::SendFiles {
                    chat: chat.clone(),
                    paths: vec![path],
                    caption: item_caption,
                    mentions: item_mentions,
                    quoting: item_quote,
                },
                Pending::Document(item) => Command::SendDocument {
                    chat: chat.clone(),
                    source: match *item {
                        Pending::File(path) => crate::model::DocumentSource::File(path),
                        Pending::Picture {
                            width,
                            height,
                            rgba,
                            ..
                        } => crate::model::DocumentSource::Pixels {
                            width: width as u32,
                            height: height as u32,
                            rgba: std::sync::Arc::try_unwrap(rgba)
                                .unwrap_or_else(|arc| (*arc).clone()),
                        },
                        Pending::Document(_) | Pending::Recovered { .. } => {
                            unreachable!("document choices never nest")
                        }
                    },
                    caption: item_caption,
                    mentions: item_mentions,
                    quoting: item_quote,
                },
                Pending::Recovered { .. } => {
                    unreachable!("recovery wrappers are removed before dispatch")
                }
            };
            self.backend.send(command);
        }
        if caption.is_some() {
            self.replace_composer_text(original_caption);
            self.composer_mentions = original_mentions.clone();
            let draft_text = self.composer.clone();
            self.drafts.insert(chat.clone(), draft_text);
            self.draft_mentions.insert(chat.clone(), original_mentions);
            self.store_draft(&chat, &self.composer);
        }
        if self.reply_to.is_none() {
            self.reply_to = quoting;
        }
        self.follow_outgoing();
    }

    #[allow(dead_code)]
    fn send_files(&mut self, paths: Vec<PathBuf>) {
        let Some(chat) = self.open_chat.clone() else {
            self.toast_error("Open a chat first");
            return;
        };
        if paths.is_empty() {
            return;
        }
        self.toast(format!(
            "Sending {} file{}…",
            paths.len(),
            if paths.len() == 1 { "" } else { "s" }
        ));
        self.backend.send(Command::SendFiles {
            chat,
            paths,
            caption: None,
            mentions: Vec::new(),
            quoting: None,
        });
        self.follow_outgoing();
    }

    fn tick(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        if self.composing
            && let Some(last) = self.last_keystroke
            && now.duration_since(last) > COMPOSING_TIMEOUT
            && let Some(chat) = self.open_chat.clone()
        {
            self.stop_composing(&chat);
        }
        for typers in self.typing.values_mut() {
            typers.retain(|(_, since)| now.duration_since(*since) < TYPING_TIMEOUT);
        }
        self.typing.retain(|_, typers| !typers.is_empty());
        self.toasts.retain(|toast| {
            toast.kind == ToastKind::Error || toast.created.elapsed() < toast.lifetime()
        });
        if self.settings.check_for_updates
            && !self.backend.is_offline()
            && self
                .last_update_check
                .is_none_or(|at| at.elapsed() >= crate::updates::CHECK_INTERVAL)
        {
            self.last_update_check = Some(now);
            self.backend.send(Command::CheckForUpdates);
        }
        self.maybe_download_update();
        if self.settings_dirty && self.last_settings_save.elapsed() > Duration::from_secs(2) {
            self.save_settings();
        }
        for account in &mut self.accounts {
            if account.settings_dirty {
                account.save_settings();
            }
        }
        if !self.typing.is_empty() || self.composing {
            ctx.request_repaint_after(Duration::from_secs(1));
        }
        // A pin that has run out its seven days is no longer on the message,
        // and the worker only speaks when a pin changes. Drop it from the
        // cache the footer and the message menu read, so an expired pin stops
        // painting its mark and stops offering Unpin.
        let stamp = crate::util::now();
        for (chat, rows) in self.chat_pins.clone() {
            let Some(ids) = self.pins.get_mut(&chat) else {
                continue;
            };
            ids.retain(|id| {
                rows.iter()
                    .any(|row| row.id == *id && row.expires_at > stamp)
            });
        }
    }

    fn inspect_update(&mut self) {
        if self.update_support.is_none() && !self.update_inspecting {
            self.update_inspecting = true;
            self.backend.send(Command::InspectUpdate);
        }
    }

    fn maybe_download_update(&mut self) {
        if !self.settings.check_for_updates
            || !self.settings.download_updates_automatically
            || self.update.is_none()
            || !matches!(self.update_download, crate::updates::DownloadState::Idle)
        {
            return;
        }
        self.inspect_update();
        if matches!(self.update_support, Some(Ok(_))) {
            self.download_update();
        }
    }

    fn download_update(&mut self) {
        if !matches!(
            self.update_download,
            crate::updates::DownloadState::Idle | crate::updates::DownloadState::Failed(_)
        ) || !matches!(self.update_support, Some(Ok(_)))
        {
            return;
        }
        if let Some(release) = self.update.clone() {
            self.update_download = crate::updates::DownloadState::Downloading {
                received: 0,
                total: 0,
            };
            self.backend.send(Command::DownloadUpdate {
                release,
                source: crate::updates::Source::github(),
            });
        }
    }

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    fn save_settings(&mut self) {
        self.settings_dirty = false;
        self.last_settings_save = Instant::now();
        if let Err(error) = self.settings.save(&self.dirs.settings_file()) {
            log::warn!("could not save settings: {error}");
        }
    }

    /// Remembers the window's size, position and maximized state in the
    /// settings, in logical pixels, so a window saved while zoomed opens at
    /// the same size; see [`fastframe_shell::window::Geometry`].
    fn sync_window_state(&mut self, ctx: &egui::Context) {
        let mut geometry = self.settings.window_geometry();
        if geometry.remember(ctx) {
            self.settings.set_window_geometry(geometry);
            self.mark_settings_dirty();
        }
    }

    /// What must happen in every pass of an open window, including the
    /// passes eframe runs without `ui` for a minimized or covered one. Call it
    /// from `eframe::App::logic`, after [`Self::background_frame`].
    pub fn window_logic(&mut self, ctx: &egui::Context) {
        self.sync_window_state(ctx);
        // Close the window and continue headless when background mode is
        // enabled. Decided here rather than in `frame_ui`, which eframe skips
        // for a minimized window: closing one from the taskbar quit instead.
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quit_requested
            && self.hides_to_tray()
        {
            self.hide_intent = true;
        }
    }

    pub fn load_custom_themes(&mut self) {
        let waker = self.waker.clone();
        self.custom_themes.start(
            self.dirs.config.join("themes"),
            self.settings.custom_theme.clone(),
            &fastframe_theme::Waker::new(move || waker.wake()),
        );
    }

    fn poll_custom_themes(&mut self) {
        if self.custom_themes.needs_reload() {
            self.load_custom_themes();
        }
        if self.custom_themes.poll() {
            self.cache_custom_themes();
        }
    }

    /// Keeps the selected and the desktop's palettes in settings, so the
    /// last usable appearance survives a missing file or a slow scan.
    fn cache_custom_themes(&mut self) {
        let mut changed = false;
        if let Some(filename) = &self.settings.custom_theme
            && let Some(theme) = self.custom_themes.find(filename)
            && self.settings.custom_theme_cache.as_ref() != Some(theme)
        {
            self.settings.custom_theme_cache = Some(theme.clone());
            changed = true;
        }
        if self.custom_themes.follows_omarchy() {
            if let Some(theme) = self.custom_themes.system_theme()
                && self.settings.system_theme_cache.as_ref() != Some(theme)
            {
                self.settings.system_theme_cache = Some(theme.clone());
                changed = true;
            }
        } else if self.settings.system_theme_cache.take().is_some() {
            changed = true;
        }
        if changed {
            self.mark_settings_dirty();
        }
    }

    fn apply_font(&mut self, ctx: &egui::Context) {
        if crate::theme::set_font(
            ctx,
            self.settings.font,
            self.settings.custom_font.as_deref(),
        )
        .is_err()
        {
            self.remove_custom_font();
            self.mark_settings_dirty();
            self.toast_error(
                crate::i18n::gettext(self.locale, "Could not use this font").into_owned(),
            );
        }
        ctx.request_repaint();
    }

    fn remove_custom_font(&mut self) {
        self.settings.custom_font = None;
        let request = self.claim_custom_font_request();
        let _ = crate::custom_font::cleanup(self.dirs.clone(), None, request);
    }

    fn claim_custom_font_request(&mut self) -> u64 {
        self.custom_font_request = self.custom_font_request.wrapping_add(1);
        crate::custom_font::claim(self.custom_font_request);
        self.custom_font_request
    }

    fn apply_theme(&mut self, ctx: &egui::Context) {
        let preference = self.settings.cached_palette().map_or_else(
            || match self.settings.theme {
                ThemeChoice::Dark => egui::ThemePreference::Dark,
                ThemeChoice::Light => egui::ThemePreference::Light,
                // Messages follows the system's appearance, as Messages does.
                ThemeChoice::System | ThemeChoice::Messages => egui::ThemePreference::System,
            },
            |palette| {
                if palette.dark {
                    egui::ThemePreference::Dark
                } else {
                    egui::ThemePreference::Light
                }
            },
        );
        if !self.zoom_applied {
            ctx.set_zoom_factor(self.settings.zoom);
            self.zoom_applied = true;
        }
        // Resolved here rather than after `set_theme`, so a change can keep the
        // old colours, egui's own controls included, while it is revealed.
        let dark = match preference {
            egui::ThemePreference::Dark => true,
            egui::ThemePreference::Light => false,
            egui::ThemePreference::System => {
                ctx.system_theme()
                    .unwrap_or_else(|| ctx.options(|options| options.fallback_theme))
                    == egui::Theme::Dark
            }
        };
        let palette = self.settings.cached_palette().unwrap_or_else(|| {
            if self.settings.theme == ThemeChoice::Messages {
                if dark {
                    Palette::messages()
                } else {
                    Palette::messages_light()
                }
            } else if dark {
                Palette::dark()
            } else {
                Palette::light()
            }
        });
        if crate::theme::apply_text_rendering_change(ctx) {
            self.applied_dark = None;
        }
        // A change of colours after the window's first is revealed from the
        // middle outwards, as Omarchy does; the old palette stays until the
        // window's picture of it arrives.
        // A vibrant window's picture misses the desktop behind it, so changes
        // to or from one switch at once.
        if self.applied_dark.is_some()
            && self.palette != palette
            && self.reveal_theme_changes
            && !self.palette.vibrant()
            && !palette.vibrant()
        {
            self.theme_transition.begin(ctx);
            if self.theme_transition.holding(ctx) {
                return;
            }
        }
        // Use the same preference for our palette and egui's native controls.
        ctx.set_theme(preference);
        if self.applied_dark.is_none() || self.palette != palette {
            self.palette = palette;
            crate::theme::apply(ctx, &self.palette);
            self.applied_dark = Some(dark);
        }
    }

    fn apply_actions(&mut self, ctx: &egui::Context) {
        let deferred = std::mem::take(&mut self.deferred_account_actions);
        for (id, action) in deferred {
            let Some(index) = self.accounts.iter().position(|account| account.id == id) else {
                continue;
            };
            let previous = self.active;
            self.active = index;
            self.apply(action, ctx);
            self.active = previous;
        }
        let mut actions = std::mem::take(&mut self.actions);
        while !actions.is_empty() {
            for action in actions.drain(..) {
                self.apply(action, ctx);
                self.record_location();
            }
            actions = std::mem::take(&mut self.actions);
        }
        #[cfg(any(test, feature = "demo"))]
        crate::demo::queued::respond(self, ctx, Instant::now());
        // A frame with no queued action still fixes the opening place, so the
        // first navigation has somewhere to return to.
        self.record_location();
    }

    /// Resets egui drag state only after native drag success; failures retain input state.
    fn finish_attachment_drag(
        &mut self,
        ctx: &egui::Context,
        result: Result<(), crate::file_drag::Failure>,
    ) {
        match result {
            Ok(()) => {
                // Only a native drag loop can consume the button release.
                ctx.stop_dragging();
                ctx.input_mut(|input| input.pointer = Default::default());
            }
            Err(error) => self.toast_error(crate::file_drag::failure_message(self.locale, &error)),
        }
        ctx.request_repaint();
    }

    /// Applies queued view actions after drawing, routing state changes and backend commands.
    fn apply(&mut self, action: Action, ctx: &egui::Context) {
        self.apply_inner(action, ctx);
        self.sync_automation_authority();
    }

    fn apply_inner(&mut self, action: Action, ctx: &egui::Context) {
        if self.app_lock.is_locked() && !allowed_while_locked(&action) {
            // A clicked notification opens its message once unlocked; the
            // rest would show or change what the lock hides.
            if let Action::OpenMessage { chat, message } = action {
                self.app_lock.deferred = Some(crate::notify::NotificationTarget {
                    account: self.account().id.clone(),
                    chat,
                    message,
                });
            }
            return;
        }
        let reply_send = match &action {
            Action::SendText { chat, quoting, .. } => Some((chat.as_str(), quoting.as_ref())),
            Action::SendPending { chat, .. } => Some((chat.as_str(), self.reply_to.as_ref())),
            Action::SendGif(_) | Action::SendSticker(_) | Action::MakeSticker { send: true } => {
                self.open_chat
                    .as_deref()
                    .map(|chat| (chat, self.reply_to.as_ref()))
            }
            _ => None,
        };
        if let Some((chat, Some(target))) = reply_send
            && !self.reply_source_visible(target, chat)
        {
            let chat = chat.to_owned();
            let target = target.clone();
            let text = match &action {
                Action::SendText { text, .. } => text.clone(),
                Action::SendPending { caption, .. } => caption.clone(),
                _ => String::new(),
            };
            self.send_refused(
                chat,
                Some(target),
                Unsent::Text(text),
                Refusal::QuoteUnavailable,
            );
            return;
        }
        let remote_target = match &action {
            Action::DeleteForEveryone { chat, id } | Action::DeleteForMe { chat, id } => {
                Some((chat.as_str(), id.as_str()))
            }
            Action::Edit(id) | Action::Reply(id) => {
                self.open_chat.as_deref().map(|chat| (chat, id.as_str()))
            }
            _ => None,
        };
        if let Some((chat, id)) = remote_target
            && self
                .conversations
                .get(chat)
                .and_then(|conversation| conversation.message(id))
                .is_some_and(|row| {
                    row.from_me
                        && row.status.is_local()
                        && !(row.status == Delivery::Unconfirmed
                            && matches!(action, Action::DeleteForMe { .. }))
                })
        {
            return;
        }
        let automatic_download = matches!(action, Action::DownloadAutomatic { .. });
        match action {
            Action::ToggleCallLog => {
                self.call_log_open = !self.call_log_open;
                if self.call_log_open {
                    self.backend.send(Command::LoadCallLog);
                }
            }
            Action::StartCall(chat) => {
                if self.accounts.iter().any(|account| {
                    account.call_preparing
                        || account.pending_call_request.is_some()
                        || account
                            .call
                            .as_ref()
                            .is_some_and(|call| call.phase.is_live())
                }) {
                    self.toast_error("Finish the current call first");
                } else if !self.is_connected() {
                    self.toast_error("Wait for WhatsApp to connect before starting a call");
                } else {
                    self.call_preparing = true;
                    let request = crate::calls::CallRequest::default();
                    self.pending_call_request = Some(request.clone());
                    log::info!("call request queued: voice");
                    self.backend.send(Command::StartCall { chat, request });
                }
            }
            Action::StartVideoCall(chat) => {
                if self.accounts.iter().any(|account| {
                    account.call_preparing
                        || account.pending_call_request.is_some()
                        || account
                            .call
                            .as_ref()
                            .is_some_and(|call| call.phase.is_live())
                }) {
                    self.toast_error("Finish the current call first");
                } else if !self.is_connected() {
                    self.toast_error("Wait for WhatsApp to connect before starting a call");
                } else {
                    self.call_preparing = true;
                    let request = crate::calls::CallRequest::default();
                    self.pending_call_request = Some(request.clone());
                    log::info!("call request queued: video");
                    self.backend.send(Command::StartVideoCall { chat, request });
                }
            }
            Action::AnswerCall => {
                let active = self.account().id.clone();
                if self.accounts.iter().any(|account| {
                    account.id != active
                        && (account.call_preparing
                            || account
                                .call
                                .as_ref()
                                .is_some_and(|call| call.phase.is_answered()))
                }) {
                    self.toast_error("Finish the current call first");
                } else {
                    self.backend.send(Command::AnswerCall);
                }
            }
            Action::AnswerVideoCall => {
                let active = self.account().id.clone();
                if self.accounts.iter().any(|account| {
                    account.id != active
                        && (account.call_preparing
                            || account
                                .call
                                .as_ref()
                                .is_some_and(|call| call.phase.is_answered()))
                }) {
                    self.toast_error("Finish the current call first");
                } else {
                    self.backend.send(Command::AnswerVideoCall);
                }
            }
            Action::DeclineCall => self.backend.send(Command::DeclineCall),
            Action::HangupCall => {
                if let Some(request) = &self.pending_call_request {
                    request.cancel();
                    self.call_preparing = false;
                }
                self.backend.send(Command::HangupCall);
            }
            Action::SetCallMuted(muted) => self.backend.send(Command::SetCallMuted(muted)),
            Action::SetCallVideo(enabled) => self.backend.send(Command::SetCallVideo(enabled)),
            Action::RefreshAudioDevices => self.backend.send(Command::RefreshAudioDevices),
            Action::SetCallMicrophone(device) => {
                self.account_mut().settings.call_microphone = device.clone();
                self.account_mut().mark_settings_dirty();
                self.backend.send(Command::SetCallMicrophone(device));
            }
            Action::SetCallSpeaker(device) => {
                self.account_mut().settings.call_speaker = device.clone();
                self.account_mut().mark_settings_dirty();
                self.backend.send(Command::SetCallSpeaker(device));
            }
            Action::SetCallCamera(device) => {
                self.account_mut().settings.call_camera = device.clone();
                self.account_mut().mark_settings_dirty();
                self.backend.send(Command::SetCallCamera(device));
            }
            Action::SetCacheRetention(days) => {
                let days = days.filter(|days| (1..=3650).contains(days));
                self.account_mut().settings.cache_retention_days = days;
                self.account_mut().mark_settings_dirty();
                self.backend.send(Command::SetCacheRetention(days));
            }

            Action::Open(page) => {
                let opens_chats = page == Page::Chats;
                // Privacy can change on the phone at any time, and nothing
                // announces it: read it again whenever Settings opens.
                if page == Page::Settings && self.page != Page::Settings && self.is_connected() {
                    self.backend.send(Command::FetchAccountPrivacy);
                }
                // Each visit to Settings reads the message count again, even
                // when the last sizes were read less than a minute ago.
                if page == Page::Settings && self.page != Page::Settings {
                    self.storage_stats_counted = false;
                    self.storage_stats_at = None;
                    self.storage_stats_asked = false;
                }
                // Typed passwords do not wait in a form nobody sees.
                if page != Page::Settings && !self.app_lock.checking() {
                    self.app_lock.form = None;
                }
                if matches!(
                    self.dialog,
                    Some(Dialog::NewContact | Dialog::MessageNumber)
                ) {
                    self.cancel_contact_dialog_request();
                }
                self.page = page;
                self.dialog = None;
                self.emoji_start = None;
                self.mention_start = None;
                if opens_chats {
                    // Settings open unfiltered next time.
                    self.settings_search.clear();
                    self.refocus_composer(ctx);
                }
            }
            Action::ToggleSettings => {
                // The button that opened settings closes them again, and
                // closing lands on what was showing, exactly as Escape does.
                let page = if self.page == Page::Settings {
                    Page::Chats
                } else {
                    Page::Settings
                };
                self.apply(Action::Open(page), ctx);
            }
            Action::OpenChat(id) => self.open_chat(id),
            Action::NavigateBack => self.navigate_back(ctx),
            Action::NavigateForward => self.navigate_forward(ctx),
            Action::StartChat {
                id,
                name,
                dismiss_dialog,
            } => {
                if self.chat(&id).is_none() {
                    self.chats.push(Chat::new(id.clone(), name.clone()));
                    self.backend.send(Command::EnsureChat {
                        chat: id.clone(),
                        name,
                    });
                }
                self.open_chat(id);
                if dismiss_dialog {
                    self.dialog = None;
                }
            }
            Action::MessageYourself => {
                if let Some(id) = self.me.clone() {
                    if self.chat(&id).is_some_and(|chat| chat.locked) && !self.locked_folder_open()
                    {
                        self.apply(Action::OpenLockedFolder, ctx);
                    } else {
                        self.apply(
                            Action::StartChat {
                                id,
                                name: "You".to_owned(),
                                dismiss_dialog: true,
                            },
                            ctx,
                        );
                    }
                }
            }
            Action::OpenMessage { chat, message } => {
                // A stored cross-chat quote may outlive its locally deleted source.
                if self.chat(&chat).is_none() {
                    return;
                }
                // A result picked in the search pane keeps the keyboard in
                // the pane, so the arrows can walk on to the next one.
                let from_pane =
                    self.chat_search_visible() && self.open_chat.as_deref() == Some(chat.as_str());
                self.open_chat(chat.clone());
                // A locked chat stays shut outside its folder, even for a
                // notification clicked before the chat was locked.
                if self.open_chat.as_deref() != Some(chat.as_str()) {
                    return;
                }
                if from_pane {
                    self.focus_composer = false;
                }
                // Keep the search result, not the chat end, in view.
                self.scroll_to_bottom = false;
                self.at_bottom = false;
                self.scroll_anchor = Some(message.clone());
                self.jump_highlight = Some(JumpHighlight::new(chat.clone(), message.clone()));
                let load = {
                    let conversation = self.conversations.entry(chat.clone()).or_default();
                    if conversation.message(&message).is_none() && !conversation.loading_older {
                        conversation.messages.first().map(|oldest| {
                            conversation.loading_older = true;
                            (oldest.timestamp, oldest.id.clone())
                        })
                    } else {
                        None
                    }
                };
                if let Some(before) = load {
                    self.backend.send(Command::LoadUntil {
                        chat,
                        id: message,
                        before,
                    });
                }
            }
            Action::CloseChat => {
                if let Some(chat) = self.open_chat.take() {
                    self.save_attachment_draft(&chat);
                    if let Some(target) =
                        self.reply_to.take().filter(|target| target.chat.is_some())
                    {
                        self.private_reply_drafts.insert(chat.clone(), target);
                    } else {
                        self.private_reply_drafts.remove(&chat);
                    }
                    self.stop_composing(&chat);
                    let draft = self.take_composer_text();
                    if self.editing.take().is_none() && !draft.trim().is_empty() {
                        self.drafts.insert(chat.clone(), draft);
                        let mentions = std::mem::take(&mut self.composer_mentions);
                        self.draft_mentions.insert(chat.clone(), mentions);
                    } else {
                        self.composer_mentions.clear();
                    }
                    self.store_draft(&chat, self.drafts.get(&chat).map_or("", String::as_str));
                }
                self.reply_to = None;
                self.emoji_start = None;
                self.mention_start = None;
                self.reaction_target = None;
                self.reaction_anchor = None;
                self.emoji_jump = None;
            }
            Action::SendText {
                chat,
                text,
                quoting,
            } => {
                self.send_text(chat, text, quoting);
                self.reply_to = None;
            }
            Action::SendLocation {
                chat,
                latitude,
                longitude,
                quoting,
            } => {
                self.send_location(chat, latitude, longitude, quoting);
                self.reply_to = None;
                self.location_draft.clear();
                self.dialog = None;
            }
            Action::RefreshPoll { chat, message } => {
                if let Some(row) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|chat| chat.message_mut(&message))
                    && let Content::Poll { state, .. } = &mut row.content
                {
                    state.refreshing = true;
                }
                self.backend.send(Command::RefreshPoll { chat, message });
            }
            Action::ReplyInteractive {
                chat,
                message,
                button,
                choice,
            } => {
                if self.link.is_connected() && self.chat(&chat).is_some_and(|chat| chat.can_send())
                {
                    self.backend.send(Command::ReplyInteractive {
                        chat,
                        message,
                        button,
                        choice,
                    });
                    self.follow_outgoing();
                }
            }
            Action::CreatePoll { chat, draft } => {
                if !self.poll_creating {
                    match draft.validated() {
                        Ok(draft) => {
                            self.poll_creating = true;
                            self.backend.send(Command::CreatePoll { chat, draft });
                            self.follow_sent_chat();
                        }
                        Err(error) => self.toast_error(error),
                    }
                }
            }
            Action::VotePoll {
                chat,
                message,
                choices,
            } => {
                if self.poll_voting.insert((chat.clone(), message.clone())) {
                    self.backend.send(Command::VotePoll {
                        chat,
                        message,
                        choices,
                    });
                }
            }
            Action::Composing { chat, composing } => {
                if composing {
                    self.note_keystroke();
                } else {
                    self.stop_composing(&chat);
                }
            }
            Action::MarkRead(chat) => self.mark_read(&chat),
            Action::MarkUnread(chat) => self.mark_unread(&chat),
            Action::LoadOlder { chat, explicit } => self.load_older(&chat, explicit),
            Action::FetchOlder(chat) => self.fetch_older(&chat, true),
            Action::ReloadHistory { chat, message } => {
                if !self.is_connected() {
                    return;
                }
                let Some(conversation) = self.conversations.get_mut(&chat) else {
                    return;
                };
                if conversation.fetching_phone {
                    self.toast("History is already being requested for this chat");
                    return;
                }
                if conversation.message(&message).is_none() {
                    return;
                }
                conversation.fetching_phone = true;
                conversation.phone_explicit = true;
                self.backend.send(Command::ReloadHistory { chat, message });
                self.toast("Requesting earlier messages from your phone");
            }
            Action::ToggleMotion { chat, message } => {
                let playing = (chat, message);
                if self.motion_playing.as_ref() == Some(&playing) {
                    self.motion_playing = None;
                    return;
                }
                let (chat, message) = playing.clone();
                self.motion_playing = Some(playing);
                if self
                    .motion_mut(&chat, &message)
                    .is_some_and(|motion| motion.path.is_none())
                {
                    self.actions.push(Action::DownloadMotion { chat, message });
                }
            }
            Action::DownloadMotion { chat, message } => {
                let Some(motion) = self.motion_mut(&chat, &message) else {
                    return;
                };
                if matches!(
                    motion.state,
                    MediaState::Downloading | MediaState::Transferring { .. }
                ) {
                    return;
                }
                motion.state = MediaState::Downloading;
                self.backend.send(Command::DownloadMotion { chat, message });
            }
            Action::Download {
                card,
                chat,
                message,
            }
            | Action::DownloadAutomatic {
                card,
                chat,
                message,
            } => {
                self.hydrate_gallery_message(&chat, &message);
                let Some(media) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|conversation| conversation.message_mut(&message))
                    .and_then(|message| message.content.media_at_mut(card))
                else {
                    return;
                };
                if media.size
                    > if automatic_download {
                        crate::model::ATTACHMENT_DOWNLOAD_LIMIT
                    } else {
                        crate::model::MANUAL_DOWNLOAD_LIMIT
                    }
                {
                    media.state =
                        MediaState::Failed("This attachment exceeds the download limit".into());
                    return;
                }
                if matches!(
                    media.state,
                    MediaState::Downloading | MediaState::Transferring { .. }
                ) {
                    return;
                }
                media.state = MediaState::Downloading;
                self.backend.send(Command::Download {
                    explicit: !automatic_download,
                    card,
                    chat,
                    message,
                });
            }
            Action::CancelUpload(token) => self.backend.send(Command::CancelUpload(token)),
            Action::CancelDownload {
                card,
                chat,
                message,
            } => self.backend.send(Command::CancelDownload {
                card,
                chat,
                message,
            }),
            Action::OpenGallery { chat } => {
                if self.gallery_accessible(&chat) {
                    self.image_preview = None;
                    self.video.stop();
                    self.request_gallery(&chat, true);
                    self.gallery.tab = Some(crate::model::MediaTab::Media);
                    self.dialog = Some(Dialog::ChatInfo(chat));
                }
            }
            Action::GalleryTab(tab) => self.gallery.tab = tab,
            Action::RefreshGallery => {
                if let Some(chat) = self.gallery.chat.clone() {
                    self.request_gallery(&chat, true);
                }
            }
            Action::PreviewMedia { chat, message } => self.preview_media(chat, message, ctx),
            Action::UnreadableMedia {
                chat,
                message,
                path,
            } => {
                // The file may come back under the same name, which must be
                // decoded afresh rather than answered with this failure.
                crate::animation::forget(ctx, &path);
                crate::image_cache::forget(ctx, &crate::util::image_uri(&path));
                let Some(media) = self
                    .conversations
                    .get_mut(&chat)
                    .and_then(|conversation| conversation.message_mut(&message))
                    .and_then(|message| message.content.media_at_mut(None))
                    .filter(|media| media.path.as_ref() == Some(&path))
                else {
                    return;
                };
                media.path = None;
                if path.is_file() {
                    // A file that does not decode is not fetched again on its
                    // own, or one that never decodes would download forever:
                    // the bubble offers the retry, as a failed download does.
                    log::warn!("a downloaded attachment could not be displayed");
                    media.state =
                        MediaState::Failed("Zap Faster could not display this file".into());
                } else if !matches!(media.state, MediaState::Downloading) {
                    // Gone from the disk: it downloads again like any
                    // attachment that was never downloaded.
                    media.state = MediaState::Idle;
                }
            }
            Action::PreviewImage(path) => {
                if crate::safety::can_preview_image(&path) && path.is_file() {
                    let chat = self.open_chat.clone();
                    let message = chat
                        .as_ref()
                        .and_then(|chat| self.conversations.get(chat))
                        .and_then(|c| {
                            c.messages.iter().find(|row| {
                                row.content.media().and_then(|m| m.path.as_deref())
                                    == Some(path.as_path())
                            })
                        })
                        .map(|row| row.id.clone());
                    self.image_preview =
                        Some(PreviewState::in_conversation(path.clone(), chat.clone()));
                    if let Some(message) = message
                        && let Some(preview) = &mut self.image_preview
                    {
                        preview.select_message(message, Some(path));
                    }
                    if let Some(chat) = chat {
                        self.request_gallery(&chat, true);
                    }
                    self.dialog = None;
                    self.picker = None;
                    // egui drops the focus of widgets behind a modal only from
                    // the frame after it first shows; until then a focused
                    // composer would still take Enter and send the draft.
                    ctx.memory_mut(|memory| {
                        if let Some(focused) = memory.focused() {
                            memory.surrender_focus(focused);
                        }
                    });
                } else {
                    self.actions.push(Action::OpenFile(path));
                }
            }
            Action::PreviousImage | Action::NextImage => {
                if self
                    .image_preview
                    .as_ref()
                    .and_then(PreviewState::chat)
                    .is_some_and(|id| {
                        self.chat(id)
                            .is_none_or(|chat| chat.locked && !self.locked_folder_open())
                    })
                {
                    self.image_preview = None;
                    return;
                }
                let next = self.image_preview.as_ref().and_then(|preview| {
                    let listing = self
                        .gallery
                        .listing
                        .as_ref()
                        .filter(|listing| Some(listing.chat.as_str()) == preview.chat())?;
                    let at = listing
                        .media
                        .iter()
                        .position(|row| Some(row.id.as_str()) == preview.message())?;
                    let index = if matches!(action, Action::NextImage) {
                        at.checked_sub(1)?
                    } else {
                        at.checked_add(1)?
                    };
                    let row = listing.media.get(index)?;
                    Some((listing.chat.clone(), row.id.clone()))
                });
                if let Some((chat, id)) = next {
                    self.preview_media(chat, id, ctx);
                    return;
                }
                if self.gallery.listing.as_ref().is_some_and(|listing| {
                    self.image_preview.as_ref().is_some_and(|p| {
                        Some(listing.chat.as_str()) == p.chat()
                            && listing
                                .media
                                .iter()
                                .any(|row| Some(row.id.as_str()) == p.message())
                    })
                }) {
                    return;
                }
                let conversations = &self.accounts[self.active].conversations;
                if let Some(preview) = &mut self.image_preview
                    && let Some(conversation) =
                        preview.chat().and_then(|chat| conversations.get(chat))
                {
                    preview.navigate(&conversation.messages, matches!(action, Action::NextImage));
                }
            }
            Action::ZoomImageBy(factor) => {
                if let Some(preview) = &mut self.image_preview {
                    preview.zoom_by(factor);
                }
            }
            Action::ZoomImageIn => {
                if let Some(preview) = &mut self.image_preview {
                    preview.zoom_in();
                }
            }
            Action::ZoomImageOut => {
                if let Some(preview) = &mut self.image_preview {
                    preview.zoom_out();
                }
            }
            Action::FitImage => {
                if let Some(preview) = &mut self.image_preview {
                    preview.fit();
                }
            }
            Action::ImageActualSize => {
                if let Some(preview) = &mut self.image_preview {
                    preview.actual_size();
                }
            }
            Action::DownloadStatus(key) => {
                self.backend.send(Command::DownloadStatus(key));
            }
            Action::CloseImagePreview => {
                self.video.stop();
                self.image_preview = None;
                self.refocus_composer(ctx);
            }
            Action::PreviewSticker(path) => {
                if crate::safety::can_preview_image(&path)
                    && crate::safety::image_preview_size_allowed(&path)
                    && path.is_file()
                {
                    let chat = self.open_chat.clone();
                    let message = chat
                        .as_ref()
                        .and_then(|chat| self.conversations.get(chat))
                        .and_then(|conversation| {
                            conversation.messages.iter().find(|message| {
                                message
                                    .content
                                    .media()
                                    .and_then(|media| media.path.as_deref())
                                    == Some(path.as_path())
                            })
                        })
                        .map(|message| message.id.clone());
                    self.sticker_view = Some(
                        crate::image_preview::StickerPreview::in_conversation(path, chat, message),
                    );
                    self.image_preview = None;
                    self.dialog = None;
                    self.picker = None;
                    ctx.memory_mut(|memory| {
                        if let Some(focused) = memory.focused() {
                            memory.surrender_focus(focused);
                        }
                    });
                }
            }
            Action::CloseStickerPreview => {
                self.sticker_view = None;
                self.refocus_composer(ctx);
            }
            Action::OpenFile(path) => {
                if crate::safety::can_open_attachment(&path) && path.is_file() {
                    if let Err(error) = open::that_detached(&path) {
                        self.toast_error(format!("Could not open the attachment: {error}"));
                    }
                } else {
                    self.toast("For safety, open this file yourself from its folder");
                    if let Some(folder) = path.parent() {
                        self.actions.push(Action::OpenFolder(folder.to_owned()));
                    }
                }
            }
            Action::OpenLog(path) => self.backend.send(Command::OpenLog(path)),
            Action::SaveAttachments { files } => {
                self.backend.send(Command::SaveAttachments { files });
            }
            Action::SaveAttachmentAs { path, name } => {
                self.backend
                    .send(Command::SaveAttachmentAs { source: path, name });
            }
            Action::DragAttachment(path) => {
                #[cfg(test)]
                ctx.data_mut(|data| {
                    data.insert_temp(egui::Id::new("fixture-native-drag"), path.clone())
                });
                let result = if cfg!(test) {
                    Ok(())
                } else {
                    crate::file_drag::start(&path)
                };
                self.finish_attachment_drag(ctx, result);
            }
            Action::OpenFolder(path) => {
                if path.is_dir() {
                    if let Err(error) = open::that_detached(&path) {
                        self.toast_error(format!("Could not open the folder: {error}"));
                    }
                } else {
                    self.toast_error("The folder is unavailable");
                }
            }
            Action::OpenUrl(url) => {
                if let Some(code) = crate::safety::group_invite_code(&url) {
                    self.invite = Some(crate::model::GroupInvite {
                        code: code.clone(),
                        state: crate::model::InviteState::Loading,
                    });
                    self.dialog = Some(Dialog::JoinGroup);
                    self.backend.send(Command::PreviewInvite(code));
                } else if let Some(url) = crate::safety::external_url(&url) {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                } else {
                    self.toast_error("This link type cannot be opened from Zap Faster");
                }
            }
            Action::CopyText(text) => {
                ctx.copy_text(text);
                self.toast("Copied");
            }
            Action::CopyImage(path) => {
                self.backend.send(Command::PrepareClipboardImage(path));
            }
            Action::DismissToast(index) => {
                if index < self.toasts.len() {
                    self.toasts.remove(index);
                }
            }
            Action::Reply(id) => {
                self.reply_when_loaded = None;
                // Replying while editing starts a new message: the edited
                // text must not go out as the reply.
                if self.editing.take().is_some() {
                    self.clear_composer_text();
                    self.composer_mentions.clear();
                    self.emoji_start = None;
                    self.mention_start = None;
                }
                self.reply_to = Some(id.clone().into());
                self.focus_composer = true;
                let loaded = self.open_chat.as_deref().is_some_and(|chat| {
                    self.conversations
                        .get(chat)
                        .is_some_and(|conversation| conversation.message(&id).is_some())
                });
                if !loaded && let Some(chat) = self.open_chat.clone() {
                    // A starred row can reply before its chat has paged to
                    // the message. The quote starts again once that page arrives.
                    self.reply_when_loaded = Some((chat, id));
                }
            }
            Action::ReplyPrivately { chat, message } => {
                if self.open_chat.as_deref() != Some(chat.as_str())
                    || self
                        .chat(&chat)
                        .is_some_and(|source| source.locked && !self.locked_folder_open())
                {
                    return;
                }
                let Some(original) = self
                    .conversations
                    .get(&chat)
                    .and_then(|conversation| conversation.message(&message))
                    .cloned()
                else {
                    return;
                };
                let Some(recipient) = original.private_reply_recipient().map(str::to_owned) else {
                    return;
                };
                if self.chat(&recipient).is_some_and(|chat| {
                    chat.read_only || (chat.locked && !self.locked_folder_open())
                }) {
                    return;
                }
                let name = self.display_name_or(&recipient, original.sender_name.as_deref());
                self.apply(
                    Action::StartChat {
                        dismiss_dialog: false,
                        id: recipient.clone(),
                        name,
                    },
                    ctx,
                );
                if self.open_chat.as_deref() == Some(recipient.as_str()) {
                    self.reply_to = Some(crate::model::ReplyTarget {
                        id: message,
                        chat: Some(chat),
                        destination_generation: self
                            .private_reply_generations
                            .get(&recipient)
                            .copied()
                            .unwrap_or(self.private_reply_generation),
                    });
                    self.focus_composer = true;
                }
            }
            Action::CancelReply => {
                self.reply_to = None;
                self.reply_when_loaded = None;
            }
            Action::SetStar {
                chat,
                message,
                starred,
            } => {
                self.backend.send(Command::SetStar {
                    chat,
                    message,
                    starred,
                });
            }
            Action::Forward {
                from_chat,
                messages,
                to_chat,
            } => {
                self.backend.send(Command::Forward {
                    from_chat,
                    messages,
                    to_chat,
                });
                self.follow_sent_chat();
                self.dialog = None;
                self.forward_search.clear();
                self.forward_recipients.clear();
                self.forward_reviewing = false;
                self.selection = None;
            }
            Action::ToggleForwardRecipient(id) => {
                if matches!(self.dialog, Some(Dialog::Forward { .. }))
                    && !self.forward_reviewing
                    && self.chat(&id).is_some_and(|chat| chat.can_send())
                {
                    if self.forward_recipients.contains(&id) {
                        self.forward_recipients.retain(|recipient| recipient != &id);
                    } else {
                        self.forward_recipients.push(id);
                    }
                }
            }
            Action::ReviewForward => {
                if matches!(self.dialog, Some(Dialog::Forward { .. }))
                    && !self.forward_recipients.is_empty()
                {
                    self.forward_reviewing = true;
                }
            }
            Action::BackToForwardSelection => self.forward_reviewing = false,
            Action::ConfirmForward => {
                let Some(Dialog::Forward { chat, messages }) = self.dialog.clone() else {
                    return;
                };
                if !self.forward_reviewing
                    || messages.is_empty()
                    || self.forward_recipients.is_empty()
                {
                    return;
                }
                // Recheck the complete selection before dispatch. A destination may
                // have become locked or read-only while the review was open.
                let writable: std::collections::HashSet<_> = self
                    .chats
                    .iter()
                    .filter(|chat| crate::ui::dialogs::forwardable(chat))
                    .map(|chat| chat.id.clone())
                    .collect();
                let count = self.forward_recipients.len();
                self.forward_recipients.retain(|id| writable.contains(id));
                if self.forward_recipients.len() != count {
                    self.forward_reviewing = false;
                    self.toast_error(
                        crate::i18n::gettext(self.locale, "A selected chat is no longer writable")
                            .into_owned(),
                    );
                    return;
                }
                for to_chat in std::mem::take(&mut self.forward_recipients) {
                    self.backend.send(Command::Forward {
                        from_chat: chat.clone(),
                        messages: messages.clone(),
                        to_chat,
                    });
                }
                self.follow_sent_chat();

                self.dialog = None;
                self.forward_search.clear();
                self.forward_recipients.clear();
                self.forward_reviewing = false;
                self.selection = None;
            }
            Action::StartSelection => {
                // Choosing it again keeps what is already ticked.
                if let Some(chat) = self.open_chat.clone()
                    && self
                        .selection
                        .as_ref()
                        .is_none_or(|(selected, _)| *selected != chat)
                {
                    self.selection = Some((chat, Vec::new()));
                    self.selection_anchor = None;
                }
            }
            Action::SelectMessage(id) => {
                if let Some(chat) = self.open_chat.clone()
                    && self
                        .conversations
                        .get(&chat)
                        .and_then(|conversation| conversation.message(&id))
                        .or_else(|| {
                            self.starred
                                .iter()
                                .find(|entry| entry.message.chat == chat && entry.message.id == id)
                                .map(|entry| &entry.message)
                        })
                        .is_some_and(|message| can_select(&message.content))
                {
                    self.selection = Some((chat, vec![id.clone()]));
                    self.selection_anchor = Some(id);
                }
            }
            Action::SelectRange(id) => {
                let Some((chat, ids)) = self.selection.clone() else {
                    return;
                };
                let Some(conversation) = self.conversations.get(chat.as_str()) else {
                    return;
                };
                let anchor = self.selection_anchor.clone().unwrap_or_else(|| id.clone());
                let mut ids = ids;
                add_range(&conversation.messages, &mut ids, &anchor, &id);
                self.selection = Some((chat, ids));
                self.selection_anchor = Some(id);
            }
            Action::SweepMessages { anchor, to } => {
                let Some(chat) = self.open_chat.clone() else {
                    return;
                };
                // A sweep adds to what was selected when it began, so
                // dragging back leaves out the rows it passes again.
                if self
                    .sweep
                    .as_ref()
                    .is_none_or(|sweep| sweep.chat != chat || sweep.anchor != anchor)
                {
                    let base = self
                        .selection
                        .as_ref()
                        .filter(|(selected, _)| *selected == chat)
                        .map(|(_, ids)| ids.clone())
                        .unwrap_or_default();
                    self.sweep = Some(Sweep {
                        chat: chat.clone(),
                        anchor: anchor.clone(),
                        to: to.clone(),
                        base,
                    });
                }
                {
                    let Some(sweep) = self.sweep.as_mut() else {
                        return;
                    };
                    sweep.to.clone_from(&to);
                }
                let base = self
                    .sweep
                    .as_ref()
                    .map(|sweep| sweep.base.clone())
                    .unwrap_or_default();
                let range = self.conversations.get(chat.as_str()).map(|conversation| {
                    let mut ids = base;
                    add_range(&conversation.messages, &mut ids, &anchor, &to);
                    ids
                });
                let Some(ids) = range else {
                    return;
                };
                // An open selection stays open, as its check boxes do, even
                // with nothing in it.
                let open = self
                    .selection
                    .as_ref()
                    .is_some_and(|(selected, _)| *selected == chat);
                self.selection = (open || !ids.is_empty()).then_some((chat, ids));
                self.selection_anchor = Some(to);
            }
            Action::EndSweep => self.sweep = None,
            Action::ToggleSelected(id) => {
                let mut next = self.selection.clone();
                if let Some((chat, ids)) = next.as_mut() {
                    let selectable = self
                        .conversations
                        .get(chat.as_str())
                        .and_then(|conversation| conversation.message(&id))
                        .is_some_and(|message| can_select(&message.content));
                    if !selectable {
                        return;
                    }
                    self.selection_anchor = Some(id.clone());
                    if let Some(index) = ids.iter().position(|selected| *selected == id) {
                        ids.remove(index);
                    } else {
                        ids.push(id);
                        if let Some(conversation) = self.conversations.get(chat.as_str()) {
                            let position = |id: &String| {
                                conversation
                                    .messages
                                    .iter()
                                    .position(|message| message.id == *id)
                                    .unwrap_or(usize::MAX)
                            };
                            ids.sort_by_key(position);
                        }
                    }
                }
                self.selection = next;
            }
            Action::StarSelection(starred) => {
                let Some((chat, ids)) = self.selection.clone() else {
                    return;
                };
                if self.open_chat.as_deref() != Some(chat.as_str())
                    || self
                        .chat(&chat)
                        .is_none_or(|known| known.locked && !self.locked_folder_open())
                {
                    return;
                }
                let Some(conversation) = self.conversations.get(&chat) else {
                    return;
                };
                let mut seen = HashSet::new();
                for id in ids {
                    if seen.insert(id.clone())
                        && conversation.message(&id).is_some_and(|message| {
                            !message.status.is_local()
                                && !matches!(message.content, Content::Revoked)
                        })
                    {
                        self.backend.send(Command::SetStar {
                            chat: chat.clone(),
                            message: id,
                            starred,
                        });
                    }
                }
            }
            Action::CancelSelection => {
                self.selection = None;
                self.sweep = None;
            }
            Action::Edit(id) => {
                self.reply_when_loaded = None;
                let text = self
                    .open_chat
                    .as_deref()
                    .and_then(|chat| self.conversations.get(chat))
                    .and_then(|conversation| conversation.message(&id))
                    .or_else(|| {
                        self.open_chat.as_deref().and_then(|chat| {
                            self.starred
                                .iter()
                                .find(|entry| entry.message.chat == chat && entry.message.id == id)
                                .map(|entry| &entry.message)
                        })
                    })
                    .filter(|message| self.can_edit(message))
                    .and_then(|message| match &message.content {
                        Content::Text { text, .. } => Some(text.clone()),
                        _ => None,
                    });
                if let Some(text) = text {
                    self.editing = Some(id);
                    self.composer_tools_open = false;
                    self.reply_to = None;
                    self.replace_composer_text(text);
                    self.set_composer_cursor(ctx, self.composer.chars().count());
                    self.composer_mentions.clear();
                    self.emoji_start = None;
                    self.mention_start = None;
                    self.focus_composer = true;
                }
            }
            Action::CancelEdit => {
                if self.editing.take().is_some() {
                    self.clear_composer_text();
                    self.composer_mentions.clear();
                    self.emoji_start = None;
                    self.mention_start = None;
                }
            }
            Action::CancelQueued { chat, id } => {
                self.backend.send(Command::CancelQueued { chat, id })
            }
            Action::DeleteForEveryone { chat, id } => {
                self.backend.send(Command::Revoke { chat, id });
            }
            Action::DeleteForMe { chat, id } => {
                // The message stays until WhatsApp accepts the deletion.
                // `Event::MessageDeleted` removes it and prunes the selection.
                self.schedule_undo(
                    crate::undo::Operation::DeleteMessage { chat, id },
                    "Deletion scheduled",
                );
            }
            Action::UndoToast(token) => self.undo_toast(token),
            Action::Attach => {
                if let Some(chat) = self.open_chat.clone() {
                    self.backend.send(Command::PickFiles(chat));
                }
            }
            Action::SetComposerTools(open) => {
                self.composer_tools_open = open;
                if open {
                    self.picker = None;
                    self.reaction_target = None;
                    self.reaction_anchor = None;
                }
            }
            Action::SendFiles(paths) => {
                self.stage_files(paths);
                if !self.pending.is_empty() {
                    ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("composer-text")));
                }
            }
            Action::SendPending {
                account,
                chat,
                caption,
                mentions,
            } => self.send_pending(account, chat, caption, mentions),
            Action::TogglePendingDocument(index) => {
                if index < self.pending.len() {
                    let item = self.pending.remove(index);
                    let item = match item {
                        Pending::Recovered {
                            item,
                            caption,
                            quoting,
                            mentions,
                            composer_owned,
                        } => Pending::Recovered {
                            item: Box::new(match *item {
                                Pending::Document(inner) => *inner,
                                item => Pending::Document(Box::new(item)),
                            }),
                            caption,
                            quoting,
                            mentions,
                            composer_owned,
                        },
                        Pending::Document(inner) => *inner,
                        item => Pending::Document(Box::new(item)),
                    };
                    self.pending.insert(index, item);
                }
            }
            Action::RemovePending(index) => {
                if index < self.pending.len() {
                    self.pending.remove(index);
                }
            }
            Action::EditPicture(index) => match self.pending.get(index) {
                Some(Pending::Picture {
                    width,
                    height,
                    rgba,
                    ..
                }) => {
                    self.picture_edit = Some(crate::model::PictureEdit::new(
                        index,
                        crate::model::PictureSource::Pasted(rgba.clone()),
                        *width as u32,
                        *height as u32,
                    ));
                }
                Some(Pending::File(path)) => {
                    // A picture that was cropped before opens on the original
                    // with the crop it had, so cropping it again does not
                    // stack another lossy pass on the one before it.
                    let origin = self.picture_origins.get(path).cloned();
                    if let Some(origin) = origin {
                        self.picture_edit = Some(crate::model::PictureEdit {
                            index,
                            source: origin.source,
                            width: origin.width,
                            height: origin.height,
                            crop: origin.crop,
                            turns: origin.turns,
                        });
                    } else if Pending::is_picture_file(path) {
                        self.backend.send(Command::InspectPicture {
                            index,
                            path: path.clone(),
                        });
                    }
                }
                Some(Pending::Document(_) | Pending::Recovered { .. }) | None => {}
            },
            Action::ApplyPictureEdit => {
                if let Some(edit) = self.picture_edit.take() {
                    // Held until the file lands, so it can be filed under the
                    // path the cropped picture is written to.
                    self.picture_applying = Some(crate::model::PictureOrigin {
                        source: edit.source.clone(),
                        width: edit.width,
                        height: edit.height,
                        crop: edit.crop,
                        turns: edit.turns,
                    });
                    self.backend.send(Command::ApplyPictureEdit {
                        index: edit.index,
                        source: edit.source,
                        width: edit.width,
                        height: edit.height,
                        crop: edit.crop,
                        turns: edit.turns,
                    });
                }
            }
            Action::CancelPictureEdit => self.picture_edit = None,
            Action::ClearPending => self.pending.clear(),
            Action::PlayVoice { message, path } => self.play_voice(message, path),
            Action::PlayVideo { message, path } => self.play_video(message, path),
            Action::PlayVideoWhenDownloaded(message) => {
                self.voice_chat = None;
                self.voice_wanted = None;
                self.video_wanted = self.open_chat.clone().map(|chat| (chat, message));
            }
            Action::SeekVideo { message, fraction } => self.video.seek(&message, fraction),
            Action::ToggleVideoSound => self.video.toggle_mute(),
            Action::CycleVideoSpeed => self.video.cycle_speed(),
            Action::SetVideoSpeed(speed) => self.video.set_speed(speed),
            Action::ExpandVideo { message, path } => {
                if self.video.message() != Some(message.as_str()) {
                    self.video.set_expanded(true);
                    self.play_video(message, path);
                }
                if self.video.message().is_some() {
                    self.video.set_expanded(true);
                    self.video.resume();
                    self.video_expanded = true;
                }
            }
            Action::CollapseVideo => {
                self.video_expanded = false;
                self.video.set_expanded(false);
            }
            Action::SeekVoice {
                message,
                path,
                fraction,
            } => {
                // One sound at a time, and the clip the reader picked wins
                // over one a run is still fetching.
                self.video.stop();
                self.voice_wanted = None;
                self.voice_chat = self.open_chat.clone();
                self.player
                    .set_output_device(self.account().settings.call_speaker.clone());
                if let Err(error) = self.player.seek(&message, &path, fraction) {
                    self.toast_error(error);
                }
            }
            Action::SetVoiceSpeed(speed) => {
                self.settings.voice_speed = self.player.set_speed(speed);
                self.mark_settings_dirty();
            }
            Action::StartRecording => {
                if self.open_chat.is_some() && self.recording.is_none() {
                    self.picker = None;
                    self.composer_tools_open = false;
                    // An offline demo never opens the microphone.
                    #[cfg(any(test, feature = "demo"))]
                    let recorder = if self.backend.is_offline() {
                        Recorder::simulated(self.waker.clone())
                    } else {
                        Recorder::start_on(
                            self.waker.clone(),
                            self.account().settings.call_microphone.clone(),
                        )
                    };
                    #[cfg(not(any(test, feature = "demo")))]
                    let recorder = Recorder::start_on(
                        self.waker.clone(),
                        self.account().settings.call_microphone.clone(),
                    );
                    self.recording = Some(recorder);
                }
            }
            Action::CancelRecording => {
                self.recording = None;
                self.refocus_composer(ctx);
            }
            Action::SendRecording => {
                self.send_recording();
                self.refocus_composer(ctx);
            }
            Action::DiscardUnsentVoice => self.unsent_voice = None,
            Action::SetDisappearingTimer(chat, seconds) => {
                self.backend
                    .send(Command::SetDisappearingTimer { chat, seconds });
            }
            Action::SetMuted(chat, until) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.muted_until = until;
                }
                self.backend.send(Command::SetMuted(chat, until));
            }
            Action::SetLocked(chat, locked) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.locked = locked;
                }
                // Locking the open chat closes it, as the phone does.
                if locked && self.open_chat.as_deref() == Some(chat.as_str()) {
                    self.hide_locked_chat(&chat);
                }
                self.backend.send(Command::SetLocked(chat, locked));
            }
            Action::TogglePicker(tab) => {
                self.composer_tools_open = false;
                self.emoji_start = None;
                self.mention_start = None;
                self.reaction_target = None;
                self.reaction_anchor = None;
                if self.picker == Some(tab) {
                    self.picker = None;
                    self.refocus_composer(ctx);
                } else {
                    self.picker = Some(tab);
                    self.picker_search.clear();
                    self.picker_focus = tab == PickerTab::Emoji;
                    if tab == PickerTab::Emoji {
                        crate::emoji::prewarm(
                            ctx,
                            crate::ui::picker::first_page(&self.settings.recent_emoji),
                        );
                    }
                    self.emoji_selected = 0;
                    self.emoji_jump = None;
                    if tab == PickerTab::Stickers {
                        self.stickers_pending = self.stickers.is_empty()
                            && self.stickers_saved.is_empty()
                            && self.sticker_packs.is_empty();
                        self.backend.send(Command::RecentStickers);
                    }
                    if tab == PickerTab::Gifs && self.gif_results.is_empty() {
                        self.actions.push(Action::SearchGifs(String::new()));
                    }
                }
            }
            Action::ClosePicker => {
                let was_reaction = self.reaction_target.is_some();
                if let Some((chat, message)) = &self.reaction_target {
                    egui::Popup::close_id(
                        ctx,
                        crate::ui::conversation::bubble_id(chat, message).with("popup"),
                    );
                }
                self.picker = None;
                self.reaction_target = None;
                self.reaction_anchor = None;
                self.emoji_jump = None;
                if !was_reaction {
                    self.refocus_composer(ctx);
                }
            }
            Action::OpenReactionPicker {
                chat,
                message,
                beside_menu,
            } => {
                self.focus_composer = false;
                self.emoji_start = None;
                self.mention_start = None;
                self.picker = None;
                let id = crate::ui::conversation::bubble_id(&chat, &message);
                // Anchor to the menu when it stays open, else to the hover
                // button that opened the picker.
                let anchor = if beside_menu {
                    "menu-rect"
                } else {
                    "react-rect"
                };
                self.reaction_anchor =
                    ctx.data(|data| data.get_temp::<egui::Rect>(id.with(anchor)));
                self.reaction_beside_menu = beside_menu;
                self.reaction_target = Some((chat, message));
                let frequent: Vec<String> = self
                    .settings
                    .reaction_emoji
                    .iter()
                    .map(|(emoji, _)| emoji.clone())
                    .collect();
                crate::emoji::prewarm(ctx, crate::ui::picker::first_page(&frequent));
                self.picker_search.clear();
                self.picker_focus = true;
                self.emoji_selected = 0;
                self.emoji_jump = None;
            }
            Action::InsertEmoji(emoji) => {
                self.insert_in_composer(ctx, &emoji);
                self.remember_emoji(&emoji);
                self.focus_composer = true;
            }
            Action::InsertEmojiCompletion { emoji, start, end } => {
                let starts_with_colon = start
                    .checked_add(1)
                    .is_some_and(|after| self.composer.get(start..after) == Some(":"));
                if starts_with_colon
                    && start <= end
                    && self.composer.is_char_boundary(start)
                    && self.composer.is_char_boundary(end)
                {
                    self.composer.replace_range(start..end, &emoji);
                    let cursor = self.composer[..start].chars().count() + emoji.chars().count();
                    self.set_composer_cursor(ctx, cursor);
                    self.remember_emoji(&emoji);
                    self.focus_composer = true;
                }
                self.emoji_start = None;
            }
            Action::ReplaceTypedEmoticon {
                source,
                emoji,
                start,
                end,
            } => {
                if self.settings.convert_typed_emoticons
                    && start < end
                    && self.composer.is_char_boundary(start)
                    && self.composer.is_char_boundary(end)
                    && self.composer.get(start..end) == Some(source.as_str())
                {
                    self.composer.replace_range(start..end, &emoji);
                    let cursor_after =
                        self.composer[..start].chars().count() + emoji.chars().count();
                    self.set_composer_cursor(ctx, cursor_after);
                    self.emoticon_undo = Some(EmoticonUndo {
                        start_byte: start,
                        source,
                        emoji: emoji.clone(),
                        cursor_after,
                    });
                    self.remember_emoji(&emoji);
                    self.focus_composer = true;
                }
                self.emoji_start = None;
                self.mention_start = None;
            }
            Action::UndoTypedEmoticon => {
                if let Some(undo) = self.emoticon_undo.take() {
                    let end = undo.start_byte + undo.emoji.len();
                    if self.composer.get(undo.start_byte..end) == Some(undo.emoji.as_str()) {
                        self.composer
                            .replace_range(undo.start_byte..end, &undo.source);
                        let cursor = self.composer[..undo.start_byte].chars().count()
                            + undo.source.chars().count();
                        self.set_composer_cursor(ctx, cursor);
                    }
                }
            }
            Action::CloseEmojiSuggestions => {
                self.emoji_start = None;
                self.focus_composer = true;
            }
            Action::InsertMention {
                id,
                name,
                start,
                end,
            } => {
                let member = self.current_chat().is_some_and(|chat| {
                    chat.is_group() && chat.participants.iter().any(|known| known == &id)
                });
                let mention_at = start
                    .checked_add(1)
                    .is_some_and(|after| self.composer.get(start..after) == Some("@"));
                if member
                    && start <= end
                    && self.composer.is_char_boundary(start)
                    && self.composer.is_char_boundary(end)
                    && mention_at
                {
                    let mention = format!("@{name}");
                    let inserted = format!("{mention} ");
                    self.composer.replace_range(start..end, &inserted);
                    self.composer_mentions.push(ComposerMention { id, name });
                    let cursor = self.composer[..start + inserted.len()].chars().count();
                    self.set_composer_cursor(ctx, cursor);
                    self.focus_composer = true;
                }
                self.emoji_start = None;
                self.mention_start = None;
            }
            Action::CloseMentions => self.mention_start = None,
            Action::SaveSticker(path) => {
                self.backend.send(Command::SaveSticker { path });
                self.toast(crate::i18n::gettext(self.locale, "Added to favorites"));
            }
            Action::RemoveRecentSticker(path) => {
                self.stickers.retain(|recent| *recent != path);
                self.backend.send(Command::RemoveRecentSticker { path });
            }
            Action::ForgetSticker(path) => {
                self.backend.send(Command::ForgetSticker { path });
            }
            Action::ImportStickerUrl(url) => {
                self.sticker_import_pending = true;
                self.sticker_link.clear();
                self.backend.send(Command::ImportStickerUrl { url });
            }
            Action::PickStickerArchive => {
                self.sticker_import_pending = true;
                self.backend.send(Command::PickStickerArchive);
            }
            Action::DeleteStickerPack(dir) => {
                if self.sticker_shelf == StickerShelf::Pack(dir.clone()) {
                    self.sticker_shelf = StickerShelf::Recent;
                }
                self.backend.send(Command::DeleteStickerPack { dir });
            }
            Action::CreateStickerPack(name) => {
                let name = name.trim().to_owned();
                if !name.is_empty() {
                    // The backend picks the folder; select the pack once the
                    // next Stickers event lists it.
                    self.sticker_pack_created = Some(name.clone());
                    self.backend.send(Command::CreateStickerPack { name });
                }
            }
            Action::SelectStickerShelf(shelf) => {
                self.sticker_shelf = shelf;
            }
            Action::ViewStickerPack(message) => {
                if let Some(chat) = self.open_chat.clone() {
                    self.sticker_preview = None;
                    self.sticker_preview_pending = true;
                    self.dialog = Some(Dialog::StickerPack);
                    self.backend
                        .send(Command::ViewStickerPack { chat, message });
                }
            }
            Action::PickStickerPicture => {
                self.backend.send(Command::PickStickerPicture);
            }
            Action::MakeSticker { send } => {
                if let Some(draft) = self.sticker_draft.take() {
                    let chat = if send { self.open_chat.clone() } else { None };
                    let quoting = chat.as_ref().and(self.reply_to.clone());
                    self.backend.send(Command::MakeSticker {
                        source: draft.source,
                        crop: draft.crop,
                        transparent: draft.transparent && draft.keep_transparent,
                        emojis: crate::sticker_meta::clean_emojis(&draft.emojis),
                        chat,
                        quoting,
                    });
                }
                self.dialog = None;
            }
            Action::AddStickerPack => {
                if let Some((pack, _)) = &self.sticker_preview {
                    self.backend.send(Command::AddStickerPack {
                        dir: pack.dir.clone(),
                        name: pack.name.clone(),
                    });
                }
                self.dialog = None;
            }
            Action::ShareStickerPack(dir) => {
                if let Some(chat) = self.open_chat.clone() {
                    self.picker = None;
                    self.toast(crate::i18n::gettext(
                        self.locale,
                        "Sending the sticker pack…",
                    ));
                    self.backend.send(Command::SendStickerPack { chat, dir });
                    self.follow_sent_chat();
                }
            }
            Action::SetStickerPack {
                pack,
                sticker,
                member,
            } => {
                self.backend.send(Command::SetStickerPack {
                    pack,
                    sticker,
                    member,
                });
            }
            Action::SendSticker(path) => {
                if let Some(chat) = self.open_chat.clone() {
                    let quoting = self.reply_to.take();
                    self.backend.send(Command::SendSticker {
                        chat,
                        path,
                        quoting,
                    });
                    self.picker = None;
                    self.follow_outgoing();
                    self.refocus_composer(ctx);
                }
            }
            Action::SearchGifs(query) => {
                self.gif_query = query.clone();
                self.gif_pending = true;
                self.gif_error = None;
                self.backend.send(Command::SearchGifs {
                    query,
                    key: self.settings.effective_giphy_key().unwrap_or_default(),
                });
            }
            Action::SendGif(gif) => {
                if let Some(chat) = self.open_chat.clone() {
                    self.toast("Sending GIF…");
                    let quoting = self.reply_to.take();
                    self.backend.send(Command::SendGif { chat, gif, quoting });
                    self.picker = None;
                    self.follow_outgoing();
                    self.refocus_composer(ctx);
                }
            }
            Action::PasteImage {
                width,
                height,
                rgba,
            } => {
                // Stage the files so the user can add a caption.
                if self.open_chat.is_some() {
                    self.pending.push(Pending::Picture {
                        width,
                        height,
                        rgba: std::sync::Arc::new(rgba),
                        texture: None,
                    });
                    self.focus_composer = true;
                    ctx.memory_mut(|memory| {
                        memory.request_focus(egui::Id::new("composer-text"));
                    });
                }
            }
            Action::React {
                chat,
                message,
                emoji,
            } => {
                if !emoji.is_empty() {
                    let count = self
                        .settings
                        .reaction_emoji
                        .iter()
                        .find(|(known, _)| known == &emoji)
                        .map_or(1, |(_, count)| count.saturating_add(1));
                    self.settings
                        .reaction_emoji
                        .retain(|(known, _)| known != &emoji);
                    self.settings
                        .reaction_emoji
                        .insert(0, (emoji.clone(), count));
                    self.settings
                        .reaction_emoji
                        .sort_by_key(|(_, count)| std::cmp::Reverse(*count));
                    self.settings.reaction_emoji.truncate(36);
                    self.remember_emoji(&emoji);
                }
                egui::Popup::close_id(
                    ctx,
                    crate::ui::conversation::bubble_id(&chat, &message).with("popup"),
                );
                self.reaction_target = None;
                self.reaction_anchor = None;
                self.backend.send(Command::React {
                    chat,
                    message,
                    emoji,
                });
            }
            Action::LeaveGroup { chat, archive } => {
                self.dialog = None;
                let ours: Vec<String> = self.our_ids().into_iter().map(str::to_owned).collect();
                if let Some(known) = self.chat_mut(&chat) {
                    known.read_only = true;
                    known.left = true;
                    known.participants.retain(|id| !ours.contains(id));
                }
                // Archiving and closing the open conversation both wait for the
                // phone: a refused leave rolls the mark back, and a chat that
                // archived and closed itself would not come back. The
                // confirmed update does both.
                self.backend.send(Command::LeaveGroup { chat, archive });
            }
            Action::SetArchived(chat, archived) => {
                let before = self.chat(&chat).is_some_and(|chat| chat.archived);
                if let Some(known) = self.chat_mut(&chat) {
                    known.archived = archived;
                }
                if archived && self.open_chat.as_deref() == Some(chat.as_str()) {
                    self.actions.push(Action::CloseChat);
                }
                self.schedule_undo(
                    crate::undo::Operation::ArchiveChat {
                        chat,
                        before,
                        after: archived,
                    },
                    if archived {
                        "Archive scheduled"
                    } else {
                        "Unarchive scheduled"
                    },
                );
            }
            // The chat leaves the list once the phone confirmed, through
            // `Event::ChatRemoved`.
            Action::DeleteChat(chat) => self.backend.send(Command::DeleteChat(chat)),
            // The messages go once the phone confirmed, through
            // `Event::ChatCleared`; the chat stays either way.
            Action::ClearChat(chat) => self.backend.send(Command::ClearChat(chat)),
            Action::SetPinned(chat, pinned) => {
                if pinned && self.pinned_count() >= self.pin_limit {
                    self.toast(format!("You can only pin {} chats", self.pin_limit));
                    return;
                }
                if let Some(known) = self.chat_mut(&chat) {
                    known.pinned = pinned;
                    known.pinned_at = if pinned {
                        jiff::Timestamp::now().as_millisecond()
                    } else {
                        0
                    };
                }
                self.backend.send(Command::SetPinned(chat, pinned));
            }
            Action::SetMessagePinned {
                chat,
                message,
                pinned,
                duration,
            } => {
                // WhatsApp keeps three active pins per chat, and the phone
                // replaces an existing one when a fourth arrives. Refuse the
                // fourth here, where the count is already known.
                let now = crate::util::now();
                let active = self.chat_pins.get(&chat).map_or(0, |rows| {
                    rows.iter()
                        .filter(|row| row.expires_at > now && row.id != message)
                        .count()
                });
                if pinned && active >= 3 {
                    self.toast(crate::i18n::gettext(
                        self.locale,
                        "This chat already has three pinned messages",
                    ));
                } else {
                    self.backend.send(Command::SetMessagePinned {
                        chat,
                        message,
                        pinned,
                        duration,
                    });
                }
            }
            Action::SetFavorite(chat, favorite) => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.favorite = favorite;
                    // A new favorite joins the end until the archive numbers it.
                    known.favorite_position = u32::MAX;
                }
                self.backend.send(Command::SetFavorite(chat, favorite));
            }
            Action::ShowDialog(dialog) => {
                if dialog == Dialog::Statuses {
                    self.backend.send(Command::Statuses);
                }
                if let Dialog::ChatInfo(chat) = &dialog {
                    self.gallery.tab = None;
                    if self.gallery_accessible(chat) {
                        self.request_gallery(chat, true);
                    }
                }
                self.cancel_contact_dialog_request();
                self.clear_chat_lock_entry();
                if dialog == Dialog::NewChat {
                    self.new_chat_search.clear();
                }
                self.emoji_start = None;
                self.mention_start = None;
                if matches!(&dialog, Dialog::CreatePoll(_)) && !self.poll_creating {
                    self.poll_draft = Default::default();
                }
                if matches!(&dialog, Dialog::SendLocation(_)) {
                    self.location_draft.clear();
                }
                if matches!(&dialog, Dialog::Forward { .. }) {
                    self.forward_search.clear();
                    self.forward_recipients.clear();
                    self.forward_reviewing = false;
                }
                if dialog == Dialog::PairWithPhone {
                    self.pair_phone.clear();
                }
                if matches!(dialog, Dialog::NewContact | Dialog::MessageNumber) {
                    self.new_contact_to_phone = self.account().settings.save_contacts_to_phone;

                    self.new_contact_phone.clear();
                    self.new_contact_name.clear();
                    self.new_contact_last.clear();
                    self.new_contact_pending = false;
                }
                self.contact_edit = None;
                self.group_name_edit = None;
                self.dialog = Some(dialog);
            }
            Action::CloseDialog => {
                self.backup_draft = crate::ui::archive_backup::BackupDraft::default();
                self.cancel_contact_dialog_request();
                self.clear_chat_lock_entry();
                self.dialog = None;
                self.invite = None;
                self.forward_search.clear();
                self.forward_recipients.clear();
                self.forward_reviewing = false;
                self.contact_edit = None;
                self.group_name_edit = None;
                self.refocus_composer(ctx);
            }
            Action::EditContact { id, name } => {
                let first = self.contacts.get(&id).and_then(Contact::first_name);
                self.contact_edit = Some(crate::util::editor_names(&name, first));
            }
            Action::SaveContact { id, first, last } => {
                self.contact_edit = None;
                let (full_name, first_name) = compose_name(&first, &last);
                let Some(full_name) = full_name else {
                    return;
                };
                self.backend.send(Command::SaveContact {
                    id,
                    full_name,
                    first_name,
                    to_phone: self.account().settings.save_contacts_to_phone,
                });
            }
            Action::NewContact {
                phone,
                first,
                last,
                to_phone,
            } => {
                let Some(phone) = crate::util::international_phone(&phone) else {
                    self.toast_error(
                        crate::i18n::gettext(
                            self.locale,
                            "Enter a valid international phone number",
                        )
                        .into_owned(),
                    );
                    return;
                };
                self.new_contact_pending = true;
                self.new_contact_sequence = self.new_contact_sequence.wrapping_add(1);
                let request = self.new_contact_sequence;
                self.new_contact_request = Some(request);
                self.new_contact_dialog_request = matches!(
                    self.dialog,
                    Some(Dialog::NewContact | Dialog::MessageNumber)
                )
                .then_some(request);
                let (full_name, first_name) = compose_name(&first, &last);
                // The dialog's choice starts the next one.
                if let Some(to_phone) = to_phone
                    && full_name.is_some()
                    && to_phone != self.account().settings.save_contacts_to_phone
                {
                    self.account_mut().settings.save_contacts_to_phone = to_phone;
                    self.account_mut().mark_settings_dirty();
                }
                self.backend.send(Command::NewContact {
                    request,
                    phone,
                    full_name,
                    first_name,
                    to_phone: to_phone.unwrap_or(self.account().settings.save_contacts_to_phone),
                });
            }
            Action::ToggleSidebar => self.sidebar_visible = !self.sidebar_visible,
            Action::ToggleScreenPrivacy => {
                self.settings.screen_privacy.enabled = !self.settings.screen_privacy.enabled;
                self.mark_settings_dirty();
            }
            Action::SetVipChat { chat, enabled } => {
                if self.chat(&chat).is_none() {
                    return;
                }
                let chats = &mut self.account_mut().settings.vip_chats;
                chats.retain(|id| id != &chat);
                if enabled {
                    chats.push(chat);
                }
                self.account_mut().mark_settings_dirty();
            }
            Action::SetFocusMode(enabled) => {
                self.account_mut().settings.focus_enabled =
                    enabled && !self.account().settings.focus_labels.is_empty();
                self.account_mut().mark_settings_dirty();
            }
            Action::SetFocusLabels(labels) => {
                let mut seen = HashSet::new();
                let valid = labels
                    .into_iter()
                    .filter(|id| {
                        self.labels.iter().any(|known| &known.id == id) && seen.insert(id.clone())
                    })
                    .collect();
                self.account_mut().settings.focus_labels = valid;
                if self.account().settings.focus_labels.is_empty() {
                    self.account_mut().settings.focus_enabled = false;
                }
                self.account_mut().mark_settings_dirty();
            }
            Action::SetFocusMentions(enabled) => {
                self.account_mut().settings.focus_mentions = enabled;
                self.account_mut().mark_settings_dirty();
            }
            Action::SetLabelSound { label, sound } => {
                if self.labels.iter().any(|known| known.id == label) {
                    match sound {
                        Some(sound) => {
                            self.account_mut()
                                .settings
                                .label_sounds
                                .insert(label, sound);
                        }
                        None => {
                            self.account_mut().settings.label_sounds.remove(&label);
                        }
                    }
                    self.account_mut().mark_settings_dirty();
                }
            }
            Action::SetChatFilter(filter) => {
                if self.locked_folder {
                    self.close_locked_folder();
                    self.search.clear();
                    self.search_hits.clear();
                }
                self.chat_filter = filter;
                self.label_filter = None;
                self.account_mut().settings.last_filter = filter;
                self.account_mut().settings.last_label = None;
                self.account_mut().mark_settings_dirty();
                self.show_archived = false;
                self.unread_kept.clear();
            }
            Action::JoinGroup => {
                use crate::model::InviteState;
                let ready = self.invite.as_ref().and_then(|invite| match &invite.state {
                    InviteState::Ready(info) => {
                        Some((info.id.clone(), invite.code.clone(), info.clone()))
                    }
                    _ => None,
                });
                if let Some((id, code, info)) = ready {
                    if self.chats.iter().any(|chat| chat.id == id) {
                        self.invite = None;
                        self.dialog = None;
                        self.actions.push(Action::OpenChat(id));
                    } else if let Some(invite) = self.invite.as_mut() {
                        invite.state = InviteState::Joining(info);
                        self.backend.send(Command::JoinInvite(code));
                    }
                }
            }
            Action::MuteAllChannels(mute) => {
                let channels: Vec<ChatId> = self
                    .chats
                    .iter()
                    .filter(|chat| chat.is_channel())
                    .map(|chat| chat.id.clone())
                    .collect();
                for chat in channels {
                    self.actions.push(Action::SetMuted(chat, mute.then_some(0)));
                }
            }
            Action::ShowArchived(show) => {
                if self.locked_folder {
                    self.close_locked_folder();
                    self.search.clear();
                    self.search_hits.clear();
                }
                self.show_archived = show;
                self.unread_kept.clear();
            }
            Action::ToggleCommunity(id) => {
                if self.chat(&id).is_some_and(|chat| {
                    !chat.locked
                        && matches!(chat.community, Some(crate::model::CommunityRole::Parent))
                }) && !self.collapsed_communities.remove(&id)
                {
                    self.collapsed_communities.insert(id);
                }
            }
            Action::RefreshMessagePins(chat) => {
                self.backend.send(Command::LoadPins(chat));
            }
            Action::LoadMoreStarred => {
                self.backend.send(Command::LoadStarred {
                    offset: self.starred.len(),
                });
            }
            Action::ToggleStarred => {
                self.show_starred = !self.show_starred;
                if self.show_starred {
                    self.show_archived = false;
                    // The starred list takes the panel the folder held, and
                    // the folder is a locked one: leave it on the way in.
                    if self.locked_folder {
                        self.close_locked_folder();
                        self.search.clear();
                        self.search_hits.clear();
                    }
                    self.backend.send(Command::LoadStarred { offset: 0 });
                }
            }
            Action::SelectLabel(label) => self.select_label(label),
            Action::SetChatLabels { chat, labels } => {
                self.backend.send(Command::SetChatLabels { chat, labels });
            }
            Action::CreateLabel { name, color_hex } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                if self.labels.len() >= crate::archive::LABEL_LIMIT {
                    self.toast_error(
                        crate::i18n::gettext(
                            self.locale,
                            "You have {limit} labels, the most Zap Faster keeps.",
                        )
                        .replace("{limit}", &crate::archive::LABEL_LIMIT.to_string()),
                    );
                    return;
                }
                if let Some(refusal) = self.label_name_refusal(&name, None) {
                    self.toast_error(refusal);
                    return;
                }
                self.backend.send(Command::CreateLabel { name, color_hex });
                self.label_name.clear();
                self.label_color = crate::archive::DEFAULT_COLOR.to_owned();
            }
            Action::UpdateLabel {
                id,
                name,
                color_hex,
            } => {
                let name = name.trim().to_owned();
                if name.is_empty() {
                    return;
                }
                if let Some(refusal) = self.label_name_refusal(&name, Some(&id)) {
                    self.toast_error(refusal);
                    return;
                }
                self.backend.send(Command::UpdateLabel {
                    id,
                    name,
                    color_hex,
                });
            }
            Action::DeleteLabel(id) => {
                self.backend.send(Command::DeleteLabel(id));
                self.label_editing = None;
            }
            // Reading a chat must not pull its row out from under the pointer.
            // Only the filtered list sends this: search results and
            // notifications open chats without keeping them.
            Action::KeepUnread(id) => {
                if self.chat_filter == ChatFilter::Unread {
                    self.unread_kept.insert(id);
                }
            }
            // Ctrl+K and Ctrl+Shift+F search the chat list everywhere; Ctrl+F
            // searches the open chat when there is one.
            Action::FocusSearch => self.actions.push(Action::FocusChatList),
            Action::FocusChatList => {
                // The list search takes over Escape and Enter from the chat's.
                self.close_chat_search();
                self.sidebar_visible = true;
                self.page = Page::Chats;
                self.focus_composer = false;
                self.focus_search = true;
                self.emoji_start = None;
                self.mention_start = None;
            }
            Action::FocusSettingsSearch => {
                self.page = Page::Settings;
                self.focus_settings_search = true;
            }
            Action::SearchSettings(text) => self.settings_search = text,
            Action::FocusComposer => {
                self.focus_search = false;
                self.focus_composer = true;
            }
            Action::ScrollToBottom => {
                self.scroll_to_bottom = true;
                // Ctrl+End and the return-to-bottom button are explicit: they
                // must win over a message bubble's retained keyboard focus.
                self.scroll_to_bottom_forced = true;
            }
            Action::ScrollPage(scroll) => {
                // Reaching the top releases stick-to-bottom, as the wheel and
                // the edge-scroll drag do; reaching the bottom (paging down or
                // End) lets it take over again, so it is left alone here.
                if matches!(scroll, Scroll::PageUp | Scroll::Top) {
                    self.scroll_to_bottom = false;
                }
                self.scroll_page = Some(scroll);
            }
            Action::ScrollTo(id) => {
                self.scroll_to_bottom = false;
                let Some(chat) = self.open_chat.clone() else {
                    return;
                };
                let load = {
                    let conversation = self.conversations.entry(chat.clone()).or_default();
                    if conversation.message(&id).is_none() && !conversation.loading_older {
                        conversation.messages.first().map(|oldest| {
                            conversation.loading_older = true;
                            (oldest.timestamp, oldest.id.clone())
                        })
                    } else {
                        None
                    }
                };
                if let Some(before) = load {
                    self.backend.send(Command::LoadUntil {
                        chat: chat.clone(),
                        id: id.clone(),
                        before,
                    });
                }
                self.jump_highlight = Some(JumpHighlight::new(chat, id.clone()));
                self.scroll_anchor = Some(id);
            }
            Action::Search(text) => {
                self.search = text;
                self.search_selected = None;
                let query = self.search.trim().to_owned();
                // Editing the search away from the secret code hides the
                // locked folder again, like leaving the phone's home screen.
                if !self.chat_lock_authenticated() && !self.secret_code_matched() {
                    self.close_locked_folder();
                }
                if query.is_empty() || self.locked_folder_open() || self.secret_code_matched() {
                    self.search_hits.clear();
                } else {
                    self.backend.send(Command::SearchMessages { query });
                }
            }
            Action::OpenChatSearch => self.open_chat_search(),
            Action::CloseChatSearch => {
                self.close_chat_search();
                self.refocus_composer(ctx);
            }
            Action::ChatSearch(query) => {
                self.chat_search = query;
                self.request_chat_search();
            }
            Action::SetChatSearchDay(day) => {
                self.chat_search_day = day;
                self.chat_search_calendar = false;
                if let Some(day) = day {
                    self.chat_search_month = day;
                }
                self.request_chat_search();
            }
            Action::ShowUpdate => {
                self.show_update = self.update.is_some();
                self.inspect_update();
            }
            Action::CloseUpdate => self.show_update = false,
            Action::DownloadUpdate => self.download_update(),
            Action::InstallUpdate => {
                if matches!(
                    self.update_download,
                    crate::updates::DownloadState::Ready(_)
                ) {
                    let crate::updates::DownloadState::Ready(prepared) = std::mem::replace(
                        &mut self.update_download,
                        crate::updates::DownloadState::Installing,
                    ) else {
                        unreachable!()
                    };
                    self.backend.send(Command::InstallUpdate {
                        prepared,
                        arguments: self.update_arguments.clone(),
                    });
                }
            }
            Action::SetTheme(choice) => {
                self.settings.theme = choice;
                self.settings.custom_theme = None;
                self.settings.custom_theme_cache = None;
                self.mark_settings_dirty();
                self.apply_theme(ctx);
            }
            Action::SetFont(choice) => {
                self.settings.font = choice;
                self.mark_settings_dirty();
                self.remove_custom_font();
                self.apply_font(ctx);
            }
            Action::PickCustomFont => {
                let request = self.claim_custom_font_request();
                match crate::custom_font::pick(
                    self.dirs.clone(),
                    request,
                    crate::i18n::gettext(self.locale, "Choose a font file").into_owned(),
                    crate::i18n::gettext(self.locale, "Font files").into_owned(),
                    ctx.clone(),
                ) {
                    Ok(pending) => self.custom_font_pending = Some(pending),
                    Err(error) => {
                        self.toast_error(format!("Could not open the font picker: {error}"))
                    }
                }
            }
            Action::SetCustomFont(path) => {
                self.settings.custom_font = Some(path.clone());
                self.mark_settings_dirty();
                self.apply_font(ctx);
                if self.settings.custom_font.is_some() {
                    let _ = crate::custom_font::cleanup(
                        self.dirs.clone(),
                        Some(path),
                        self.custom_font_request,
                    );
                }
            }
            Action::SetEmojiRenderer(choice) => {
                self.settings.emoji_renderer = choice;
                self.settings.custom_emoji_font = None;
                self.custom_emoji_request = self.custom_emoji_request.wrapping_add(1);
                crate::custom_emoji::claim(self.custom_emoji_request);
                let _ = crate::custom_emoji::cleanup(
                    self.dirs.clone(),
                    None,
                    self.custom_emoji_request,
                );
                self.mark_settings_dirty();
            }
            Action::PickCustomEmojiFont => {
                self.custom_emoji_request = self.custom_emoji_request.wrapping_add(1);
                crate::custom_emoji::claim(self.custom_emoji_request);
                match crate::custom_emoji::pick(
                    self.dirs.clone(),
                    self.custom_emoji_request,
                    ctx.clone(),
                ) {
                    Ok(pending) => self.custom_emoji_pending = Some(pending),
                    Err(error) => {
                        self.toast_error(format!("Could not open the emoji font picker: {error}"))
                    }
                }
            }
            Action::SetCustomEmojiFont { path, request } => {
                if request != self.custom_emoji_request {
                    return;
                }
                self.settings.custom_emoji_font = Some(path.clone());
                self.mark_settings_dirty();
                let _ = crate::custom_emoji::cleanup(
                    self.dirs.clone(),
                    Some(path),
                    self.custom_emoji_request,
                );
            }
            Action::SetInterfaceLanguage(choice) => {
                self.settings.interface_language = choice;
                self.locale = crate::i18n::resolve(choice);
                self.mark_settings_dirty();
            }
            Action::SetCustomTheme(filename) => {
                if let Some(theme) = self.custom_themes.find(&filename) {
                    self.settings.custom_theme_cache = Some(theme.clone());
                    self.settings.custom_theme = Some(filename);
                    self.mark_settings_dirty();
                    self.apply_theme(ctx);
                }
            }
            Action::SetWallpaperColor(color) => {
                if self.palette.dark {
                    self.settings.dark_wallpaper_color = color;
                } else {
                    self.settings.wallpaper_color = color;
                }
                self.mark_settings_dirty();
            }
            Action::SetWallpaperDoodles(show) => {
                self.settings.show_wallpaper = show;
                self.mark_settings_dirty();
            }
            Action::PickWallpaperImage => self.backend.send(Command::PickWallpaperImage),
            Action::RemoveWallpaperImage => {
                if self.account_mut().settings.wallpaper_image.take().is_some() {
                    self.account_mut().mark_settings_dirty();
                }
                crate::wallpaper::forget_image(ctx);
                self.backend.send(Command::RemoveWallpaperImage);
            }
            Action::ReloadThemes => self.load_custom_themes(),
            Action::OpenThemesFolder => {
                let directory = self.dirs.config.join("themes");
                std::thread::spawn(move || {
                    if std::fs::create_dir_all(&directory).is_ok() {
                        let _ = open::that(directory);
                    }
                });
            }
            Action::SetShortcutHints(show) => {
                self.settings.show_shortcut_hints = show;
                self.mark_settings_dirty();
            }
            Action::DismissChatLockHint => {
                self.settings.chat_lock_hint_dismissed = true;
                self.mark_settings_dirty();
            }
            Action::OpenLockedFolder => {
                if self.locked_folder_open() || self.secret_code_matched() {
                    self.enter_locked_folder();
                } else {
                    self.clear_chat_lock_entry();
                    self.dialog = Some(Dialog::UnlockLockedChats);
                }
            }
            Action::UnlockLockedFolder(code) => {
                if self.settings.verifies_chat_lock_code(code.trim()) {
                    self.enter_locked_folder();
                } else {
                    self.chat_lock_entry.clear();
                    self.chat_lock_error = true;
                }
            }
            Action::CreateChatLockCode(code) => {
                if self.settings.chat_lock_code_hash.is_none() && !code.trim().is_empty() {
                    self.settings.set_chat_lock_code(Some(code.trim()));
                    self.mark_settings_dirty();
                    self.enter_locked_folder();
                }
            }
            Action::CloseLockedFolder => {
                self.close_locked_folder();
                self.search.clear();
                self.search_hits.clear();
            }
            Action::SetChatLockCode(code) => {
                self.settings.set_chat_lock_code(code.as_deref());
                self.close_locked_folder();
                self.search.clear();
                self.search_hits.clear();
                self.mark_settings_dirty();
            }
            Action::SettingsChanged => self.mark_settings_dirty(),
            Action::SetAccountPrivacy { kind, choice } => {
                // The value lives on the phone: nothing is written without a
                // connection and a snapshot to write against.
                if self.is_connected()
                    && self.account_privacy.editable()
                    && self.account_privacy.begin_set(kind, choice)
                {
                    self.backend
                        .send(Command::SetAccountPrivacy { kind, choice });
                }
            }
            Action::SetNotificationSound { mention, sound } => {
                if mention {
                    self.settings.mention_sound = sound;
                } else {
                    self.settings.message_sound = sound;
                }
                self.mark_settings_dirty();
            }
            Action::PickNotificationSound { mention } => {
                self.backend
                    .send(Command::PickNotificationSound { mention });
            }
            Action::PreviewSound(sound) => {
                crate::notify::play_sound_on(sound, self.account().settings.call_speaker.clone())
            }
            Action::PickDownloadFolder => self.backend.send(Command::PickDownloadFolder),
            Action::SetProfile { name, about } => {
                self.backend.send(Command::SetProfile { name, about });
            }
            Action::PickLocalAvatar(id) => {
                if self
                    .chats
                    .iter()
                    .any(|chat| chat.id == id && chat.phone().is_some())
                    && self.me.as_deref() != Some(&id)
                {
                    self.backend.send(Command::PickLocalAvatar(id));
                }
            }
            Action::ResetLocalAvatar(id) => {
                if self
                    .chats
                    .iter()
                    .any(|chat| chat.id == id && chat.phone().is_some())
                    && self.me.as_deref() != Some(&id)
                {
                    self.backend.send(Command::ResetLocalAvatar(id));
                }
            }
            Action::PickProfilePicture => self.backend.send(Command::PickProfilePicture),
            Action::EditGroupName(name) => self.group_name_edit = Some(name),
            Action::CloseGroupName => self.group_name_edit = None,
            Action::SetGroupName { chat, name } => {
                self.group_name_edit = None;
                self.backend.send(Command::SetGroupName { chat, name });
            }
            Action::PickGroupPicture(chat) => self.backend.send(Command::PickGroupPicture(chat)),
            Action::RemoveGroupPicture(chat) => {
                self.backend
                    .send(Command::SetGroupPicture { chat, jpeg: None });
            }
            Action::SetChatSound { chat, sound } => {
                if let Some(known) = self.chat_mut(&chat) {
                    known.notification_sound = sound.clone();
                }
                self.backend.send(Command::SetChatSound { chat, sound });
            }
            Action::PickChatSound(chat) => self.backend.send(Command::PickChatSound(chat)),
            Action::SetSyncMode(mode) => {
                self.account_mut().settings.sync_mode = Some(mode);
                self.account_mut().settings.background_history =
                    mode == crate::settings::SyncMode::Full;
                self.account_mut().settings.archive_media = mode == crate::settings::SyncMode::Full;
                self.account_mut().mark_settings_dirty();
                self.backend.send(Command::ConfigureSync(mode));
                self.report_background_history();
            }
            Action::ChooseSync(mode) => {
                if !matches!(
                    self.link,
                    LinkStatus::Unlinked {
                        qr: None,
                        pair_code: None,
                        pairing_phone: None
                    }
                ) {
                    return;
                }
                let account = self.account_mut();
                let mut settings = account.settings.clone();
                settings.sync_mode = Some(mode);
                settings.background_history = mode == crate::settings::SyncMode::Full;
                settings.archive_media = mode == crate::settings::SyncMode::Full;
                // Save before connecting: a restart must keep the exact pairing choice.
                if let Err(error) = settings.save(&account.dirs.settings_file()) {
                    self.toast_error(format!("Could not save your sync choice: {error}"));
                    return;
                }
                account.settings = settings;
                account.backend.send(Command::ConfigureSync(mode));
                self.report_background_history();
            }
            Action::SetDownloadOrganization {
                organization,
                descriptive_names,
            } => {
                self.settings.download_organization = organization;
                self.settings.descriptive_download_names = descriptive_names;
                self.mark_settings_dirty();
                for account in &self.accounts {
                    account.backend.send(Command::SetDownloadOrganization {
                        organization,
                        descriptive_names,
                    });
                }
            }
            Action::ExportArchiveBackup(passphrase) => {
                if !self.backup_busy {
                    self.backup_busy = true;
                    self.backup_status = Some(
                        crate::i18n::gettext(self.locale, "Saving encrypted archive backup…")
                            .into_owned(),
                    );
                    self.backend.send(Command::ExportArchiveBackup(passphrase));
                }
            }
            Action::RestoreArchiveBackup(passphrase) => self.begin_archive_restore(passphrase),
            Action::PrepareBackupMerge(passphrase) => {
                if !self.backup_busy {
                    self.backup_busy = true;
                    self.backup_merge_preview = None;
                    self.backend.send(Command::PrepareBackupMerge(passphrase));
                }
            }
            Action::ConfirmBackupMerge => {
                if self.backup_merge_preview.is_some() && !self.backup_busy {
                    self.backup_busy = true;
                    self.backend.send(Command::ConfirmBackupMerge);
                }
            }
            Action::CancelBackupMerge => {
                self.backup_merge_preview = None;
                self.backend.send(Command::CancelBackupMerge);
            }
            Action::ResumeRestoredAccount(id) => {
                if let Some(index) = self.accounts.iter().position(|account| account.id == id) {
                    let account = &mut self.accounts[index];
                    if account.settings.archive_only && account.dirs.session_db().is_file() {
                        let mut settings = account.settings.clone();
                        settings.archive_only = false;
                        match settings.save(&account.dirs.settings_file()) {
                            Ok(()) => {
                                account.settings = settings;
                                account.backend.send(Command::ResumeRestoredAccount);
                            }
                            Err(error) => self
                                .toast_error(format!("Could not resume restored account: {error}")),
                        }
                    }
                }
            }
            Action::SetDownloadFolder(folder) => {
                self.settings.download_folder = folder.clone();
                self.mark_settings_dirty();
                for account in &self.accounts {
                    account
                        .backend
                        .send(Command::SetDownloadFolder(folder.clone()));
                }
            }
            Action::SetKeepChatsArchived(keep) => {
                self.settings.keep_chats_archived = keep;
                self.mark_settings_dirty();
                for account in &self.accounts {
                    account.backend.send(Command::SetKeepChatsArchived(keep));
                }
            }
            Action::SetProxy(value) => {
                let value = value.trim().to_owned();
                if value == self.settings.proxy {
                    return;
                }
                if !value.is_empty()
                    && let Err(error) = crate::proxy::Proxy::parse(&value)
                {
                    self.toast_error(error);
                    return;
                }
                self.settings.proxy = value.clone();
                self.mark_settings_dirty();
                crate::proxy::configure(&value);
                for account in &self.accounts {
                    account.backend.send(Command::SetProxy(value.clone()));
                }
            }
            Action::SetStartWithSystem(enabled) => match crate::autostart::set(enabled) {
                Ok(()) => self.start_with_system = Some(crate::autostart::enabled()),
                Err(error) => self.toast_error(format!("Could not change the login item: {error}")),
            },
            Action::ZoomBy(delta) => {
                self.settings.zoom = (self.settings.zoom + delta).clamp(0.6, 2.0);
                self.zoom_applied = false;
                self.mark_settings_dirty();
            }
            Action::SetTextScale(value) => {
                if value.is_finite() {
                    self.settings.text_scale = value.clamp(0.85, 1.5);
                    for account in &mut self.accounts {
                        for conversation in account.conversations.values_mut() {
                            conversation.rows.clear();
                        }
                    }
                    self.mark_settings_dirty();
                }
            }
            Action::ResetZoom => {
                self.settings.zoom = 1.0;
                self.zoom_applied = false;
                self.mark_settings_dirty();
            }
            Action::PairWithPhone(phone) => {
                let digits: String = phone.chars().filter(char::is_ascii_digit).collect();
                if digits.len() < 7 {
                    self.toast_error(
                        "Enter the phone number with its country code, using digits only",
                    );
                } else {
                    self.backend.send(Command::PairWithPhone(digits));
                }
            }
            Action::Unlink => {
                self.dialog = None;
                self.backend.send(Command::Unlink);
            }
            Action::LockApp => self.lock_app(),
            Action::UnlockApp => {
                if let Some(stored) = self.settings.app_lock_hash.clone() {
                    let waker = self.waker.clone();
                    self.app_lock.try_unlock(&stored, move || waker.wake());
                }
            }
            Action::ForgotAppPassword(open) => {
                use crate::app_lock::Forgetting;
                if self.app_lock.forgetting != Forgetting::Unlinking {
                    self.app_lock.forgetting = if open {
                        Forgetting::Confirming
                    } else {
                        Forgetting::No
                    };
                }
            }
            Action::UnlinkLockedApp => {
                // The lock lifts only once WhatsApp has unlinked and the
                // chats are gone (`LinkStatus::LoggedOut`), never before.
                // Every number linked here goes: the lock guards them all.
                if self.app_lock.is_locked() {
                    self.app_lock.forgetting = crate::app_lock::Forgetting::Unlinking;
                    for account in &self.accounts {
                        account.backend.send(Command::Unlink);
                    }
                }
            }
            Action::AppLockForm(mode) => {
                use crate::app_lock::{Form, FormMode};
                // Setting a password must not replace one without it, and
                // changing or removing one needs one to exist.
                let fits = |mode: FormMode| {
                    (mode == FormMode::Set) == self.settings.app_lock_hash.is_none()
                };
                if !self.app_lock.checking() {
                    self.app_lock.form = mode.filter(|mode| fits(*mode)).map(Form::new);
                }
            }
            Action::SubmitAppLockForm => {
                use crate::app_lock::FormMode;
                let stored = self.settings.app_lock_hash.clone();
                let fits = self
                    .app_lock
                    .form
                    .as_ref()
                    .is_some_and(|form| (form.mode == FormMode::Set) == stored.is_none());
                if fits {
                    let waker = self.waker.clone();
                    self.app_lock
                        .submit_form(stored.as_deref(), move || waker.wake());
                } else {
                    self.app_lock.form = None;
                }
            }
            Action::SetAutoLock(after) => {
                self.settings.app_lock_after = after;
                self.app_lock.note_input();
                self.mark_settings_dirty();
            }
            Action::Reconnect => self.backend.send(Command::Reconnect),
            Action::StartOverArchive => {
                self.dialog = None;
                self.backend.send(Command::StartOverArchive);
            }
            Action::Quit => {
                self.quit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Action::DismissInAppNotification(id) => {
                self.in_app_notifications
                    .retain(|notification| notification.id != id);
            }
            Action::OpenInAppNotification(id) => {
                let now = std::time::Instant::now();
                let target = self
                    .in_app_notifications
                    .iter()
                    .find(|notification| notification.id == id && notification.expires > now)
                    .map(|notification| notification.target.clone());
                self.in_app_notifications
                    .retain(|notification| notification.id != id);
                if let Some(target) = target
                    && self.accounts.iter().any(|account| {
                        account.id == target.account
                            && account.settings.notifications
                            && account.settings.in_app_notifications
                            && account
                                .chats
                                .iter()
                                .any(|chat| chat.id == target.chat && !chat.locked)
                    })
                {
                    self.notification_opens
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .push(target);
                    self.handle_notification_opens();
                }
            }
            Action::ShowWindow => {
                let closing_to_hide = self.hide_intent;
                self.hide_intent = false;
                if self.window_hidden {
                    // The headless loop in `main` will create the window.
                    self.wants_show = true;
                } else if self.wayland
                    && !closing_to_hide
                    && (self.niri_focus_pending.is_some() || {
                        self.niri_focus_pending = crate::window::start_niri_focus(ctx.clone());
                        self.niri_focus_pending.is_some()
                    })
                {
                    // The compositor request completes off the interface thread.
                } else if self.wayland || closing_to_hide {
                    // A previous hide already queued Close, so every platform
                    // must recreate the window when Show supersedes that hide.
                    // Wayland drops a programmatic focus or unminimize
                    // request, so a minimized or covered window cannot come
                    // forward that way. Close it and let the shell open a
                    // fresh one at once: the compositor raises a new toplevel,
                    // and a notification click lands on a visible window.
                    self.reopen = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    // Focus alone leaves a minimized window where it is on
                    // Windows, so restore it first.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
            Action::HideWindow => {
                if self.tray_shown() {
                    self.hide_window(ctx);
                }
            }
            // Route through the configured window-close behavior.
            Action::CloseWindow => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Action::SwitchAccount(id) => self.switch_account(&id),
            Action::AddAccount => self.add_account(),
            Action::CancelAddAccount => {
                // Back to the account the window showed before; leaving an
                // unlinked new account removes it.
                if self.adding_account
                    && let Some(back) = self.account_before_adding.clone().or_else(|| {
                        self.accounts
                            .iter()
                            .find(|account| account.id != self.account().id)
                            .map(|account| account.id.clone())
                    })
                {
                    self.switch_account(&back);
                }
            }
            Action::RemoveAccount(id) => self.remove_account(id),
        }
    }

    /// Makes an explicit hide supersede any pending window recreation or show request.
    fn hide_window(&mut self, ctx: &egui::Context) {
        self.reopen = false;
        self.wants_show = false;
        self.hide_intent = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    pub fn toast(&mut self, message: impl Into<String>) {
        self.toasts.push(Toast {
            message: message.into(),
            kind: ToastKind::Info,
            created: Instant::now(),
            undo: None,
        });
        while self
            .toasts
            .iter()
            .filter(|toast| toast.kind == ToastKind::Info)
            .count()
            > 4
        {
            if let Some(oldest) = self
                .toasts
                .iter()
                .position(|toast| toast.kind == ToastKind::Info)
            {
                self.toasts.remove(oldest);
            }
        }
    }

    fn schedule_undo(&mut self, operation: crate::undo::Operation, label: &'static str) {
        let (token, superseded) =
            self.undo
                .schedule(self.account().id.clone(), operation, Instant::now());
        if let Some(old) = superseded {
            self.toasts.retain(|toast| toast.undo != Some(old));
        }
        self.toast(crate::i18n::gettext(self.locale, label).into_owned());
        if let Some(toast) = self.toasts.last_mut() {
            toast.undo = Some(token);
        }
    }

    fn undo_toast(&mut self, token: u64) {
        let Some(pending) = self.undo.cancel(token) else {
            return;
        };
        self.toasts.retain(|toast| toast.undo != Some(token));
        if let crate::undo::Operation::ArchiveChat {
            chat,
            before,
            after,
        } = pending.operation
            && let Some(account) = self
                .accounts
                .iter_mut()
                .find(|account| account.id == pending.account)
            && let Some(known) = account.chats.iter_mut().find(|known| known.id == chat)
            && known.archived == after
        {
            known.archived = before;
        }
    }

    fn flush_undo(&mut self, now: Instant, ctx: &egui::Context) {
        for pending in self.undo.due(now) {
            self.toasts
                .retain(|toast| toast.undo != Some(pending.token));
            let Some(account) = self
                .accounts
                .iter()
                .find(|account| account.id == pending.account)
            else {
                continue;
            };
            if matches!(account.link, LinkStatus::LoggedOut) {
                continue;
            }
            match pending.operation {
                crate::undo::Operation::DeleteMessage { chat, id } => {
                    account.backend.send(Command::DeleteLocal { chat, id })
                }
                crate::undo::Operation::ArchiveChat {
                    chat,
                    before,
                    after,
                } if before != after => account.backend.send(Command::SetArchived(chat, after)),
                crate::undo::Operation::ArchiveChat { .. } => {}
            }
        }
        if let Some(deadline) = self.undo.next_deadline() {
            ctx.request_repaint_after(deadline.saturating_duration_since(now));
        }
    }

    #[cfg(test)]
    pub(crate) fn expire_undo_for_test(&mut self, ctx: &egui::Context) {
        self.flush_undo(Instant::now() + crate::undo::GRACE, ctx);
    }

    pub fn toast_error(&mut self, message: impl Into<String>) {
        self.announce_accessibility("An operation failed. Review the error notification.");
        let message = message.into();
        log::warn!("an operation failed; details are shown in the window");
        // Errors stay until dismissed: a repeat moves to the end instead of
        // stacking, and only the newest few are kept.
        self.toasts
            .retain(|toast| toast.kind != ToastKind::Error || toast.message != message);
        while self
            .toasts
            .iter()
            .filter(|toast| toast.kind == ToastKind::Error)
            .count()
            >= MAX_ERROR_TOASTS
        {
            let oldest = self
                .toasts
                .iter()
                .position(|toast| toast.kind == ToastKind::Error)
                .expect("counted above");
            self.toasts.remove(oldest);
        }
        self.toasts.push(Toast {
            message,
            kind: ToastKind::Error,
            created: Instant::now(),
            undo: None,
        });
    }

    fn announce_accessibility(&mut self, message: &str) {
        if !self.app_lock.is_locked()
            && !self.events_hidden
            && !self.window_hidden
            && self.accessibility_announcements.len() < 8
        {
            self.accessibility_announcements.push(message.to_owned());
        }
    }

    /// Starts the quote a starred row asked for, once the message it points at
    /// is in the open chat. The composer drops a quote whose message has not
    /// loaded, so waiting for it here keeps Reply working from the list.
    fn settle_pending_reply(&mut self) {
        if self.events_hidden {
            return;
        }
        let Some((chat, id)) = self.reply_when_loaded.clone() else {
            return;
        };
        let loaded = self.open_chat.as_deref() == Some(chat.as_str())
            && self
                .conversations
                .get(&chat)
                .is_some_and(|conversation| conversation.message(&id).is_some());
        if !loaded {
            return;
        }
        self.reply_when_loaded = None;
        self.reply_to = Some(id.into());
        self.focus_composer = true;
    }

    /// Asks the worker for the starred list again, for the one on screen. A
    /// message that changed in the archive is a stale row otherwise.
    fn reload_lists(&mut self) {
        if self.show_starred {
            self.backend.send(Command::LoadStarred { offset: 0 });
        }
    }

    /// Chats pinned to the top, counted the way WhatsApp limits them.
    fn pinned_count(&self) -> usize {
        self.chats
            .iter()
            .filter(|chat| chat.pinned && !chat.archived)
            .count()
    }

    /// Tells the backend whether the person is looking at the app, so the
    /// phone keeps its notifications while they are not.
    fn report_presence(&mut self) {
        let looking = self.window_focused && !self.window_hidden;
        let active = self.active;
        for (index, account) in self.accounts.iter_mut().enumerate() {
            let online = looking && index == active;
            if account.reported_online != Some(online) {
                account.reported_online = Some(online);
                account.backend.send(Command::SetOnline(online));
            }
        }
    }

    /// Forward recovery policy only when the account policy or lock changes.
    fn report_background_history(&mut self) {
        let paused = self.app_lock.is_locked();
        for account in &mut self.accounts {
            if account.settings.auto_download && account.settings.automatic_download_since.is_none()
            {
                let mut settings = account.settings.clone();
                settings.automatic_download_since = Some(crate::util::now());
                // Commit the enable boundary before authorizing any automatic user-folder copy.
                if settings.save(&account.dirs.settings_file()).is_ok() {
                    account.settings = settings;
                } else {
                    log::warn!("automatic export cutoff could not be saved; exports remain paused");
                }
            }
            let mut exports = account.settings.automatic_export_policy();
            exports.enabled &= exports.since.is_some();
            if account.reported_exports != Some(exports) {
                account.reported_exports = Some(exports);
                account
                    .backend
                    .send(Command::SetAutomaticExportPolicy(exports));
            }
            let policy = (
                account.settings.background_history,
                account.settings.archive_media,
                paused,
                account.settings.history_requests_per_day,
                account.settings.background_media_budget_mib,
                account.open_chat.clone(),
            );
            if account.reported_history.as_ref() != Some(&policy) {
                account.reported_history = Some(policy.clone());
                if paused && let Some(request) = &account.pending_call_request {
                    request.cancel();
                    account.call_preparing = false;
                }
                account.backend.send(Command::CallPrivacyPaused(paused));
                account.backend.send(Command::BackgroundHistory {
                    enabled: policy.0,
                    media: policy.1,
                    paused: policy.2,
                    daily_budget: policy.3,
                    media_budget_mib: policy.4,
                    focused: policy.5,
                });
            }
        }
    }

    /// Processes app state shared by windowed and headless modes.
    pub fn background_frame(&mut self, ctx: &egui::Context) {
        self.flush_undo(Instant::now(), ctx);
        if let Some(pending) = &self.custom_emoji_pending {
            let request = pending.request;
            match pending.receiver.try_recv() {
                Ok(result) => {
                    self.custom_emoji_pending = None;
                    if request == self.custom_emoji_request {
                        match result {
                            Some(Ok(path)) => self
                                .actions
                                .push(Action::SetCustomEmojiFont { path, request }),
                            Some(Err(error)) => {
                                self.toast_error(format!("Could not use this emoji font: {error}"))
                            }
                            None => {}
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.custom_emoji_pending = None;
                    if request == self.custom_emoji_request {
                        self.toast_error("The emoji font picker closed unexpectedly");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(pending) = &self.custom_font_pending {
            let request = pending.request;
            match pending.receiver.try_recv() {
                Ok(result) => {
                    self.custom_font_pending = None;
                    if request == self.custom_font_request {
                        match result {
                            Some(Ok(path)) => self.actions.push(Action::SetCustomFont(path)),
                            Some(Err(error)) => {
                                self.toast_error(format!("Could not use this font: {error}"))
                            }
                            None => {}
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.custom_font_pending = None;
                    if request == self.custom_font_request {
                        self.toast_error("The font picker closed unexpectedly");
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(pending) = &self.niri_focus_pending {
            match pending.try_recv() {
                Ok(focused) => {
                    self.niri_focus_pending = None;
                    if !focused && !self.window_hidden && !self.hide_intent && !self.quit_requested
                    {
                        self.reopen = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.niri_focus_pending = None;
                    if !self.window_hidden && !self.hide_intent && !self.quit_requested {
                        self.reopen = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }

        if let Some(deadline) = self.tray_start_deadline {
            if self.tray_shown() {
                self.tray_start_deadline = None;
            } else if std::time::Instant::now() >= deadline {
                self.tray_start_deadline = None;
                self.wants_show = true;
            } else {
                ctx.request_repaint_after(
                    deadline.saturating_duration_since(std::time::Instant::now()),
                );
            }
        }
        // Events are drained before frame_ui observes focus. Losing focus in
        // this frame must take effect before an incoming chat update can read it.
        if self.window_hidden || ctx.input(|input| input.viewport().focused) == Some(false) {
            self.window_focused = false;
        }
        self.tick_app_lock(ctx);
        // Nobody reads behind the lock screen: messages stay unread, the
        // phone keeps notifying, and notifications here say nothing.
        if self.app_lock.is_locked() {
            self.window_focused = false;
        }
        self.report_presence();
        self.report_background_history();
        self.handle_power();
        self.handle_tray();
        #[cfg(target_os = "macos")]
        {
            let close = crate::ui::keys::close_shortcut_action(self, ctx);
            self.actions
                .extend(crate::macos::drain(self.window_hidden, close));
        }
        self.handle_control_commands();
        self.poll_custom_themes();
        let wallpaper = self.account().settings.wallpaper_image.clone();
        self.wallpaper_image.sync(wallpaper.as_deref(), &self.waker);
        self.handle_notification_opens();
        self.handle_events();
        self.handle_chat_links(ctx);
        self.tick(ctx);
        self.tick_audio(ctx);
        self.tick_video(ctx);
        self.apply_actions(ctx);
        self.hold_media();
        self.follow_receipts();
        self.sync_badge();
        self.request_attention(ctx);
        #[cfg(target_os = "windows")]
        self.sync_native_notifications(ctx);
    }

    #[cfg(target_os = "windows")]
    fn sync_native_notifications(&mut self, ctx: &egui::Context) {
        let enabled = self
            .accounts
            .iter()
            .any(|account| account.settings.notifications && account.settings.in_app_notifications);
        if !enabled {
            self.native_notifications = None;
            self.native_notifications_tried = false;
            return;
        }
        if !self.native_notifications_tried {
            self.native_notifications_tried = true;
            self.native_notifications = crate::notify::NativePopups::start(self.waker.clone());
        }
        let snapshots = if self.window_focused && !self.window_hidden {
            Vec::new()
        } else {
            crate::ui::notification_overlay::snapshots(self)
        };
        let Some(native) = self.native_notifications.as_mut() else {
            return;
        };
        self.actions.extend(native.drain());
        if !native.sync(snapshots) {
            self.native_notifications = None;
        }
        self.apply_actions(ctx);
    }

    /// Answers the system's sleep and wake. Every account reconnects its link
    /// after a wake (Linux and Windows report it; macOS does not yet, so
    /// there the worker's link watch notices the sleep on its own). Settings
    /// waiting for their debounced save are saved before the machine sleeps
    /// or the session ends.
    fn handle_power(&mut self) {
        for event in self.power.events() {
            match event {
                fastframe_power::Event::Suspending(ack)
                | fastframe_power::Event::EndingSession(ack) => {
                    self.save_state();
                    ack.done();
                }
                fastframe_power::Event::Resumed => {
                    for account in &self.accounts {
                        account.backend.send(Command::Resumed);
                    }
                }
            }
        }
    }

    /// Highlights the taskbar entry for a message that notified while the
    /// window is open behind others, as KDE Plasma's task manager shows for
    /// chat apps. The desktop clears it once the window is focused. Linux
    /// only: elsewhere it would bounce the Dock or flash the taskbar. Asks the
    /// viewport too, since `window_focused` stays false behind the app lock
    /// even while the lock screen has the focus.
    fn request_attention(&mut self, ctx: &egui::Context) {
        self.request_attention_on(ctx, fastframe_desktop::is_gnome());
    }

    /// [`Self::request_attention`] on a desktop that is GNOME or not, which
    /// shows its own "is ready" notice instead of marking the taskbar (#466).
    fn request_attention_on(&mut self, ctx: &egui::Context, gnome: bool) {
        if std::mem::take(&mut self.wants_attention)
            && cfg!(target_os = "linux")
            && !gnome
            && !self.window_hidden
            && !self.window_focused
            && ctx.input(|input| input.viewport().focused) != Some(true)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }

    /// Collects a finished password check, and locks once Zap Faster has gone
    /// unused for the chosen time. Inactivity counts while the window is
    /// hidden too: only input in the window restarts it.
    fn tick_app_lock(&mut self, ctx: &egui::Context) {
        if let Some(outcome) = self.app_lock.poll() {
            self.app_lock_outcome(outcome);
        }
        if self.settings.app_lock_hash.is_none() {
            // Nothing to unlock with; never leave the window stuck.
            if self.app_lock.is_locked() {
                self.app_lock.release();
            }
            return;
        }
        if self.app_lock.is_locked() {
            return;
        }
        // Recording a voice message is using Zap Faster, keys or not.
        if self.recording.is_some() {
            self.app_lock.note_input();
        }
        let left = self
            .app_lock
            .idle_left(self.settings.app_lock_after.duration());
        if left.is_zero() {
            self.lock_app();
        } else {
            ctx.request_repaint_after(left);
        }
    }

    /// Hides everything behind the lock screen. What was open stays open
    /// for after the unlock, except what plays, records, or shows beyond the
    /// window: media stops, a recording is discarded, the locked-chats folder
    /// closes, and notifications already on the desktop are withdrawn.
    pub fn lock_app(&mut self) {
        for account in &self.accounts {
            if let Some(request) = &account.pending_call_request {
                request.cancel();
            }
            account.backend.send(Command::CallPrivacyPaused(true));
        }
        if self.settings.app_lock_hash.is_none() || self.app_lock.is_locked() {
            return;
        }
        if let Some(server) = &self.automation {
            server.permits.revoke_all();
            server
                .locked
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.app_lock.lock();
        self.accessibility_announcements.clear();
        self.in_app_notifications.clear();
        self.window_focused = false;
        self.flush_open_draft();
        self.recording = None;
        self.player.stop();
        self.video.stop();
        self.voice_chat = None;
        self.voice_wanted = None;
        self.video_wanted = None;
        self.dropping = false;
        self.notifications.clear_all();
        self.clear_chat_lock_entry();
        if self.dialog == Some(Dialog::UnlockLockedChats) {
            self.dialog = None;
        }
        if self.locked_folder || self.secret_code_matched() {
            self.close_locked_folder();
            self.search.clear();
            self.search_hits.clear();
        }
        self.copy_rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// Applies what the password thread worked out.
    fn app_lock_outcome(&mut self, outcome: crate::app_lock::Outcome) {
        use crate::app_lock::{FormError, Outcome};
        match outcome {
            Outcome::Unlock(matched) => {
                if !self.app_lock.is_locked() {
                    return;
                }
                self.app_lock.unlocked(matched);
                if matched && let Some(target) = self.app_lock.deferred.take() {
                    self.switch_account(&target.account);
                    self.actions.push(Action::OpenMessage {
                        chat: target.chat,
                        message: target.message,
                    });
                }
            }
            Outcome::WrongCurrent => {
                if let Some(form) = &mut self.app_lock.form {
                    form.busy = false;
                    form.error = Some(FormError::WrongCurrent);
                }
            }
            Outcome::Set(verifier) => {
                if self.app_lock.form.take().is_none() {
                    return;
                }
                self.settings.app_lock_hash = Some(verifier);
                self.app_lock.note_input();
                self.save_settings();
                self.toast(crate::i18n::gettext(self.locale, "App lock password saved"));
            }
            Outcome::TurnOff => {
                if self.app_lock.form.take().is_none() {
                    return;
                }
                self.settings.app_lock_hash = None;
                self.save_settings();
                self.toast(crate::i18n::gettext(self.locale, "App lock turned off"));
            }
        }
    }

    /// Forgets the app lock password and lifts the lock: after unlinking,
    /// when nothing is left behind it.
    fn forget_app_lock(&mut self) {
        self.app_lock.form = None;
        self.app_lock.deferred = None;
        self.app_lock.release();
        if self.settings.app_lock_hash.take().is_some() {
            self.save_settings();
        }
    }

    /// Mirrors the unread chat count onto the taskbar icon, where the desktop
    /// reads it. The badge ignores repeats, so calling this each frame is cheap.
    fn sync_badge(&mut self) {
        let count = if self.app_lock.is_locked() {
            0
        } else {
            self.unread_chat_count_everywhere()
        };
        if let Some(badge) = &mut self.badge {
            badge.set(count);
        }
        let private = self.app_lock.is_locked() || self.settings.screen_privacy.enabled;
        let vip = !private
            && self.settings.tray_vip
            && self.accounts.iter().any(|account| {
                account.chats.iter().any(|chat| {
                    account.settings.vip_chats.contains(&chat.id)
                        && !chat.locked
                        && !chat.archived
                        && !chat.muted(crate::util::now())
                        && chat.looks_unread()
                })
            });
        let count = if private { 0 } else { count };
        if crate::tray_indicator::publish(count, vip, self.settings.tray_unread_numbers)
            && let Some(tray) = &mut self.tray
        {
            if vip || (self.settings.tray_unread_numbers && count > 0) {
                tray.set_icon(crate::tray_indicator::draw, None);
            } else {
                tray.set_icon(
                    crate::util::app_icon_rgba,
                    Some(crate::util::tray_template_rgba),
                );
            }
        }
    }

    /// The unread chat count for the Windows taskbar overlay, which the window
    /// applies itself; `None` in demo and test runs.
    #[cfg(target_os = "windows")]
    pub fn taskbar_badge_count(&self) -> Option<u32> {
        self.badge.as_ref()?.count()
    }

    /// Pauses other apps' music while recording or playing audio, as the
    /// settings allow, and resumes it once neither needs quiet.
    fn hold_media(&mut self) {
        let wanted = self.pauses_media && self.wants_quiet();
        if wanted != self.media_hold.is_some() {
            self.media_hold = wanted.then(crate::media_pause::hold);
        }
    }

    /// Whether recording or playback needs other apps' media paused now. A
    /// run of voice messages keeps it paused while the next clip downloads, so
    /// music does not resume and pause again between clips, but only for a
    /// while.
    fn wants_quiet(&self) -> bool {
        let fetching_next = self
            .voice_wanted
            .as_ref()
            .is_some_and(|(_, _, since)| since.elapsed() < VOICE_FETCH_HOLD);
        self.settings.pause_other_media
            && (self.recording.is_some()
                || self.player.is_playing()
                || fetching_next
                || self.video.is_active() && !self.video.muted())
    }

    /// Keeps the backend following receipts for exactly the group message
    /// whose "Message info" is open. A direct message's times are on its row.
    fn follow_receipts(&mut self) {
        let wanted = match &self.dialog {
            Some(Dialog::MessageInfo { chat, message })
                if crate::model::ChatKind::from_id(chat) == crate::model::ChatKind::Group =>
            {
                Some((chat.clone(), message.clone()))
            }
            _ => None,
        };
        if wanted == self.receipts_watch {
            return;
        }
        self.message_receipts = None;
        self.receipts_watch = wanted.clone();
        self.backend.send(Command::WatchReceipts(wanted));
    }

    /// Polls audio state and schedules repaints while it changes.
    fn tick_audio(&mut self, ctx: &egui::Context) {
        self.player
            .set_output_device(self.account().settings.call_speaker.clone());
        if let Some(notice) = self.player.take_notice() {
            self.toast_error(notice);
        }
        if let Err(error) = self.player.poll() {
            self.toast_error(error);
        }
        if let Some(finished) = self.player.take_finished() {
            self.continue_voice(&finished);
        }
        if let Some(error) = self.recording.as_ref().and_then(Recorder::failure) {
            self.recording = None;
            self.toast_error(format!("Could not record: {error}"));
        }
        // The playback bar, the recording light and the waveform only show
        // progress, so they step while a screen reader is attached.
        let stepped = crate::motion::screen_reader_attached(ctx);
        if let Some(recorder) = &self.recording {
            recorder.set_stepped(stepped);
        }
        if self.player.is_playing() || self.recording.is_some() {
            self.waker.wake_after(if stepped {
                crate::motion::STEP
            } else {
                Duration::from_millis(40)
            });
        }
        // Let the media hold lapse on time if the next clip never lands.
        if let Some((_, _, since)) = &self.voice_wanted {
            let left = VOICE_FETCH_HOLD.saturating_sub(since.elapsed());
            if !left.is_zero() {
                self.waker.wake_after(left);
            }
        }
    }

    /// Shows the playing video's frames, stops it once its chat is left, and
    /// hands a video it cannot decode to the system player.
    fn tick_video(&mut self, ctx: &egui::Context) {
        self.video
            .set_output_device(self.account().settings.call_speaker.clone());
        if self.video.message().is_some() && self.video_chat != self.open_chat {
            self.video.stop();
        }
        if self.video_expanded && self.video.message().is_none() {
            self.video_expanded = false;
            self.video.set_expanded(false);
        }
        match self.video.poll(ctx) {
            Some(crate::video::Notice::Unsupported(path)) => {
                self.toast_error(crate::i18n::gettext(self.locale, "Cannot play this video here; opening it in your system player"));
                self.actions.push(Action::OpenFile(path));
            }
            Some(crate::video::Notice::OutputFallback(notice)) => self.toast_error(notice),
            Some(crate::video::Notice::OutputUnavailable) => self.toast_error(crate::i18n::gettext(self.locale, "The selected audio output is unavailable. Choose an output and resume playback.")),
            None => {}
        }
    }

    /// Plays or pauses a video in its message. A video message, the round
    /// kind, sends its played receipt like a voice message.
    fn play_video(&mut self, message: String, path: PathBuf) {
        self.video
            .set_output_device(self.account().settings.call_speaker.clone());
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        // One sound at a time; a video also ends a run of voice messages.
        self.player.stop();
        self.voice_chat = None;
        self.voice_wanted = None;
        let starting = self.video.message() != Some(message.as_str());
        self.video.toggle(&message, &path);
        self.video_chat = Some(chat.clone());
        let note = self
            .conversations
            .get(&chat)
            .and_then(|conversation| conversation.message(&message))
            .is_some_and(|row| matches!(row.content, Content::Video { note: true, .. }));
        if starting && note {
            self.tell_played(message);
        }
    }

    /// Plays or pauses audio and sends the first played receipt when needed.
    fn play_voice(&mut self, message: String, path: PathBuf) {
        self.player
            .set_output_device(self.account().settings.call_speaker.clone());
        self.video.stop();
        self.voice_wanted = None;
        self.voice_chat = self.open_chat.clone();
        if let Err(error) = self.player.toggle(&message, &path) {
            self.toast_error(error);
            return;
        }
        self.tell_played(message);
    }

    fn tell_played(&mut self, message: String) {
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        if self.played_told.contains(&message) {
            return;
        }
        let Some(row) = self
            .conversations
            .get(&chat)
            .and_then(|conversation| conversation.message(&message))
        else {
            return;
        };
        // Played on another device: the phone already told the sender.
        if row.from_me || row.status == Delivery::Played {
            return;
        }
        let sender = row.sender.clone();
        self.played_told.insert(message.clone());
        self.backend.send(Command::MarkPlayed {
            chat,
            message,
            sender,
            receipts: self.account().settings.send_read_receipts,
        });
    }

    /// Starts the next unheard voice message after one plays to its end, as
    /// the phone does. Only a clip that finished in the chat the reader is
    /// still in carries on. A clip that is not downloaded yet is fetched first
    /// and plays when it lands.
    fn continue_voice(&mut self, finished: &str) {
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        if self.voice_chat.as_ref() != Some(&chat) {
            return;
        }
        let Some(next) = self.next_voice_after(&chat, finished) else {
            return;
        };
        match self
            .conversations
            .get(&chat)
            .and_then(|conversation| conversation.message(&next))
            .and_then(|message| message.content.media())
            .and_then(|media| media.path.clone())
        {
            Some(path) => self.actions.push(Action::PlayVoice {
                message: next,
                path,
            }),
            None => {
                self.voice_wanted = Some((chat.clone(), next.clone(), Instant::now()));
                self.actions.push(Action::Download {
                    card: None,
                    chat,
                    message: next,
                });
            }
        }
    }

    /// The next voice message after `finished` that the reader has not heard
    /// yet. As on the phone, the run goes through consecutive voice messages:
    /// the reader's own and those already played here are passed over, and
    /// anything else, an audio file included, ends it.
    fn next_voice_after(&self, chat: &str, finished: &str) -> Option<String> {
        let conversation = self.conversations.get(chat)?;
        let position = conversation
            .messages
            .iter()
            .position(|message| message.id == finished)?;
        let voice_note = |message: &Message| {
            matches!(
                message.content,
                Content::Audio {
                    voice_note: true,
                    ..
                }
            )
        };
        if !voice_note(&conversation.messages[position]) {
            return None;
        }
        conversation.messages[position + 1..]
            .iter()
            .take_while(|message| voice_note(message))
            .find(|message| {
                !message.from_me
                    && message.status != Delivery::Played
                    && !self.played_told.contains(&message.id)
            })
            .map(|message| message.id.clone())
    }

    /// Stops and sends a recording unless it is under one second. With no
    /// recorder running, sends the open chat's refused voice message again;
    /// it quotes whatever the reply banner shows now, so a reply the worker
    /// refused for its missing original is only sent unquoted after the
    /// user cancels the reply.
    fn send_recording(&mut self) {
        let Some(recorder) = self.recording.take() else {
            if let Some(chat) = self.open_chat.clone()
                && self
                    .unsent_voice
                    .as_ref()
                    .is_some_and(|(unsent, _)| *unsent == chat)
            {
                if self
                    .reply_to
                    .as_ref()
                    .is_some_and(|target| !self.reply_source_visible(target, &chat))
                {
                    self.send_refused(
                        chat,
                        self.reply_to.clone(),
                        Unsent::Text(String::new()),
                        Refusal::QuoteUnavailable,
                    );
                    return;
                }
                let Some((_, samples)) = self.unsent_voice.take() else {
                    return;
                };
                let quoting = self.reply_to.take();
                self.backend.send(Command::SendVoice {
                    chat,
                    samples,
                    quoting,
                });
                self.follow_outgoing();
            }
            return;
        };
        let Some(chat) = self.open_chat.clone() else {
            return;
        };
        match recorder.finish() {
            Ok(samples) if samples.len() < crate::voice::RATE as usize / 2 => {}
            Ok(samples) => {
                if self
                    .reply_to
                    .as_ref()
                    .is_some_and(|target| !self.reply_source_visible(target, &chat))
                {
                    self.send_refused(
                        chat,
                        self.reply_to.clone(),
                        Unsent::Voice(samples),
                        Refusal::QuoteUnavailable,
                    );
                    return;
                }
                let quoting = self.reply_to.take();
                self.backend.send(Command::SendVoice {
                    chat,
                    samples,
                    quoting,
                });
                self.follow_sent_chat();
            }
            Err(error) => self.toast_error(format!("Could not record: {error}")),
        }
    }

    /// Asks the worker for the archive's storage numbers while Settings is
    /// open, at most once per [`STORAGE_STATS_TTL`]. The request lives here
    /// rather than in the view: `src/ui` draws and pushes actions, and only
    /// `App` talks to the worker.
    fn refresh_storage_stats(&mut self, ctx: &egui::Context) {
        if self.page != Page::Settings || self.storage_stats_asked {
            return;
        }
        let stale = self
            .storage_stats_at
            .is_none_or(|at| at.elapsed() >= STORAGE_STATS_TTL);
        if stale {
            self.storage_stats_token = self.storage_stats_token.wrapping_add(1);
            self.backend.send(Command::StorageStats {
                messages: !self.storage_stats_counted,
                token: self.storage_stats_token,
            });
            self.storage_stats_asked = true;
        } else if let Some(at) = self.storage_stats_at {
            // Settings can sit idle. Ask for a frame when the minute ends,
            // or the sizes stay frozen until the reader does something.
            let left = STORAGE_STATS_TTL.saturating_sub(at.elapsed());
            ctx.request_repaint_after(left);
        }
    }

    pub fn frame_ui(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.settle_pending_reply();
        self.copy_rows
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        *self
            .selection_view
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = None;
        self.apply_theme(ctx);
        if ctx.input(|input| input.events.iter().any(is_user_input)) {
            self.app_lock.note_input();
        }
        let locked = self.app_lock.is_locked();
        // Behind the lock screen nobody is reading; unlocking counts as
        // coming back to the window.
        let focused = ctx.input(|input| input.viewport().focused.unwrap_or(true)) && !locked;
        let regained_focus = focused && !self.window_focused;
        // Mark messages received while hidden as read on window return.
        if regained_focus
            && self.page == Page::Chats
            && let Some(open) = self.open_chat.clone()
            && self.chat(&open).is_some_and(|chat| chat.unread > 0)
        {
            self.mark_read(&open);
        }
        if regained_focus {
            self.refocus_composer(ctx);
        }
        self.window_focused = focused;
        self.report_presence();
        self.report_background_history();
        if locked {
            // Files dropped or pasted on the lock screen go nowhere.
            self.dropping = false;
        } else {
            self.scrolling.apply(ctx);
            self.route_scroll(ctx);
            self.take_drops_and_pastes(ctx);
        }
        crate::ui::show(self, ui);
        self.apply_actions(ctx);
        let title = self.accessible_window_title();
        let title_id = egui::Id::new("native-window-title");
        if ctx.data(|data| data.get_temp::<String>(title_id)).as_ref() != Some(&title) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            ctx.data_mut(|data| data.insert_temp(title_id, title));
        }
        self.refresh_storage_stats(ctx);
        // The old colours, if a change is being revealed, go over everything.
        self.theme_transition.paint(ctx);
        // Release the image caches of everything that scrolled away.
        crate::image_cache::sweep(ctx);
        // Only fading info toasts animate. Errors wait for the reader.
        if self
            .toasts
            .iter()
            .any(|toast| toast.kind == ToastKind::Info)
        {
            ctx.request_repaint_after(Duration::from_millis(120));
        }
    }

    pub(crate) fn accessible_window_title(&self) -> String {
        if self.app_lock.is_locked()
            || self
                .settings
                .screen_privacy
                .hides(crate::settings::ScreenPrivacyWhat::Name, false)
            || self.page != Page::Chats
        {
            return "Zap Faster".into();
        }
        self.current_chat().filter(|chat| !chat.locked).map_or_else(
            || "Zap Faster".into(),
            |chat| {
                let name: String = self
                    .chat_title(chat)
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(120)
                    .collect();
                format!("{name}, Zap Faster")
            },
        )
    }

    /// Inserts text at the composer cursor or end.
    fn insert_in_composer(&mut self, ctx: &egui::Context, text: &str) {
        let id = egui::Id::new("composer-text");
        let at = egui::TextEdit::load_state(ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index.0)
            .unwrap_or_else(|| self.composer.chars().count());
        let at = at.min(self.composer.chars().count());
        let byte = self
            .composer
            .char_indices()
            .nth(at)
            .map_or(self.composer.len(), |(byte, _)| byte);
        self.composer.insert_str(byte, text);
        self.set_composer_cursor(ctx, at + text.chars().count());
    }

    fn set_composer_cursor(&self, ctx: &egui::Context, at: usize) {
        let id = egui::Id::new("composer-text");
        {
            let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(at),
                )));
            egui::TextEdit::store_state(ctx, id, state);
        }
    }

    fn remember_emoji(&mut self, emoji: &str) {
        self.settings.recent_emoji.retain(|known| known != emoji);
        self.settings.recent_emoji.insert(0, emoji.to_owned());
        self.settings.recent_emoji.truncate(36);
        self.mark_settings_dirty();
    }

    /// Handles dropped files and pasted images for the open chat.
    fn take_drops_and_pastes(&mut self, ctx: &egui::Context) {
        let (dropped, hovering) = ctx.input(|input| {
            let dropped: Vec<PathBuf> = input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect();
            let hovering = !input.raw.hovered_files.is_empty();
            (dropped, hovering)
        });
        self.dropping = hovering && self.open_chat.is_some();
        let frame = ctx.cumulative_frame_nr();
        let drop_id = egui::Id::new("dropped-files-input-frame");
        let already_staged = ctx.data(|data| data.get_temp::<u64>(drop_id) == Some(frame));
        if !dropped.is_empty() && !already_staged {
            ctx.data_mut(|data| data.insert_temp(drop_id, frame));
            self.stage_clipboard(ctx, ClipboardPaste::Files(dropped));
        }
        // Tests inject fixtures and must never read the system clipboard.
        self.take_clipboard_paste(ctx, || {
            if cfg!(test) {
                clipboard_contents(
                    || {
                        ctx.data_mut(|data| {
                            data.remove_temp::<Vec<PathBuf>>(egui::Id::new(
                                "fixture-clipboard-files",
                            ))
                        })
                    },
                    || {
                        ctx.data_mut(|data| {
                            data.remove_temp::<(usize, usize, Vec<u8>)>(egui::Id::new(
                                "fixture-clipboard-image",
                            ))
                        })
                    },
                )
            } else {
                clipboard_contents(clipboard_files, clipboard_image)
            }
        });
    }

    /// Stages only detected attachments before drawing; queued view actions
    /// remain deferred until the frame ends.
    fn stage_clipboard(&mut self, ctx: &egui::Context, contents: ClipboardPaste) {
        match contents {
            ClipboardPaste::Files(paths) => self.stage_files(paths),
            ClipboardPaste::Image {
                width,
                height,
                rgba,
            } => {
                if self.open_chat.is_some() {
                    self.pending.push(Pending::Picture {
                        width,
                        height,
                        rgba: std::sync::Arc::new(rgba),
                        texture: None,
                    });
                }
            }
        }
        if !self.pending.is_empty() {
            self.focus_composer = true;
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("composer-text")));
        }
    }

    /// Stages pasted files or a pasted picture for the open chat.
    fn take_clipboard_paste(
        &mut self,
        ctx: &egui::Context,
        read_clipboard: impl FnOnce() -> Option<ClipboardPaste>,
    ) {
        let frame = ctx.cumulative_frame_nr();
        let frame_id = egui::Id::new("clipboard-input-frame");
        if let Some((last, staged)) = ctx.data(|data| data.get_temp::<(u64, bool)>(frame_id))
            && last == frame
        {
            if staged {
                ctx.input_mut(|input| {
                    input
                        .events
                        .retain(|event| !matches!(event, egui::Event::Paste(_)))
                });
            }
            return;
        }
        ctx.data_mut(|data| data.insert_temp(frame_id, (frame, false)));
        if ctx.input(|input| {
            !input.focused
                || input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::WindowFocused(false)))
        }) {
            self.saw_paste_command = false;
            self.paste_was_focused = false;
        }
        let (paste, text, released, focused, command) = ctx.input(|input| {
            let text = input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Paste(_)));
            let ordinary_v = self.ordinary_v_down
                || input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::V,
                            pressed: true,
                            ..
                        }
                    )
                });
            (
                text || (!ordinary_v
                    && (wants_paste(input)
                        || input.events.iter().any(|event| {
                            matches!(
                                event,
                                egui::Event::Key {
                                    key: egui::Key::V,
                                    pressed: false,
                                    ..
                                }
                            ) && !self.ordinary_v_down
                                && (self.saw_paste_command
                                    || (self.paste_was_focused && input.modifiers.command))
                                && !input.events.iter().any(|event| {
                                    matches!(
                                        event,
                                        egui::Event::Key {
                                            key: egui::Key::V,
                                            pressed: true,
                                            ..
                                        }
                                    )
                                })
                        }))),
                text,
                input.events.iter().any(|event| {
                    matches!(
                        event,
                        egui::Event::Key {
                            key: egui::Key::V,
                            pressed: false,
                            ..
                        }
                    )
                }),
                input.focused,
                input.modifiers.command,
            )
        });
        if command && !self.paste_command_down {
            // A new command press starts another shortcut, including after a
            // menu paste that had no V release.
            self.paste_before_release = false;
        }
        self.paste_command_down = command && focused;
        let requested =
            paste && (text || self.paste_was_focused) && (text || !self.paste_before_release);
        self.paste_was_focused = focused;
        // egui-winit swallows Ctrl/Cmd+V's press even for an image-only
        // clipboard. A release after an observed command modifier can be that
        // shortcut, including when Ctrl/Cmd was released first. Ordinary typing has a
        // delivered press, and must never stage an unrelated clipboard image.
        if ctx.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: true,
                        ..
                    }
                )
            })
        }) {
            self.ordinary_v_down = true;
        }
        if released || !focused {
            self.paste_before_release = false;
            self.ordinary_v_down = false;
            self.saw_paste_command = false;
        } else if text {
            // A menu paste has no key release to wait for.
            self.paste_before_release = command || self.saw_paste_command;
        }
        if focused && !released && command {
            self.saw_paste_command = true;
        }
        // A message or button may retain keyboard focus after clicking it.
        // Other text fields still own their clipboard input.
        let editing_text = ctx.text_edit_focused();
        let composing = ctx.memory(|memory| {
            memory.has_focus(egui::Id::new("composer-text"))
                || (!editing_text
                    && !memory.has_focus(egui::Id::new("search"))
                    && !memory.has_focus(egui::Id::new("chat-search")))
        });
        if requested
            && focused
            && composing
            && self.page == Page::Chats
            && self.dialog.is_none()
            && self.image_preview.is_none()
            && self.sticker_view.is_none()
            && !self.video_expanded
            && self.picker.is_none()
            && self.reaction_target.is_none()
            && self.recording.is_none()
            && !egui::Popup::is_any_open(ctx)
            && !self.show_update
            && self
                .open_chat
                .as_deref()
                .and_then(|id| self.chat(id))
                .is_some_and(Chat::can_send)
            && let Some(contents) = read_clipboard()
        {
            ctx.data_mut(|data| data.insert_temp(frame_id, (frame, true)));
            // A browser can offer both pixels and its source URL, and a file
            // manager both paths and their text. Consume the text before the
            // composer sees it, keeping any existing caption.
            ctx.input_mut(|input| {
                input
                    .events
                    .retain(|event| !matches!(event, egui::Event::Paste(_)))
            });
            self.stage_clipboard(ctx, contents);
        }
    }

    /// Keeps this frame's scrolling with the pane its gesture began over,
    /// after fastframe-scroll's axis lock and glide have had their say.
    fn route_scroll(&mut self, ctx: &egui::Context) {
        let (moved, lifted) = ctx.input(|input| {
            input
                .events
                .iter()
                .fold((false, false), |(moved, lifted), event| match event {
                    egui::Event::MouseWheel { delta, phase, .. } => (
                        moved || *delta != egui::Vec2::ZERO,
                        lifted || matches!(phase, egui::TouchPhase::End | egui::TouchPhase::Cancel),
                    ),
                    _ => (moved, lifted),
                })
        });
        let gliding = self.scrolling.gliding();
        self.scroll_route.route(ctx, moved, lifted, gliding);
    }

    pub fn save_state(&mut self) {
        if self.settings_dirty {
            self.save_settings();
        }
        for account in &mut self.accounts {
            if account.settings_dirty {
                account.save_settings();
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.save_state();
        self.flush_open_draft();
        self.recording = None;
        // A background resume would die with the process.
        if self.media_hold.take().is_some() {
            crate::media_pause::settle(Duration::from_secs(2));
        }
        for account in &mut self.accounts {
            account.backend.shutdown();
        }
    }

    /// Stores the open chat's unsent text, which otherwise only moves into
    /// the archive when another chat opens.
    fn flush_open_draft(&self) {
        if let Some(chat) = self.open_chat.as_deref()
            && self.editing.is_none()
        {
            self.store_draft(chat, &self.composer);
        }
    }

    /// Returns attachment state for a loaded message.
    pub fn media_of(&self, chat: &str, id: &str) -> Option<&Media> {
        self.conversations.get(chat)?.message(id)?.content.media()
    }

    fn handle_clipboard_image(
        &mut self,
        result: Result<crate::model::DecodedImage, String>,
        writer: impl FnOnce(&crate::model::DecodedImage) -> Result<(), String>,
    ) {
        match result {
            Ok(image) => match writer(&image) {
                Ok(()) => self.toast(crate::i18n::gettext(self.locale, "Copied image")),
                Err(error) => {
                    log::warn!("failed to write image to clipboard: {error}");
                    self.toast_error(crate::i18n::gettext(self.locale, "Failed to copy image"));
                }
            },
            Err(error) => {
                log::warn!("failed to decode image for clipboard: {error}");
                self.toast_error(crate::i18n::gettext(self.locale, "Failed to copy image"));
            }
        }
    }
}

/// Detects paste from the key release. egui consumes the press and emits a
/// `Paste` event only for text, so image paste has no key-press event.
/// Builds WhatsApp's full and short contact names. A first name is required.
fn compose_name(first: &str, last: &str) -> (Option<String>, Option<String>) {
    let first = first.trim();
    let last = last.trim();
    if first.is_empty() && last.is_empty() {
        return (None, None);
    }
    let full = if last.is_empty() {
        first.to_owned()
    } else if first.is_empty() {
        last.to_owned()
    } else {
        format!("{first} {last}")
    };
    let short = (!first.is_empty()).then(|| first.to_owned());
    (Some(full), short)
}

fn contains_mention_token(text: &str, user: &str) -> bool {
    let token = format!("@{user}");
    let mut rest = text;
    while let Some(at) = rest.find(&token) {
        let after = &rest[at + token.len()..];
        if after
            .chars()
            .next()
            .is_none_or(|character| !character.is_ascii_digit())
        {
            return true;
        }
        rest = &rest[at + 1..];
    }
    false
}

fn find_named_mention(text: &str, token: &str) -> Option<usize> {
    text.match_indices(token).find_map(|(at, _)| {
        let after = &text[at + token.len()..];
        after
            .chars()
            .next()
            .is_none_or(|character| !character.is_alphanumeric())
            .then_some(at)
    })
}

pub fn wants_paste(input: &egui::InputState) -> bool {
    input.events.iter().any(|event| {
        matches!(event, egui::Event::Paste(_))
            || matches!(
                event,
                egui::Event::Key {
                    key: egui::Key::V,
                    pressed: false,
                    modifiers,
                    ..
                } if modifiers.command
            )
    })
}

/// What a paste into the composer stages.
#[derive(Debug)]
enum ClipboardPaste {
    /// Files copied in a file manager, staged like dropped files.
    Files(Vec<PathBuf>),
    /// Picture data as width, height, and straight-alpha RGBA.
    Image {
        width: usize,
        height: usize,
        rgba: Vec<u8>,
    },
}

/// Prefers the clipboard's file list over its picture: Finder, Explorer and
/// Linux file managers offer the file's icon as an image alongside the path,
/// so a copied PDF or ZIP would otherwise arrive as its icon (#285).
fn clipboard_contents(
    read_files: impl FnOnce() -> Option<Vec<PathBuf>>,
    read_image: impl FnOnce() -> Option<(usize, usize, Vec<u8>)>,
) -> Option<ClipboardPaste> {
    let files: Vec<PathBuf> = read_files()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| {
            // text/uri-list lines end in CRLF, and arboard splits only on LF.
            let path = match path.to_str() {
                Some(text) if text.ends_with('\r') => PathBuf::from(text.trim_end_matches('\r')),
                _ => path,
            };
            path.exists().then_some(path)
        })
        .collect();
    if !files.is_empty() {
        return Some(ClipboardPaste::Files(files));
    }
    read_image().map(|(width, height, rgba)| ClipboardPaste::Image {
        width,
        height,
        rgba,
    })
}

/// Files copied to the clipboard by a file manager.
fn clipboard_files() -> Option<Vec<PathBuf>> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get().file_list().ok()
}

/// Clipboard image as width, height, and straight-alpha RGBA.
fn clipboard_image() -> Option<(usize, usize, Vec<u8>)> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    if image.width == 0 || image.height == 0 {
        return None;
    }
    Some((image.width, image.height, image.bytes.into_owned()))
}

/// Writes decoded straight-alpha RGBA image bytes to the system clipboard.
///
/// The handle stays open: on X11 the copying process serves the data, and
/// dropping arboard's last handle leaves the image only to a clipboard
/// manager, if there is one.
fn write_clipboard_image(image: &crate::model::DecodedImage) -> Result<(), String> {
    thread_local! {
        static CLIPBOARD: std::cell::RefCell<Option<arboard::Clipboard>> =
            const { std::cell::RefCell::new(None) };
    }
    CLIPBOARD.with_borrow_mut(|slot| {
        let clipboard = match slot {
            Some(clipboard) => clipboard,
            None => slot.insert(arboard::Clipboard::new().map_err(|error| error.to_string())?),
        };
        clipboard
            .set_image(arboard::ImageData {
                width: image.width,
                height: image.height,
                bytes: std::borrow::Cow::Borrowed(&image.bytes),
            })
            .map_err(|error| error.to_string())
    })
}

impl Delivery {
    /// Whether an outgoing message is still pending.
    pub fn in_flight(self) -> bool {
        matches!(self, Delivery::Pending)
    }
}

/// Whether an incoming message in this chat warrants a desktop notification.
///
/// Archived chats stay silent, direct and group alike, and speak up again once
/// they are unarchived. Muted and locked chats give no signal that one arrived,
/// and delayed reconnect backlogs are not news.
/// The sound a notification plays. A chat's own sound wins, even for
/// mentions, so a chat set to no sound stays silent. Otherwise a group message
/// that mentions or answers us plays the mention sound, and other group
/// messages stay silent while group sounds are off.
fn notification_sound(
    settings: &Settings,
    chat_sound: Option<NotificationSound>,
    is_group: bool,
    for_us: bool,
) -> NotificationSound {
    match chat_sound {
        Some(sound) => sound,
        None if for_us => settings.mention_sound.clone(),
        None if is_group && !settings.group_sounds => NotificationSound::None,
        None => settings.message_sound.clone(),
    }
}

/// What may still happen while the app lock is on: the lock screen's own
/// actions, the window and the tray, and settings results that show
/// nothing. Everything else would show or change what the lock hides.
/// Only for the newly allocated restore destination, before roster registration.
fn discard_unregistered_archive_restore(dirs: &crate::paths::AccountDirs) -> anyhow::Result<()> {
    crate::archive::backup::rollback_restore(dirs)
}

fn allowed_while_locked(action: &Action) -> bool {
    matches!(
        action,
        Action::UnlockApp
            | Action::LockApp
            | Action::ForgotAppPassword(_)
            | Action::UnlinkLockedApp
            | Action::ShowWindow
            | Action::HideWindow
            | Action::CloseWindow
            | Action::Quit
            | Action::ReloadThemes
            | Action::ZoomBy(_)
            | Action::ResetZoom
            | Action::SettingsChanged
            | Action::SetChatSound { .. }
            | Action::SetNotificationSound { .. }
            | Action::SetCustomFont(_)
            | Action::SetCustomEmojiFont { .. }
            | Action::SetDownloadFolder(_)
    )
}

/// Whether an input event is someone using the window, which restarts the
/// app lock's inactivity count. Focus changes and the pointer leaving are
/// the desktop's doing, not the person's.
fn is_user_input(event: &egui::Event) -> bool {
    matches!(
        event,
        egui::Event::Key { .. }
            | egui::Event::Text(_)
            | egui::Event::Copy
            | egui::Event::Cut
            | egui::Event::Paste(_)
            | egui::Event::PointerMoved(_)
            | egui::Event::MouseMoved(_)
            | egui::Event::PointerButton { .. }
            | egui::Event::MouseWheel { .. }
            | egui::Event::Zoom(_)
            | egui::Event::Touch { .. }
            | egui::Event::Ime(_)
    )
}

fn notification_eligible(chat: &Chat, now: i64, message_at: i64) -> bool {
    if chat.archived || chat.unread == 0 || chat.muted(now) || chat.locked {
        return false;
    }
    now - message_at <= 60
}

#[cfg(test)]
mod tests {
    #[test]
    fn undo_cancels_before_transport_and_expiry_sends_only_once() {
        let mut app = app();
        let (backend, mut commands) = crate::backend::Backend::recording();
        app.backend = backend;
        let operation = crate::undo::Operation::DeleteMessage {
            chat: "fixture".into(),
            id: "synthetic".into(),
        };
        app.schedule_undo(operation.clone(), "Deletion scheduled");
        assert!(commands.try_recv().is_err());
        let token = app.toasts.last().unwrap().undo.unwrap();
        app.undo_toast(token);
        app.flush_undo(
            std::time::Instant::now() + crate::undo::GRACE,
            &egui::Context::default(),
        );
        assert!(commands.try_recv().is_err());
        app.schedule_undo(operation, "Deletion scheduled");
        let now = std::time::Instant::now() + crate::undo::GRACE;
        app.flush_undo(now, &egui::Context::default());
        assert!(matches!(
            commands.try_recv().unwrap(),
            crate::backend::Command::DeleteLocal { .. }
        ));
        app.flush_undo(now, &egui::Context::default());
        assert!(commands.try_recv().is_err());
    }

    use super::*;
    use crate::model::{ChatKind, Content, Dialog, Media, MediaState, ReplyTarget, ToastKind};

    #[test]
    fn discovered_call_devices_and_failures_populate_the_current_account() {
        let mut app = app();
        let devices = crate::calls::DeviceList {
            microphones: vec![crate::calls::AudioDevice {
                id: "fixture-mic".into(),
                label: "Synthetic microphone".into(),
            }],
            speakers: vec![crate::calls::AudioDevice {
                id: "fixture-speaker".into(),
                label: "Synthetic speaker".into(),
            }],
            microphone_error: Some("Microphone discovery failed. Try refreshing devices.".into()),
            ..Default::default()
        };
        app.apply_backend_event(Event::CallDevices(Box::new(devices.clone())), false);
        assert_eq!(app.account().call_devices, devices);
    }

    #[test]
    fn accessibility_announces_live_open_chat_events_but_never_locked_or_hidden_accounts() {
        let mut app = app();
        app.account_mut().settings.notifications = false;
        app.open_chat = Some("synthetic-open".into());
        let mut incoming = message("synthetic-open", "incoming-a11y", 1);
        incoming.from_me = false;
        app.apply_backend_event(
            Event::Incoming {
                chat: "synthetic-open".into(),
                message: Box::new(incoming.clone()),
            },
            true,
        );
        assert_eq!(
            app.accessibility_announcements,
            ["New message in the open conversation"]
        );
        app.accessibility_announcements.clear();
        for (live, chat) in [(false, "synthetic-open"), (true, "synthetic-other")] {
            app.apply_backend_event(
                Event::Incoming {
                    chat: chat.into(),
                    message: Box::new(incoming.clone()),
                },
                live,
            );
            assert!(app.accessibility_announcements.is_empty());
        }
        app.apply_backend_event(
            Event::Typing {
                chat: "synthetic-open".into(),
                sender: "synthetic-sender".into(),
                composing: true,
            },
            true,
        );
        assert_eq!(app.accessibility_announcements.len(), 1);
        app.apply_backend_event(
            Event::Typing {
                chat: "synthetic-open".into(),
                sender: "synthetic-sender".into(),
                composing: true,
            },
            true,
        );
        assert_eq!(
            app.accessibility_announcements.len(),
            1,
            "typing repeats must not chatter"
        );
        app.accessibility_announcements.clear();
        app.events_hidden = true;
        app.toast_error("Synthetic hidden error");
        assert!(app.accessibility_announcements.is_empty());
        app.events_hidden = false;
        app.app_lock.lock();
        app.apply_backend_event(
            Event::Incoming {
                chat: "synthetic-open".into(),
                message: Box::new(incoming),
            },
            true,
        );
        app.apply_backend_event(
            Event::Link(LinkStatus::Disconnected {
                reason: "Synthetic disconnect".into(),
            }),
            true,
        );
        app.toast_error("Synthetic locked error");
        assert!(app.accessibility_announcements.is_empty());
    }

    #[test]
    fn emoji_import_cannot_open_a_picker_while_locked() {
        assert!(!allowed_while_locked(&Action::PickCustomEmojiFont));
        assert!(allowed_while_locked(&Action::SetCustomEmojiFont {
            path: "synthetic-emoji.ttf".into(),
            request: 1,
        }));
    }

    #[test]
    fn a_stale_emoji_import_result_cannot_change_preferences() {
        let mut app = app();
        app.custom_emoji_request = 2;
        app.apply(
            Action::SetCustomEmojiFont {
                path: "stale-emoji.ttf".into(),
                request: 1,
            },
            &egui::Context::default(),
        );
        assert!(app.settings.custom_emoji_font.is_none());
    }

    fn app() -> App {
        let root = std::env::temp_dir().join(format!("zapfast-app-{}", std::process::id()));
        App::headless(AppDirs::under(&root), Settings::default()).0
    }

    #[test]
    fn call_click_before_connection_is_explained_and_never_queued_for_later() {
        for video in [false, true] {
            let mut app = app();
            let (backend, mut commands) = crate::backend::Backend::recording();
            app.backend = backend;
            app.link = LinkStatus::Connecting;
            let chat = "15550000001@s.whatsapp.net".to_owned();
            let action = if video {
                Action::StartVideoCall(chat)
            } else {
                Action::StartCall(chat)
            };
            app.apply(action, &egui::Context::default());
            assert!(!app.call_preparing);
            assert!(commands.try_recv().is_err());
            assert!(
                app.toasts
                    .iter()
                    .any(|toast| toast.message.contains("Wait for WhatsApp to connect"))
            );
            app.link = LinkStatus::Connected;
            assert!(
                commands.try_recv().is_err(),
                "reconnect cannot place the earlier rejected call"
            );
        }
    }

    #[test]
    fn connected_call_click_shows_preparation_before_worker_acknowledges_and_blocks_duplicates() {
        for video in [false, true] {
            let mut app = app();
            let (backend, mut commands) = crate::backend::Backend::recording();
            app.backend = backend;
            app.link = LinkStatus::Connected;
            let chat = "15550000001@s.whatsapp.net".to_owned();
            let start = || {
                if video {
                    Action::StartVideoCall(chat.clone())
                } else {
                    Action::StartCall(chat.clone())
                }
            };
            app.apply(start(), &egui::Context::default());
            assert!(
                app.call_preparing,
                "feedback must not wait for worker queue processing"
            );
            let command = commands.try_recv().unwrap();
            assert!(matches!(
                (video, command),
                (true, Command::StartVideoCall { .. }) | (false, Command::StartCall { .. })
            ));
            app.apply(start(), &egui::Context::default());
            assert!(
                commands.try_recv().is_err(),
                "repeated clicks must not queue another call"
            );
            app.apply_backend_event(Event::CallPreparing(false), true);
            assert!(!app.call_preparing);
        }
    }

    #[test]
    fn cancelling_a_queued_call_dismisses_preparation_and_ignores_late_start_feedback() {
        for video in [false, true] {
            let mut app = app();
            let (backend, mut commands) = crate::backend::Backend::recording();
            app.backend = backend;
            app.link = LinkStatus::Connected;
            let chat = "15550000001@s.whatsapp.net".to_owned();
            let ctx = egui::Context::default();
            app.apply(
                if video {
                    Action::StartVideoCall(chat)
                } else {
                    Action::StartCall(chat)
                },
                &ctx,
            );
            let request = match commands.try_recv().unwrap() {
                Command::StartCall { request, .. } | Command::StartVideoCall { request, .. } => {
                    request
                }
                _ => panic!("expected queued call"),
            };
            app.apply(Action::HangupCall, &ctx);
            assert!(
                request.is_cancelled(),
                "cancellation reaches the pending command before worker dispatch"
            );
            assert!(!app.call_preparing);
            assert!(matches!(commands.try_recv(), Ok(Command::HangupCall)));
            app.apply_backend_event(Event::CallPreparing(true), true);
            assert!(
                !app.call_preparing,
                "late preparation feedback must not reopen the canceled dialog"
            );
            app.apply_backend_event(Event::CallPreparing(false), true);
            assert!(app.pending_call_request.is_none());
        }
    }

    #[test]
    fn window_title_follows_chat_and_hides_private_names() {
        let mut app = app();
        let id = "synthetic-title@s.whatsapp.net".to_owned();
        app.chats
            .push(Chat::new(id.clone(), "Synthetic\nname".into()));
        app.open_chat = Some(id);
        assert_eq!(app.accessible_window_title(), "Syntheticname, Zap Faster");
        app.chats[0].locked = true;
        assert_eq!(app.accessible_window_title(), "Zap Faster");
        app.chats[0].locked = false;
        app.settings.screen_privacy.enabled = true;
        app.settings.screen_privacy.blur_names = true;
        assert_eq!(app.accessible_window_title(), "Zap Faster");
        app.settings.screen_privacy.enabled = false;
        app.page = Page::Settings;
        assert_eq!(app.accessible_window_title(), "Zap Faster");
    }

    #[test]
    fn automation_collection_scope_changes_only_with_privacy_membership() {
        let mut app = app();
        app.chats
            .push(Chat::new("synthetic-a@s.whatsapp.net".into(), "A".into()));
        app.chats
            .push(Chat::new("synthetic-b@s.whatsapp.net".into(), "B".into()));
        let initial = App::automation_scope(app.account());
        app.chats.reverse();
        app.chats[0].unread = 8;
        assert_eq!(initial, App::automation_scope(app.account()));
        app.chats[0].locked = true;
        assert_ne!(initial, App::automation_scope(app.account()));
        app.chats[0].locked = false;
        app.chats[0].ephemeral_expiration = Some(60);
        assert_ne!(initial, App::automation_scope(app.account()));
    }

    #[test]
    fn community_expansion_is_local_and_navigation_reveals_a_child() {
        let mut app = app();
        let parent = "synthetic-parent@g.us".to_owned();
        let child = "synthetic-child@g.us".to_owned();
        let mut community = Chat::new(parent.clone(), "Synthetic community".into());
        community.community = Some(crate::model::CommunityRole::Parent);
        let mut member = Chat::new(child.clone(), "Synthetic member".into());
        member.community = Some(crate::model::CommunityRole::Member {
            parent: parent.clone(),
            announcement: false,
            general: true,
        });
        app.chats.extend([community, member]);
        let ctx = egui::Context::default();
        app.apply(Action::ToggleCommunity(parent.clone()), &ctx);
        assert!(app.collapsed_communities.contains(&parent));
        app.apply(Action::OpenChat(child), &ctx);
        assert!(!app.collapsed_communities.contains(&parent));
        app.chats[0].locked = true;
        app.apply(Action::ToggleCommunity(parent), &ctx);
        assert!(app.collapsed_communities.is_empty());
    }

    fn gallery_fixture(app: &mut App) -> (String, crate::model::ChatMedia) {
        let chat = "gallery@s.whatsapp.net".to_owned();
        app.chats
            .push(Chat::new(chat.clone(), "Synthetic gallery".into()));
        app.open_chat = Some(chat.clone());
        let rows = ["older", "newer"]
            .into_iter()
            .enumerate()
            .map(|(n, id)| {
                let mut row = message(&chat, id, n as i64);
                row.content = Content::Image {
                    caption: None,
                    motion: None,
                    media: Media {
                        mime: "image/jpeg".into(),
                        size: 200 * 1024 * 1024,
                        width: Some(400),
                        height: Some(300),
                        album: None,
                        path: None,
                        state: MediaState::Idle,
                    },
                };
                row
            })
            .collect();
        let listing = crate::model::ChatMedia::collect(chat.clone(), rows, 500);
        (chat, listing)
    }

    #[test]
    fn delayed_tray_start_has_a_deadline_and_arrival_preserves_the_hidden_window() {
        let ctx = egui::Context::default();
        let mut waiting = app();
        waiting.window_hidden = true;
        waiting.test_tray_shown = Some(false);
        waiting.tray_start_deadline = Some(Instant::now() + Duration::from_secs(15));
        waiting.background_frame(&ctx);
        assert!(!waiting.wants_show);
        assert!(waiting.tray_start_deadline.is_some());
        // Arrival wins even if the deadline expires in the same frame.
        waiting.test_tray_shown = Some(true);
        waiting.tray_start_deadline = Some(Instant::now() - Duration::from_secs(1));
        waiting.background_frame(&ctx);
        assert!(!waiting.wants_show);
        assert!(waiting.tray_start_deadline.is_none());

        let mut absent = app();
        absent.window_hidden = true;
        absent.test_tray_shown = Some(false);
        absent.tray_start_deadline = Some(Instant::now() - Duration::from_secs(1));
        absent.background_frame(&ctx);
        assert!(
            absent.wants_show,
            "a desktop without a tray gets a usable window"
        );
        assert!(absent.tray_start_deadline.is_none());
    }

    #[test]
    fn notification_masks_use_the_originating_hidden_account_and_hide_avatar_and_custom_sound() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(root.path());
        let chat = "fixture@s.whatsapp.net";
        for account in &mut app.accounts {
            let mut row = Chat::new(chat.into(), "Synthetic sender".into());
            row.unread = 1;
            row.notification_sound = Some(NotificationSound::Alert);
            account.chats.push(row);
            account
                .avatars
                .insert(chat.into(), Some(root.path().join("synthetic-avatar.png")));
        }
        app.accounts[1].settings.notification_sender = false;
        app.accounts[1].settings.notification_content = true;
        let original_account = app.account().id.clone();
        let (backend, events) = Backend::detached();
        app.accounts[1].backend = backend;
        let incoming = message(chat, "Synthetic private preview", crate::util::now());
        events
            .send(Event::Incoming {
                chat: chat.into(),
                message: Box::new(incoming.clone()),
            })
            .unwrap();
        app.handle_events();
        let masked = app.notifications.shown.last().unwrap();
        assert_eq!(masked.title, "Zap Faster");
        assert_eq!(masked.body, "New message");
        assert!(masked.picture.is_none());
        assert_eq!(masked.sound, app.settings.message_sound);
        assert_eq!(app.account().id, original_account);
        assert!(
            app.account().settings.notification_sender,
            "hidden settings do not change visible account"
        );

        app.maybe_notify(chat, &incoming);
        let visible = app.notifications.shown.last().unwrap();
        assert_eq!(visible.title, "Synthetic sender");
        assert_eq!(visible.body, "Synthetic private preview");
        assert!(visible.picture.is_some());
        assert_eq!(visible.sound, NotificationSound::Alert);

        app.account_mut().settings.notification_content = false;
        app.maybe_notify(chat, &incoming);
        let content_hidden = app.notifications.shown.last().unwrap();
        assert_eq!(content_hidden.title, "Synthetic sender");
        assert_eq!(content_hidden.body, "New message");
        assert!(
            content_hidden.picture.is_some(),
            "sender-visible choice retains its picture"
        );
    }

    #[test]
    fn editing_places_the_caret_after_unicode_characters() {
        let mut app = app();
        let ctx = egui::Context::default();
        let text = "Olá 👩🏽‍💻 日本語";
        app.open_chat = Some("fixture".into());
        let mut row = message("fixture", "edit", crate::util::now());
        row.from_me = true;
        row.status = Delivery::Sent;
        row.content = Content::text(text);
        app.conversations
            .entry("fixture".into())
            .or_default()
            .merge(vec![row], false);
        app.set_composer_cursor(&ctx, 0);
        app.apply(Action::Edit("edit".into()), &ctx);
        let state = egui::TextEdit::load_state(&ctx, egui::Id::new("composer-text")).unwrap();
        let range = state.cursor.char_range().unwrap();
        assert_eq!(range.primary.index.0, text.chars().count());
        assert_eq!(range.secondary.index.0, text.chars().count());
    }

    #[test]
    fn a_refused_document_caption_remains_editable_when_it_owns_the_composer() {
        let mut app = app();
        let chat = "fixture";
        app.chats.push(Chat::new(chat.into(), "Synthetic".into()));
        app.open_chat = Some(chat.into());
        app.send_refused(
            chat.into(),
            Some("original-quote".into()),
            Unsent::Document {
                source: crate::model::DocumentSource::Pixels {
                    width: 1,
                    height: 1,
                    rgba: vec![0, 1, 2, 255],
                },
                caption: Some("Original caption".into()),
            },
            Refusal::Offline,
        );
        assert_eq!(app.composer, "Original caption");
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.send_pending(
            app.account().id.clone(),
            chat.into(),
            "Edited caption".into(),
            vec![],
        );
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
            Command::SendDocument { caption: Some(caption), quoting: Some(quote), .. } if caption == "Edited caption" && quote.id == "original-quote")));
    }

    #[test]
    fn recovered_documents_keep_caption_quote_order_and_a_newer_draft() {
        let mut app = app();
        let chat = "fixture";
        app.chats.push(Chat::new(chat.into(), "Synthetic".into()));
        app.open_chat = Some(chat.into());
        app.composer = "Newer draft".into();
        app.pending.push(Pending::File("new.pdf".into()));
        app.send_refused_with_mentions(
            chat.into(),
            Some("old-quote".into()),
            Unsent::Document {
                source: crate::model::DocumentSource::File("original.png".into()),
                caption: Some("Original caption".into()),
            },
            Refusal::Offline,
            vec!["491702222222@s.whatsapp.net".into()],
        );
        assert_eq!(app.composer, "Newer draft");
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let caption = std::mem::take(&mut app.composer);
        app.send_pending(app.account().id.clone(), chat.into(), caption, vec![]);
        let sends: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter(|command| {
                matches!(
                    command,
                    Command::SendDocument { .. } | Command::SendFiles { .. }
                )
            })
            .collect();
        assert!(matches!(sends.as_slice(), [
            Command::SendDocument { source: crate::model::DocumentSource::File(path), caption: Some(caption), quoting: Some(quote), mentions, .. },
            Command::SendFiles { paths, caption: Some(new), quoting: None, .. },
        ] if path == std::path::Path::new("original.png") && caption == "Original caption" && quote.id == "old-quote" && mentions == &["491702222222@s.whatsapp.net"] && paths == &[PathBuf::from("new.pdf")] && new == "Newer draft"));
    }

    #[test]
    fn editing_a_gallery_link_replaces_its_url_and_removing_it_clears_the_row() {
        let mut app = app();
        let (chat, _) = gallery_fixture(&mut app);
        let mut row = message(&chat, "link", 1);
        row.content = Content::text("https://example.com/old");
        app.gallery.listing = Some(crate::model::ChatMedia::collect(
            chat,
            vec![row.clone()],
            500,
        ));
        row.content = Content::text("https://example.com/new");
        app.message_updated(row.clone(), false);
        assert_eq!(
            app.gallery.listing.as_ref().unwrap().links[0].url,
            "https://example.com/new"
        );
        row.content = Content::text("Removed the link");
        app.message_updated(row, false);
        assert!(app.gallery.listing.as_ref().unwrap().links.is_empty());
    }

    #[test]
    fn archive_viewer_navigates_missing_items_without_disturbing_drafts_or_selection() {
        let mut app = app();
        let ctx = egui::Context::default();
        let (chat, listing) = gallery_fixture(&mut app);
        app.gallery.chat = Some(chat.clone());
        app.gallery.listing = Some(listing);
        app.composer = "Keep this draft".into();
        app.pending = vec![Pending::File(PathBuf::from("synthetic-attachment.png"))];
        app.selection = Some((chat.clone(), vec!["selected".into()]));
        app.apply(
            Action::PreviewMedia {
                chat: chat.clone(),
                message: "older".into(),
            },
            &ctx,
        );
        assert_eq!(app.image_preview.as_ref().unwrap().message(), Some("older"));
        assert!(!app.image_preview.as_ref().unwrap().path().is_file());
        app.apply(Action::NextImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().message(), Some("newer"));
        app.apply(Action::PreviousImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().message(), Some("older"));
        app.backend.record_demo_commands();
        app.apply(
            Action::Download {
                chat: chat.clone(),
                message: "older".into(),
                card: None,
            },
            &ctx,
        );
        assert!(
            app.backend
                .take_demo_commands()
                .iter()
                .any(|command| matches!(command, Command::Download { explicit: true, .. }))
        );
        assert_eq!(app.composer, "Keep this draft");
        assert!(
            matches!(&app.pending[..],[Pending::File(path)] if path == &PathBuf::from("synthetic-attachment.png"))
        );
        assert_eq!(app.selection, Some((chat, vec!["selected".into()])));
    }

    #[test]
    fn stale_gallery_results_and_results_after_relocking_are_discarded() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        let (chat, listing) = gallery_fixture(&mut app);
        app.request_gallery(&chat, true);
        let stale = app.gallery.token;
        app.request_gallery(&chat, true);
        events
            .send(Event::Gallery {
                chat: chat.clone(),
                token: stale,
                listing: Some(listing.clone()),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(app.gallery.pending && app.gallery.listing.is_none());
        app.chats
            .iter_mut()
            .find(|row| row.id == chat)
            .unwrap()
            .locked = true;
        events
            .send(Event::Gallery {
                chat,
                token: app.gallery.token,
                listing: Some(listing),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(app.gallery.listing.is_none());
    }

    /// Failed native drags leave pointer state intact; completed drags release it.
    #[test]
    fn native_drag_failure_preserves_the_pointer_until_a_drag_succeeds() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::PointerMoved(egui::pos2(20.0, 20.0)),
                    egui::Event::PointerButton {
                        pos: egui::pos2(20.0, 20.0),
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    },
                ],
                ..Default::default()
            },
            |ui| {
                app.finish_attachment_drag(ui.ctx(), Err(crate::file_drag::Failure::Unavailable));
                assert!(ui.input(|input| input.pointer.primary_down()));
                app.finish_attachment_drag(ui.ctx(), Ok(()));
                assert!(!ui.input(|input| input.pointer.primary_down()));
            },
        );
        output.textures_delta.clear();
    }

    /// Unrelated dialogs do not cancel a shared-contact lookup.
    #[test]
    fn shared_contact_lookup_survives_opening_and_closing_unrelated_dialogs() {
        for success in [false, true] {
            let mut app = app();
            app.locale = crate::i18n::Locale::PortugueseBrazil;
            let (backend, _commands, events) = Backend::recording_with_events();
            app.backend = backend;
            let ctx = egui::Context::default();
            app.apply(
                Action::NewContact {
                    phone: "15550000001".into(),
                    first: String::new(),
                    last: String::new(),
                    to_phone: None,
                },
                &ctx,
            );
            let request = app.new_contact_request.unwrap();
            app.apply(Action::ShowDialog(Dialog::Shortcuts), &ctx);
            app.apply(Action::CloseDialog, &ctx);
            app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
            assert_eq!(app.new_contact_request, Some(request));
            events
                .send(if success {
                    Event::ContactReady {
                        request,
                        id: "15550000001@s.whatsapp.net".into(),
                        name: None,
                    }
                } else {
                    Event::ContactFailed {
                        request,
                        error: "Could not check the number. Please try again.".into(),
                    }
                })
                .unwrap();
            app.handle_events();
            app.apply_actions(&ctx);
            assert_eq!(
                app.dialog,
                Some(Dialog::MessageNumber),
                "independent completion does not dismiss the new dialog"
            );
            assert!(app.new_contact_request.is_none());
            if success {
                assert_eq!(app.open_chat.as_deref(), Some("15550000001@s.whatsapp.net"));
            } else {
                assert!(app.toasts.iter().any(|toast| toast.message
                    == "Não foi possível verificar o número. Tente novamente."));
            }
        }
    }

    /// Verifies that a later sticker dialog survives dispatching a completed number lookup.
    #[test]
    fn completed_number_lookup_preserves_a_later_sticker_dialog() {
        let mut app = app();
        let (backend, _commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
        app.apply(
            Action::NewContact {
                phone: "15550000001".into(),
                first: String::new(),
                last: String::new(),
                to_phone: None,
            },
            &ctx,
        );
        let request = app.new_contact_request.unwrap();
        events
            .send(Event::ContactReady {
                request,
                id: "15550000001@s.whatsapp.net".into(),
                name: None,
            })
            .unwrap();
        events
            .send(Event::StickerPicture {
                path: PathBuf::from("synthetic-sticker.png"),
                width: 32,
                height: 32,
                transparent: false,
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.dialog, Some(Dialog::StickerMaker));
        app.apply_actions(&ctx);
        assert_eq!(app.dialog, Some(Dialog::StickerMaker));
        assert_eq!(app.open_chat.as_deref(), Some("15550000001@s.whatsapp.net"));
    }

    /// Settings cancel number-entry lookups while shared-contact requests remain active.
    #[test]
    fn opening_settings_invalidates_dialog_lookups_but_keeps_shared_contacts() {
        for dialog in [Some(Dialog::MessageNumber), Some(Dialog::NewContact), None] {
            let mut app = app();
            let (backend, _commands, events) = Backend::recording_with_events();
            app.backend = backend;
            let ctx = egui::Context::default();
            app.dialog = dialog.clone();
            app.apply(
                Action::NewContact {
                    phone: "15550000001".into(),
                    first: String::new(),
                    last: String::new(),
                    to_phone: None,
                },
                &ctx,
            );
            let request = app.new_contact_request.unwrap();
            app.apply(Action::ToggleSettings, &ctx);
            events
                .send(Event::ContactReady {
                    request,
                    id: "15550000001@s.whatsapp.net".into(),
                    name: None,
                })
                .unwrap();
            app.handle_events();
            assert_eq!(app.page, Page::Settings);
            let opened = app
                .deferred_account_actions
                .iter()
                .any(|(account, action)| {
                    account == &app.account().id && matches!(action, Action::StartChat { .. })
                });
            assert_eq!(
                opened,
                dialog.is_none(),
                "shared-contact requests are independent of dialogs"
            );
        }
    }

    /// Invalid international numbers cannot reach the backend lookup.
    #[test]
    fn invalid_number_actions_never_start_a_lookup() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.apply(
            Action::NewContact {
                phone: "+1 555 1234567 ext. 89".into(),
                first: String::new(),
                last: String::new(),
                to_phone: None,
            },
            &egui::Context::default(),
        );
        assert!(commands.try_recv().is_err());
        assert!(!app.new_contact_pending);
    }

    /// Cancelled lookup completions cannot disturb a newer dialog.
    #[test]
    fn cancelled_number_lookups_cannot_open_or_close_a_new_dialog() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let lookup = |phone: &str| Action::NewContact {
            phone: phone.into(),
            first: String::new(),
            last: String::new(),
            to_phone: None,
        };
        app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
        app.apply(lookup("15550000001"), &ctx);
        let first = app.new_contact_request.unwrap();
        app.apply(Action::CloseDialog, &ctx);
        app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
        app.apply(lookup("15550000002"), &ctx);
        let second = app.new_contact_request.unwrap();
        events
            .send(Event::ContactReady {
                request: first,
                id: "15550000001@s.whatsapp.net".into(),
                name: None,
            })
            .unwrap();
        events
            .send(Event::ContactFailed {
                request: first,
                error: "Stale failure".into(),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.dialog, Some(Dialog::MessageNumber));
        assert_eq!(app.new_contact_request, Some(second));
        assert!(app.new_contact_pending);
        assert!(
            !app.actions
                .iter()
                .any(|action| matches!(action, Action::StartChat { .. }))
        );
        assert!(app.toasts.is_empty());
        events
            .send(Event::ContactFailed {
                request: second,
                error: "Try again".into(),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.dialog, Some(Dialog::MessageNumber));
        assert!(!app.new_contact_pending);
        assert!(app.new_contact_request.is_none());
        app.apply(lookup("15550000002"), &ctx);
        let retry = app.new_contact_request.unwrap();
        assert_ne!(retry, second);
        events
            .send(Event::ContactReady {
                request: retry,
                id: "15550000002@s.whatsapp.net".into(),
                name: None,
            })
            .unwrap();
        app.handle_events();
        assert!(app.dialog.is_none());
        assert!(app.deferred_account_actions.iter().any(|(account, action)| account == &app.account().id && matches!(action, Action::StartChat { id, .. } if id == "15550000002@s.whatsapp.net")));
        assert!(commands.try_recv().is_ok());
    }

    /// A hidden account's late lookup result cannot alter the visible account's dialog.
    #[test]
    fn hidden_account_number_lookups_leave_the_visible_request_alone() {
        for failed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let dirs = AppDirs::under(directory.path());
            let mut app = App::headless(dirs.clone(), Settings::default()).0;
            let (backend, _commands, events) = Backend::recording_with_events();
            app.backend = backend;
            let second_id = AccountId::parse("2").unwrap();
            let (second, _) = Account::detached(
                &dirs,
                second_id.clone(),
                crate::settings::AccountSettings::default(),
            )
            .unwrap();
            app.accounts.push(second);
            let ctx = egui::Context::default();
            let lookup = || Action::NewContact {
                phone: "15550000001".into(),
                first: String::new(),
                last: String::new(),
                to_phone: None,
            };
            app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
            app.apply(lookup(), &ctx);
            let first_request = app.new_contact_request.unwrap();
            app.switch_account(&second_id);
            app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
            app.apply(lookup(), &ctx);
            let visible_request = app.new_contact_request.unwrap();
            events
                .send(if failed {
                    Event::ContactFailed {
                        request: first_request,
                        error: "Synthetic lookup failure".into(),
                    }
                } else {
                    Event::ContactReady {
                        request: first_request,
                        id: "15550000001@s.whatsapp.net".into(),
                        name: None,
                    }
                })
                .unwrap();
            app.handle_events();
            assert_eq!(app.account().id, second_id);
            assert_eq!(app.dialog, Some(Dialog::MessageNumber));
            assert_eq!(app.new_contact_request, Some(visible_request));
            assert!(app.new_contact_pending);
            assert!(app.toasts.is_empty());
            assert!(
                !app.actions
                    .iter()
                    .any(|action| matches!(action, Action::StartChat { .. }))
            );
            assert!(!app.accounts[0].new_contact_pending);
            assert!(
                !app.deferred_account_actions
                    .iter()
                    .any(|(_, action)| matches!(action, Action::StartChat { .. }))
            );
            assert!(app.accounts[0].new_contact_request.is_none());
        }
    }

    /// Logout invalidates pending lookups and reconnect permits a fresh request.
    #[test]
    fn logout_ends_number_lookups_and_allows_retry_after_reconnect() {
        let mut app = app();
        let (backend, _commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let lookup = || Action::NewContact {
            phone: "15550000001".into(),
            first: String::new(),
            last: String::new(),
            to_phone: None,
        };
        app.apply(Action::ShowDialog(Dialog::MessageNumber), &ctx);
        app.apply(lookup(), &ctx);
        let old_request = app.new_contact_request.unwrap();
        app.handle_link(LinkStatus::LoggedOut, true);
        assert!(!app.new_contact_pending);
        assert!(app.new_contact_request.is_none());
        assert!(app.new_contact_dialog_request.is_none());
        app.handle_link(LinkStatus::Connected, true);
        app.apply(lookup(), &ctx);
        let retry = app.new_contact_request.unwrap();
        assert_ne!(retry, old_request);
        events
            .send(Event::ContactReady {
                request: old_request,
                id: "15550000001@s.whatsapp.net".into(),
                name: None,
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.new_contact_request, Some(retry));
        assert!(app.new_contact_pending);
        assert_eq!(app.dialog, Some(Dialog::MessageNumber));
        assert!(
            !app.actions
                .iter()
                .any(|action| matches!(action, Action::StartChat { .. }))
        );
        events
            .send(Event::ContactFailed {
                request: retry,
                error: "Fixture retry failure".into(),
            })
            .unwrap();
        app.handle_events();
        assert!(!app.new_contact_pending);
    }

    #[test]
    /// Checks forwarding requires review and confirmation while back navigation preserves recipients.
    fn forwarding_requires_review_and_confirmation_and_preserves_selection() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        for id in ["first", "second"] {
            app.chats.push(Chat::new(id.into(), id.into()));
        }
        let dialog = Dialog::Forward {
            chat: "source".into(),
            messages: vec!["m1".into(), "m2".into()],
        };
        app.apply(Action::ShowDialog(dialog.clone()), &ctx);
        app.apply(Action::ConfirmForward, &ctx);
        app.apply(Action::ToggleForwardRecipient("first".into()), &ctx);
        app.forward_search = "second".into();
        app.apply(Action::ToggleForwardRecipient("second".into()), &ctx);
        app.apply(Action::ReviewForward, &ctx);
        app.apply(Action::BackToForwardSelection, &ctx);
        assert_eq!(app.forward_recipients, ["first", "second"]);
        assert!(
            commands.try_recv().is_err(),
            "selection and review never send"
        );
        app.apply(Action::ReviewForward, &ctx);
        app.apply(Action::ConfirmForward, &ctx);
        for target in ["first", "second"] {
            let Command::Forward {
                from_chat,
                messages,
                to_chat,
            } = commands.try_recv().unwrap()
            else {
                panic!("forward command");
            };
            assert_eq!(from_chat, "source");
            assert_eq!(messages, ["m1", "m2"]);
            assert_eq!(to_chat, target);
        }
        app.apply(Action::ConfirmForward, &ctx);
        assert!(
            commands.try_recv().is_err(),
            "double confirmation never duplicates"
        );
        assert!(app.dialog.is_none());
        app.apply(Action::ShowDialog(dialog), &ctx);
        app.apply(Action::ToggleForwardRecipient("first".into()), &ctx);
        app.apply(Action::CloseDialog, &ctx);
        assert!(app.forward_recipients.is_empty());
        assert!(!app.forward_reviewing);
        assert!(commands.try_recv().is_err(), "cancel never sends");
    }

    #[test]
    /// Checks confirmation rejects destinations that became locked or left after selection.
    fn forwarding_rechecks_destinations_before_sending() {
        for left in [false, true] {
            let mut app = app();
            app.locale = crate::i18n::Locale::PortugueseBrazil;
            let (backend, mut commands) = Backend::recording();
            app.backend = backend;
            let ctx = egui::Context::default();
            app.chats.push(Chat::new("target".into(), "Target".into()));
            app.chats.push(Chat::new("valid".into(), "Valid".into()));
            app.apply(
                Action::ShowDialog(Dialog::Forward {
                    chat: "source".into(),
                    messages: vec!["m".into()],
                }),
                &ctx,
            );
            app.apply(Action::ToggleForwardRecipient("target".into()), &ctx);
            app.apply(Action::ToggleForwardRecipient("valid".into()), &ctx);
            app.apply(Action::ReviewForward, &ctx);
            let target = app
                .chats
                .iter_mut()
                .find(|chat| chat.id == "target")
                .unwrap();
            target.left = left;
            target.locked = !left;
            app.apply(Action::ConfirmForward, &ctx);
            assert!(commands.try_recv().is_err());
            assert!(app.dialog.is_some());
            assert!(!app.forward_reviewing);
            assert_eq!(app.forward_recipients, ["valid"]);
            app.apply(Action::ReviewForward, &ctx);
            app.apply(Action::ConfirmForward, &ctx);
            assert!(
                matches!(commands.try_recv().unwrap(), Command::Forward { to_chat, .. } if to_chat == "valid")
            );
            assert!(commands.try_recv().is_err());
            assert!(app.toasts.iter().any(|toast| toast.message
                == "Não é mais possível enviar mensagens para uma das conversas selecionadas"));
        }
    }

    #[test]
    /// Checks a chat the user has left cannot be selected as a forwarding destination.
    fn forwarding_cannot_select_a_left_chat() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut target = Chat::new("left".into(), "Left".into());
        target.left = true;
        app.chats.push(target);
        app.apply(
            Action::ShowDialog(Dialog::Forward {
                chat: "source".into(),
                messages: vec!["m".into()],
            }),
            &ctx,
        );
        app.apply(Action::ToggleForwardRecipient("left".into()), &ctx);
        assert!(app.forward_recipients.is_empty());
    }

    /// Delayed edit refusals wait in the originating account instead of replacing another composer.
    #[test]
    fn hidden_account_edit_refusals_remain_in_that_accounts_recovery_queue() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let (backend, events) = Backend::detached();
        app.accounts[1].backend = backend;
        app.composer = "Visible account draft".into();
        events
            .send(Event::EditRefused {
                chat: "fixture@s.whatsapp.net".into(),
                id: "synthetic-edit".into(),
                draft: EditDraft {
                    text: "Hidden correction".into(),
                    mentions: Vec::new(),
                },
                error: EditFailure::Offline,
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "Visible account draft");
        assert!(app.accounts[0].refused_edits.is_empty());
        assert_eq!(app.accounts[1].refused_edits.len(), 1);
        assert_eq!(app.accounts[1].refused_edits[0].2.text, "Hidden correction");
    }

    /// Edit eligibility and submission use the original sent text and timestamp.
    #[test]
    fn edit_window_and_submission_use_original_sent_text() {
        let mut row = message("chat", "sent", 1_000);
        row.from_me = true;
        row.status = Delivery::Sent;
        assert!(row.editable_at(1_000));
        assert!(row.editable_at(1_900));
        assert!(!row.editable_at(1_901));
        assert!(!row.editable_at(999));
        for status in [Delivery::Pending, Delivery::Failed] {
            row.status = status;
            assert!(!row.editable_at(1_100));
        }
        row.status = Delivery::Sent;
        row.edited = true;
        assert!(
            !row.editable_at(1_901),
            "editing does not extend the window"
        );
        row.from_me = false;
        assert!(!row.editable_at(1_100));
        row.from_me = true;
        row.content = Content::Revoked;
        assert!(!row.editable_at(1_100));

        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.open_chat = Some("chat".into());
        row.content = Content::text("Original");
        row.timestamp = crate::util::now() - 60;
        app.conversations
            .entry("chat".into())
            .or_default()
            .merge(vec![row], false);
        let ctx = egui::Context::default();
        app.apply(Action::Edit("sent".into()), &ctx);
        assert_eq!(app.editing.as_deref(), Some("sent"));
        app.conversations
            .get_mut("chat")
            .unwrap()
            .message_mut("sent")
            .unwrap()
            .timestamp = crate::util::now() - 901;
        app.send_text("chat".into(), "Correction".into(), None);
        assert!(commands.try_recv().is_err(), "expired edits never dispatch");
        assert_eq!(
            app.composer, "Correction",
            "keep the correction for copying"
        );
        assert!(app.editing.is_none());
        app.apply(Action::CancelEdit, &ctx);
        assert_eq!(
            app.composer, "Correction",
            "cancel cannot discard a normal correction draft"
        );
        app.conversations
            .get_mut("chat")
            .unwrap()
            .message_mut("sent")
            .unwrap()
            .timestamp = crate::util::now() - 60;
        app.apply(Action::Edit("sent".into()), &ctx);
        app.send_text("chat".into(), "Correction".into(), None);
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::EditText { .. }
        ));
        let original = app.conversations["chat"].message("sent").unwrap();
        assert_eq!(original.content, Content::text("Original"));
        assert!(original.edited, "original metadata is retained");
        app.conversations
            .get_mut("chat")
            .unwrap()
            .message_mut("sent")
            .unwrap()
            .from_me = false;
        app.apply(Action::Edit("sent".into()), &ctx);
        assert!(
            app.editing.is_none(),
            "direct actions cannot edit incoming text"
        );
    }

    /// A refused mention edit preserves its display text and selected mention identities.
    #[test]
    fn refused_mention_edit_round_trips_visible_text_and_selected_metadata() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Fixture".into());
        chat.participants.push(member.into());
        app.chats.push(chat);
        app.open_chat = Some(chat_id.into());
        let mut row = message(chat_id, "sent", crate::util::now() - 60);
        row.from_me = true;
        row.status = Delivery::Sent;
        row.content = Content::text("Original");
        app.conversations
            .entry(chat_id.into())
            .or_default()
            .merge(vec![row], false);
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        app.editing = Some("sent".into());
        app.composer_mentions = vec![ComposerMention {
            id: member.into(),
            name: "Mira Example".into(),
        }];
        let visible = "Correction @Mira Example";
        app.send_text(chat_id.into(), visible.into(), None);
        let draft = std::iter::from_fn(|| commands.try_recv().ok())
            .find_map(|command| match command {
                Command::EditText {
                    text,
                    mentions,
                    draft,
                    ..
                } => {
                    assert_eq!(text, "Correction @491702222222");
                    assert_eq!(mentions, vec![member.to_owned()]);
                    assert_eq!(draft.text, visible);
                    Some(draft)
                }
                _ => None,
            })
            .expect("edit dispatched");
        app.composer.clear();
        events
            .send(Event::EditRefused {
                chat: chat_id.into(),
                id: "sent".into(),
                draft,
                error: EditFailure::Send("Fixture refusal".into()),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, visible);
        assert_eq!(
            app.composer_mentions,
            vec![ComposerMention {
                id: member.into(),
                name: "Mira Example".into()
            }]
        );
        app.send_text(chat_id.into(), app.composer.clone(), None);
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command, Command::EditText { text, mentions, .. } if text == "Correction @491702222222" && mentions == vec![member.to_owned()])));
    }

    /// A locally unsaved accepted edit is not offered as an unsent correction.
    #[test]
    fn accepted_edit_save_failure_does_not_offer_to_resend_it() {
        let mut app = app();
        let (backend, _, events) = Backend::recording_with_events();
        app.backend = backend;
        app.open_chat = Some("fixture".into());
        events
            .send(Event::EditRefused {
                chat: "fixture".into(),
                id: "sent".into(),
                draft: EditDraft {
                    text: "Accepted".into(),
                    mentions: vec![],
                },
                error: EditFailure::Save,
            })
            .unwrap();
        app.handle_events();
        assert!(app.refused_edits.is_empty());
        assert!(app.composer.is_empty());
        assert!(app.editing.is_none());
    }

    /// Expired edits recover their correction without replacing a newer composer draft.
    #[test]
    fn worker_refusal_restores_an_expired_edit_without_overwriting_a_new_draft() {
        let mut app = app();
        app.open_chat = Some("fixture".into());
        let mut original = message("fixture", "sent", 0);
        original.from_me = true;
        app.conversations
            .entry("fixture".into())
            .or_default()
            .messages
            .push(original);
        let (backend, _commands, events) = Backend::recording_with_events();
        app.backend = backend;
        events
            .send(Event::EditRefused {
                chat: "fixture".into(),
                id: "sent".into(),
                draft: EditDraft {
                    text: "Correction @Mira Example".into(),
                    mentions: vec![ComposerMention {
                        id: "fixture-member@g.us".into(),
                        name: "Mira Example".into(),
                    }],
                },
                error: EditFailure::Expired,
            })
            .unwrap();
        app.composer = "New draft".into();
        app.handle_events();
        assert_eq!(app.composer, "New draft");
        assert_eq!(app.refused_edits.len(), 1);
        app.composer.clear();
        app.reply_to = Some("another-reply".into());
        app.restore_refused_edit();
        assert!(app.composer.is_empty());
        assert_eq!(app.reply_to.as_deref(), Some("another-reply"));
        assert_eq!(app.refused_edits.len(), 1);
        app.reply_to = None;
        app.restore_refused_edit();
        assert_eq!(app.composer, "Correction @Mira Example");
        assert_eq!(
            app.composer_mentions,
            vec![ComposerMention {
                id: "fixture-member@g.us".into(),
                name: "Mira Example".into()
            }]
        );
        assert!(
            app.editing.is_none(),
            "an expired correction is a normal draft"
        );
        assert!(app.refused_edits.is_empty());
    }

    /// Refused corrections cannot revive revoked or deleted edit targets.
    #[test]
    fn refused_edits_do_not_restore_revoked_or_deleted_targets() {
        for revoked in [false, true] {
            let mut app = app();
            app.open_chat = Some("fixture".into());
            let conversation = app.conversations.entry("fixture".into()).or_default();
            if revoked {
                let mut original = message("fixture", "sent", 0);
                original.from_me = true;
                original.content = Content::Revoked;
                conversation.messages.push(original);
            }
            app.refused_edits.push((
                "fixture".into(),
                "sent".into(),
                EditDraft {
                    text: "Correction".into(),
                    mentions: vec![],
                },
            ));
            app.restore_refused_edit();
            assert!(app.editing.is_none());
            assert!(app.composer.is_empty());
            assert!(app.refused_edits.is_empty());
        }
    }

    /// An expired correction can be deliberately sent as a new message.
    #[test]
    fn an_expired_refused_correction_can_be_sent_as_a_new_message() {
        let mut app = app();
        app.open_chat = Some("fixture".into());
        let mut original = message("fixture", "sent", 0);
        original.from_me = true;
        app.conversations
            .entry("fixture".into())
            .or_default()
            .messages
            .push(original);
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        events
            .send(Event::EditRefused {
                chat: "fixture".into(),
                id: "sent".into(),
                draft: EditDraft {
                    text: "Unsent correction".into(),
                    mentions: vec![],
                },
                error: EditFailure::Expired,
            })
            .unwrap();
        app.handle_events();
        assert!(app.editing.is_none());
        assert!(
            commands.try_recv().is_err(),
            "restoration never sends automatically"
        );
        app.apply(Action::CancelEdit, &egui::Context::default());
        assert_eq!(app.composer, "Unsent correction");
        app.send_text("fixture".into(), app.composer.clone(), None);
        let dispatched: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(
            dispatched
                .iter()
                .any(|command| matches!(command, Command::SendText {
            chat, text, ..
        } if chat == "fixture" && text == "Unsent correction"))
        );
        assert!(
            !dispatched
                .iter()
                .any(|command| matches!(command, Command::EditText { .. }))
        );
    }

    /// Repeated immediate refusals recover only the latest attempted correction.
    #[test]
    fn repeated_immediate_edit_refusals_restore_only_the_latest_correction() {
        let mut app = app();
        app.open_chat = Some("fixture".into());
        let mut original = message("fixture", "sent", 0);
        original.from_me = true;
        app.conversations
            .entry("fixture".into())
            .or_default()
            .messages
            .push(original);
        let (backend, _commands, events) = Backend::recording_with_events();
        app.backend = backend;
        app.composer = "New draft".into();
        for (text, error) in [
            ("Older correction", EditFailure::Expired),
            ("Latest correction", EditFailure::Offline),
        ] {
            events
                .send(Event::EditRefused {
                    chat: "fixture".into(),
                    id: "sent".into(),
                    draft: EditDraft {
                        text: text.into(),
                        mentions: vec![],
                    },
                    error,
                })
                .unwrap();
        }
        app.handle_events();
        assert_eq!(app.refused_edits.len(), 1);
        assert_eq!(app.composer, "New draft");
        app.composer.clear();
        app.restore_refused_edit();
        assert_eq!(app.composer, "Latest correction");
        assert!(app.refused_edits.is_empty());
    }

    /// Drafts override pins without rewriting message activity, and clearing them restores normal order.
    #[test]
    fn drafts_rise_above_pins_and_return_to_normal_order_when_cleared() {
        let mut app = app();
        for (id, activity, pinned) in [("old", 1, false), ("recent", 100, false), ("pin", 50, true)]
        {
            let mut chat = Chat::new(id.into(), id.into());
            chat.last_activity = activity;
            chat.pinned = pinned;
            app.chats.push(chat);
        }
        let order = |app: &App| {
            app.visible_chats()
                .iter()
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>()
        };
        app.drafts.insert("old".into(), "Unsent text".into());
        assert_eq!(order(&app), ["old", "pin", "recent"]);
        app.open_chat("old".into());
        assert_eq!(order(&app), ["old", "pin", "recent"]);
        app.composer = " \n ".into();
        assert_eq!(
            order(&app),
            ["pin", "recent", "old"],
            "active blank text overrides saved text"
        );
        app.composer = "Editing an existing message".into();
        app.editing = Some("sent-message".into());
        assert!(!app.has_draft("old"));
        app.editing = None;
        app.composer.clear();
        app.stage_files(vec!["Synthetic document.pdf".into()]);
        assert_eq!(
            app.draft_preview("old").as_deref(),
            Some("Synthetic document.pdf")
        );
        assert_eq!(order(&app), ["old", "pin", "recent"]);
        app.open_chat("recent".into());
        assert!(
            app.has_draft("old"),
            "attachment-only draft survives switching"
        );
        assert_eq!(order(&app), ["old", "pin", "recent"]);
        app.open_chat("old".into());
        app.apply(Action::ClearPending, &egui::Context::default());
        assert!(!app.has_draft("old"));
        assert_eq!(order(&app), ["pin", "recent", "old"]);
        assert_eq!(
            app.chat("old").unwrap().last_activity,
            1,
            "drafts never rewrite message activity"
        );
    }

    /// Images without captions count as drafts while Favorites, search and locked-chat boundaries remain intact.
    #[test]
    fn draft_order_preserves_filters_and_favorites_and_images_count_as_drafts() {
        let mut app = app();
        for (id, position) in [("first", 0), ("second", 1), ("hidden", 2)] {
            let mut chat = Chat::new(id.into(), id.into());
            chat.favorite = true;
            chat.favorite_position = position;
            chat.locked = id == "hidden";
            app.chats.push(chat);
        }
        app.chat_filter = ChatFilter::Favorites;
        app.drafts.insert("second".into(), "Saved draft".into());
        app.drafts.insert("hidden".into(), "Private draft".into());
        assert_eq!(
            app.visible_chats()
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
        app.drafts.remove("second");
        assert_eq!(app.visible_chats()[0].id, "first");
        app.open_chat("second".into());
        app.apply(
            Action::PasteImage {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            },
            &egui::Context::default(),
        );
        assert!(app.has_draft("second"));
        assert_eq!(app.draft_preview("second").as_deref(), Some("Image"));
        assert_eq!(app.visible_chats()[0].id, "second");
        app.open_chat("first".into());
        assert_eq!(app.draft_preview("second").as_deref(), Some("Image"));
        app.search = "first".into();
        assert_eq!(
            app.visible_chats().len(),
            1,
            "draft promotion respects search"
        );
        assert_eq!(app.visible_chats()[0].id, "first");
    }

    /// Identical chat IDs across accounts retain separate files, captions, mentions and reply quotes.
    #[test]
    fn attachment_drafts_keep_their_account_caption_mentions_and_quote() {
        let directory = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(directory.path());
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let second_id = AccountId::parse("2").unwrap();
        let (second, _) = Account::detached(
            &dirs,
            second_id.clone(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        app.accounts.push(second);
        let chat = "fixture@g.us";
        let member = "fixture-member@s.whatsapp.net";
        for account in &mut app.accounts {
            let mut group = Chat::new(chat.into(), "Synthetic group".into());
            group.participants.push(member.into());
            account.chats.push(group);
        }
        app.open_chat(chat.into());
        app.composer = "Hello @Mira Example".into();
        app.composer_mentions.push(ComposerMention {
            id: member.into(),
            name: "Mira Example".into(),
        });
        app.reply_to = Some("synthetic-quote".into());
        app.stage_files(vec!["First account.pdf".into()]);
        let first_id = app.account().id.clone();
        let queued_caption = std::mem::take(&mut app.composer);
        let queued_mentions = app.composer_mentions.clone();
        app.switch_account(&second_id);
        app.open_chat(chat.into());
        assert!(
            !app.has_draft(chat),
            "identical chat IDs in another account must not expose a draft"
        );
        assert!(app.pending.is_empty());
        app.stage_files(vec!["Second account.pdf".into()]);
        app.send_pending(
            first_id.clone(),
            chat.into(),
            queued_caption,
            queued_mentions,
        );
        assert_eq!(
            app.pending.len(),
            1,
            "a queued send from another account cannot consume these files"
        );
        app.switch_account(&first_id);
        assert_eq!(app.composer, "Hello @Mira Example");
        assert_eq!(app.composer_mentions.len(), 1);
        assert_eq!(app.reply_to.as_deref(), Some("synthetic-quote"));
        assert!(
            matches!(app.pending.as_slice(), [Pending::File(path)] if path == std::path::Path::new("First account.pdf"))
        );
        app.apply(Action::CloseChat, &egui::Context::default());
        assert!(app.pending.is_empty());
        assert!(app.has_draft(chat));
        app.open_chat(chat.into());
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let caption = std::mem::take(&mut app.composer);
        app.send_pending(
            first_id.clone(),
            chat.into(),
            caption,
            app.composer_mentions.clone(),
        );
        assert!(!app.has_draft(chat));
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(sent.iter().any(|command| matches!(command, Command::SaveDraft { chat: id, text } if id == chat && text.is_empty())));
        assert!(sent.iter().any(|command| matches!(command,
            Command::SendFiles { chat: id, caption: Some(caption), mentions, quoting: Some(quote), .. }
            if id == chat && caption == "Hello @fixture-member" && mentions == &[member] && quote.id == "synthetic-quote")));
        app.switch_account(&second_id);
        assert!(
            matches!(app.pending.as_slice(), [Pending::File(path)] if path == std::path::Path::new("Second account.pdf"))
        );
    }

    /// Attachment-only drafts remain recognizable after leaving their conversation.
    #[test]
    fn attachment_only_drafts_have_list_previews() {
        let mut app = app();
        let chat = "preview-fixture@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.open_chat(chat.into());
        app.stage_files(vec!["Synthetic document.pdf".into()]);
        app.apply(Action::CloseChat, &egui::Context::default());
        assert_eq!(
            app.draft_preview(chat).as_deref(),
            Some("Synthetic document.pdf")
        );
        app.attachment_drafts.get_mut(chat).unwrap().pending = vec![Pending::Picture {
            width: 1,
            height: 1,
            rgba: std::sync::Arc::new(vec![0; 4]),
            texture: None,
        }];
        assert_eq!(app.draft_preview(chat).as_deref(), Some("Image"));
        app.open_chat(chat.into());
        app.pending.clear();
        app.composer = "Active text stays in the composer".into();
        assert!(app.draft_preview(chat).is_none());
    }

    /// A quote without text or attachments remains separate across chats and accounts.
    #[test]
    fn quote_only_drafts_survive_navigation_without_entering_another_account() {
        let directory = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(directory.path());
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let first_id = app.account().id.clone();
        let second_id = AccountId::parse("2").unwrap();
        let (second, _) = Account::detached(
            &dirs,
            second_id.clone(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        app.accounts.push(second);
        app.open_chat("synthetic-chat".into());
        app.reply_to = Some("first-quote".into());
        app.open_chat("other-chat".into());
        assert!(app.reply_to.is_none());
        app.open_chat("synthetic-chat".into());
        assert_eq!(app.reply_to.as_deref(), Some("first-quote"));
        app.switch_account(&second_id);
        app.open_chat("synthetic-chat".into());
        assert!(app.reply_to.is_none());
        app.reply_to = Some("second-quote".into());
        app.switch_account(&first_id);
        assert_eq!(app.reply_to.as_deref(), Some("first-quote"));
        app.apply(Action::CloseChat, &egui::Context::default());
        assert!(app.reply_to.is_none());
        app.open_chat("synthetic-chat".into());
        assert_eq!(app.reply_to.as_deref(), Some("first-quote"));
        app.reply_to = None;
        app.open_chat("other-chat".into());
        app.open_chat("synthetic-chat".into());
        assert!(
            app.reply_to.is_none(),
            "cancelled quotes must stay cancelled"
        );
        app.switch_account(&second_id);
        assert_eq!(app.reply_to.as_deref(), Some("second-quote"));
    }

    /// Late file and image refusals recover only into their originating account's draft.
    #[test]
    fn hidden_account_attachment_refusals_cannot_enter_the_visible_draft() {
        for kind in 0..3 {
            let image = kind == 1;
            let directory = tempfile::tempdir().unwrap();
            let (mut app, _) = two_accounts(directory.path());
            let chat = "fixture@s.whatsapp.net";
            for account in &mut app.accounts {
                account
                    .chats
                    .push(Chat::new(chat.into(), "Synthetic chat".into()));
                account.open_chat = Some(chat.into());
            }
            let (backend, events) = Backend::detached();
            app.accounts[1].backend = backend;
            let caption = "Recovered hidden caption";
            let unsent = if kind == 2 {
                Unsent::Document {
                    source: crate::model::DocumentSource::File("Hidden account.pdf".into()),
                    caption: Some(caption.into()),
                }
            } else if image {
                Unsent::Image {
                    width: 1,
                    height: 1,
                    rgba: vec![1, 2, 3, 4],
                    caption: Some(caption.into()),
                }
            } else {
                Unsent::Files {
                    paths: vec!["Hidden account.pdf".into()],
                    caption: Some(caption.into()),
                }
            };
            events
                .send(refused(
                    chat,
                    Some("hidden-quote"),
                    unsent,
                    Refusal::Offline,
                ))
                .unwrap();
            app.handle_events();
            assert!(
                app.pending.is_empty(),
                "inactive account files cannot enter the visible composer"
            );
            assert!(app.composer.is_empty());
            assert!(app.reply_to.is_none());
            app.open_chat("other@s.whatsapp.net".into());
            assert!(!app.accounts[0].attachment_drafts.contains_key(chat));
            let second = app.accounts[1].id.clone();
            app.switch_account(&second);
            assert_eq!(app.composer, caption);
            assert_eq!(app.reply_to.as_deref(), Some("hidden-quote"));
            assert_eq!(app.pending.len(), 1);
            if kind == 2 {
                assert!(
                    matches!(&app.pending[0], Pending::Recovered { item, .. } if matches!(item.as_ref(), Pending::Document(_)))
                );
            }
            if image {
                assert!(
                    matches!(app.pending[0].media(), Pending::Picture { rgba, .. } if rgba.as_slice() == [1, 2, 3, 4])
                );
            } else {
                assert!(
                    matches!(app.pending[0].media(), Pending::File(path) if path == std::path::Path::new("Hidden account.pdf"))
                );
            }
        }
    }

    /// A send queued before switching chats preserves both drafts instead of sending the next chat's files.
    #[test]
    fn stale_attachment_sends_preserve_both_chats_and_the_original_caption() {
        let mut app = app();
        app.open_chat("first".into());
        app.stage_files(vec!["First.pdf".into()]);
        app.reply_to = Some("first-quote".into());
        app.composer = "First caption".into();
        let caption = std::mem::take(&mut app.composer);
        app.open_chat("second".into());
        app.stage_files(vec!["Second.pdf".into()]);
        app.reply_to = Some("second-quote".into());
        app.send_pending(
            app.account().id.clone(),
            "first".into(),
            caption,
            Vec::new(),
        );
        assert!(
            matches!(app.pending.as_slice(), [Pending::File(path)] if path == std::path::Path::new("Second.pdf"))
        );
        assert_eq!(app.reply_to.as_deref(), Some("second-quote"));
        app.open_chat("first".into());
        assert_eq!(app.composer, "First caption");
        assert_eq!(app.reply_to.as_deref(), Some("first-quote"));
        assert!(
            matches!(app.pending.as_slice(), [Pending::File(path)] if path == std::path::Path::new("First.pdf"))
        );
    }

    /// Clearing, deleting or unlinking removes the affected stored and active attachment drafts.
    #[test]
    fn clearing_deleting_and_unlinking_discard_attachment_drafts() {
        let mut app = app();
        app.open_chat("cleared".into());
        app.composer = "Caption".into();
        app.stage_files(vec!["Cleared.pdf".into()]);
        app.handle_chat_cleared("cleared", 1, true);
        assert!(!app.has_draft("cleared"));
        assert!(app.composer.is_empty());
        app.stage_files(vec!["Deleted.pdf".into()]);
        app.open_chat("other".into());
        app.forget_chat("cleared");
        assert!(!app.has_draft("cleared"));
        app.stage_files(vec!["Unlinked.pdf".into()]);
        app.open_chat("last".into());
        app.stage_files(vec!["Active.pdf".into()]);
        app.handle_link(LinkStatus::LoggedOut, true);
        assert!(app.attachment_drafts.is_empty());
        assert!(app.pending.is_empty());
    }

    /// Hiding a locked conversation retains its attachment draft without exposing it in the regular list.
    #[test]
    fn hiding_a_locked_chat_keeps_its_attachments_out_of_the_regular_list() {
        let mut app = app();
        let mut chat = Chat::new("locked".into(), "Synthetic locked chat".into());
        chat.locked = true;
        app.chats.push(chat);
        app.open_chat = Some("locked".into());
        app.stage_files(vec!["Private synthetic document.pdf".into()]);
        app.reply_to = Some("private-quote".into());
        app.hide_locked_chat("locked");
        assert!(app.open_chat.is_none());
        assert!(app.pending.is_empty());
        assert!(app.has_draft("locked"));
        assert!(app.visible_chats().iter().all(|chat| chat.id != "locked"));
        assert_eq!(
            app.attachment_drafts["locked"].quoting.as_deref(),
            Some("private-quote")
        );
    }

    /// While recording, the light, the timer and the waveform ask for a
    /// frame every 40 ms, or every step while a screen reader is attached,
    /// and the recorder then stops asking for frames of its own.
    #[test]
    fn recording_progress_steps_while_a_screen_reader_is_attached() {
        let run = |reader: bool| {
            let directory = tempfile::tempdir().unwrap();
            let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
            let ctx = crate::motion::tests::reading(reader);
            app.waker.attach(&ctx);
            app.recording = Some(Recorder::rehearsal());
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| app.tick_audio(ui.ctx()));
            output.textures_delta.clear();
            let stepped = app.recording.as_ref().is_some_and(Recorder::is_stepped);
            (
                output.viewport_output[&egui::ViewportId::ROOT].repaint_delay,
                stepped,
            )
        };
        let predicted = Duration::from_secs_f32(1.0 / 60.0);
        assert_eq!(run(false), (Duration::from_millis(40) - predicted, false));
        assert_eq!(run(true), (crate::motion::STEP - predicted, true));
    }

    #[test]
    fn back_and_forward_walk_between_chats_and_settings() {
        let ctx = egui::Context::default();
        let mut app = app();
        let chat = "15550008888@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        // The first frame fixes the place the window opened on.
        app.apply_actions(&ctx);

        let open = |app: &mut App, action: Action| {
            app.actions.push(action);
            app.apply_actions(&ctx);
        };
        open(&mut app, Action::OpenChat(chat.into()));
        assert_eq!(app.open_chat.as_deref(), Some(chat));
        open(&mut app, Action::Open(Page::Settings));
        assert_eq!(app.page, Page::Settings);
        assert_eq!(app.open_chat.as_deref(), Some(chat));

        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.page, Page::Chats, "back leaves Settings");
        assert_eq!(app.open_chat.as_deref(), Some(chat));
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.open_chat, None, "back leaves the chat");

        app.apply(Action::NavigateForward, &ctx);
        assert_eq!(app.open_chat.as_deref(), Some(chat), "forward reopens it");
        app.apply(Action::NavigateForward, &ctx);
        assert_eq!(app.page, Page::Settings);
        assert_eq!(app.open_chat.as_deref(), Some(chat));
    }

    #[test]
    fn a_dialog_is_not_a_place_in_the_history() {
        let ctx = egui::Context::default();
        let mut app = app();
        let chat = "15550007777@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);

        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::ShowDialog(Dialog::NewChat));
        app.apply_actions(&ctx);
        app.actions.push(Action::CloseDialog);
        app.apply_actions(&ctx);

        // The dialog never became a place, so one back step reaches the list.
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.open_chat, None);
        assert_eq!(app.page, Page::Chats);
        assert_eq!(app.dialog, None, "the dialog is not restored");
    }

    #[test]
    fn a_deleted_chat_is_not_reopened_from_the_history() {
        let ctx = egui::Context::default();
        let mut app = app();
        let chat = "15550007777@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);

        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::Open(Page::Settings));
        app.apply_actions(&ctx);

        // The phone deletes the chat while the window is elsewhere.
        app.forget_chat(chat);
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat, None);

        // The step skips the removed chat instead of reopening it.
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.page, Page::Chats);
        assert_eq!(app.open_chat, None);
    }

    #[test]
    fn a_gone_chat_is_never_reopened_from_a_stale_place() {
        let ctx = egui::Context::default();
        let mut app = app();
        let chat = "15550007777@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::Open(Page::Settings));
        app.apply_actions(&ctx);

        // Defence in depth: even when a path clears chats without pruning the
        // history, a place naming a gone chat is not opened.
        app.chats.clear();
        app.open_chat = None;
        app.apply(Action::NavigateBack, &ctx);
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.open_chat, None);
        assert_eq!(app.page, Page::Chats);
    }

    /// Two accounts sharing `chat`: the first on screen, with `chat` opened
    /// and Settings over it, and the second's event channel.
    fn two_accounts_on_settings(
        directory: &std::path::Path,
        chat: &str,
    ) -> (App, std::sync::mpsc::Sender<crate::backend::Event>) {
        let ctx = egui::Context::default();
        let dirs = AppDirs::under(directory);
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let (second, events) = Account::detached(
            &dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        app.accounts.push(second);
        app.active = 0;
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.accounts[1]
            .chats
            .push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::Open(Page::Settings));
        app.apply_actions(&ctx);
        (app, events)
    }

    #[test]
    fn a_hidden_accounts_chat_removal_leaves_the_history_alone() {
        let directory = tempfile::tempdir().unwrap();
        let chat = "15550007777@s.whatsapp.net";
        let (mut app, events) = two_accounts_on_settings(directory.path(), chat);
        let ctx = egui::Context::default();

        events
            .send(crate::backend::Event::ChatRemoved { chat: chat.into() })
            .unwrap();
        app.handle_events();

        // The chat is gone from the hidden account only, and chat ids repeat
        // across accounts, so the window's own place still names it.
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.page, Page::Chats);
        assert_eq!(app.open_chat.as_deref(), Some(chat));
    }

    #[test]
    fn a_hidden_accounts_logout_leaves_the_history_alone() {
        let directory = tempfile::tempdir().unwrap();
        let chat = "15550007777@s.whatsapp.net";
        let (mut app, events) = two_accounts_on_settings(directory.path(), chat);
        let ctx = egui::Context::default();

        events
            .send(crate::backend::Event::Link(LinkStatus::LoggedOut))
            .unwrap();
        app.handle_events();

        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(
            app.page,
            Page::Chats,
            "back still leaves the window's own Settings"
        );
        assert_eq!(app.open_chat.as_deref(), Some(chat));
    }

    #[test]
    fn a_gone_chat_keeps_the_forward_history() {
        let ctx = egui::Context::default();
        let mut app = app();
        let gone = "15550007771@s.whatsapp.net";
        let kept = "15550007772@s.whatsapp.net";
        app.chats.push(Chat::new(gone.into(), "Ada".into()));
        app.chats.push(Chat::new(kept.into(), "Grace".into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(gone.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(kept.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::Open(Page::Settings));
        app.apply_actions(&ctx);

        // A path that drops a chat without pruning the history, which the
        // guard in `show_location` exists for.
        app.chats.retain(|chat| chat.id != gone);

        app.actions.push(Action::NavigateBack);
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat.as_deref(), Some(kept));
        app.actions.push(Action::NavigateBack);
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat, None, "the gone chat is not reopened");

        // The step that skipped the gone chat must not drop where a forward
        // step would go.
        app.actions.push(Action::NavigateForward);
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat.as_deref(), Some(kept));
    }

    #[test]
    fn a_locked_chat_outside_its_folder_does_not_strand_the_history() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.settings.set_chat_lock_code(Some("123456"));
        let locked = "15550007774@s.whatsapp.net";
        let mut chat = Chat::new(locked.into(), "Ada".into());
        chat.locked = true;
        app.chats.push(chat);
        app.apply_actions(&ctx);

        // The reader opens the locked chat from the locked folder, then opens
        // Settings over it.
        app.apply(Action::UnlockLockedFolder("123456".into()), &ctx);
        assert!(app.locked_folder_open());
        app.actions.push(Action::OpenChat(locked.into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::Open(Page::Settings));
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat.as_deref(), Some(locked));

        // Hiding the window closes the folder and drops the code, so the chat
        // can no longer be opened from the history.
        app.window_gone();
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat, None, "the locked chat is hidden again");

        // Back must step past the place it cannot show in one press, and the
        // way forward must survive the step.
        app.actions.push(Action::NavigateBack);
        app.apply_actions(&ctx);
        assert_eq!(app.page, Page::Chats, "one press past the locked place");
        assert_eq!(app.open_chat, None);

        app.actions.push(Action::NavigateForward);
        app.apply_actions(&ctx);
        assert_eq!(app.page, Page::Settings, "forward still reaches Settings");
    }

    #[test]
    fn a_new_place_drops_the_forward_history() {
        let ctx = egui::Context::default();
        let mut app = app();
        let first = "15550009991@s.whatsapp.net";
        let second = "15550009992@s.whatsapp.net";
        app.chats.push(Chat::new(first.into(), "Ada".into()));
        app.chats.push(Chat::new(second.into(), "Grace".into()));
        app.apply_actions(&ctx);

        app.actions.push(Action::OpenChat(first.into()));
        app.apply_actions(&ctx);
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(app.open_chat, None);

        app.actions.push(Action::OpenChat(second.into()));
        app.apply_actions(&ctx);
        app.apply(Action::NavigateForward, &ctx);
        assert_eq!(
            app.open_chat.as_deref(),
            Some(second),
            "forward no longer reaches the chat the back step left"
        );
    }

    #[test]
    fn switching_accounts_forgets_the_history() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _commands) = two_accounts(directory.path());
        let ctx = egui::Context::default();
        let chat = "15550003333@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        app.apply(Action::SwitchAccount(AccountId::parse("2").unwrap()), &ctx);
        assert_eq!(app.open_chat, None, "the other account has no chat open");
        app.apply(Action::NavigateBack, &ctx);
        assert_eq!(
            app.open_chat, None,
            "the first account's chat is not restored"
        );
    }

    #[test]
    fn a_back_side_button_press_steps_back_through_a_frame() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.attach(&ctx);
        let chat = "15550007777@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.apply_actions(&ctx);
        app.actions.push(Action::OpenChat(chat.into()));
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat.as_deref(), Some(chat));

        let press = |app: &mut App, button: egui::PointerButton| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::PointerButton {
                        pos: egui::pos2(20.0, 20.0),
                        button,
                        pressed: true,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| app.frame_ui(ui),
            );
            output.textures_delta.clear();
        };
        press(&mut app, egui::PointerButton::Extra1);
        assert_eq!(app.open_chat, None, "the back button closed the chat");
        press(&mut app, egui::PointerButton::Extra2);
        assert_eq!(
            app.open_chat.as_deref(),
            Some(chat),
            "the forward button reopened it"
        );
    }

    #[test]
    fn switching_chats_discards_typed_emoticon_undo() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
        app.settings.convert_typed_emoticons = true;
        app.open_chat = Some("chat-a".to_owned());
        app.composer = "same 😊".to_owned();
        app.emoticon_undo = Some(EmoticonUndo {
            start_byte: 5,
            source: ":)".to_owned(),
            emoji: "😊".to_owned(),
            cursor_after: 6,
        });

        app.open_chat("chat-b".to_owned());
        assert!(app.emoticon_undo.is_none());
        app.composer = "same 😊".to_owned();
        app.apply(Action::UndoTypedEmoticon, &egui::Context::default());

        assert_eq!(app.composer, "same 😊");
    }

    #[test]
    fn a_second_account_is_remembered_with_the_one_on_screen() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
        assert!(!app.has_several_accounts());
        let ctx = egui::Context::default();
        app.apply(Action::AddAccount, &ctx);
        assert!(app.has_several_accounts());
        assert_eq!(app.account().id.as_str(), "2");
        assert!(app.adding_account);
        app.link = LinkStatus::Connected;
        let first = AccountId::first();
        app.apply(Action::SwitchAccount(first.clone()), &ctx);
        assert_eq!(app.account().id, first);
        let roster = AccountRoster::load(&app.dirs.accounts_file()).unwrap();
        assert_eq!(roster.order, ["1".to_string(), "2".to_string()]);
        assert_eq!(roster.active, "1");
    }

    /// Two accounts, the second with a recording backend; the first on screen.
    fn two_accounts(
        directory: &std::path::Path,
    ) -> (App, tokio::sync::mpsc::UnboundedReceiver<Command>) {
        let dirs = AppDirs::under(directory);
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let mut second = Account::detached(
            &dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap()
        .0;
        let (backend, commands) = Backend::recording();
        second.backend = backend;
        second.link = LinkStatus::Connected;
        app.accounts.push(second);
        app.active = 0;
        (app, commands)
    }

    #[test]
    fn local_avatar_override_wins_without_replacing_fetched_photo_and_hidden_events_stay_account_owned()
     {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let id = "1@s.whatsapp.net";
        let fetched = directory.path().join("fetched-fixture.png");
        let local = directory.path().join("local-fixture.png");
        let hidden = directory.path().join("hidden-fixture.png");
        app.avatars.insert(id.into(), Some(fetched.clone()));
        app.avatars_full.insert(id.into(), Some(fetched.clone()));
        app.apply_backend_event(
            Event::LocalAvatarChanged {
                id: id.into(),
                result: Ok(Some(local.clone())),
            },
            true,
        );
        assert_eq!(app.avatar(id), Some(local.clone()));
        assert_eq!(app.avatar_full(id), Some(local.clone()));
        assert_eq!(app.cached_avatar(id), Some(local));
        assert_eq!(app.avatars.get(id), Some(&Some(fetched.clone())));
        app.active = 1;
        app.apply_backend_event(
            Event::LocalAvatarChanged {
                id: id.into(),
                result: Ok(Some(hidden.clone())),
            },
            false,
        );
        app.active = 0;
        assert_ne!(app.avatar(id), Some(hidden.clone()));
        app.apply_backend_event(
            Event::LocalAvatarChanged {
                id: id.into(),
                result: Ok(None),
            },
            true,
        );
        assert_eq!(app.avatar(id), Some(fetched));
        assert_eq!(
            app.accounts[1]
                .local_avatars
                .get(&crate::local_avatars::key(id)),
            Some(&hidden)
        );
    }

    #[test]
    fn locked_local_picture_actions_never_open_picker_or_change_disk() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let id = "1@s.whatsapp.net";
        app.chats
            .push(Chat::new(id.into(), "Synthetic contact".into()));
        app.app_lock.lock();
        let ctx = egui::Context::default();
        app.apply(Action::PickLocalAvatar(id.into()), &ctx);
        app.apply(Action::ResetLocalAvatar(id.into()), &ctx);
        assert!(commands.try_recv().is_err());
        assert!(!app.dirs.state.join("avatar-overrides").exists());
    }

    #[test]
    fn dispatched_automation_is_revoked_by_account_opt_out_local_and_remote_chat_locks() {
        use crate::automation::{Permit, Request};
        use std::sync::{Arc, atomic::AtomicBool};
        for cause in ["opt-out", "local-lock", "remote-lock"] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
            let id = "1@s.whatsapp.net";
            app.chats
                .push(Chat::new(id.into(), "Synthetic contact".into()));
            app.account_mut().settings.cli_access = true;
            let (backend, mut commands) = Backend::recording();
            app.backend = backend;
            app.automation = Some(
                crate::automation::Server::start(&app.dirs.runtime, Waker::default()).unwrap(),
            );
            app.sync_automation_authority();
            let (reply, received) = std::sync::mpsc::channel();
            let pending = crate::automation::Pending {
                request: Request::Read {
                    account: "1".into(),
                    chat: id.into(),
                    before: None,
                    limit: 1,
                },
                permit: Arc::new(Permit::default()),
                reply,
                cancelled: Arc::new(AtomicBool::new(false)),
                locked: app.automation.as_ref().unwrap().locked.clone(),
            };
            app.automation
                .as_ref()
                .unwrap()
                .queue
                .lock()
                .unwrap()
                .push(pending);
            app.handle_automation();
            let Command::Automation(dispatched) = commands.try_recv().unwrap() else {
                panic!("authorization must dispatch to the owning account");
            };
            assert!(!dispatched.blocked());
            match cause {
                "opt-out" => {
                    app.account_mut().settings.cli_access = false;
                    app.sync_automation_authority();
                }
                "local-lock" => app.apply(
                    Action::SetLocked(id.into(), true),
                    &egui::Context::default(),
                ),
                _ => {
                    let mut chat = app.chats[0].clone();
                    chat.locked = true;
                    app.apply_backend_event(Event::ChatUpdated(Box::new(chat)), true);
                }
            }
            assert!(dispatched.blocked(), "{cause}");
            // Re-enable/unlock does not release an old queued result.
            app.account_mut().settings.cli_access = true;
            app.chats[0].locked = false;
            app.sync_automation_authority();
            dispatched.answer("synthetic private result".into());
            assert!(
                !received
                    .recv()
                    .unwrap()
                    .contains("synthetic private result"),
                "{cause}"
            );
        }
    }

    #[test]
    fn leaving_an_account_before_it_is_linked_removes_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = App::headless(AppDirs::under(directory.path()), Settings::default()).0;
        let ctx = egui::Context::default();
        app.apply(Action::AddAccount, &ctx);
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.link = LinkStatus::Unlinked {
            qr: None,
            pair_code: None,
            pairing_phone: None,
        };
        app.apply(Action::CancelAddAccount, &ctx);
        assert_eq!(app.account().id, AccountId::first());
        assert!(!app.adding_account);
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, Command::RemoveAccount)),
            "the unlinked account is removed"
        );
    }

    #[test]
    fn a_hidden_account_never_marks_its_remembered_chat_read() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = two_accounts(directory.path());
        app.window_focused = true;
        app.window_hidden = false;
        let chat = "15550003333@s.whatsapp.net";
        app.accounts[1].open_chat = Some(chat.into());
        app.accounts[1].settings.notifications = true;
        let mut update = Chat::new(chat.into(), "Grace".into());
        update.unread = 1;
        app.active = 1;
        app.events_hidden = true;
        app.handle_chat_updated(update, false);
        app.events_hidden = false;
        app.active = 0;
        let kept = app.accounts[1].chats.iter().find(|known| known.id == chat);
        assert_eq!(kept.unwrap().unread, 1);
        assert!(
            !std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, Command::MarkRead { .. })),
            "no read receipt for a chat nobody is looking at"
        );
    }

    #[test]
    fn the_taskbar_counts_every_account_and_the_switcher_the_others() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _commands) = two_accounts(directory.path());
        let mut mine = Chat::new("15550004444@s.whatsapp.net".into(), "Ada".into());
        mine.unread = 1;
        app.accounts[0].chats.push(mine);
        for id in ["15550005555@s.whatsapp.net", "15550006666@s.whatsapp.net"] {
            let mut theirs = Chat::new(id.into(), "Grace".into());
            theirs.unread = 3;
            app.accounts[1].chats.push(theirs);
        }
        assert_eq!(app.unread_chat_count(), 1);
        assert_eq!(app.unread_chat_count_everywhere(), 3);
        assert_eq!(app.unread_chat_count_elsewhere(), 2);
    }

    #[test]
    fn global_settings_reach_every_account() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, mut commands) = two_accounts(directory.path());
        let ctx = egui::Context::default();
        let folder = directory.path().join("Downloads");
        app.apply(Action::SetDownloadFolder(Some(folder.clone())), &ctx);
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(
                command,
                Command::SetDownloadFolder(Some(ref chosen)) if *chosen == folder
            )),
            "the hidden account downloads there too"
        );
        app.apply(Action::SetKeepChatsArchived(false), &ctx);
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, Command::SetKeepChatsArchived(false))),
            "the hidden account unarchives on new messages too"
        );
    }

    #[test]
    fn a_notification_behind_the_lock_keeps_its_account() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _commands) = two_accounts(directory.path());
        app.app_lock = crate::app_lock::AppLock::new(true);
        app.notification_opens
            .lock()
            .unwrap()
            .push(crate::notify::NotificationTarget {
                account: AccountId::parse("2").unwrap(),
                chat: "15550003333@s.whatsapp.net".into(),
                message: "m1".into(),
            });
        app.handle_notification_opens();
        assert_eq!(
            app.account().id,
            AccountId::first(),
            "nothing changes while locked"
        );
        assert_eq!(
            app.app_lock
                .deferred
                .as_ref()
                .map(|target| target.account.as_str()),
            Some("2")
        );
    }

    #[test]
    fn removing_an_earlier_account_keeps_the_open_one() {
        let directory = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(directory.path());
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let second = Account::detached(
            &dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap()
        .0;
        let third = Account::detached(
            &dirs,
            AccountId::parse("3").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap()
        .0;
        app.accounts.push(second);
        app.accounts.push(third);
        app.active = 1;
        app.composer = "keep typing".into();
        let kept = app.account().id.clone();
        app.finish_removed(0);
        assert_eq!(app.account().id, kept);
        assert_eq!(app.accounts.len(), 2);
        assert_eq!(app.composer, "keep typing");
    }

    #[test]
    fn a_hidden_account_draft_does_not_fill_the_open_composer() {
        let directory = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(directory.path());
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let (second, events) = Account::detached(
            &dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        app.accounts.push(second);
        app.active = 0;
        app.composer = "visible".into();
        app.accounts[1].open_chat = Some("1@s.whatsapp.net".into());
        events
            .send(crate::backend::Event::Drafts(vec![(
                "1@s.whatsapp.net".into(),
                "from work".into(),
            )]))
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "visible");
        assert_eq!(
            app.accounts[1]
                .drafts
                .get("1@s.whatsapp.net")
                .map(String::as_str),
            Some("from work")
        );
    }

    #[test]
    fn a_notification_action_keeps_its_account_when_another_emits() {
        let directory = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(directory.path());
        let mut app = App::headless(dirs.clone(), Settings::default()).0;
        let (second, events) = Account::detached(
            &dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        app.accounts.push(second);
        app.active = 0;
        app.actions.push(Action::ShowWindow);
        events
            .send(crate::backend::Event::ChatSoundPicked {
                chat: "1@s.whatsapp.net".into(),
                path: directory.path().join("ding.ogg"),
            })
            .unwrap();
        app.handle_events();
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::ShowWindow)),
            "a click waiting before the batch stays untagged"
        );
        assert_eq!(app.deferred_account_actions.len(), 1);
        assert_eq!(app.deferred_account_actions[0].0.as_str(), "2");
    }

    /// A trackpad gesture keeps its pane while fastframe-scroll says it goes
    /// on: a frame that is slow to draw is no pause, and the gesture's glide
    /// stays with the pane too. A pause in the input itself ends the gesture.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_trackpad_gesture_keeps_its_pane_through_a_slow_frame_and_its_glide() {
        let mut app = app();
        let ctx = egui::Context::default();
        let run = |app: &mut App, time: f64, delta: egui::Vec2| {
            let events = if delta == egui::Vec2::ZERO {
                Vec::new()
            } else {
                vec![egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta,
                    modifiers: egui::Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                }]
            };
            let mut output = ctx.run_ui(
                egui::RawInput {
                    time: Some(time),
                    events,
                    ..Default::default()
                },
                |ui| {
                    let ctx = ui.ctx().clone();
                    app.scrolling.apply(&ctx);
                    app.route_scroll(&ctx);
                },
            );
            output.textures_delta.clear();
        };
        for frame in 0..6 {
            run(&mut app, f64::from(frame) / 60.0, egui::vec2(0.0, 30.0));
        }
        assert!(app.scrolling.from_trackpad());
        app.scroll_route.owner = Some(ScrollPane::Messages);
        // A frame that takes a quarter of a second to draw, 16 ms of input
        // time after the last: nothing about the gesture has changed.
        std::thread::sleep(Duration::from_millis(250));
        run(&mut app, 6.0 / 60.0, egui::Vec2::ZERO);
        assert!(
            !app.scrolling.gliding(),
            "a slow frame lets the gesture glide"
        );
        assert_eq!(
            app.scroll_route.owner,
            Some(ScrollPane::Messages),
            "a slow frame lets go of the gesture's pane"
        );
        // A pause in the input itself ends the gesture, which glides, still
        // with its pane.
        run(&mut app, 6.0 / 60.0 + 0.2, egui::Vec2::ZERO);
        assert!(app.scrolling.gliding(), "the lifted gesture glides");
        run(&mut app, 6.0 / 60.0 + 0.25, egui::Vec2::ZERO);
        assert_eq!(
            app.scroll_route.owner,
            Some(ScrollPane::Messages),
            "the glide lets go of the gesture's pane"
        );
        // Once the glide is over, the pane is free for the next gesture.
        let mut time = 6.0 / 60.0 + 0.25;
        while app.scrolling.gliding() && time < 10.0 {
            time += 1.0 / 60.0;
            run(&mut app, time, egui::Vec2::ZERO);
        }
        assert!(!app.scrolling.gliding(), "the glide stops");
        run(&mut app, time + 1.0, egui::Vec2::ZERO);
        assert_eq!(app.scroll_route.owner, None, "the next gesture picks");
    }

    /// A chat that is gone or emptied takes its confirmation with it: a modal
    /// left behind for a chat that no longer exists still dispatches its
    /// action, and after a remote clear that action would take what arrived
    /// since.
    #[test]
    fn a_remote_removal_or_clear_closes_the_confirmation() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Peer".into()));

        app.dialog = Some(Dialog::ConfirmClearChat(chat.into()));
        events
            .send(Event::ChatCleared {
                chat: chat.into(),
                through: 100,
            })
            .unwrap();
        app.handle_events();
        assert!(
            app.dialog.is_none(),
            "a cleared chat closes its confirmation"
        );

        app.dialog = Some(Dialog::ConfirmClearChat(chat.into()));
        events
            .send(Event::ChatRemoved { chat: chat.into() })
            .unwrap();
        app.handle_events();
        assert!(
            app.dialog.is_none(),
            "a removed chat closes its confirmation"
        );
    }

    /// Demo and test runs share the machine with a linked Zap Faster, whose real
    /// taskbar badge they must not overwrite.
    #[test]
    fn demo_and_test_runs_do_not_publish_a_taskbar_badge() {
        let app = app();
        assert!(app.badge.is_none());
        #[cfg(target_os = "windows")]
        assert!(app.taskbar_badge_count().is_none());
    }

    /// Every account reconnects when the system reports a wake, and a sleep
    /// is answered at once, with the settings saved.
    #[test]
    fn a_wake_reconnects_every_account_and_a_sleep_saves_first() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let (mut second, _) = Account::detached(
            &app.dirs,
            AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap();
        let (second_backend, mut second_commands) = Backend::recording();
        second.backend = second_backend;
        app.accounts.push(second);
        app.power.send(fastframe_power::Event::Resumed);
        app.handle_power();
        assert!(matches!(commands.try_recv(), Ok(Command::Resumed)));
        assert!(matches!(second_commands.try_recv(), Ok(Command::Resumed)));
        assert!(commands.try_recv().is_err(), "once per wake");
        assert!(
            second_commands.try_recv().is_err(),
            "once for the hidden account too"
        );

        app.settings_dirty = true;
        let ack = fastframe_power::Ack::default();
        app.power.send(fastframe_power::Event::Suspending(ack));
        app.handle_power();
        assert!(!app.settings_dirty, "saved before sleeping");
        assert!(commands.try_recv().is_err());
        assert!(second_commands.try_recv().is_err());
    }

    /// The window's geometry is remembered in the logic pass, which eframe
    /// runs for a minimized window too, and only a change marks the settings
    /// for saving.
    #[test]
    fn the_logic_pass_remembers_the_window_geometry() {
        let mut app = app();
        let ctx = egui::Context::default();
        let run = |app: &mut App, rect: egui::Rect| {
            let mut input = egui::RawInput::default();
            let info = input.viewports.entry(egui::ViewportId::ROOT).or_default();
            info.inner_rect = Some(rect);
            info.outer_rect = Some(rect.expand(4.0));
            info.maximized = Some(false);
            let mut output = ctx.run_ui(input, |ui| app.window_logic(ui.ctx()));
            output.textures_delta.clear();
        };
        let rect = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(1280.0, 800.0));
        run(&mut app, rect);
        assert!(app.settings_dirty);
        assert_eq!(
            (app.settings.window_width, app.settings.window_height),
            (Some(1280.0), Some(800.0))
        );

        app.settings_dirty = false;
        run(&mut app, rect);
        assert!(!app.settings_dirty, "nothing changed, nothing to save");
    }

    /// A short chat asks the phone by itself; only the reader scrolling to
    /// the top makes a request theirs, so only then may a silent phone be
    /// reported (#325). Scrolling up during an automatic request claims it
    /// once rather than asking twice.
    #[test]
    fn only_the_reader_scrolling_up_asks_the_phone_explicitly() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        app.link = LinkStatus::Connected;
        let chat = "4915700000003@s.whatsapp.net";
        let conversation = app.conversations.entry(chat.into()).or_default();
        conversation.merge(vec![message(chat, "m1", 100)], false);
        conversation.complete = true;
        let mut asked = || {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter_map(|command| match command {
                    Command::FetchOlder { explicit, .. } => Some(explicit),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };

        app.load_older(chat, false);
        assert_eq!(asked(), [false]);
        app.load_older(chat, false);
        assert_eq!(asked(), [false; 0], "one request at a time");
        app.load_older(chat, true);
        assert_eq!(asked(), [true], "the reader claims the waiting request");
        app.load_older(chat, true);
        assert_eq!(asked(), [false; 0], "and only once");

        events
            .send(Event::OlderFetched {
                chat: chat.into(),
                more: false,
            })
            .unwrap();
        app.handle_events();
        assert!(!app.conversations[chat].phone_explicit);
        app.load_older(chat, true);
        assert_eq!(
            asked(),
            [false; 0],
            "a chat at its start is not asked again"
        );
    }

    #[test]
    fn the_new_contact_box_remembers_whether_to_save_to_the_phone() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let to_phone = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>| {
            std::iter::from_fn(|| commands.try_recv().ok())
                .find_map(|command| match command {
                    Command::NewContact { to_phone, .. } => Some(to_phone),
                    _ => None,
                })
                .expect("the number is checked")
        };
        app.apply(Action::ShowDialog(Dialog::NewContact), &ctx);
        assert!(app.new_contact_to_phone, "on until turned off");
        let add = |to_phone, first: &str| Action::NewContact {
            phone: "15550002222".into(),
            first: first.into(),
            last: String::new(),
            to_phone,
        };
        app.apply(add(Some(false), "Ada"), &ctx);
        assert!(!to_phone(&mut commands));
        assert!(!app.account().settings.save_contacts_to_phone);
        assert!(app.account().settings_dirty);
        app.apply(Action::ShowDialog(Dialog::NewContact), &ctx);
        assert!(!app.new_contact_to_phone, "the next dialog starts from it");
        // Opening a chat without a name saves nothing, so it keeps the choice.
        app.apply(add(Some(true), ""), &ctx);
        assert!(to_phone(&mut commands));
        assert!(!app.account().settings.save_contacts_to_phone);
        // A shared contact's Add follows the last choice.
        app.apply(add(None, "Bob"), &ctx);
        assert!(!to_phone(&mut commands));
    }

    #[test]
    fn hiding_the_chat_list_always_collapses_it_to_avatars() {
        let mut app = app();
        let ctx = egui::Context::default();
        assert_eq!(app.sidebar_mode(), SidebarDisplayMode::Expanded);
        app.apply(Action::ToggleSidebar, &ctx);
        assert_eq!(
            app.sidebar_mode(),
            SidebarDisplayMode::CollapsedIconsOnly,
            "hiding the list leaves the avatar column"
        );
        app.apply(Action::ToggleSidebar, &ctx);
        assert_eq!(
            app.sidebar_mode(),
            SidebarDisplayMode::Expanded,
            "and brings the full list back"
        );
    }

    #[test]
    fn wallpaper_colors_remain_independent_between_themes() {
        let mut app = app();
        let ctx = egui::Context::default();
        let light_color = crate::settings::WallpaperColor::Cruise;
        let dark_color = crate::settings::WallpaperColor::Nordic;
        let original_dark_color = app.settings.dark_wallpaper_color;

        app.palette.dark = false;
        app.apply(Action::SetWallpaperColor(light_color), &ctx);
        assert_eq!(app.settings.wallpaper_color, light_color);
        assert_eq!(app.settings.dark_wallpaper_color, original_dark_color);
        assert!(app.settings_dirty);

        app.settings_dirty = false;
        app.palette.dark = true;
        app.apply(Action::SetWallpaperColor(dark_color), &ctx);
        assert_eq!(app.settings.dark_wallpaper_color, dark_color);
        assert_eq!(app.settings.wallpaper_color, light_color);
        assert!(app.settings_dirty);
    }

    fn local_pack(name: &str, dir: &str) -> StickerPack {
        StickerPack {
            name: name.into(),
            dir: PathBuf::from(dir),
            stickers: Vec::new(),
            local: true,
        }
    }

    fn stickers_event(packs: Vec<StickerPack>) -> Event {
        Event::Stickers {
            favorites: Vec::new(),
            packs,
            recent: Vec::new(),
            received: Vec::new(),
            emojis: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn a_new_pack_is_selected_once_the_backend_lists_it() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, events) = App::headless(AppDirs::under(root.path()), Settings::default());
        let ctx = egui::Context::default();
        app.apply(Action::CreateStickerPack("   ".into()), &ctx);
        app.apply(Action::CreateStickerPack("  Bom dia  ".into()), &ctx);
        assert_eq!(app.sticker_shelf, StickerShelf::Recent, "no folder yet");
        events.send(stickers_event(Vec::new())).unwrap();
        app.handle_events();
        assert_eq!(app.sticker_shelf, StickerShelf::Recent, "an update waits");
        events
            .send(stickers_event(vec![
                local_pack("Bom dia", "/packs/Bom dia 2"),
                local_pack("Bom dia", "/packs/Bom dia"),
            ]))
            .unwrap();
        app.handle_events();
        assert_eq!(
            app.sticker_shelf,
            StickerShelf::Pack(PathBuf::from("/packs/Bom dia 2")),
            "the trimmed name picks the newest pack of that name"
        );
        app.apply(Action::SelectStickerShelf(StickerShelf::Favorites), &ctx);
        assert_eq!(app.sticker_shelf, StickerShelf::Favorites);
    }

    #[test]
    fn a_pack_that_vanished_cannot_stay_selected() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, events) = App::headless(AppDirs::under(root.path()), Settings::default());
        events
            .send(stickers_event(vec![local_pack(
                "Bom dia",
                "/packs/Bom dia",
            )]))
            .unwrap();
        app.handle_events();
        let ctx = egui::Context::default();
        app.apply(
            Action::SelectStickerShelf(StickerShelf::Pack(PathBuf::from("/packs/Bom dia"))),
            &ctx,
        );
        assert!(app.selected_pack().is_some());
        events.send(stickers_event(Vec::new())).unwrap();
        app.handle_events();
        assert_eq!(
            app.sticker_shelf,
            StickerShelf::Recent,
            "a pack deleted elsewhere cannot stay open"
        );
    }

    fn paste_release() -> egui::Event {
        egui::Event::Key {
            key: egui::Key::V,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        }
    }

    fn clipboard_frame(
        app: &mut App,
        ctx: &egui::Context,
        events: Vec<egui::Event>,
        image: bool,
    ) -> usize {
        clipboard_frame_with_files(app, ctx, events, None, image).0
    }

    /// Runs a frame whose clipboard holds `files` and, if `image`, a picture.
    /// Returns how often the clipboard and its picture were read.
    fn clipboard_frame_with_files(
        app: &mut App,
        ctx: &egui::Context,
        mut events: Vec<egui::Event>,
        files: Option<Vec<PathBuf>>,
        image: bool,
    ) -> (usize, usize) {
        let mut reads = 0;
        let mut image_reads = 0;
        if !events
            .iter()
            .any(|event| matches!(event, egui::Event::ModifiersChanged(_)))
        {
            events.insert(0, egui::Event::ModifiersChanged(egui::Modifiers::COMMAND));
        }
        let mut output = ctx.run_ui(
            egui::RawInput {
                events,
                ..Default::default()
            },
            |ui| {
                app.take_clipboard_paste(ui.ctx(), || {
                    reads += 1;
                    clipboard_contents(
                        || files.clone(),
                        || {
                            image_reads += 1;
                            image.then(|| (2, 2, vec![200; 16]))
                        },
                    )
                });
                ui.add(
                    egui::TextEdit::singleline(&mut app.composer)
                        .id(egui::Id::new("composer-text")),
                );
                app.apply_actions(ui.ctx());
            },
        );
        output.textures_delta.clear();
        (reads, image_reads)
    }

    fn clipboard_app() -> (App, egui::Context) {
        let mut app = app();
        app.open_chat = Some("fixture".into());
        app.chats
            .push(Chat::new("fixture".into(), "Fixture".into()));
        app.composer = "caption".into();
        let ctx = egui::Context::default();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("composer-text")));
        clipboard_frame(&mut app, &ctx, vec![], false);
        (app, ctx)
    }

    #[test]
    fn image_paste_consumes_source_text_and_stages_once_across_frames() {
        let (mut app, ctx) = clipboard_app();
        let reads = clipboard_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("https://example.org/picture.png".into())],
            true,
        );
        assert_eq!(reads, 1);
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.composer, "caption");
        assert_eq!(
            clipboard_frame(&mut app, &ctx, vec![paste_release()], true),
            0
        );
        assert_eq!(
            app.pending.len(),
            1,
            "release must not duplicate the picture"
        );
        assert_eq!(
            clipboard_frame(&mut app, &ctx, vec![paste_release()], true),
            1
        );
        assert_eq!(app.pending.len(), 2, "a later image-only paste still works");
    }

    #[test]
    fn a_menu_paste_does_not_suppress_a_later_image_only_shortcut() {
        let (mut app, ctx) = clipboard_app();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                    egui::Event::Paste("fixture URL".into()),
                ],
                ..Default::default()
            },
            |ui| {
                app.take_clipboard_paste(ui.ctx(), || {
                    Some(ClipboardPaste::Image {
                        width: 2,
                        height: 2,
                        rgba: vec![200; 16],
                    })
                });
                app.apply_actions(ui.ctx());
            },
        );
        output.textures_delta.clear();
        assert_eq!(app.pending.len(), 1);
        clipboard_frame(&mut app, &ctx, vec![paste_release()], true);
        assert_eq!(app.pending.len(), 2);
    }

    /// A keyboard paste remains owned until V-up even if Control went up first.
    #[test]
    fn a_paste_after_modifier_up_is_not_repeated_on_v_up() {
        let (mut app, ctx) = clipboard_app();
        assert_eq!(clipboard_frame(&mut app, &ctx, vec![], true), 0);
        assert_eq!(
            clipboard_frame(
                &mut app,
                &ctx,
                vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                    egui::Event::Paste("fixture image URL".into()),
                ],
                true,
            ),
            1,
        );
        let mut release = paste_release();
        if let egui::Event::Key { modifiers, .. } = &mut release {
            *modifiers = egui::Modifiers::NONE;
        }
        assert_eq!(
            clipboard_frame(
                &mut app,
                &ctx,
                vec![
                    egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                    release
                ],
                true,
            ),
            0,
        );
        assert_eq!(app.pending.len(), 1);
    }

    /// Replayed drop input must stage once across discarded layout passes.
    #[test]
    fn a_layout_retry_does_not_stage_the_same_dropped_file_twice() {
        #[derive(Debug)]
        struct FixtureDrop(PathBuf);
        impl egui::DroppedFile for FixtureDrop {
            /// Supplies a synthetic path without consulting the user's files.
            fn path(&self) -> &std::path::Path {
                &self.0
            }
            /// Drop staging needs only the path, so fixture bytes are empty.
            fn bytes(&self) -> Result<Vec<u8>, String> {
                Ok(Vec::new())
            }
        }
        let (mut app, ctx) = clipboard_app();
        let mut passes = 0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                dropped_files: vec![std::sync::Arc::new(FixtureDrop("fixture.png".into()))],
                ..Default::default()
            },
            |ui| {
                passes += 1;
                app.take_drops_and_pastes(ui.ctx());
                if passes == 1 {
                    ui.ctx().request_discard("fixture drop relayout");
                }
            },
        );
        output.textures_delta.clear();
        assert_eq!(passes, 2);
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn image_paste_handles_press_and_release_in_one_frame() {
        let (mut app, ctx) = clipboard_app();
        clipboard_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::Paste("<img src='fixture'>".into()),
                paste_release(),
            ],
            true,
        );
        assert_eq!(app.pending.len(), 1);
        assert_eq!(app.composer, "caption");
        clipboard_frame(&mut app, &ctx, vec![paste_release()], true);
        assert_eq!(app.pending.len(), 2);
    }

    #[test]
    fn screenshot_paste_survives_releasing_control_before_v() {
        let (mut app, ctx) = clipboard_app();
        let mut release = paste_release();
        if let egui::Event::Key { modifiers, .. } = &mut release {
            *modifiers = egui::Modifiers::NONE;
        }
        assert_eq!(clipboard_frame(&mut app, &ctx, vec![], true), 0);
        assert_eq!(
            clipboard_frame(
                &mut app,
                &ctx,
                vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)],
                true
            ),
            0
        );
        clipboard_frame(
            &mut app,
            &ctx,
            vec![
                egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                release.clone(),
            ],
            true,
        );
        assert_eq!(app.pending.len(), 1);
        clipboard_frame(&mut app, &ctx, vec![release], true);
        assert_eq!(app.pending.len(), 2, "each shortcut stages one image");
    }

    #[test]
    fn a_bare_v_release_after_refocusing_never_reads_the_clipboard() {
        let (mut app, ctx) = clipboard_app();
        let mut reads = 0;
        let mut release = paste_release();
        if let egui::Event::Key { modifiers, .. } = &mut release {
            *modifiers = egui::Modifiers::NONE;
        }
        for (focused, events) in [
            (
                true,
                vec![egui::Event::ModifiersChanged(egui::Modifiers::COMMAND)],
            ),
            (false, vec![]),
            (true, vec![release]),
        ] {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    focused,
                    events,
                    ..Default::default()
                },
                |ui| {
                    app.take_clipboard_paste(ui.ctx(), || {
                        reads += 1;
                        Some(ClipboardPaste::Image {
                            width: 2,
                            height: 2,
                            rgba: vec![200; 16],
                        })
                    });
                },
            );
            output.textures_delta.clear();
        }
        assert_eq!(reads, 0);
        assert!(app.pending.is_empty());
    }

    #[test]
    fn typing_v_does_not_paste_a_screenshot_on_release() {
        let (mut app, ctx) = clipboard_app();
        let mut press = paste_release();
        if let egui::Event::Key {
            pressed, modifiers, ..
        } = &mut press
        {
            *pressed = true;
            *modifiers = egui::Modifiers::NONE;
        }
        let mut release = paste_release();
        if let egui::Event::Key { modifiers, .. } = &mut release {
            *modifiers = egui::Modifiers::NONE;
        }
        assert_eq!(clipboard_frame(&mut app, &ctx, vec![press], true), 0);
        assert_eq!(clipboard_frame(&mut app, &ctx, vec![release], true), 0);
        assert!(app.pending.is_empty());
    }

    /// Adding Control before an ordinary V release never reads images or copied files.
    #[test]
    fn typing_v_then_control_before_release_never_stages_attachments() {
        for same_frame in [true, false] {
            for files in [None, Some(vec![PathBuf::from("unrelated-fixture.png")])] {
                let (mut app, ctx) = clipboard_app();
                let mut press = paste_release();
                if let egui::Event::Key {
                    pressed, modifiers, ..
                } = &mut press
                {
                    *pressed = true;
                    *modifiers = egui::Modifiers::NONE;
                }
                let events = if same_frame {
                    vec![
                        egui::Event::ModifiersChanged(egui::Modifiers::NONE),
                        press,
                        egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                        paste_release(),
                    ]
                } else {
                    assert_eq!(
                        clipboard_frame_with_files(
                            &mut app,
                            &ctx,
                            vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE), press],
                            files.clone(),
                            true
                        ),
                        (0, 0)
                    );
                    vec![
                        egui::Event::ModifiersChanged(egui::Modifiers::COMMAND),
                        paste_release(),
                    ]
                };
                assert_eq!(
                    clipboard_frame_with_files(&mut app, &ctx, events, files, true),
                    (0, 0)
                );
                assert!(app.pending.is_empty());
            }
        }
    }

    #[test]
    fn screenshot_paste_takes_focus_back_from_a_message() {
        let (mut app, ctx) = clipboard_app();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("fixture-message")));
        clipboard_frame(&mut app, &ctx, vec![paste_release()], true);
        assert_eq!(app.pending.len(), 1);
        assert!(ctx.memory(|memory| memory.has_focus(egui::Id::new("composer-text"))));
    }

    #[test]
    fn a_layout_retry_does_not_stage_the_same_screenshot_twice() {
        let (mut app, ctx) = clipboard_app();
        let mut passes = 0;
        let mut reads = 0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![paste_release()],
                ..Default::default()
            },
            |ui| {
                passes += 1;
                app.take_clipboard_paste(ui.ctx(), || {
                    reads += 1;
                    Some(ClipboardPaste::Image {
                        width: 2,
                        height: 2,
                        rgba: vec![200; 16],
                    })
                });
                app.apply_actions(ui.ctx());
                if passes == 1 {
                    ui.ctx().request_discard("fixture relayout");
                }
            },
        );
        output.textures_delta.clear();
        assert_eq!(passes, 2);
        assert_eq!(reads, 1);
        assert_eq!(app.pending.len(), 1);
    }

    #[test]
    fn text_paste_is_preserved_when_the_clipboard_has_no_image() {
        let (mut app, ctx) = clipboard_app();
        clipboard_frame(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("plain text".into())],
            false,
        );
        assert!(app.composer.contains("plain text"));
        assert!(app.pending.is_empty());
        assert_eq!(
            clipboard_frame(&mut app, &ctx, vec![paste_release()], true),
            0,
            "a clipboard change before release must not stage an unrelated image"
        );
        assert!(app.pending.is_empty());
    }

    #[test]
    fn a_copied_file_stages_the_file_and_not_its_icon() {
        let directory = tempfile::tempdir().unwrap();
        let pdf = directory.path().join("fixture.pdf");
        let zip = directory.path().join("fixture archive.zip");
        std::fs::write(&pdf, b"%PDF-fixture").unwrap();
        std::fs::write(&zip, b"PK-fixture").unwrap();
        let (mut app, ctx) = clipboard_app();
        // Finder offers the path, the file name as text, and an icon picture.
        let (reads, image_reads) = clipboard_frame_with_files(
            &mut app,
            &ctx,
            vec![egui::Event::Paste("fixture.pdf".into()), paste_release()],
            Some(vec![pdf.clone(), zip.clone()]),
            true,
        );
        assert_eq!(reads, 1);
        assert_eq!(image_reads, 0, "the icon picture is never read");
        assert!(matches!(
            app.pending.as_slice(),
            [Pending::File(first), Pending::File(second)] if *first == pdf && *second == zip
        ));
        assert_eq!(app.composer, "caption", "the file name is not pasted");
    }

    #[test]
    fn a_uri_list_line_ending_does_not_hide_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("fixture.zip");
        std::fs::write(&file, b"PK-fixture").unwrap();
        let mut listed = file.clone().into_os_string();
        listed.push("\r");
        let contents = clipboard_contents(|| Some(vec![PathBuf::from(listed)]), || None);
        assert!(matches!(contents, Some(ClipboardPaste::Files(paths)) if paths == [file]));
    }

    #[test]
    fn a_missing_copied_file_falls_back_to_the_picture() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, ctx) = clipboard_app();
        let (_, image_reads) = clipboard_frame_with_files(
            &mut app,
            &ctx,
            vec![paste_release()],
            Some(vec![directory.path().join("gone.pdf")]),
            true,
        );
        assert_eq!(image_reads, 1);
        assert!(matches!(app.pending.as_slice(), [Pending::Picture { .. }]));
    }

    #[test]
    fn clipboard_fallback_leaves_overlay_and_recording_focus_alone() {
        for state in ["reaction", "popup", "recording"] {
            let (mut app, ctx) = clipboard_app();
            let control = egui::Id::new("fixture-active-control");
            match state {
                "reaction" => {
                    app.reaction_target = Some(("fixture".into(), "fixture-message".into()))
                }
                "popup" => egui::Popup::open_id(&ctx, egui::Id::new("fixture-menu")),
                "recording" => app.recording = Some(crate::audio::Recorder::rehearsal()),
                _ => unreachable!(),
            }
            let mut reads = 0;
            let _ = ctx.run_ui(
                egui::RawInput {
                    events: vec![paste_release()],
                    ..Default::default()
                },
                |ui| {
                    let response = ui.interact(
                        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(50.0, 20.0)),
                        control,
                        egui::Sense::click(),
                    );
                    response.request_focus();
                    app.take_clipboard_paste(ui.ctx(), || {
                        reads += 1;
                        Some(ClipboardPaste::Image {
                            width: 2,
                            height: 2,
                            rgba: vec![200; 16],
                        })
                    });
                    assert!(ui.memory(|memory| memory.has_focus(control)), "{state}");
                },
            );
            assert_eq!(reads, 0, "{state}");
            assert!(app.pending.is_empty(), "{state}");
        }
    }

    #[test]
    fn image_paste_only_reads_the_clipboard_for_a_writable_composer() {
        for state in ["search", "dialog", "settings", "read-only", "closed"] {
            let (mut app, ctx) = clipboard_app();
            match state {
                "search" => ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("search"))),
                "dialog" => app.dialog = Some(Dialog::NewContact),
                "settings" => app.page = Page::Settings,
                "read-only" => app.chats[0].read_only = true,
                "closed" => app.open_chat = None,
                _ => unreachable!(),
            }
            let reads = clipboard_frame(
                &mut app,
                &ctx,
                vec![egui::Event::Paste("fixture".into())],
                true,
            );
            assert_eq!(reads, 0, "{state}");
            assert!(app.pending.is_empty(), "{state}");
            assert!(
                ctx.input(|input| input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Paste(_)))),
                "{state}"
            );
        }
    }

    #[test]
    fn interactive_send_events_release_only_the_matching_message() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        for id in ["first", "second"] {
            events
                .send(Event::InteractiveReplyState {
                    chat: "chat".into(),
                    message: id.into(),
                    pending: true,
                })
                .unwrap();
        }
        app.background_frame(&ctx);
        assert_eq!(app.interactive_sending.len(), 2);
        events
            .send(Event::InteractiveReplyState {
                chat: "chat".into(),
                message: "first".into(),
                pending: false,
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(
            !app.interactive_sending
                .contains(&("chat".into(), "first".into()))
        );
        assert!(
            app.interactive_sending
                .contains(&("chat".into(), "second".into()))
        );
        events.send(Event::Link(LinkStatus::LoggedOut)).unwrap();
        app.background_frame(&ctx);
        assert!(app.interactive_sending.is_empty());
    }

    #[test]
    fn a_sizes_only_storage_answer_keeps_the_message_count() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, events) = App::headless(AppDirs::under(root.path()), Settings::default());
        app.storage_stats = Some(crate::model::StorageStats {
            messages: 5,
            ..crate::model::StorageStats::default()
        });
        app.storage_stats_counted = false;
        events
            .send(Event::StorageStats {
                stats: Some(crate::model::StorageStats {
                    messages: 0,
                    images: 2,
                    ..crate::model::StorageStats::default()
                }),
                counted: false,
                token: app.storage_stats_token,
            })
            .unwrap();
        app.handle_events();
        let stats = app.storage_stats.expect("stats");
        assert_eq!(stats.messages, 5);
        assert_eq!(stats.images, 2);
        assert!(!app.storage_stats_counted);
    }

    #[test]
    fn reopening_settings_counts_messages_again() {
        let mut app = app();
        let (backend, mut commands, _) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        app.page = Page::Settings;
        app.storage_stats_counted = true;
        app.storage_stats_at = Some(std::time::Instant::now());
        app.apply(Action::Open(Page::Chats), &ctx);
        app.apply(Action::Open(Page::Settings), &ctx);
        app.refresh_storage_stats(&ctx);
        let asked = std::iter::from_fn(|| commands.try_recv().ok())
            .any(|command| matches!(command, Command::StorageStats { messages: true, .. }));
        assert!(asked, "a new visit asks for the message total");
    }

    #[test]
    fn fresh_storage_stats_ask_for_a_frame_when_they_go_stale() {
        let mut app = app();
        app.page = Page::Settings;
        app.storage_stats_at = Some(std::time::Instant::now());
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |_| {
            app.refresh_storage_stats(&ctx);
        });
        output.textures_delta.clear();
        let delay = output
            .viewport_output
            .values()
            .map(|viewport| viewport.repaint_delay)
            .min()
            .unwrap_or(std::time::Duration::MAX);
        assert!(
            delay.as_secs() < 120,
            "the idle page wakes before the minute ends"
        );
    }

    #[test]
    fn archived_chats_do_not_qualify_for_notifications_until_unarchived() {
        let now = crate::util::now();
        for (id, kind) in [
            ("1@s.whatsapp.net", ChatKind::Direct),
            ("2@g.us", ChatKind::Group),
        ] {
            let mut chat = Chat::new(id.into(), "Fixture".into());
            assert_eq!(chat.kind, kind, "fixture id picks the chat kind");
            chat.unread = 1;

            chat.archived = true;
            assert!(!notification_eligible(&chat, now, now), "{id} archived");

            chat.archived = false;
            assert!(notification_eligible(&chat, now, now), "{id} unarchived");
        }
    }

    #[test]
    fn expanded_video_keeps_clipboard_and_control_focus() {
        let (mut app, ctx) = clipboard_app();
        app.video_expanded = true;
        let control = egui::Id::new("fixture-video-control");
        ctx.memory_mut(|memory| memory.request_focus(control));
        let mut reads = 0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![paste_release()],
                ..Default::default()
            },
            |ui| {
                ui.interact(
                    egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(40.0, 40.0)),
                    control,
                    egui::Sense::click(),
                );
                app.take_clipboard_paste(ui.ctx(), || {
                    reads += 1;
                    Some(ClipboardPaste::Image {
                        width: 2,
                        height: 2,
                        rgba: vec![200; 16],
                    })
                });
            },
        );
        output.textures_delta.clear();
        assert_eq!(reads, 0);
        assert!(app.pending.is_empty());
        assert!(ctx.memory(|memory| memory.has_focus(control)));
    }

    #[test]
    fn image_preview_opens_zooms_fits_and_closes() {
        let ctx = egui::Context::default();
        let mut app = app();
        let file = tempfile::NamedTempFile::with_suffix(".png").unwrap();
        std::fs::write(file.path(), b"not a real image").unwrap();

        app.apply(Action::PreviewImage(file.path().to_owned()), &ctx);
        let preview = app.image_preview.as_ref().expect("preview opens");
        assert_eq!(preview.path(), file.path());
        assert!(preview.is_fit());

        app.apply(Action::ZoomImageIn, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().zoom(), 1.25);
        app.apply(Action::FitImage, &ctx);
        assert!(app.image_preview.as_ref().unwrap().is_fit());

        app.image_preview.as_mut().unwrap().zoom_in();
        app.apply(Action::CloseImagePreview, &ctx);
        assert!(app.image_preview.is_none());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn image_navigation_stays_in_chat_order_and_skips_unavailable_media() {
        let ctx = egui::Context::default();
        let mut app = app();
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<_> = [
            "first.png",
            "middle.jpg",
            "last.webp",
            "sticker.png",
            "other.png",
        ]
        .iter()
        .map(|name| dir.path().join(name))
        .collect();
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
        }
        let media = |path: Option<PathBuf>| Media {
            album: None,
            mime: "image/png".into(),
            size: 7,
            width: None,
            height: None,
            path,
            state: MediaState::Idle,
        };
        let mut rows = Vec::new();
        for (index, path) in paths[..3].iter().enumerate() {
            if index == 2 {
                rows.push(message("chat", "text-between-photos", 3));
            }
            let mut row = message("chat", &format!("photo-{index}"), index as i64 * 2);
            row.content = Content::Image {
                motion: None,
                caption: None,
                media: media(Some(path.clone())),
            };
            rows.push(row);
        }
        let mut sticker = message("chat", "sticker", 6);
        sticker.content = Content::Sticker {
            media: media(Some(paths[3].clone())),
            animated: false,
        };
        rows.push(sticker);
        let mut pending = message("chat", "pending", 7);
        pending.content = Content::Image {
            motion: None,
            caption: None,
            media: media(None),
        };
        rows.push(pending);
        let mut video = message("chat", "video", 8);
        video.content = Content::Video {
            caption: None,
            media: media(Some(paths[3].clone())),
            seconds: Some(1),
            gif: false,
            note: false,
        };
        rows.push(video);
        let mut card = message("chat", "card", 9);
        card.content = Content::Interactive {
            text: String::new(),
            card: Some(Box::new(crate::model::InteractiveCard {
                carousel: vec![crate::model::InteractiveCard {
                    image: Some(media(Some(paths[0].clone()))),
                    ..Default::default()
                }],
                ..Default::default()
            })),
        };
        rows.push(card);
        app.conversations.insert(
            "chat".into(),
            Conversation {
                messages: rows,
                ..Default::default()
            },
        );
        let mut other = message("other", "photo", 0);
        other.content = Content::Image {
            motion: None,
            caption: None,
            media: media(Some(paths[4].clone())),
        };
        app.conversations.insert(
            "other".into(),
            Conversation {
                messages: vec![other],
                ..Default::default()
            },
        );
        app.open_chat = Some("chat".into());
        app.chats.push(Chat::new("chat".into(), "Fixture".into()));
        app.apply(Action::PreviewImage(paths[1].clone()), &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[1]);
        app.apply(Action::ZoomImageIn, &ctx);
        app.apply(Action::NextImage, &ctx);
        let preview = app.image_preview.as_ref().unwrap();
        assert_eq!(preview.path(), paths[2]);
        assert!(preview.is_fit());
        assert_eq!(preview.zoom(), 1.0);
        app.apply(Action::NextImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[0]);
        app.apply(Action::NextImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[0]);
        std::fs::remove_file(&paths[2]).unwrap();
        app.apply(Action::PreviousImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[1]);
        app.apply(Action::PreviousImage, &ctx);
        app.apply(Action::PreviousImage, &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[0]);
        app.apply(Action::PreviewImage(paths[4].clone()), &ctx);
        assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[4]);
    }

    #[test]
    fn image_navigation_revokes_access_when_a_conversation_is_locked() {
        let ctx = egui::Context::default();
        let dir = tempfile::tempdir().unwrap();
        let paths = [dir.path().join("first.png"), dir.path().join("second.png")];
        for path in &paths {
            std::fs::write(path, b"fixture").unwrap();
        }
        for remote_lock in [false, true] {
            let mut app = app();
            let mut chat = Chat::new("fixture".into(), "Fixture".into());
            chat.locked = !remote_lock;
            app.chats.push(chat.clone());
            if !remote_lock {
                app.settings.set_chat_lock_code(Some("fixture-code"));
                app.enter_locked_folder();
            }
            app.open_chat = Some(chat.id.clone());
            let rows = paths
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    let mut row = message(&chat.id, &format!("photo-{index}"), index as i64);
                    row.content = Content::Image {
                        motion: None,
                        caption: None,
                        media: Media {
                            album: None,
                            mime: "image/png".into(),
                            size: 7,
                            width: None,
                            height: None,
                            path: Some(path.clone()),
                            state: MediaState::Idle,
                        },
                    };
                    row
                })
                .collect();
            app.conversations.insert(
                chat.id.clone(),
                Conversation {
                    messages: rows,
                    ..Default::default()
                },
            );
            app.apply(Action::PreviewImage(paths[0].clone()), &ctx);
            app.apply(Action::NextImage, &ctx);
            assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[1]);
            app.apply(Action::PreviousImage, &ctx);
            assert_eq!(app.image_preview.as_ref().unwrap().path(), paths[0]);
            let retained_preview = app.image_preview.clone();
            if remote_lock {
                chat.locked = true;
                app.apply_backend_event(Event::ChatUpdated(Box::new(chat)), true);
            } else {
                app.apply(Action::CloseLockedFolder, &ctx);
            }
            assert!(app.image_preview.is_none());
            assert!(app.open_chat.is_none());
            // Even a retained preview cannot traverse the still-loaded messages.
            for action in [Action::PreviousImage, Action::NextImage] {
                app.image_preview = retained_preview.clone();
                app.apply(action, &ctx);
                assert!(app.image_preview.is_none());
            }
        }
    }

    #[test]
    fn chat_snapshots_reconcile_image_preview_access_only_for_the_visible_account() {
        let ctx = egui::Context::default();
        for present in [false, true] {
            for locked in [false, true] {
                for authorized in [false, true] {
                    for hidden in [false, true] {
                        let mut app = app();
                        if authorized {
                            app.settings.set_chat_lock_code(Some("fixture-code"));
                            app.enter_locked_folder();
                        }
                        let preview = PreviewState::in_conversation(
                            "fixture.png".into(),
                            Some("fixture".into()),
                        );
                        app.image_preview = Some(preview.clone());
                        // Preview access depends on its captured chat, even if no chat is open.
                        app.open_chat = None;
                        let mut chat = Chat::new("fixture".into(), "Fixture".into());
                        chat.locked = locked;
                        let chats = if present { vec![chat] } else { Vec::new() };
                        app.events_hidden = hidden;
                        app.apply_backend_event(Event::Chats(chats), true);
                        let accessible = present && (!locked || authorized);
                        assert_eq!(app.image_preview.is_some(), hidden || accessible);
                        if !hidden {
                            for action in [Action::PreviousImage, Action::NextImage] {
                                app.image_preview = Some(preview.clone());
                                app.apply(action, &ctx);
                                assert_eq!(app.image_preview.is_some(), accessible);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn copying_an_image_dispatches_background_decode() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let path = std::path::PathBuf::from("sample-photo.png");

        app.apply(Action::CopyImage(path.clone()), &ctx);
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::PrepareClipboardImage(p)) if p == path
        ));
    }

    #[test]
    fn clipboard_image_event_preserves_pixels_and_toasts() {
        let mut app = app();
        let pixels = vec![
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 0, 255, // yellow
        ];
        let decoded = crate::model::DecodedImage {
            width: 2,
            height: 2,
            bytes: pixels.clone(),
        };

        let mut written = None;
        app.handle_clipboard_image(Ok(decoded), |image| {
            written = Some((image.width, image.height, image.bytes.clone()));
            Ok(())
        });

        assert_eq!(written, Some((2, 2, pixels)));
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].message, "Copied image");
        assert_eq!(app.toasts[0].kind, ToastKind::Info);

        // Failure during clipboard write surfaces an error toast.
        app.toasts.clear();
        let decoded_err = crate::model::DecodedImage {
            width: 1,
            height: 1,
            bytes: vec![0; 4],
        };
        app.handle_clipboard_image(Ok(decoded_err), |_| Err("OS clipboard locked".into()));
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].message, "Failed to copy image");
        assert_eq!(app.toasts[0].kind, ToastKind::Error);

        // Failure during background decoding also surfaces an error toast.
        app.toasts.clear();
        app.handle_clipboard_image(Err("Corrupt image data".into()), |_| unreachable!());
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(app.toasts[0].message, "Failed to copy image");
        assert_eq!(app.toasts[0].kind, ToastKind::Error);
    }

    #[test]
    fn a_hidden_start_starts_the_backend_without_a_frame() {
        let root = tempfile::tempdir().unwrap();
        let (backend, mut started) = Backend::detached_with_startup();
        let mut app = App::with_backend(
            AppDirs::under(root.path()),
            Settings::default(),
            backend,
            Waker::default(),
        );
        assert!(started.try_recv().is_err(), "nothing starts before asked");
        app.start_hidden();
        assert!(app.hide_intent);
        assert_eq!(started.try_recv(), Ok(()));
    }

    /// The shell asks the app what a closed window means and what each
    /// headless tick wants; without a tray a hidden start opens the window.
    #[test]
    fn the_shell_hides_shows_and_quits_as_the_app_asks() {
        use fastframe_shell::{Closed, Headless, Resident};
        let mut app = app();
        assert_eq!(app.closed(), Closed::Quit);
        app.hide_intent = true;
        assert_eq!(app.closed(), Closed::Hide);
        app.quit_requested = true;
        assert_eq!(app.closed(), Closed::Quit);
        app.quit_requested = false;
        Resident::window_gone(&mut app);
        assert!(!app.hide_intent && !app.wants_show);
        let ctx = egui::Context::default();
        assert_eq!(app.headless_frame(&ctx), Headless::Wait);
        app.wants_show = true;
        assert_eq!(app.headless_frame(&ctx), Headless::Show);
        app.quit_requested = true;
        assert_eq!(app.headless_frame(&ctx), Headless::Quit);

        let mut app = self::app();
        assert!(app.tray.is_none());
        assert!(!Resident::start_hidden(&mut app), "no tray, no way back");
        assert!(!app.hide_intent);
    }

    /// On Linux the tray item exists before a panel shows it (Zap Faster started
    /// at login before the panel, or a desktop without one). Until a panel
    /// shows it, closing quits, a hidden start opens the window, and the
    /// window is not hidden, since nothing could bring it back.
    #[test]
    fn a_tray_no_panel_shows_does_not_keep_the_app_running() {
        use fastframe_shell::Resident;
        let ctx = egui::Context::default();
        let mut app = app();
        assert!(app.settings.keep_running_in_background);
        app.test_tray_shown = Some(false);
        assert!(!app.hides_to_tray());
        assert!(!Resident::start_hidden(&mut app));
        app.apply(Action::HideWindow, &ctx);
        assert!(!app.hide_intent);

        app.test_tray_shown = Some(true);
        assert!(app.hides_to_tray());
        app.apply(Action::HideWindow, &ctx);
        assert!(app.hide_intent);
        app.hide_intent = false;
        assert!(Resident::start_hidden(&mut app));
        assert!(app.hide_intent);

        app.settings.keep_running_in_background = false;
        assert!(!app.hides_to_tray(), "the setting still decides");
    }

    /// eframe runs only the logic pass for a minimized window, so closing one
    /// (from the taskbar, say) must still hide it to the tray rather than
    /// quit, and quit when nothing could bring it back.
    #[test]
    fn closing_a_minimized_window_hides_it_to_the_tray() {
        use fastframe_shell::{Closed, Resident};
        let ctx = egui::Context::default();
        let close_minimized = |app: &mut App| {
            let mut input = egui::RawInput::default();
            let info = input.viewports.entry(egui::ViewportId::ROOT).or_default();
            info.minimized = Some(true);
            info.events.push(egui::ViewportEvent::Close);
            let mut output = ctx.run_ui(input, |ui| app.window_logic(ui.ctx()));
            output.textures_delta.clear();
        };

        let mut app = app();
        app.test_tray_shown = Some(true);
        close_minimized(&mut app);
        assert!(app.hide_intent);
        assert_eq!(app.closed(), Closed::Hide);

        let mut app = self::app();
        app.test_tray_shown = Some(false);
        close_minimized(&mut app);
        assert_eq!(app.closed(), Closed::Quit, "no tray, no way back");

        let mut app = self::app();
        app.test_tray_shown = Some(true);
        app.quit_requested = true;
        close_minimized(&mut app);
        assert_eq!(app.closed(), Closed::Quit);
    }

    /// A Wayland compositor ignores an app's focus and unminimize requests, so
    /// a notification click or a second launch closes the window and asks the
    /// shell for a fresh one, which the compositor raises. Elsewhere the
    /// window is only told to restore and focus.
    #[test]
    fn a_wayland_show_reopens_the_window_instead_of_focusing_it() {
        use fastframe_shell::{Closed, Resident};
        let ctx = egui::Context::default();

        let mut app = app();
        app.wayland = true;
        app.apply(Action::ShowWindow, &ctx);
        assert!(app.reopen, "Wayland closes the window to make a fresh one");
        assert_eq!(app.closed(), Closed::Reopen);

        let mut app = self::app();
        app.wayland = false;
        app.apply(Action::ShowWindow, &ctx);
        assert!(!app.reopen, "elsewhere the window is only focused");
        assert_ne!(app.closed(), Closed::Reopen);
    }

    /// A later hide wins over Wayland recreation; a subsequent explicit show still works.
    #[test]
    fn window_visibility_follows_the_latest_explicit_request() {
        use fastframe_shell::{Closed, Resident};
        let ctx = egui::Context::default();
        for wayland in [true, false] {
            let mut app = app();
            app.wayland = wayland;
            app.apply(Action::ShowWindow, &ctx);
            if wayland {
                assert_eq!(app.closed(), Closed::Reopen);
            }
            app.wants_show = true;
            app.hide_window(&ctx);
            assert_eq!(app.closed(), Closed::Hide);
            assert!(!app.reopen && !app.wants_show);
            app.apply(Action::ShowWindow, &ctx);
            assert_eq!(app.closed(), Closed::Reopen);
            assert!(!app.hide_intent);
        }
    }

    #[test]
    fn unsupported_media_falls_back_to_the_external_opener() {
        let ctx = egui::Context::default();
        let mut app = app();
        let file = tempfile::NamedTempFile::with_suffix(".heic").unwrap();
        std::fs::write(file.path(), b"not a real image").unwrap();

        app.apply(Action::PreviewImage(file.path().to_owned()), &ctx);

        assert!(
            app.image_preview.is_none(),
            "no preview for unsupported media"
        );
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::OpenFile(path) if path == file.path())),
            "the external opener is queued instead"
        );
    }

    #[test]
    fn marking_unread_uses_the_empty_dot_until_the_chat_opens() {
        let mut app = app();
        let id = "1@s.whatsapp.net".to_owned();
        app.chats.push(Chat::new(id.clone(), "Ada".to_owned()));
        app.mark_unread(&id);
        let chat = app.chat(&id).expect("chat");
        assert!(chat.marked_unread);
        assert_eq!(chat.unread, 0);
        assert!(chat.looks_unread());
        app.open_chat(id.clone());
        let chat = app.chat(&id).expect("chat");
        assert!(!chat.marked_unread);
        assert!(!chat.looks_unread());
    }

    #[test]
    fn marking_unread_leaves_a_real_count_alone() {
        let mut app = app();
        let id = "1@s.whatsapp.net".to_owned();
        let mut chat = Chat::new(id.clone(), "Ada".to_owned());
        chat.unread = 3;
        app.chats.push(chat);
        app.mark_unread(&id);
        let chat = app.chat(&id).expect("chat");
        assert_eq!(chat.unread, 3);
        assert!(!chat.marked_unread);
    }

    #[test]
    fn leaving_a_group_marks_it_read_only_and_can_archive() {
        let mut app = app();
        let me = "me@s.whatsapp.net";
        app.me = Some(me.into());
        let id = "1-2@g.us".to_owned();
        let mut chat = Chat::new(id.clone(), "Rust".into());
        chat.participants = vec![me.into(), "other@s.whatsapp.net".into()];
        app.chats.push(chat);
        app.open_chat = Some(id.clone());
        app.dialog = Some(Dialog::ConfirmLeaveGroup(id.clone()));
        let ctx = egui::Context::default();
        app.apply(
            Action::LeaveGroup {
                chat: id.clone(),
                archive: false,
            },
            &ctx,
        );
        let chat = app.chat(&id).expect("chat");
        assert!(chat.read_only);
        assert!(!chat.participants.iter().any(|id| id == me));
        assert!(!chat.archived);
        assert_eq!(app.open_chat.as_deref(), Some(id.as_str()));
        assert!(app.dialog.is_none());
        // The chat no longer offers leave once we are out of it.
        assert!(!chat.can_leave(&app.our_ids()));
        app.apply(
            Action::LeaveGroup {
                chat: id.clone(),
                archive: true,
            },
            &ctx,
        );
        let chat = app.chat(&id).expect("chat");
        assert!(!chat.archived, "the archive waits for the phone");
        assert_eq!(
            app.open_chat.as_deref(),
            Some(id.as_str()),
            "and the conversation stays open until then"
        );
        // The phone agreed, so the archive lands and the open chat closes.
        let mut confirmed = chat.clone();
        confirmed.archived = true;
        app.handle_chat_updated(confirmed, true);
        app.apply_actions(&ctx);
        assert!(app.chat(&id).expect("chat").archived);
        assert!(app.open_chat.is_none(), "the confirmed archive closes it");
    }

    #[test]
    fn a_refused_leave_rolls_back_the_local_mark() {
        let mut app = app();
        let me = "me@s.whatsapp.net";
        app.me = Some(me.into());
        let id = "1-2@g.us".to_owned();
        let mut chat = Chat::new(id.clone(), "Rust".into());
        chat.participants = vec![me.into(), "other@s.whatsapp.net".into()];
        app.chats.push(chat.clone());
        let ctx = egui::Context::default();
        app.apply(
            Action::LeaveGroup {
                chat: id.clone(),
                archive: false,
            },
            &ctx,
        );
        assert!(
            app.chat(&id).expect("chat").read_only,
            "the menu marks the chat at once"
        );
        // The worker could not reach the phone, so it sends the archive row
        // back untouched. The mark has to go with it.
        app.handle_chat_updated(chat, true);
        let chat = app.chat(&id).expect("chat");
        assert!(!chat.read_only, "the refused leave is rolled back");
        assert!(!chat.left, "and so is the leave itself");
        assert!(chat.participants.iter().any(|id| id == me));
        assert!(chat.can_leave(&app.our_ids()), "and it can be tried again");
    }

    #[test]
    fn leaving_a_channel_marks_it_read_only_and_can_archive() {
        let mut app = app();
        let id = "1@newsletter".to_owned();
        let chat = Chat::new(id.clone(), "News".into());
        app.chats.push(chat);
        app.open_chat = Some(id.clone());
        app.dialog = Some(Dialog::ConfirmLeaveGroup(id.clone()));
        let ctx = egui::Context::default();
        app.apply(
            Action::LeaveGroup {
                chat: id.clone(),
                archive: false,
            },
            &ctx,
        );
        let chat = app.chat(&id).expect("chat");
        assert!(chat.read_only);
        assert!(!chat.archived);
        assert_eq!(app.open_chat.as_deref(), Some(id.as_str()));
        assert!(app.dialog.is_none());
        assert!(!chat.can_leave(&app.our_ids()));
        app.apply(
            Action::LeaveGroup {
                chat: id.clone(),
                archive: true,
            },
            &ctx,
        );
        let chat = app.chat(&id).expect("chat");
        assert!(!chat.archived, "the archive waits for the phone");
        assert_eq!(
            app.open_chat.as_deref(),
            Some(id.as_str()),
            "and the conversation stays open until then"
        );
        // The phone agreed, so the archive lands and the open chat closes.
        let mut confirmed = chat.clone();
        confirmed.archived = true;
        app.handle_chat_updated(confirmed, true);
        app.apply_actions(&ctx);
        assert!(app.chat(&id).expect("chat").archived);
        assert!(app.open_chat.is_none(), "the confirmed archive closes it");
    }

    #[test]
    fn failed_poll_requests_keep_the_draft_and_clear_pending_controls() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let ctx = egui::Context::default();
        let draft = crate::model::PollDraft {
            question: "Lunch?".into(),
            options: vec!["Pizza".into(), "Pasta".into()],
            multiple: false,
        };
        app.dialog = Some(Dialog::CreatePoll("chat".into()));
        app.poll_draft = draft.clone();
        app.apply(
            Action::CreatePoll {
                chat: "chat".into(),
                draft: draft.clone(),
            },
            &ctx,
        );
        assert!(app.poll_creating);
        events
            .send(Event::PollCreated {
                chat: "chat".into(),
                error: Some("Could not send".into()),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(!app.poll_creating);
        assert_eq!(app.poll_draft, draft);
        assert!(app.dialog.is_some());
        app.apply(
            Action::VotePoll {
                chat: "chat".into(),
                message: "poll".into(),
                choices: vec![0],
            },
            &ctx,
        );
        assert_eq!(app.poll_voting.len(), 1);
        events
            .send(Event::PollVoted {
                chat: "chat".into(),
                message: "poll".into(),
                error: Some("Could not vote".into()),
            })
            .unwrap();
        app.background_frame(&ctx);
        assert!(app.poll_voting.is_empty());
    }

    fn label(id: &str, name: &str) -> Label {
        Label {
            id: id.to_owned(),
            name: name.to_owned(),
            color_hex: "#3b82f6".to_owned(),
            created_at: 1,
        }
    }

    fn labeled(id: &str, unread: u32, labels: &[&str]) -> Chat {
        let mut chat = Chat::new(id.to_owned(), id.to_owned());
        chat.unread = unread;
        chat.labels = labels.iter().map(|label| (*label).to_owned()).collect();
        chat.last_activity = i64::from(unread) + 1;
        chat
    }

    #[test]
    fn picking_a_chip_lets_go_of_the_label() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        app.apply(Action::SelectLabel(Some("label-1".into())), &ctx);
        app.apply(Action::SetChatFilter(ChatFilter::Groups), &ctx);
        assert!(app.label_filter.is_none());
        assert_eq!(app.chat_filter, ChatFilter::Groups);
    }

    #[test]
    fn a_label_lists_its_channels_and_leaves_the_archive_alone() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        let channel = labeled("1@newsletter", 0, &["label-1"]);
        let mut archived = labeled("2@s.whatsapp.net", 0, &["label-1"]);
        archived.archived = true;
        app.chats = vec![channel, archived, labeled("3@s.whatsapp.net", 0, &[])];
        app.apply(Action::ShowArchived(true), &ctx);
        app.apply(Action::SelectLabel(Some("label-1".into())), &ctx);
        assert!(!app.show_archived, "a label chip leaves the archive");
        let listed: Vec<String> = app
            .visible_chats()
            .into_iter()
            .map(|chat| chat.id.clone())
            .collect();
        assert_eq!(listed, ["1@newsletter".to_owned()]);
        app.apply(Action::ShowArchived(true), &ctx);
        assert_eq!(
            app.visible_chats().len(),
            1,
            "the archive lists every archived chat, labelled or not"
        );
    }

    #[test]
    fn a_taken_label_name_is_refused_in_the_app() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        app.label_name = "work".into();
        app.apply(
            Action::CreateLabel {
                name: " work ".into(),
                color_hex: "#3b82f6".into(),
            },
            &ctx,
        );
        assert_eq!(app.label_name, "work", "the typed name stays to be fixed");
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.kind == ToastKind::Error)
        );
    }

    #[test]
    fn a_label_chip_replaces_the_chip_filter() {
        let ctx = egui::Context::default();
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        app.chat_filter = ChatFilter::Unread;
        app.apply(Action::SelectLabel(Some("label-1".into())), &ctx);
        assert_eq!(app.label_filter.as_deref(), Some("label-1"));
        assert_eq!(
            app.chat_filter,
            ChatFilter::All,
            "a label replaces what the chips were filtering"
        );
    }

    #[test]
    fn a_label_counts_its_unread_chats() {
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        let mut archived = labeled("3@s.whatsapp.net", 5, &["label-1"]);
        archived.archived = true;
        let mut locked = labeled("5@s.whatsapp.net", 4, &["label-1"]);
        locked.locked = true;
        app.chats = vec![
            labeled("1@s.whatsapp.net", 3, &["label-1"]),
            labeled("2@s.whatsapp.net", 2, &["label-1", "label-2"]),
            labeled("4@s.whatsapp.net", 7, &[]),
            labeled("6@s.whatsapp.net", 0, &["label-1"]),
            archived,
            locked,
        ];
        assert_eq!(
            app.label_unread("label-1"),
            2,
            "unread chats, as the other chips count; archived and locked ones stay out"
        );
        assert!(app.chat_wears(&app.chats[1], "label-2"));
        assert!(!app.chat_wears(&app.chats[2], "label-1"));
    }

    #[test]
    fn the_list_shows_only_the_chosen_label() {
        let mut app = app();
        app.labels = vec![label("label-1", "Work")];
        app.chats = vec![
            labeled("1@s.whatsapp.net", 0, &["label-1"]),
            labeled("2@s.whatsapp.net", 0, &[]),
        ];
        app.label_filter = Some("label-1".into());
        let listed: Vec<String> = app
            .visible_chats()
            .into_iter()
            .map(|chat| chat.id.clone())
            .collect();
        assert_eq!(listed, vec!["1@s.whatsapp.net".to_owned()]);
        app.label_filter = None;
        assert_eq!(
            app.visible_chats().len(),
            2,
            "without a label every chat shows"
        );
    }

    #[test]
    fn a_gone_label_leaves_the_list_showing_all_chats() {
        let root = std::env::temp_dir().join(format!("zapfast-labels-{}", std::process::id()));
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        let ctx = egui::Context::default();
        app.labels = vec![label("label-1", "Work"), label("label-2", "Home")];
        app.label_filter = Some("label-2".into());
        app.label_editing = Some(("label-2".into(), "Hous".into()));
        events
            .send(Event::Labels(vec![label("label-1", "Work")]))
            .unwrap();
        app.background_frame(&ctx);
        assert!(
            app.label_filter.is_none(),
            "the list falls back to showing every chat"
        );
        assert!(
            app.label_editing.is_none(),
            "the editor let go of the label"
        );
    }

    #[test]
    fn follow_system_retains_the_os_theme_between_platform_events() {
        let mut app = app();
        app.settings.theme = ThemeChoice::System;
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        for theme in [egui::Theme::Light, egui::Theme::Dark] {
            input.system_theme = Some(theme);
            // Native input preserves the OS preference when taking each frame.
            for _ in 0..2 {
                let mut output = ctx.run_ui(input.take(), |_| app.apply_theme(&ctx));
                output.textures_delta.clear();
                assert_eq!(app.palette.dark, theme == egui::Theme::Dark);
                assert_eq!(ctx.theme(), theme);
            }
        }
    }

    #[test]
    fn tray_clicks_show_hide_and_quit() {
        use fastframe_tray::Event;
        assert!(matches!(
            super::tray_action(Event::Show, false),
            Some(Action::ShowWindow)
        ));
        for event in [Event::Toggle, Event::Menu(super::TRAY_SHOW)] {
            assert!(matches!(
                super::tray_action(event, true),
                Some(Action::ShowWindow)
            ));
            assert!(matches!(
                super::tray_action(event, false),
                Some(Action::HideWindow)
            ));
        }
        assert!(matches!(
            super::tray_action(Event::Menu(super::TRAY_QUIT), false),
            Some(Action::Quit)
        ));
        assert!(super::tray_action(Event::Menu("other"), false).is_none());
        assert!(matches!(
            super::tray_action(Event::Menu(super::TRAY_LOCK), false),
            Some(Action::LockApp)
        ));
        for lockable in [false, true] {
            assert_eq!(
                super::tray_config(lockable).menu,
                [
                    fastframe_tray::MenuItem::action(super::TRAY_SHOW, "Show or hide Zap Faster"),
                    fastframe_tray::MenuItem::action(super::TRAY_LOCK, "Lock Zap Faster")
                        .visible(lockable),
                    fastframe_tray::MenuItem::Separator,
                    fastframe_tray::MenuItem::action(super::TRAY_QUIT, "Quit"),
                ]
            );
        }
    }

    /// The tray offers "Lock Zap Faster" as soon as a password is set and stops
    /// as soon as it is removed, not at the next start.
    #[test]
    fn the_tray_offers_the_lock_while_a_password_is_set() {
        let mut app = app();
        assert!(!app.tray_lockable);
        app.settings.app_lock_hash = Some(crate::app_lock::verifier("secret"));
        app.handle_tray();
        assert!(app.tray_lockable);
        app.settings.app_lock_hash = None;
        app.handle_tray();
        assert!(!app.tray_lockable);
    }

    #[test]
    fn custom_theme_cache_survives_a_missing_file_and_follows_system_updates() {
        use crate::theme::{Catalog, CustomTheme};
        let mut app = app();
        let ctx = egui::Context::default();
        let mut first = CustomTheme {
            filename: "mine.json".into(),
            palette: Palette::dark(),
        };
        first.palette.accent = egui::Color32::RED;
        app.custom_themes = Catalog::preview(vec![first.clone()], false);
        app.apply(Action::SetCustomTheme(first.filename.clone()), &ctx);
        assert_eq!(app.palette.accent, egui::Color32::RED);
        // Cached selection remains usable while the file is temporarily missing.
        app.custom_themes = Catalog::default();
        app.settings =
            serde_json::from_str(&serde_json::to_string(&app.settings).unwrap()).unwrap();
        app.apply_theme(&ctx);
        assert_eq!(app.palette, first.palette);
        app.apply(Action::SetTheme(ThemeChoice::System), &ctx);
        assert!(app.settings.custom_theme.is_none());
        let mut system = first;
        system.filename = "omarchy.json".into();
        system.palette.accent = egui::Color32::GREEN;
        app.custom_themes = Catalog::preview(vec![system.clone()], true);
        app.cache_custom_themes();
        assert_eq!(app.settings.system_theme_cache.as_ref(), Some(&system));
        app.apply_theme(&ctx);
        assert_eq!(app.palette.accent, egui::Color32::GREEN);
        app.apply(Action::SetTheme(ThemeChoice::Light), &ctx);
        assert_eq!(app.palette, Palette::light());
    }

    /// A change of colours keeps the old palette while the window's picture
    /// of it is on its way, and applies the new one once it arrives or after
    /// a short wait without it. The window's first palette is not revealed.
    #[test]
    fn a_theme_change_holds_the_old_colours_until_its_reveal_can_start() {
        let mut app = app();
        app.reveal_theme_changes = true;
        app.settings.theme = ThemeChoice::Dark;
        app.settings.custom_theme = None;
        let ctx = egui::Context::default();
        let at = |app: &mut App, time: f64| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    time: Some(time),
                    ..Default::default()
                },
                |ui| app.apply_theme(ui.ctx()),
            );
            output.textures_delta.clear();
        };
        at(&mut app, 0.0);
        assert_eq!(
            app.palette,
            Palette::dark(),
            "the first palette applies at once"
        );

        app.settings.theme = ThemeChoice::Light;
        at(&mut app, 1.0);
        assert_eq!(
            app.palette,
            Palette::dark(),
            "held for the window's picture"
        );
        at(&mut app, 1.1);
        assert_eq!(app.palette, Palette::dark());
        at(&mut app, 1.5);
        assert_eq!(
            app.palette,
            Palette::light(),
            "no picture came: applied anyway"
        );
    }

    #[test]
    fn automatic_updates_require_opt_in_and_explicit_restart() {
        use crate::updates::{DownloadState, Installation, Kind, Prepared};
        let mut app = app();
        let ctx = egui::Context::default();
        app.update = Some(crate::updates::Release {
            version: "99.0.0".into(),
            url: "https://github.com/crmne/zapfast/releases/latest".into(),
        });
        app.update_support = Some(Err("Use your package manager".into()));
        app.settings.download_updates_automatically = true;
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Idle));
        let installation = Installation {
            executable: PathBuf::from("/fixture/zapfast"),
            kind: Kind::Portable,
        };
        app.update_support = Some(Ok(installation.clone()));
        app.settings.download_updates_automatically = false;
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Idle));
        app.settings.download_updates_automatically = true;
        app.maybe_download_update();
        assert!(matches!(
            app.update_download,
            DownloadState::Downloading { .. }
        ));
        app.update_download =
            DownloadState::Ready(Box::new(Prepared::sample(installation, "99.0.0")));
        app.maybe_download_update();
        assert!(matches!(app.update_download, DownloadState::Ready(_)));
        assert!(!app.quit_requested);
        app.apply(Action::InstallUpdate, &ctx);
        assert!(matches!(app.update_download, DownloadState::Installing));
        assert!(!app.quit_requested, "wait for the helper before closing");
    }

    #[test]
    fn a_closed_window_does_not_read_new_messages_in_the_last_chat() {
        let mut app = app();
        let mut chat = Chat::new("peer@s.whatsapp.net".into(), "Peer".into());
        app.open_chat = Some(chat.id.clone());
        app.window_focused = true;
        app.window_gone();
        assert!(!app.window_focused);
        chat.unread = 2;
        app.handle_chat_updated(chat.clone(), true);
        assert_eq!(app.chat(&chat.id).unwrap().unread, 2);
        // Focus left over from a window callback is insufficient while hidden.
        app.window_focused = true;
        app.handle_chat_updated(chat.clone(), true);
        assert_eq!(app.chat(&chat.id).unwrap().unread, 2);
        app.window_hidden = false;
        app.handle_chat_updated(chat.clone(), true);
        assert_eq!(app.chat(&chat.id).unwrap().unread, 0);
    }

    #[test]
    fn losing_focus_takes_effect_before_processing_an_incoming_chat_update() {
        let root = std::env::temp_dir().join("zapfast-focus-test");
        let (mut app, events) = App::headless(AppDirs::under(&root), Settings::default());
        let mut chat = Chat::new("peer@s.whatsapp.net".into(), "Peer".into());
        chat.unread = 1;
        app.open_chat = Some(chat.id.clone());
        app.window_focused = true;
        events
            .send(Event::ChatUpdated(Box::new(chat.clone())))
            .unwrap();
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input
            .viewports
            .get_mut(&egui::ViewportId::ROOT)
            .unwrap()
            .focused = Some(false);
        let mut output = ctx.run_ui(input, |ui| app.background_frame(ui.ctx()));
        output.textures_delta.clear();
        assert_eq!(app.chat(&chat.id).unwrap().unread, 1);
    }

    #[test]
    fn message_info_follows_a_group_messages_receipts_only_while_open() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let group = "123-456@g.us";
        let watches = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>| {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter_map(|command| match command {
                    Command::WatchReceipts(watch) => Some(watch),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let receipts = |message: &str| crate::model::MessageReceipts {
            chat: group.into(),
            message: message.into(),
            recipients: Vec::new(),
        };
        app.apply(
            Action::ShowDialog(Dialog::MessageInfo {
                chat: group.into(),
                message: "m".into(),
            }),
            &ctx,
        );
        app.follow_receipts();
        app.follow_receipts();
        assert_eq!(
            watches(&mut commands),
            [Some((group.to_owned(), "m".to_owned()))]
        );
        // Receipts for another message, from a dialog opened earlier, are stale.
        events.send(Event::Receipts(receipts("other"))).unwrap();
        app.handle_events();
        assert!(app.message_receipts.is_none());
        events.send(Event::Receipts(receipts("m"))).unwrap();
        app.handle_events();
        assert_eq!(app.message_receipts, Some(receipts("m")));
        app.apply(Action::CloseDialog, &ctx);
        app.follow_receipts();
        assert_eq!(watches(&mut commands), [None]);
        assert!(app.message_receipts.is_none());
        // A direct message's times are on its row: nothing to follow.
        app.apply(
            Action::ShowDialog(Dialog::MessageInfo {
                chat: "1@s.whatsapp.net".into(),
                message: "m".into(),
            }),
            &ctx,
        );
        app.follow_receipts();
        assert!(watches(&mut commands).is_empty());
    }

    #[test]
    fn drafts_come_back_after_a_restart_and_leave_with_the_account() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let (open, other) = ("1@s.whatsapp.net", "2@s.whatsapp.net");
        app.open_chat = Some(open.into());
        events
            .send(Event::Drafts(vec![
                (open.into(), "half a reply".into()),
                (other.into(), "later".into()),
            ]))
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "half a reply", "the reopened chat shows it");
        assert_eq!(app.drafts.get(other).map(String::as_str), Some("later"));
        // Quitting stores what is in the composer.
        app.composer = "half a reply, finished".into();
        app.shutdown();
        let saved: Vec<(String, String)> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| match command {
                Command::SaveDraft { chat, text } => Some((chat, text)),
                _ => None,
            })
            .collect();
        assert_eq!(
            saved,
            [(open.to_owned(), "half a reply, finished".to_owned())]
        );
        // Unlinking forgets every draft.
        events.send(Event::Link(LinkStatus::LoggedOut)).unwrap();
        app.handle_events();
        assert!(app.drafts.is_empty());
        assert!(app.composer.is_empty());
    }

    #[test]
    /// Checks a selected message batch is forwarded in its original conversation order.
    fn bulk_starring_skips_local_revoked_missing_and_stale_account_selection() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats = vec![Chat::new(chat.into(), "Synthetic".into())];
        app.open_chat = Some(chat.into());
        let mut queued = message(chat, "queued", 1);
        queued.status = Delivery::Queued;
        let mut revoked = message(chat, "revoked", 2);
        revoked.content = Content::Revoked;
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "confirmed", 0), queued, revoked], false);
        app.selection = Some((
            chat.into(),
            vec![
                "confirmed".into(),
                "confirmed".into(),
                "queued".into(),
                "revoked".into(),
                "missing".into(),
            ],
        ));
        app.apply(Action::StarSelection(true), &ctx);
        let starred: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| match command {
                Command::SetStar { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(starred, ["confirmed"]);
        app.open_chat = Some("2@s.whatsapp.net".into());
        app.apply(Action::StarSelection(false), &ctx);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn list_choice_persists_and_deleted_labels_are_forgotten() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.apply(Action::SetChatFilter(ChatFilter::Groups), &ctx);
        assert_eq!(app.account().settings.last_filter, ChatFilter::Groups);
        app.labels = vec![crate::model::Label {
            id: "synthetic".into(),
            name: "Work".into(),
            color_hex: "#008800".into(),
            created_at: 0,
        }];
        app.apply(Action::SelectLabel(Some("synthetic".into())), &ctx);
        assert_eq!(
            app.account().settings.last_label.as_deref(),
            Some("synthetic")
        );
        app.labels.clear();
        app.prune_labels();
        assert!(app.account().settings.last_label.is_none());
        app.apply(Action::SelectLabel(Some("missing".into())), &ctx);
        assert!(app.label_filter.is_none());
    }

    #[test]
    fn focus_labels_remain_account_owned_and_leave_locked_chats_hidden() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let ctx = egui::Context::default();
        app.labels = vec![crate::model::Label {
            id: "work".into(),
            name: "Work".into(),
            ..Default::default()
        }];
        let mut work = Chat::new("work@s.whatsapp.net".into(), "Fixture".into());
        work.labels.push("work".into());
        let mut locked = work.clone();
        locked.id = "locked@s.whatsapp.net".into();
        locked.locked = true;
        app.chats = vec![
            work,
            locked,
            Chat::new("other@s.whatsapp.net".into(), "Other".into()),
        ];
        app.apply(
            Action::SetFocusLabels(vec!["work".into(), "missing".into()]),
            &ctx,
        );
        app.apply(Action::SetFocusMode(true), &ctx);
        assert_eq!(app.account().settings.focus_labels, ["work"]);
        assert_eq!(
            app.visible_chats()
                .iter()
                .map(|chat| chat.id.as_str())
                .collect::<Vec<_>>(),
            ["work@s.whatsapp.net"]
        );
        app.active = 1;
        assert!(!app.account().settings.focus_enabled);
        app.active = 0;
        assert!(app.account().settings.focus_enabled);
        app.labels.clear();
        app.prune_labels();
        assert!(!app.account().settings.focus_enabled);
        assert!(app.account().settings.focus_labels.is_empty());
    }

    #[test]
    fn changing_text_size_preserves_window_zoom_and_rejects_nan() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let ctx = egui::Context::default();
        app.apply(Action::SetTextScale(1.3), &ctx);
        assert_eq!(app.settings.text_scale, 1.3);
        assert_eq!(app.settings.zoom, 1.0);
        app.apply(Action::SetTextScale(f32::NAN), &ctx);
        assert_eq!(app.settings.text_scale, 1.3);
        app.apply(Action::SetTextScale(99.0), &ctx);
        assert_eq!(app.settings.text_scale, 1.5);
    }

    #[test]
    fn label_sound_overlap_is_stable_account_owned_and_mentions_keep_their_override() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        app.labels = ["first", "second"]
            .into_iter()
            .map(|id| crate::model::Label {
                id: id.into(),
                ..Default::default()
            })
            .collect();
        let mut chat = Chat::new("synthetic@s.whatsapp.net".into(), "Fixture".into());
        chat.labels = vec!["second".into(), "first".into()];
        app.account_mut()
            .settings
            .label_sounds
            .insert("first".into(), NotificationSound::None);
        app.account_mut()
            .settings
            .label_sounds
            .insert("second".into(), NotificationSound::Receive);
        assert_eq!(app.chat_label_sound(&chat), Some(NotificationSound::None));
        app.labels.reverse();
        assert_eq!(
            app.chat_label_sound(&chat),
            Some(NotificationSound::Receive)
        );
        assert_eq!(
            notification_sound(&app.settings, None, true, true),
            app.settings.mention_sound
        );
        assert_eq!(
            notification_sound(&app.settings, Some(NotificationSound::None), true, true),
            NotificationSound::None
        );
        app.active = 1;
        assert!(app.chat_label_sound(&chat).is_none());
        app.active = 0;
        app.labels.clear();
        app.prune_labels();
        assert!(app.account().settings.label_sounds.is_empty());
    }

    #[test]
    fn selected_messages_forward_together_in_chat_order() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats = vec![Chat::new(chat.into(), "Ada".into())];
        app.open_chat = Some(chat.into());
        app.conversations.entry(chat.into()).or_default().merge(
            vec![
                message(chat, "first", 1),
                message(chat, "second", 2),
                message(chat, "third", 3),
            ],
            false,
        );
        app.apply(Action::SelectMessage("third".into()), &ctx);
        app.apply(Action::ToggleSelected("first".into()), &ctx);
        assert_eq!(
            app.selection,
            Some((chat.into(), vec!["first".into(), "third".into()]))
        );
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Grace".into()));
        app.apply(
            Action::ShowDialog(Dialog::Forward {
                chat: chat.into(),
                messages: vec!["first".into(), "third".into()],
            }),
            &ctx,
        );
        app.apply(
            Action::ToggleForwardRecipient("2@s.whatsapp.net".into()),
            &ctx,
        );
        app.apply(Action::ReviewForward, &ctx);
        app.apply(Action::ConfirmForward, &ctx);
        let forwarded: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
            .flat_map(|command| match command {
                Command::Forward { messages, .. } => messages,
                _ => Vec::new(),
            })
            .collect();
        assert_eq!(forwarded, ["first", "third"]);
        assert!(app.selection.is_none());
        // Shift-click selects everything between the last click and this one,
        // skipping what cannot be forwarded.
        let mut deleted = message(chat, "gone", 4);
        deleted.content = Content::Revoked;
        app.conversations
            .get_mut(chat)
            .unwrap()
            .merge(vec![deleted, message(chat, "fifth", 5)], false);
        app.apply(Action::SelectMessage("second".into()), &ctx);
        app.apply(Action::SelectRange("fifth".into()), &ctx);
        assert_eq!(
            app.selection.as_ref().map(|(_, ids)| ids.clone()),
            Some(vec!["second".into(), "third".into(), "fifth".into()])
        );
        app.apply(Action::CancelSelection, &ctx);
        // Unselecting the last message keeps selecting, as WhatsApp Web
        // does, and the chat menu opens a selection with nothing in it.
        app.apply(Action::SelectMessage("second".into()), &ctx);
        app.apply(Action::ToggleSelected("second".into()), &ctx);
        assert_eq!(app.selection, Some((chat.into(), Vec::new())));
        app.apply(Action::CancelSelection, &ctx);
        app.apply(Action::StartSelection, &ctx);
        assert_eq!(app.selection, Some((chat.into(), Vec::new())));
        app.apply(Action::ToggleSelected("fifth".into()), &ctx);
        assert_eq!(app.selection, Some((chat.into(), vec!["fifth".into()])));
        // Choosing it again keeps what is ticked, and a deleted message
        // cannot be ticked.
        app.apply(Action::StartSelection, &ctx);
        app.apply(Action::ToggleSelected("gone".into()), &ctx);
        assert_eq!(app.selection, Some((chat.into(), vec!["fifth".into()])));
    }

    /// Selection follows confirmed message updates and deletions, retaining pending revokes.
    #[test]
    fn selection_drops_messages_that_become_ineligible() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.conversations.entry(chat.into()).or_default().merge(
            vec![message(chat, "first", 1), message(chat, "second", 2)],
            false,
        );
        app.apply(Action::SelectMessage("first".into()), &ctx);
        app.apply(
            Action::SweepMessages {
                anchor: "first".into(),
                to: "second".into(),
            },
            &ctx,
        );
        let mut revoked = message(chat, "first", 1);
        revoked.content = Content::Revoked;
        events
            .send(Event::MessageUpdated(Box::new(revoked)))
            .unwrap();
        app.handle_events();
        assert_eq!(app.selection, Some((chat.into(), vec!["second".into()])));
        assert_eq!(app.selection_anchor.as_deref(), Some("second"));
        assert!(app.sweep.as_ref().unwrap().base.is_empty());
        app.apply(
            Action::SweepMessages {
                anchor: "first".into(),
                to: "second".into(),
            },
            &ctx,
        );
        assert_eq!(app.selection, Some((chat.into(), vec!["second".into()])));

        app.apply(
            Action::DeleteForEveryone {
                chat: chat.into(),
                id: "second".into(),
            },
            &ctx,
        );
        assert_eq!(
            app.selection,
            Some((chat.into(), vec!["second".into()])),
            "a revoke request retains the original until WhatsApp accepts it"
        );
        assert!(!matches!(
            app.conversations[chat].message("second").unwrap().content,
            Content::Revoked
        ));
        let mut accepted = message(chat, "second", 2);
        accepted.content = Content::Revoked;
        events
            .send(Event::MessageUpdated(Box::new(accepted)))
            .unwrap();
        app.handle_events();
        assert_eq!(app.selection, Some((chat.into(), Vec::new())));

        // Delete for me keeps the row until the deletion is accepted, then
        // drops it from the selection along with the message.
        app.conversations
            .get_mut(chat)
            .unwrap()
            .merge(vec![message(chat, "kept", 3)], false);
        app.apply(Action::SelectMessage("kept".into()), &ctx);
        events
            .send(Event::MessageDeleted {
                chat: chat.to_owned(),
                id: "kept".into(),
            })
            .unwrap();
        app.handle_events();
        assert_eq!(app.selection, Some((chat.into(), Vec::new())));
        assert!(app.conversations[chat].message("kept").is_none());
    }

    #[test]
    fn selection_rejects_messages_that_cannot_be_forwarded() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        let blocked = [
            Content::Revoked,
            Content::PhoneOnly {
                view_once: true,
                live_location: false,
                once: None,
            },
            Content::Unsupported {
                what: "Test".into(),
            },
            Content::Poll {
                question: "Lunch?".into(),
                options: vec!["Yes".into(), "No".into()],
                state: Default::default(),
            },
            Content::Interactive {
                text: "Choose an option".into(),
                card: None,
            },
        ];
        let mut messages = vec![message(chat, "first", 1)];
        for (index, content) in blocked.into_iter().enumerate() {
            let mut row = message(chat, &format!("blocked-{index}"), index as i64 + 2);
            row.content = content;
            messages.push(row);
        }
        messages.push(message(chat, "last", 10));
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(messages, false);
        for id in (0..5)
            .map(|index| format!("blocked-{index}"))
            .chain(std::iter::once("missing".into()))
        {
            app.apply(Action::SelectMessage(id.clone()), &ctx);
            assert!(app.selection.is_none(), "{id} cannot start a selection");
            assert!(app.selection_anchor.is_none());
            app.apply(Action::SelectMessage("first".into()), &ctx);
            app.apply(Action::SelectMessage(id.clone()), &ctx);
            app.apply(Action::ToggleSelected(id.clone()), &ctx);
            assert_eq!(
                app.selection,
                Some((chat.into(), vec!["first".into()])),
                "{id} cannot replace or join a selection"
            );
            assert_eq!(app.selection_anchor.as_deref(), Some("first"));
            app.apply(Action::CancelSelection, &ctx);
            app.selection_anchor = None;
        }
        app.apply(Action::SelectMessage("first".into()), &ctx);
        app.apply(Action::SelectRange("last".into()), &ctx);
        assert_eq!(
            app.selection,
            Some((chat.into(), vec!["first".into(), "last".into()]))
        );
        app.apply(Action::CancelSelection, &ctx);
        app.apply(
            Action::SweepMessages {
                anchor: "first".into(),
                to: "last".into(),
            },
            &ctx,
        );
        assert_eq!(
            app.selection,
            Some((chat.into(), vec!["first".into(), "last".into()]))
        );
    }

    /// #246: a sweep adds its range to what was selected when it began, in
    /// the chat's order, and shrinks again when dragged back.
    #[test]
    fn a_sweep_adds_its_range_to_the_selection_it_began_from() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats = vec![Chat::new(chat.into(), "Ada".into())];
        app.open_chat = Some(chat.into());
        let mut gone = message(chat, "gone", 3);
        gone.content = Content::Revoked;
        app.conversations.entry(chat.into()).or_default().merge(
            vec![
                message(chat, "first", 1),
                message(chat, "second", 2),
                gone,
                message(chat, "fourth", 4),
                message(chat, "fifth", 5),
            ],
            false,
        );
        let selected = |app: &App| app.selection.as_ref().map(|(_, ids)| ids.clone());
        let sweep = |app: &mut App, to: &str| {
            app.apply(
                Action::SweepMessages {
                    anchor: "fifth".into(),
                    to: to.into(),
                },
                &ctx,
            );
        };
        // Outside a selection, a sweep starts one.
        sweep(&mut app, "fifth");
        assert_eq!(selected(&app), Some(vec!["fifth".into()]));
        sweep(&mut app, "second");
        assert_eq!(
            selected(&app),
            Some(vec!["second".into(), "fourth".into(), "fifth".into()]),
            "what cannot be forwarded stays out"
        );
        sweep(&mut app, "fourth");
        assert_eq!(selected(&app), Some(vec!["fourth".into(), "fifth".into()]));
        app.apply(Action::EndSweep, &ctx);
        assert!(app.sweep.is_none());
        // A new sweep keeps what was selected before it.
        app.apply(Action::ToggleSelected("first".into()), &ctx);
        app.apply(
            Action::SweepMessages {
                anchor: "second".into(),
                to: "second".into(),
            },
            &ctx,
        );
        assert_eq!(
            selected(&app),
            Some(vec![
                "first".into(),
                "second".into(),
                "fourth".into(),
                "fifth".into()
            ])
        );
        // Escape ends both.
        app.apply(Action::CancelSelection, &ctx);
        assert!(app.selection.is_none() && app.sweep.is_none());
    }

    #[test]
    fn mentions_and_other_messages_keep_their_own_notification_sound() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.apply(
            Action::SetNotificationSound {
                mention: true,
                sound: NotificationSound::None,
            },
            &ctx,
        );
        assert_eq!(app.settings.mention_sound, NotificationSound::None);
        assert_eq!(app.settings.message_sound, NotificationSound::Receive);
    }

    /// Checks that incoming group mentions and quotes identify us across phone and privacy ids.
    #[test]
    fn a_group_message_addresses_us_by_phone_number_privacy_id_or_reply() {
        let mut app = app();
        app.me = Some("15550001111@s.whatsapp.net".into());
        app.me_lid = Some("98765@lid".into());
        let group = "fixture@g.us";
        let mention = |user: &str, id: &str| crate::model::MentionRef {
            user: user.into(),
            id: id.into(),
        };
        let mut plain = message(group, "plain", 1);
        plain.sender = "15550002222@s.whatsapp.net".into();
        assert!(!app.addresses_us(&plain));

        let mut by_number = plain.clone();
        by_number.mentions = vec![mention("15550001111", "15550001111@s.whatsapp.net")];
        assert!(app.addresses_us(&by_number));

        // Once the worker knows our pair, a privacy-id mention arrives under
        // the phone number; before that, as the privacy id itself.
        let mut by_privacy_id = plain.clone();
        by_privacy_id.mentions = vec![mention("98765", "98765@lid")];
        assert!(app.addresses_us(&by_privacy_id));
        app.me_lid = None;
        assert!(
            !app.addresses_us(&by_privacy_id),
            "an unknown privacy id is someone else"
        );
        app.me_lid = Some("98765@lid".into());

        let mut someone_else = plain.clone();
        someone_else.mentions = vec![mention("15550002222", "15550002222@s.whatsapp.net")];
        assert!(!app.addresses_us(&someone_else));

        let quote = |sender: &str| crate::model::Quoted {
            chat: None,
            id: "earlier".into(),
            sender: sender.into(),
            sender_name: None,
            summary: "earlier".into(),
            mentions: Vec::new(),
        };
        let mut reply = plain.clone();
        reply.quoted = Some(quote("15550001111@s.whatsapp.net"));
        assert!(app.addresses_us(&reply));
        reply.quoted = Some(quote("98765@lid"));
        assert!(app.addresses_us(&reply));
        reply.quoted = Some(quote("15550002222@s.whatsapp.net"));
        assert!(!app.addresses_us(&reply));
    }

    #[test]
    fn mentions_sound_even_in_quiet_groups_and_chat_sounds_win() {
        let mut settings = Settings::default();
        let sound = |settings: &Settings, chat, group, for_us| {
            notification_sound(settings, chat, group, for_us)
        };
        // Pidgin's model: the message sound everywhere, the alert when
        // someone addresses us in a group.
        assert_eq!(
            sound(&settings, None, false, false),
            NotificationSound::Receive
        );
        assert_eq!(
            sound(&settings, None, true, false),
            NotificationSound::Receive
        );
        assert_eq!(sound(&settings, None, true, true), NotificationSound::Alert);

        settings.group_sounds = false;
        assert_eq!(sound(&settings, None, true, false), NotificationSound::None);
        assert_eq!(sound(&settings, None, true, true), NotificationSound::Alert);
        assert_eq!(
            sound(&settings, None, false, false),
            NotificationSound::Receive,
            "one-to-one chats are not groups"
        );

        // A chat's own sound covers every message in it, mentions included.
        let custom = NotificationSound::Custom("/sounds/ding.wav".into());
        assert_eq!(sound(&settings, Some(custom.clone()), true, false), custom);
        assert_eq!(sound(&settings, Some(custom.clone()), true, true), custom);
        assert_eq!(
            sound(&settings, Some(NotificationSound::None), true, true),
            NotificationSound::None
        );
    }

    #[test]
    fn opening_an_unread_chat_remembers_where_its_unread_messages_begin() {
        let mut app = app();
        let mut busy = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        busy.unread = 4;
        let quiet = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        app.chats = vec![busy, quiet];
        app.open_chat("1@s.whatsapp.net".into());
        assert_eq!(
            app.unread_divider.as_ref().map(|divider| divider.count),
            Some(4)
        );
        assert_eq!(app.chat("1@s.whatsapp.net").unwrap().unread, 0);
        // Reopening the same chat keeps it; another chat without unread clears it.
        app.open_chat("1@s.whatsapp.net".into());
        assert!(app.unread_divider.is_some());
        app.open_chat("2@s.whatsapp.net".into());
        assert!(app.unread_divider.is_none());
    }

    #[test]
    fn launched_invite_waits_for_unlock_and_connection_without_joining() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.chats_loaded = true;
        app.link = LinkStatus::Connecting;
        let ctx = egui::Context::default();
        app.open_group_invite(
            crate::chat_link::GroupInviteLink::parse(
                "https://chat.whatsapp.com/AbCdEf1234567890XyZ",
            )
            .unwrap(),
        );
        app.handle_chat_links(&ctx);
        assert_eq!(app.pending_group_invites.len(), 1);
        assert!(commands.try_recv().is_err());
        app.link = LinkStatus::Connected;
        app.app_lock.lock();
        app.handle_chat_links(&ctx);
        assert_eq!(app.pending_group_invites.len(), 1);
        assert!(commands.try_recv().is_err());
        app.app_lock.unlocked(true);
        app.handle_chat_links(&ctx);
        assert!(app.pending_group_invites.is_empty());
        assert_eq!(app.dialog, Some(Dialog::JoinGroup));
        assert!(
            matches!(commands.try_recv(), Ok(Command::PreviewInvite(code)) if code == "AbCdEf1234567890XyZ")
        );
        assert!(
            commands.try_recv().is_err(),
            "joining still requires the user's action"
        );
    }

    #[test]
    fn an_invite_link_is_previewed_and_joined_inside_the_app() {
        use crate::model::{InviteInfo, InviteState};
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        app.apply(
            Action::OpenUrl("https://chat.whatsapp.com/AbCdEf1234567890XyZ".into()),
            &ctx,
        );
        assert_eq!(app.dialog, Some(Dialog::JoinGroup));
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::PreviewInvite(code)) if code == "AbCdEf1234567890XyZ"
        ));
        let info = InviteInfo {
            id: "1@g.us".into(),
            subject: "Club".into(),
            description: None,
            members: 3,
            approval: false,
        };
        app.invite.as_mut().unwrap().state = InviteState::Ready(info);
        app.apply(Action::JoinGroup, &ctx);
        assert!(matches!(commands.try_recv(), Ok(Command::JoinInvite(_))));
        assert!(matches!(
            app.invite.as_ref().unwrap().state,
            InviteState::Joining(_)
        ));
        // A second click while joining sends nothing more.
        app.apply(Action::JoinGroup, &ctx);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn a_fourth_pin_is_refused_like_on_the_phone() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        for index in 0..5 {
            let mut chat = Chat::new(format!("{index}@s.whatsapp.net"), format!("Chat {index}"));
            chat.pinned = index < 3;
            chat.archived = index == 4;
            app.chats.push(chat);
        }
        app.apply(
            Action::SetPinned("3@s.whatsapp.net".into(), true),
            &egui::Context::default(),
        );
        assert!(!app.chat("3@s.whatsapp.net").unwrap().pinned);
        assert!(commands.try_recv().is_err());
        app.apply(
            Action::SetPinned("0@s.whatsapp.net".into(), false),
            &egui::Context::default(),
        );
        app.apply(
            Action::SetPinned("3@s.whatsapp.net".into(), true),
            &egui::Context::default(),
        );
        assert!(app.chat("3@s.whatsapp.net").unwrap().pinned);
    }

    #[test]
    fn a_fourth_pinned_message_is_refused_here() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net".to_owned();
        app.pins.insert(
            chat.clone(),
            ["a".to_owned(), "b".to_owned(), "c".to_owned()]
                .into_iter()
                .collect(),
        );
        app.chat_pins.insert(
            chat.clone(),
            vec![
                pinned_row(&chat, "a"),
                pinned_row(&chat, "b"),
                pinned_row(&chat, "c"),
            ],
        );
        app.apply(
            Action::SetMessagePinned {
                chat: chat.clone(),
                message: "d".into(),
                pinned: true,
                duration: crate::model::MessagePinDuration::Days7,
            },
            &ctx,
        );
        assert!(
            !matches!(commands.try_recv(), Ok(Command::SetMessagePinned { .. })),
            "a fourth pin never reaches the phone"
        );
        assert!(
            app.toasts
                .iter()
                .any(|toast| toast.message.contains("three pinned messages")),
            "and the window says why"
        );
        app.apply(
            Action::SetMessagePinned {
                chat,
                message: "a".into(),
                pinned: false,
                duration: crate::model::MessagePinDuration::Days7,
            },
            &ctx,
        );
        assert!(
            matches!(
                commands.try_recv(),
                Ok(Command::SetMessagePinned { pinned: false, .. })
            ),
            "unpinning always goes through"
        );
    }

    /// The banner reads a cache, so a delete, a clear or a lock has to take
    /// the pinned words off it: the archive join would no longer return them.
    #[test]
    fn a_deleted_message_takes_its_pin_off_the_banner() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.chat_pins
            .insert(chat.into(), vec![pinned_row(chat, "m1")]);
        app.pins
            .insert(chat.into(), ["m1".to_owned()].into_iter().collect());

        events
            .send(Event::MessageDeleted {
                chat: chat.into(),
                id: "m1".into(),
            })
            .unwrap();
        app.handle_events();

        assert!(app.chat_pins.get(chat).is_some_and(Vec::is_empty));
        assert!(app.pins.get(chat).is_some_and(HashSet::is_empty));
    }

    #[test]
    fn clearing_a_chat_drops_its_pins() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.chat_pins
            .insert(chat.into(), vec![pinned_row(chat, "m1")]);

        events
            .send(Event::ChatCleared {
                chat: chat.into(),
                through: 100,
            })
            .unwrap();
        app.handle_events();

        assert!(!app.chat_pins.contains_key(chat), "the pins went with it");
    }

    #[test]
    fn a_locked_chat_keeps_none_of_its_pins_on_screen() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.chat_pins
            .insert(chat.into(), vec![pinned_row(chat, "m1")]);
        app.pins
            .insert(chat.into(), ["m1".to_owned()].into_iter().collect());

        let mut locked = Chat::new(chat.into(), "Fixture".into());
        locked.locked = true;
        events.send(Event::ChatUpdated(Box::new(locked))).unwrap();
        app.handle_events();

        assert!(!app.chat_pins.contains_key(chat));
        assert!(!app.pins.contains_key(chat));
    }

    #[test]
    fn an_expired_pin_does_not_count_toward_the_cap() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "1@s.whatsapp.net".to_owned();
        let mut rows = vec![
            pinned_row(&chat, "a"),
            pinned_row(&chat, "b"),
            pinned_row(&chat, "c"),
        ];
        for row in &mut rows {
            row.expires_at = 1;
        }
        app.pins.insert(
            chat.clone(),
            ["a".to_owned(), "b".to_owned(), "c".to_owned()]
                .into_iter()
                .collect(),
        );
        app.chat_pins.insert(chat.clone(), rows);
        app.apply(
            Action::SetMessagePinned {
                chat,
                message: "d".into(),
                pinned: true,
                duration: crate::model::MessagePinDuration::Days7,
            },
            &egui::Context::default(),
        );
        assert!(
            matches!(
                commands.try_recv(),
                Ok(Command::SetMessagePinned { pinned: true, .. })
            ),
            "expired rows leave a free slot"
        );
    }

    #[test]
    fn a_revoked_message_leaves_the_pin_banner() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.chat_pins
            .insert(chat.into(), vec![pinned_row(chat, "m1")]);
        app.pins
            .insert(chat.into(), ["m1".to_owned()].into_iter().collect());
        let mut message = message(chat, "m1", 1);
        message.content = Content::Revoked;
        events
            .send(Event::MessageUpdated(Box::new(message)))
            .unwrap();
        app.handle_events();
        assert!(app.chat_pins.get(chat).is_some_and(Vec::is_empty));
        assert!(app.pins.get(chat).is_some_and(HashSet::is_empty));
    }

    #[test]
    fn locking_a_chat_drops_its_starred_rows() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));
        app.starred.push(crate::archive::Starred {
            message: message(chat, "m1", 1),
            starred_at: 1,
        });
        let mut locked = Chat::new(chat.into(), "Fixture".into());
        locked.locked = true;
        events.send(Event::ChatUpdated(Box::new(locked))).unwrap();
        app.handle_events();
        assert!(app.starred.is_empty());
    }

    #[test]
    fn an_edit_replaces_the_open_starred_row() {
        let mut app = app();
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.starred.push(crate::archive::Starred {
            message: message(chat, "m1", 1),
            starred_at: 1,
        });
        let mut edited = message(chat, "m1", 1);
        edited.content = Content::text("edited");
        events
            .send(Event::MessageUpdated(Box::new(edited)))
            .unwrap();
        app.handle_events();
        assert_eq!(app.starred[0].message.content, Content::text("edited"));
    }

    #[test]
    fn a_worker_pin_refusal_uses_the_reader_language() {
        for (message, translated_part) in [
            ("This chat already has three pinned messages", "fijados"),
            ("Could not save the pin change", "guardar"),
            (
                "Pin request timed out; confirm its state on your phone before retrying",
                "teléfono",
            ),
            (
                "Could not confirm the pin request; check your phone before retrying",
                "teléfono",
            ),
        ] {
            let mut app = app();
            let (backend, events) = Backend::detached();
            app.backend = backend;
            app.locale = crate::i18n::Locale::Spanish;
            events.send(Event::Error(message.into())).unwrap();
            app.handle_events();
            assert!(
                app.toasts.iter().any(|toast| {
                    toast.message.contains(translated_part) && toast.message != message
                }),
                "the refusal and phone confirmation guidance are translated"
            );
        }
    }

    /// A pin that expires while the chat is closed leaves the line when it is
    /// opened again, so the read is asked for on every open.
    #[test]
    fn opening_a_chat_reads_its_pins_again() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Fixture".into()));

        app.open_chat(chat.into());

        let mut asked = false;
        while let Ok(command) = commands.try_recv() {
            if matches!(command, Command::LoadPins(id) if id == chat) {
                asked = true;
            }
        }
        assert!(asked, "the pins are read when the chat opens");
    }

    /// One pinned row, as the archive hands it over.
    fn pinned_row(chat: &str, id: &str) -> crate::archive::Pinned {
        crate::archive::Pinned {
            chat: chat.to_owned(),
            id: id.to_owned(),
            pinned_at: 1,
            expires_at: i64::MAX,
            text: format!("message {id}"),
            from_me: false,
            sent_at: 1,
            pinner: crate::archive::Pinner {
                by_me: true,
                sender: String::new(),
            },
        }
    }

    #[test]
    fn whatsapp_plus_raises_the_pin_limit() {
        let mut app = app();
        let (backend, _commands) = Backend::recording();
        app.backend = backend;
        for index in 0..4 {
            let mut chat = Chat::new(format!("{index}@s.whatsapp.net"), format!("Chat {index}"));
            chat.pinned = index < 3;
            app.chats.push(chat);
        }
        app.pin_limit = crate::backend::PLUS_PINNED_CHATS;
        app.apply(
            Action::SetPinned("3@s.whatsapp.net".into(), true),
            &egui::Context::default(),
        );
        assert!(app.chat("3@s.whatsapp.net").unwrap().pinned);
    }

    #[test]
    fn presence_follows_focus_and_the_hidden_window() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let mut reported = || {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter_map(|command| match command {
                    Command::SetOnline(online) => Some(online),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        app.window_focused = true;
        app.report_presence();
        app.report_presence();
        assert_eq!(reported(), [true]);
        app.window_gone();
        app.report_presence();
        assert_eq!(reported(), [false]);
        // Focus left over from a window callback does not count while hidden.
        app.window_focused = true;
        app.report_presence();
        assert!(reported().is_empty());
    }

    #[test]
    fn a_deleted_chat_leaves_only_after_the_phone_confirmed_it() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        let other = "friend@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Peer".into()));
        app.chats.push(Chat::new(other.into(), "Friend".into()));
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "m1", 100)], false);
        app.drafts.insert(chat.into(), "half-written".into());
        app.open_chat = Some(chat.into());
        app.settings.last_chat = Some(chat.into());
        app.unread_kept.insert(chat.into());
        app.scroll_chat_into_view = Some(chat.into());
        app.search_hits.push(message(chat, "m1", 100));
        app.search_hits.push(message(other, "m2", 100));
        app.dialog = Some(Dialog::ChatInfo(chat.into()));

        let ctx = egui::Context::default();
        app.apply(Action::DeleteChat(chat.into()), &ctx);

        // Nothing changes here until the phone has deleted the chat too.
        assert!(app.chat(chat).is_some());
        assert!(app.drafts.contains_key(chat));
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, Command::DeleteChat(id) if id == chat))
        );

        let (backend, events) = Backend::detached();
        app.backend = backend;
        events
            .send(Event::ChatRemoved { chat: chat.into() })
            .unwrap();
        app.handle_events();

        assert!(app.chat(chat).is_none());
        assert!(!app.conversations.contains_key(chat));
        // The draft goes with the chat: closing would have kept it, but there
        // is nothing left to send it to.
        assert!(!app.drafts.contains_key(chat));
        assert_eq!(app.open_chat, None);
        // A restart must not try to reopen a chat that is gone.
        assert_eq!(app.settings.last_chat, None);
        // Nothing may keep pointing at a chat that is gone.
        assert!(!app.unread_kept.contains(chat));
        assert_eq!(app.scroll_chat_into_view, None);
        assert!(app.search_hits.iter().all(|hit| hit.chat != chat));
        assert!(app.dialog.is_none());
        // Neighbouring chats and their search hits stay.
        assert!(app.chat(other).is_some());
        assert_eq!(app.search_hits.len(), 1);
    }

    /// A pending delete-message question belongs to one chat. When that chat
    /// goes away the message is gone with it, so the question must not stay
    /// open over another chat.
    #[test]
    fn a_removed_chat_closes_its_pending_message_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let chat = "peer@s.whatsapp.net";
        let other = "other@s.whatsapp.net";
        for id in [chat, other] {
            app.chats.push(Chat::new(id.into(), "Peer".into()));
        }
        app.dialog = Some(Dialog::ConfirmDeleteMessage {
            chat: other.into(),
            message: "m1".into(),
            for_everyone: true,
        });

        events
            .send(Event::ChatRemoved { chat: chat.into() })
            .unwrap();
        app.handle_events();
        // Another chat's question stays open.
        assert!(app.dialog.is_some());

        events
            .send(Event::ChatRemoved { chat: other.into() })
            .unwrap();
        app.handle_events();
        assert_eq!(app.dialog, None);
    }

    #[test]
    fn a_cleared_chat_keeps_its_row_until_the_phone_confirmed_it() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        let other = "friend@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Peer".into()));
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "m1", 100)], false);
        app.drafts.insert(chat.into(), "half-written".into());
        app.open_chat = Some(chat.into());
        app.search_hits.push(message(chat, "m1", 100));
        app.search_hits.push(message(other, "m2", 100));

        let ctx = egui::Context::default();
        app.apply(Action::ClearChat(chat.into()), &ctx);

        // Nothing changes here until the phone has cleared the chat too.
        assert!(app.chat(chat).is_some());
        assert_eq!(
            app.conversations.get(chat).map(|open| open.messages.len()),
            Some(1)
        );
        assert!(app.drafts.contains_key(chat));
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok())
                .any(|command| matches!(command, Command::ClearChat(id) if id == chat))
        );

        let (backend, events) = Backend::detached();
        app.backend = backend;
        events
            .send(Event::ChatCleared {
                chat: chat.into(),
                through: 100,
            })
            .unwrap();
        app.handle_events();

        // The chat stays open with nothing left in it, and its draft goes.
        assert!(app.chat(chat).is_some());
        assert_eq!(
            app.conversations.get(chat).map(|open| open.messages.len()),
            Some(0)
        );
        assert!(!app.drafts.contains_key(chat));
        assert_eq!(app.open_chat, Some(chat.into()));
        // Only the cleared chat loses its search hits.
        assert!(app.search_hits.iter().all(|hit| hit.chat != chat));
        assert_eq!(app.search_hits.len(), 1);
    }

    #[test]
    fn clearing_a_chat_discards_its_open_text_draft_and_mentions() {
        let mut app = app();
        let chat = "peer@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.composer = "Synthetic draft @Mira".into();
        app.composer_mentions.push(ComposerMention {
            id: "member@s.whatsapp.net".into(),
            name: "Mira".into(),
        });
        let draft = app.composer.clone();
        app.drafts.insert(chat.into(), draft);
        app.handle_chat_cleared(chat, 100, true);
        assert!(app.composer.is_empty());
        assert!(app.composer_mentions.is_empty());
        assert!(!app.drafts.contains_key(chat));
    }

    #[test]
    fn clearing_a_chat_preserves_a_surviving_edit_and_other_account_composer() {
        let chat = "peer@s.whatsapp.net";
        for live in [true, false] {
            let mut app = app();
            app.open_chat = Some(chat.into());
            app.composer = "Synthetic edited text".into();
            app.editing = Some("newer".into());
            app.conversations
                .entry(chat.into())
                .or_default()
                .merge(vec![message(chat, "newer", 200)], false);
            app.handle_chat_cleared(chat, 100, live);
            assert_eq!(app.composer, "Synthetic edited text");
            assert_eq!(app.editing.as_deref(), Some("newer"));
        }
        let mut app = app();
        app.open_chat = Some(chat.into());
        app.composer = "Other account draft".into();
        app.handle_chat_cleared(chat, 100, false);
        assert_eq!(app.composer, "Other account draft");
        app.handle_chat_cleared("another@s.whatsapp.net", 100, true);
        assert_eq!(app.composer, "Other account draft");
    }

    #[test]
    fn a_chat_deleted_on_the_phone_disappears_here_too() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let chat = "peer@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Peer".into()));
        app.open_chat = Some(chat.into());

        events
            .send(Event::ChatRemoved { chat: chat.into() })
            .unwrap();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.background_frame(ui.ctx())
        });
        output.textures_delta.clear();

        assert!(app.chat(chat).is_none());
        assert_eq!(app.open_chat, None);
    }

    #[test]
    fn a_chat_cleared_on_the_phone_keeps_the_chat_but_drops_its_messages() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, events) =
            App::headless(AppDirs::under(directory.path()), Settings::default());
        let chat = "peer@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Peer".into()));
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "m1", 100)], false);
        app.open_chat = Some(chat.into());
        app.editing = Some("m1".into());
        app.composer = "edited text".into();
        app.reply_to = Some("m1".into());
        app.reaction_target = Some((chat.into(), "m1".into()));
        app.search_hits.push(message(chat, "m1", 100));

        events
            .send(Event::ChatCleared {
                chat: chat.into(),
                through: 100,
            })
            .unwrap();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            app.background_frame(ui.ctx())
        });
        output.textures_delta.clear();

        assert!(app.chat(chat).is_some());
        let conversation = &app.conversations[chat];
        assert!(conversation.messages.is_empty());
        // Nothing older remains, locally or on the phone, so neither is asked.
        assert!(conversation.complete && conversation.phone_exhausted);
        // Nothing may point at a message that was just removed.
        assert_eq!(app.editing, None);
        assert!(app.composer.is_empty());
        assert_eq!(app.reply_to, None);
        assert_eq!(app.reaction_target, None);
        assert!(app.search_hits.is_empty());
        // The chat itself stays open.
        assert_eq!(app.open_chat.as_deref(), Some(chat));
    }

    #[test]
    fn read_receipt_preference_applies_to_both_reading_and_voice_playback() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "voice", 100)], false);
        app.account_mut().settings.send_read_receipts = false;
        app.mark_read(chat);
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkRead {
                receipts: false,
                ..
            }
        ));
        app.tell_played("voice".into());
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkPlayed {
                receipts: false,
                ..
            }
        ));
        app.account_mut().settings.send_read_receipts = true;
        app.played_told.clear();
        app.tell_played("voice".into());
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::MarkPlayed { receipts: true, .. }
        ));
    }

    #[test]
    fn a_clicked_notification_leaves_a_locked_chat_shut() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        let mut locked = Chat::new(chat.into(), "Ada".into());
        locked.locked = true;
        app.chats = vec![locked];
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![message(chat, "secret", 1)], false);
        app.notification_opens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(crate::notify::NotificationTarget {
                account: crate::model::AccountId::first(),
                chat: chat.into(),
                message: "secret".into(),
            });

        app.handle_notification_opens();
        app.apply_actions(&ctx);

        assert_eq!(app.open_chat, None);
        assert_eq!(app.scroll_anchor, None);
    }

    #[test]
    fn a_clicked_notification_opens_the_message_it_announced() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats = vec![Chat::new(chat.into(), "Ada".into())];
        app.conversations.entry(chat.into()).or_default().merge(
            vec![message(chat, "first", 1), message(chat, "second", 2)],
            false,
        );
        app.notification_opens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(crate::notify::NotificationTarget {
                account: crate::model::AccountId::first(),
                chat: chat.into(),
                message: "second".into(),
            });

        app.handle_notification_opens();
        app.apply_actions(&ctx);

        assert_eq!(
            app.open_chat.as_deref(),
            Some(chat),
            "the click opens its chat"
        );
        assert_eq!(
            app.scroll_anchor.as_deref(),
            Some("second"),
            "and brings the announced message into view"
        );
    }

    /// Builds a synthetic voice message fixture without reading the user archive.
    fn voice(chat: &str, id: &str, timestamp: i64, path: Option<&str>) -> Message {
        let mut row = message(chat, id, timestamp);
        row.content = Content::Audio {
            media: Media {
                mime: "audio/ogg".into(),
                size: 1,
                width: None,
                height: None,
                album: None,
                path: path.map(PathBuf::from),
                state: MediaState::Idle,
            },
            seconds: Some(3),
            voice_note: true,
            waveform: Vec::new(),
        };
        row
    }

    #[test]
    fn a_finished_voice_message_carries_on_with_the_next_unplayed_one() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats = vec![Chat::new(chat.into(), "Ada".into())];
        app.open_chat = Some(chat.into());
        app.settings.pause_other_media = true;
        app.conversations.entry(chat.into()).or_default().merge(
            vec![
                voice(chat, "first", 1, Some("first.ogg")),
                voice(chat, "second", 2, None),
            ],
            false,
        );
        app.played_told.insert("first".into());
        app.voice_chat = Some(chat.into());

        app.continue_voice("first");
        assert_eq!(
            app.voice_wanted
                .as_ref()
                .map(|(chat, id, _)| (chat.as_str(), id.as_str())),
            Some((chat, "second")),
            "a clip that is not downloaded yet is fetched first"
        );
        assert!(
            app.wants_quiet(),
            "music stays paused while the next clip downloads"
        );
        app.apply_actions(&ctx);
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::Download { message, .. } if message == "second"
        ));

        app.handle_media(chat, "second", None, Ok(PathBuf::from("second.ogg")));
        assert!(
            app.actions.iter().any(|action| matches!(
                action,
                Action::PlayVoice { message, .. } if message == "second"
            )),
            "the fetched clip plays when it lands"
        );
        assert!(app.voice_wanted.is_none());
    }

    #[test]
    fn a_voice_run_goes_through_consecutive_unheard_voice_messages() {
        let mut app = app();
        let chat = "1@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        let mut own = voice(chat, "own", 2, None);
        own.from_me = true;
        // Played on the phone.
        let mut heard = voice(chat, "heard", 3, None);
        heard.status = Delivery::Played;
        let mut song = voice(chat, "song", 6, None);
        if let Content::Audio { voice_note, .. } = &mut song.content {
            *voice_note = false;
        }
        app.conversations.entry(chat.into()).or_default().merge(
            vec![
                voice(chat, "played", 1, None),
                own,
                heard,
                voice(chat, "waiting", 4, None),
                message(chat, "text", 5),
                voice(chat, "after text", 6, None),
                song,
                voice(chat, "after song", 7, None),
            ],
            false,
        );
        app.played_told.insert("played".into());

        assert_eq!(
            app.next_voice_after(chat, "played").as_deref(),
            Some("waiting"),
            "own voice messages and those already heard are passed over"
        );
        assert_eq!(
            app.next_voice_after(chat, "waiting"),
            None,
            "a message in between ends the run"
        );
        assert_eq!(
            app.next_voice_after(chat, "after text"),
            None,
            "an audio file ends the run"
        );
        assert_eq!(
            app.next_voice_after(chat, "song"),
            None,
            "an audio file does not start one"
        );
    }

    #[test]
    fn leaving_the_chat_or_playing_a_video_ends_a_voice_run() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        let other = "2@s.whatsapp.net";
        app.chats = vec![
            Chat::new(chat.into(), "Ada".into()),
            Chat::new(other.into(), "Bob".into()),
        ];
        app.conversations.entry(chat.into()).or_default().merge(
            vec![
                voice(chat, "first", 1, Some("first.ogg")),
                voice(chat, "second", 2, Some("second.ogg")),
                voice(chat, "third", 3, None),
            ],
            false,
        );
        app.open_chat = Some(chat.into());
        app.voice_chat = Some(chat.into());
        app.voice_wanted = Some((chat.into(), "third".into(), Instant::now()));

        app.open_chat(other.into());
        app.open_chat(chat.into());
        assert!(app.voice_wanted.is_none(), "a pending clip is dropped");
        app.continue_voice("first");
        assert!(
            app.actions.is_empty(),
            "a clip from before does not carry on"
        );
        app.handle_media(chat, "third", None, Ok(PathBuf::from("third.ogg")));
        assert!(app.actions.is_empty(), "nor does one that lands later");

        app.voice_chat = Some(chat.into());
        app.voice_wanted = Some((chat.into(), "third".into(), Instant::now()));
        app.actions
            .push(Action::PlayVideoWhenDownloaded("video".into()));
        app.apply_actions(&ctx);
        assert!(app.voice_wanted.is_none() && app.voice_chat.is_none());
    }

    #[test]
    fn a_failed_download_ends_the_wait_for_the_next_voice_message() {
        let mut app = app();
        let chat = "1@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.settings.pause_other_media = true;
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![voice(chat, "next", 1, None)], false);
        app.voice_wanted = Some((chat.into(), "next".into(), Instant::now()));
        app.handle_media(chat, "next", None, Err("404".into()));
        assert!(app.voice_wanted.is_none());
        assert!(!app.wants_quiet(), "music resumes");

        app.voice_wanted = Some((
            chat.into(),
            "next".into(),
            Instant::now() - VOICE_FETCH_HOLD,
        ));
        assert!(
            !app.wants_quiet(),
            "a download that takes too long does not keep music paused"
        );
    }

    fn message(chat: &str, id: &str, timestamp: i64) -> Message {
        Message {
            id: id.into(),
            chat: chat.into(),
            sender: chat.into(),
            sender_name: None,
            from_me: false,
            timestamp,
            content: Content::text(id),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            history_order: None,
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    /// A failed send asks the reader to check the connection only when the
    /// link was the cause; other failures get a neutral line.
    #[test]
    fn a_failed_send_names_the_connection_only_when_it_was_the_cause() {
        for (connection, expected) in [
            (
                true,
                "This message could not be sent. Check your connection before sending it again.",
            ),
            (false, "This message could not be sent. Try again later."),
        ] {
            let mut app = app();
            let (backend, events) = Backend::detached();
            app.backend = backend;
            events.send(Event::SendFailed { connection }).unwrap();
            app.handle_events();
            let toasts: Vec<_> = app.toasts.iter().map(|toast| &toast.message).collect();
            assert_eq!(toasts, [expected], "{connection}");
        }
    }

    #[test]
    fn queued_or_sending_rows_cannot_be_removed_by_a_stale_delete_action() {
        for status in [Delivery::Queued, Delivery::Pending] {
            let mut app = app();
            let chat = "fixture@s.whatsapp.net";
            app.conversations.entry(chat.into()).or_default().merge(
                vec![Message {
                    status,
                    from_me: true,
                    ..message(chat, "waiting", 100)
                }],
                false,
            );
            app.apply(
                Action::DeleteForMe {
                    chat: chat.into(),
                    id: "waiting".into(),
                },
                &egui::Context::default(),
            );
            assert!(
                app.conversations[chat].message("waiting").is_some(),
                "an unsent or in-flight message must not silently disappear"
            );
        }
    }

    /// D2: a message waiting for a rate-limit cooldown stays at the bottom,
    /// below whatever arrives meanwhile, and keeps its place once it goes
    /// out at its dispatch time.
    #[test]
    fn waiting_messages_stay_last_and_follow_their_dispatch_time() {
        let mut app = app();
        let chat = "fixture@s.whatsapp.net";
        let waiting = Message {
            from_me: true,
            status: Delivery::Queued,
            ..message(chat, "waiting", 100)
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![waiting.clone(), message(chat, "theirs", 200)], false);
        let order = |app: &App| {
            app.conversations[chat]
                .messages
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&app), ["theirs", "waiting"]);
        let (backend, events) = Backend::detached();
        app.backend = backend;
        events
            .send(Event::MessageUpdated(Box::new(Message {
                status: Delivery::Pending,
                timestamp: 300,
                ..waiting.clone()
            })))
            .unwrap();
        app.handle_events();
        assert_eq!(order(&app), ["theirs", "waiting"]);
        // A row that stops waiting without a new time returns to its time.
        events
            .send(Event::MessageUpdated(Box::new(Message {
                status: Delivery::Failed,
                ..waiting
            })))
            .unwrap();
        app.handle_events();
        assert_eq!(order(&app), ["waiting", "theirs"]);
    }

    /// A burst sent within one second starts waiting, and later goes out,
    /// one row at a time in queue order. It keeps its order both times,
    /// and goes out below a reply that came in the same second.
    #[test]
    fn rows_that_change_state_within_one_second_keep_their_order() {
        let mut app = app();
        let chat = "fixture@s.whatsapp.net";
        let ids = ["first", "second", "third"];
        let ours = |id: &str, status| Message {
            from_me: true,
            status,
            ..message(chat, id, 100)
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(ids.map(|id| ours(id, Delivery::Pending)).into(), false);
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let order = |app: &App| {
            app.conversations[chat]
                .messages
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>()
        };
        for id in ids {
            let row = ours(id, Delivery::Queued);
            events.send(Event::MessageUpdated(Box::new(row))).unwrap();
        }
        app.handle_events();
        assert_eq!(order(&app), ids);
        events
            .send(Event::Messages {
                chat: chat.into(),
                messages: vec![message(chat, "theirs", 200)],
                older: false,
                complete: false,
            })
            .unwrap();
        for id in ids {
            let row = Message {
                timestamp: 200,
                ..ours(id, Delivery::Pending)
            };
            events.send(Event::MessageUpdated(Box::new(row))).unwrap();
        }
        app.handle_events();
        assert_eq!(order(&app), ["theirs", "first", "second", "third"]);
    }

    /// A send refused again goes back to the head of the waiting rows, also
    /// when it went out in the same second it first waited.
    #[test]
    fn a_send_refused_again_waits_ahead_of_the_rows_behind_it() {
        for dispatched_at in [100, 300] {
            let mut app = app();
            let chat = "fixture@s.whatsapp.net";
            let ids = ["first", "second", "third"];
            let ours = |id: &str, status, timestamp| Message {
                from_me: true,
                status,
                ..message(chat, id, timestamp)
            };
            app.conversations
                .entry(chat.into())
                .or_default()
                .merge(ids.map(|id| ours(id, Delivery::Queued, 100)).into(), false);
            let (backend, events) = Backend::detached();
            app.backend = backend;
            let order = |app: &App| {
                app.conversations[chat]
                    .messages
                    .iter()
                    .map(|row| row.id.clone())
                    .collect::<Vec<_>>()
            };
            let first = ours("first", Delivery::Pending, dispatched_at);
            events.send(Event::MessageUpdated(Box::new(first))).unwrap();
            app.handle_events();
            assert_eq!(order(&app), ids);
            let first = ours("first", Delivery::Queued, 100);
            events
                .send(Event::MessageRequeued(Box::new(first)))
                .unwrap();
            app.handle_events();
            assert_eq!(order(&app), ids, "dispatched at {dispatched_at}");
        }
    }

    /// Checks that incoming replacements retain local downloaded media paths and transfer state.
    #[test]
    fn merge_keeps_a_downloaded_medias_path_and_state() {
        let mut conversation = Conversation::default();
        let chat = "fixture@s.whatsapp.net";
        let image = |path: Option<PathBuf>, state: MediaState| Message {
            content: Content::Image {
                motion: None,
                caption: None,
                media: Media {
                    mime: "image/jpeg".into(),
                    size: 100,
                    width: None,
                    height: None,
                    album: None,
                    path,
                    state,
                },
            },
            ..message(chat, "picture", 1)
        };
        conversation.merge(vec![image(None, MediaState::Idle)], false);
        // A download lands, then is marked failed after the fact.
        let downloaded = PathBuf::from("/tmp/picture.jpg");
        if let Some(media) = conversation
            .message_mut("picture")
            .expect("loaded")
            .content
            .media_mut()
        {
            media.path = Some(downloaded.clone());
            media.state = MediaState::Failed("gone".into());
        }
        // A reload delivers the same message freshly classified, without the
        // local path or the runtime state.
        conversation.merge(vec![image(None, MediaState::Idle)], false);
        conversation.merge(vec![image(None, MediaState::Idle)], true);
        let media = conversation
            .message("picture")
            .and_then(|message| message.content.media().cloned())
            .expect("still present");
        assert_eq!(media.path, Some(downloaded));
        assert_eq!(media.state, MediaState::Failed("gone".into()));
        // A copy with its own path replaces the in-memory one.
        let relocated = PathBuf::from("/elsewhere/picture.jpg");
        conversation.merge(
            vec![image(Some(relocated.clone()), MediaState::Idle)],
            false,
        );
        let media = conversation
            .message("picture")
            .and_then(|message| message.content.media().cloned())
            .expect("still present");
        assert_eq!(media.path, Some(relocated));
        assert_eq!(media.state, MediaState::Idle);
    }

    /// Checks that clicking a pending video note resumes playback only after its download arrives.
    #[test]
    fn a_video_note_clicked_before_download_plays_once_it_arrives() {
        let mut app = app();
        app.video.silence();
        let chat = "fixture@s.whatsapp.net";
        let mut clip = message(chat, "clip", 1);
        clip.content = Content::Video {
            caption: None,
            media: Media {
                mime: "video/mp4".into(),
                size: 100,
                width: None,
                height: None,
                album: None,
                path: None,
                state: MediaState::Idle,
            },
            seconds: Some(3),
            gif: false,
            note: true,
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![clip], false);
        app.open_chat = Some(chat.into());
        let ctx = egui::Context::default();
        app.apply(Action::PlayVideoWhenDownloaded("clip".into()), &ctx);
        let (backend, events) = Backend::detached();
        app.backend = backend;
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/video/sample.mp4"
        ));
        events
            .send(Event::Media {
                card: None,
                chat: chat.into(),
                message: "clip".into(),
                result: Ok(path.clone()),
            })
            .unwrap();
        app.handle_events();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.apply_actions(&ctx);
        assert_eq!(app.video.message(), Some("clip"));
        // A round video message is played like a voice message.
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::MarkPlayed { message, .. }) if message == "clip"
        ));
        // Leaving the chat stops it.
        app.open_chat = None;
        app.tick_video(&ctx);
        assert!(app.video.message().is_none());
    }

    /// Choosing a font saves the choice and installs it at once.
    #[test]
    fn choosing_a_font_saves_and_applies_it() {
        use crate::settings::FontChoice;
        let mut app = app();
        let ctx = egui::Context::default();
        app.attach(&ctx);
        assert_eq!(app.settings.font, FontChoice::System);
        app.apply(Action::SetFont(FontChoice::Inter), &ctx);
        assert_eq!(app.settings.font, FontChoice::Inter);
        assert!(crate::theme::inter_chosen());
        app.apply(Action::SetFont(FontChoice::System), &ctx);
        assert_eq!(app.settings.font, FontChoice::System);
        assert!(!crate::theme::inter_chosen());
    }

    /// A video opens over the window at a size worth the room, goes back to
    /// its message at the bubble's, and does not outlive the chat.
    #[test]
    fn a_video_covers_the_window_and_goes_back_to_its_message() {
        let mut app = app();
        app.video.silence();
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        let ctx = egui::Context::default();
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/video/sample.mp4"
        ));
        let bubble = app.video.side();
        let expand = || Action::ExpandVideo {
            message: "clip".into(),
            path: path.clone(),
        };

        // Not loaded yet: it starts, already at the larger size.
        app.apply(expand(), &ctx);
        assert_eq!(app.video.message(), Some("clip"));
        assert!(app.video_expanded && app.video.is_active());
        assert!(app.video.side() > bubble);

        app.apply(Action::CollapseVideo, &ctx);
        assert!(!app.video_expanded);
        assert_eq!(app.video.side(), bubble);
        assert_eq!(app.video.message(), Some("clip"), "it stays loaded");

        // Paused in its bubble, expanding it plays it.
        app.apply(
            Action::PlayVideo {
                message: "clip".into(),
                path: path.clone(),
            },
            &ctx,
        );
        assert!(!app.video.is_active());
        app.apply(expand(), &ctx);
        assert!(app.video_expanded && app.video.is_active());

        // Leaving the chat stops the video and takes the view down with it.
        app.open_chat = None;
        app.tick_video(&ctx);
        assert!(app.video.message().is_none());
        assert!(!app.video_expanded);
        assert_eq!(app.video.side(), bubble);
    }

    /// Checks that repeated media clicks do not duplicate an active download command.
    #[test]
    fn repeated_download_clicks_do_not_queue_more_requests() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        let mut attachment = message(chat, "picture", 1);
        attachment.content = Content::Image {
            motion: None,
            caption: None,
            media: Media {
                mime: "image/jpeg".into(),
                size: 100,
                width: None,
                height: None,
                album: None,
                path: None,
                state: MediaState::Idle,
            },
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![attachment], false);
        let ctx = egui::Context::default();
        for _ in 0..2 {
            app.apply(
                Action::Download {
                    card: None,
                    chat: chat.into(),
                    message: "picture".into(),
                },
                &ctx,
            );
        }
        assert!(matches!(commands.try_recv(), Ok(Command::Download { .. })));
        assert!(commands.try_recv().is_err());
        let (backend, events) = Backend::detached();
        app.backend = backend;
        events
            .send(Event::Media {
                card: None,
                chat: chat.into(),
                message: "picture".into(),
                result: Err("Download timed out".into()),
            })
            .unwrap();
        app.handle_events();
        assert!(matches!(
            app.media_of(chat, "picture").map(|media| &media.state),
            Some(MediaState::Failed(_))
        ));
        assert!(app.toasts.is_empty());
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.apply(
            Action::Download {
                card: None,
                chat: chat.into(),
                message: "picture".into(),
            },
            &ctx,
        );
        assert!(matches!(commands.try_recv(), Ok(Command::Download { .. })));
    }

    /// Checks that carousel download results update only the requested card.
    #[test]
    fn carousel_downloads_track_each_card_separately() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        let card = || crate::model::InteractiveCard {
            image: Some(Media {
                mime: "image/jpeg".into(),
                size: 100,
                width: None,
                height: None,
                album: None,
                path: None,
                state: MediaState::Idle,
            }),
            ..Default::default()
        };
        let mut carousel = message(chat, "carousel", 1);
        carousel.content = Content::Interactive {
            text: String::new(),
            card: Some(Box::new(crate::model::InteractiveCard {
                carousel: vec![card(), card()],
                ..Default::default()
            })),
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![carousel.clone()], false);
        let images = |app: &App| -> Vec<Media> {
            match &app.conversations[chat].message("carousel").unwrap().content {
                Content::Interactive {
                    card: Some(card), ..
                } => card
                    .carousel
                    .iter()
                    .map(|card| card.image.clone().unwrap())
                    .collect(),
                _ => Vec::new(),
            }
        };
        let ctx = egui::Context::default();
        app.apply(
            Action::Download {
                card: Some(1),
                chat: chat.into(),
                message: "carousel".into(),
            },
            &ctx,
        );
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::Download { card: Some(1), .. })
        ));
        let states = |app: &App| {
            images(app)
                .into_iter()
                .map(|media| media.state)
                .collect::<Vec<_>>()
        };
        assert_eq!(states(&app), [MediaState::Idle, MediaState::Downloading]);
        // A worker update carries no download state; the card keeps its own.
        let (backend, events) = Backend::detached();
        app.backend = backend;
        events
            .send(Event::MessageUpdated(Box::new(carousel)))
            .unwrap();
        app.handle_events();
        assert_eq!(states(&app), [MediaState::Idle, MediaState::Downloading]);
        let path = PathBuf::from("/cache/zapfast/media/carousel-card-1.jpg");
        events
            .send(Event::Media {
                card: Some(1),
                chat: chat.into(),
                message: "carousel".into(),
                result: Ok(path.clone()),
            })
            .unwrap();
        app.handle_events();
        let images = images(&app);
        assert_eq!(images[0].path, None);
        assert_eq!(images[1].path, Some(path));
        assert_eq!(images[1].state, MediaState::Idle);
    }

    /// Builds a synthetic send refusal without accessing the user archive.
    fn refused(chat: &str, quoting: Option<&str>, unsent: Unsent, reason: Refusal) -> Event {
        Event::SendRefused {
            chat: chat.into(),
            quoting: quoting.map(crate::model::ReplyTarget::from),
            unsent,
            reason,
            mentions: Vec::new(),
        }
    }

    fn error_toasts(app: &App) -> Vec<String> {
        app.toasts
            .iter()
            .filter(|toast| toast.kind == ToastKind::Error)
            .map(|toast| toast.message.clone())
            .collect()
    }

    /// Checks that refusing a text send restores both draft text and the original quote target.
    #[test]
    fn sending_a_message_scrolls_the_chat_list_to_the_top() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, _commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        let sends = [
            Action::SendText {
                chat: chat.into(),
                text: "Text fixture".into(),
                quoting: None,
            },
            Action::SendSticker(PathBuf::from("sticker.webp")),
            Action::ShareStickerPack(PathBuf::from("sticker-pack")),
            Action::Forward {
                from_chat: chat.into(),
                messages: vec!["fixture-message".into()],
                to_chat: "other@s.whatsapp.net".into(),
            },
            Action::CreatePoll {
                chat: chat.into(),
                draft: crate::model::PollDraft {
                    question: "Question fixture".into(),
                    options: vec!["One".into(), "Two".into()],
                    multiple: false,
                },
            },
        ];
        for keep_position in [false, true] {
            app.settings.keep_chat_list_position = keep_position;
            for send in &sends {
                let name = format!("{send:?}");
                app.poll_creating = false;
                app.scroll_chats_to_top = false;
                app.apply(send.clone(), &ctx);
                assert_eq!(
                    app.scroll_chats_to_top, !keep_position,
                    "{name} respects the chat list preference"
                );
            }
        }
    }

    #[test]
    fn sending_under_the_favorites_chip_keeps_the_list_in_place() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, _commands) = Backend::recording();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        // Favorites keep the phone's order, so the chat does not move up.
        app.chat_filter = ChatFilter::Favorites;
        app.apply(
            Action::SendText {
                chat: chat.into(),
                text: "Text fixture".into(),
                quoting: None,
            },
            &egui::Context::default(),
        );
        assert!(!app.scroll_chats_to_top);
    }

    #[test]
    fn sending_a_spot_closes_the_dialog_and_a_refusal_brings_it_back() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.dialog = Some(Dialog::SendLocation(chat.into()));
        app.location_draft = "-23.5505, -46.6333".into();
        app.apply(
            Action::SendLocation {
                chat: chat.into(),
                latitude: -23.5505,
                longitude: -46.6333,
                quoting: None,
            },
            &egui::Context::default(),
        );
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(sent.iter().any(|command| matches!(command,
            Command::SendLocation { latitude, longitude, .. }
                if *latitude == -23.5505 && *longitude == -46.6333)));
        assert_eq!(app.dialog, None, "the dialog closes over the spot");
        assert!(app.location_draft.is_empty(), "and forgets it");

        // A refusal sends the spot back to the dialog, which is the only place
        // it can be held: the composer takes text.
        events
            .send(refused(
                chat,
                None,
                Unsent::Location {
                    latitude: -23.5505,
                    longitude: -46.6333,
                },
                Refusal::Offline,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(app.dialog, Some(Dialog::SendLocation(chat.into())));
        assert_eq!(app.location_draft, "-23.55050, -46.63330");
    }

    #[test]
    fn a_refused_text_reply_returns_to_the_composer_with_its_reply() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.reply_to = Some("original".into());
        app.apply(
            Action::SendText {
                chat: chat.into(),
                text: "Reply fixture".into(),
                quoting: app.reply_to.clone(),
            },
            &egui::Context::default(),
        );
        assert!(app.reply_to.is_none());
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        assert!(sent.iter().any(|command| matches!(command,
            Command::SendText { quoting: Some(id), .. } if id.id == "original")));
        events
            .send(refused(
                chat,
                Some("original"),
                Unsent::Text("Reply fixture".into()),
                Refusal::QuoteUnavailable,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "Reply fixture");
        assert_eq!(
            app.reply_to.as_deref(),
            Some("original"),
            "the reply is re-armed"
        );
        assert!(app.focus_composer);
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
                Command::SaveDraft { chat: saved, text } if saved == chat && text == "Reply fixture")),
            "the returned text is a draft again"
        );
        assert!(error_toasts(&app)[0].contains("replying to"));
        // Text typed since is not overwritten by a later refusal.
        app.composer = "Newer draft".into();
        events
            .send(refused(
                chat,
                None,
                Unsent::Text("Older text".into()),
                Refusal::Offline,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "Newer draft");
        assert!(error_toasts(&app)[1].contains("not connected"));
    }

    /// Checks that sending a quoted reply preserves the loaded transcript rather than dropping older rows.
    #[test]
    fn sending_a_reply_keeps_older_messages_in_view() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.at_bottom = false;
        app.scroll_to_bottom = false;

        app.apply(
            Action::SendText {
                chat: chat.into(),
                text: "Reply fixture".into(),
                quoting: Some("older-message".into()),
            },
            &egui::Context::default(),
        );

        assert!(!app.scroll_to_bottom, "the older position stays selected");
        assert!(!app.at_bottom, "sending does not pretend the view moved");
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(
                command,
                Command::SendText { quoting: Some(id), .. } if id.id == "older-message"
            ))
        );

        app.at_bottom = true;
        app.scroll_to_bottom = false;
        app.apply(
            Action::SendText {
                chat: chat.into(),
                text: "Latest fixture".into(),
                quoting: None,
            },
            &egui::Context::default(),
        );

        assert!(
            app.scroll_to_bottom,
            "a reader at the newest edge keeps following outgoing messages"
        );
        assert!(app.at_bottom);
    }

    #[test]
    fn a_refused_text_for_another_chat_becomes_its_draft() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        app.open_chat = Some("other@s.whatsapp.net".into());
        app.composer = "Other chat's text".into();
        let chat = "fixture@s.whatsapp.net";
        events
            .send(refused(
                chat,
                Some("original"),
                Unsent::Text("Reply fixture".into()),
                Refusal::QuoteUnavailable,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(app.composer, "Other chat's text");
        assert!(
            app.reply_to.is_none(),
            "no reply is armed in the wrong chat"
        );
        assert_eq!(
            app.drafts.get(chat).map(String::as_str),
            Some("Reply fixture")
        );
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
                Command::SaveDraft { chat: saved, text } if saved == chat && text == "Reply fixture"))
        );
        // A draft the chat already has is kept.
        events
            .send(refused(
                chat,
                None,
                Unsent::Text("Later text".into()),
                Refusal::Offline,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(
            app.drafts.get(chat).map(String::as_str),
            Some("Reply fixture")
        );
    }

    /// Checks that attachment and GIF refusals restore the same quote alongside their unsent media.
    #[test]
    /// Attachment and GIF sends preserve replies and restore their drafts when refused.
    fn attachments_and_gifs_carry_the_reply_and_come_back_when_refused() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "fixture@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.reply_to = Some("original".into());
        app.pending.push(Pending::Picture {
            width: 1,
            height: 1,
            rgba: std::sync::Arc::new(vec![1, 2, 3, 4]),
            texture: None,
        });
        app.pending.push(Pending::File("/fixture/a.pdf".into()));
        app.apply(
            Action::SendPending {
                account: app.account().id.clone(),
                mentions: app.composer_mentions.clone(),
                chat: chat.into(),
                caption: "Caption fixture".into(),
            },
            &ctx,
        );
        let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
        // Only the first attachment, which carries the caption, is the reply.
        assert!(matches!(
            sent.as_slice(),
            [
                Command::SaveDraft { text, .. },
                Command::SendImage { quoting: Some(id), caption: Some(_), .. },
                Command::SendFiles { quoting: None, caption: None, .. },
            ] if id.id == "original"
        ));
        assert!(app.reply_to.is_none());
        assert!(app.pending.is_empty());
        app.reply_to = Some("original".into());
        app.apply(
            Action::SendGif(Gif {
                id: "fixture".into(),
                still: None,
                mp4: "https://example.invalid/fixture.mp4".into(),
                width: 2,
                height: 2,
            }),
            &ctx,
        );
        assert!(app.reply_to.is_none());
        assert!(
            std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
                Command::SendGif { quoting: Some(id), .. } if id.id == "original"))
        );
        // Refused attachments return to the composer with their caption.
        for unsent in [
            Unsent::Image {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
                caption: Some("Caption fixture".into()),
            },
            Unsent::Files {
                paths: vec!["/fixture/a.pdf".into()],
                caption: None,
            },
            Unsent::Gif,
        ] {
            events
                .send(refused(
                    chat,
                    Some("original"),
                    unsent,
                    Refusal::QuoteUnavailable,
                ))
                .unwrap();
        }
        app.handle_events();
        assert!(matches!(
            app.pending.iter().map(Pending::media).collect::<Vec<_>>().as_slice(),
            [Pending::Picture { width: 1, height: 1, .. }, Pending::File(path)]
                if path == std::path::Path::new("/fixture/a.pdf")
        ));
        assert_eq!(app.composer, "Caption fixture");
        assert_eq!(app.reply_to.as_deref(), Some("original"));
        // Repeats of one error share a toast.
        assert_eq!(error_toasts(&app).len(), 1);
    }

    /// Checks that an unsent recording remains scoped to its destination and retains its quote.
    #[test]
    fn a_refused_voice_message_is_sent_again_only_from_its_chat_or_discarded() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _) = App::headless(AppDirs::under(root.path()), Settings::default());
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "fixture@s.whatsapp.net";
        let other = "other@s.whatsapp.net";
        let clip = vec![0.25; crate::voice::RATE as usize * 2];
        app.open_chat = Some(chat.into());
        events
            .send(refused(
                chat,
                Some("original"),
                Unsent::Voice(clip.clone()),
                Refusal::QuoteUnavailable,
            ))
            .unwrap();
        app.handle_events();
        assert_eq!(app.unsent_voice, Some((chat.to_owned(), clip.clone())));
        assert_eq!(app.reply_to.as_deref(), Some("original"));
        assert!(app.composer.is_empty(), "the composer stays empty");
        // Another chat's Send neither sends nor drops the clip.
        app.open_chat = Some(other.into());
        app.reply_to = None;
        app.apply(Action::SendRecording, &ctx);
        assert!(commands.try_recv().is_err());
        assert!(app.unsent_voice.is_some());
        // Back in its chat, with the reply cancelled, it goes out unquoted
        // because the user chose so.
        app.open_chat = Some(chat.into());
        app.apply(Action::SendRecording, &ctx);
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::SendVoice { chat: sent, samples, quoting: None })
                if sent == chat && samples == clip
        ));
        assert!(app.unsent_voice.is_none());
        // Refused again: sending with the reply armed quotes it, and an active
        // recording is never preempted by the retained clip.
        events
            .send(refused(
                chat,
                Some("original"),
                Unsent::Voice(clip.clone()),
                Refusal::Offline,
            ))
            .unwrap();
        app.handle_events();
        app.recording = Some(crate::audio::Recorder::rehearsal());
        app.apply(Action::SendRecording, &ctx);
        assert!(app.recording.is_none());
        assert!(app.unsent_voice.is_some(), "the retained clip still waits");
        let _ = std::iter::from_fn(|| commands.try_recv().ok()).count();
        app.reply_to = Some("original".into());
        app.apply(Action::SendRecording, &ctx);
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::SendVoice { quoting: Some(id), .. }) if id.id == "original"
        ));
        // Discarding drops it for good.
        events
            .send(refused(chat, None, Unsent::Voice(clip), Refusal::Offline))
            .unwrap();
        app.handle_events();
        app.apply(Action::DiscardUnsentVoice, &ctx);
        assert!(app.unsent_voice.is_none());
        app.apply(Action::SendRecording, &ctx);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn canceled_or_superseded_starred_reply_cannot_reappear_on_history_arrival() {
        let mut app = app();
        let chat = "fixture@s.whatsapp.net";
        let ctx = egui::Context::default();
        app.open_chat = Some(chat.into());
        app.apply(Action::Reply("older".into()), &ctx);
        assert!(app.reply_when_loaded.is_some());
        app.apply(Action::CancelReply, &ctx);
        app.conversations.insert(
            chat.into(),
            Conversation {
                messages: vec![message(chat, "older", 100), message(chat, "newer", 101)],
                ..Default::default()
            },
        );
        app.settle_pending_reply();
        assert!(app.reply_to.is_none());
        app.reply_when_loaded = Some((chat.into(), "older".into()));
        app.apply(Action::Reply("newer".into()), &ctx);
        app.settle_pending_reply();
        assert_eq!(app.reply_to.as_deref(), Some("newer"));
        assert!(app.reply_when_loaded.is_none());
        app.reply_when_loaded = Some((chat.into(), "older".into()));
        app.apply(Action::Edit("newer".into()), &ctx);
        assert!(app.reply_when_loaded.is_none());
    }

    #[test]
    fn replying_while_editing_starts_a_new_message() {
        let mut app = app();
        app.open_chat = Some("fixture@s.whatsapp.net".into());
        app.editing = Some("edited".into());
        app.composer = "Text being edited".into();
        app.apply(Action::Reply("original".into()), &egui::Context::default());
        assert!(app.editing.is_none());
        assert!(
            app.composer.is_empty(),
            "the edit's text is not sent as a reply"
        );
        assert_eq!(app.reply_to.as_deref(), Some("original"));
        assert!(app.focus_composer);
    }

    /// Private quotes keep their source identity through chat switches and send refusals.
    #[test]
    fn private_reply_opens_the_sender_and_preserves_both_drafts() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        let group = "12345@g.us";
        let sender = "15550002222@s.whatsapp.net";
        app.chats
            .push(Chat::new(group.into(), "Group fixture".into()));
        let mut original = message(group, "original", 100);
        original.sender = sender.into();
        original.sender_name = Some("Sender fixture".into());
        app.conversations.insert(
            group.into(),
            Conversation {
                messages: vec![original],
                requested: true,
                ..Default::default()
            },
        );
        app.open_chat = Some(group.into());
        app.composer = "Group draft".into();
        app.drafts.insert(sender.into(), "Private draft".into());
        app.apply(
            Action::ReplyPrivately {
                chat: group.into(),
                message: "original".into(),
            },
            &ctx,
        );
        assert_eq!(app.open_chat.as_deref(), Some(sender));
        assert_eq!(app.composer, "Private draft");
        assert_eq!(
            app.drafts.get(group).map(String::as_str),
            Some("Group draft")
        );
        let target = app.reply_to.clone().unwrap();
        assert_eq!(target.id, "original");
        assert_eq!(target.chat.as_deref(), Some(group));
        assert!(app.focus_composer);
        assert!(
            !std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(
                command,
                Command::SendText { .. } | Command::SendFiles { .. } | Command::SendVoice { .. }
            ))
        );
        app.apply(
            Action::SendText {
                chat: sender.into(),
                text: "Private reply".into(),
                quoting: Some(target.clone()),
            },
            &ctx,
        );
        assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
            Command::SendText { chat, quoting: Some(quote), .. } if chat == sender && quote == target
        )));
        app.send_refused(
            sender.into(),
            Some(target.clone()),
            Unsent::Text("Private reply".into()),
            Refusal::QuoteUnavailable,
        );
        assert_eq!(app.reply_to, Some(target.clone()));
        app.open_chat(group.into());
        app.send_refused(
            sender.into(),
            Some(target.clone()),
            Unsent::Text("Returned private reply".into()),
            Refusal::Offline,
        );
        assert!(app.reply_to.is_none());
        app.open_chat(sender.into());
        assert_eq!(app.reply_to, Some(target));
    }

    /// Late refusals cannot restore a deleted recipient's private quote, text or voice.
    #[test]
    fn private_reply_refusals_do_not_recreate_deleted_destination_drafts() {
        for (voice, recreate_before_refusal) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut app = app();
            let (backend, mut commands) = Backend::recording();
            app.backend = backend;
            let ctx = egui::Context::default();
            let group = "12345@g.us";
            let recipient = "15550002222@s.whatsapp.net";
            app.chats
                .push(Chat::new(group.into(), "Group fixture".into()));
            let mut original = message(group, "original", 100);
            original.sender = recipient.into();
            app.conversations
                .entry(group.into())
                .or_default()
                .merge(vec![original], false);
            app.open_chat = Some(group.into());
            app.apply(
                Action::ReplyPrivately {
                    chat: group.into(),
                    message: "original".into(),
                },
                &ctx,
            );
            let target = app.reply_to.clone().unwrap();
            let unsent = if voice {
                let samples = vec![0.0; 10];
                app.unsent_voice = Some((recipient.into(), samples.clone()));
                app.apply(Action::SendRecording, &ctx);
                Unsent::Voice(samples)
            } else {
                app.apply(
                    Action::SendText {
                        chat: recipient.into(),
                        text: "Private draft".into(),
                        quoting: Some(target.clone()),
                    },
                    &ctx,
                );
                Unsent::Text("Private draft".into())
            };
            assert!(std::iter::from_fn(|| commands.try_recv().ok()).any(|command| matches!(command,
                Command::SendText { quoting: Some(quote), .. } | Command::SendVoice { quoting: Some(quote), .. } if quote == target
            )));
            let target_generation = target.destination_generation;
            app.forget_chat(recipient);
            if recreate_before_refusal {
                app.chats.push(Chat::new(
                    recipient.into(),
                    "Replacement recipient fixture".into(),
                ));
            }
            app.send_refused(recipient.into(), Some(target), unsent, Refusal::Offline);
            assert!(!app.private_reply_drafts.contains_key(recipient));
            assert!(!app.attachment_drafts.contains_key(recipient));
            assert!(!app.drafts.contains_key(recipient));
            assert!(app.reply_to.is_none());
            assert!(app.unsent_voice.is_none());
            if !recreate_before_refusal {
                app.chats.push(Chat::new(
                    recipient.into(),
                    "Recreated recipient fixture".into(),
                ));
            }
            app.open_chat(recipient.into());
            assert!(app.reply_to.is_none());
            assert!(app.composer.is_empty());
            // A new private send in the replacement lifetime still recovers normally.
            app.open_chat(group.into());
            app.apply(
                Action::ReplyPrivately {
                    chat: group.into(),
                    message: "original".into(),
                },
                &ctx,
            );
            let replacement = app.reply_to.take().unwrap();
            assert_ne!(replacement.destination_generation, target_generation);
            app.send_refused(
                recipient.into(),
                Some(replacement.clone()),
                Unsent::Text("Replacement draft".into()),
                Refusal::Offline,
            );
            assert_eq!(app.reply_to, Some(replacement));
            assert_eq!(app.composer, "Replacement draft");
        }
    }

    /// Closing and reopening a recipient restores its private quote, while cancel removes it.
    #[test]
    fn closing_a_private_reply_preserves_the_quote_until_cancelled() {
        let mut app = app();
        let ctx = egui::Context::default();
        let recipient = "15550001111@s.whatsapp.net";
        app.chats
            .push(Chat::new(recipient.into(), "Recipient fixture".into()));
        app.open_chat(recipient.into());
        let target = ReplyTarget {
            id: "original".into(),
            chat: Some("12345@g.us".into()),
            destination_generation: 0,
        };
        app.reply_to = Some(target.clone());
        app.composer = "Private draft".into();
        app.apply(Action::CloseChat, &ctx);
        assert!(app.open_chat.is_none());
        assert!(app.reply_to.is_none());
        assert_eq!(
            app.attachment_drafts
                .get(recipient)
                .and_then(|draft| draft.quoting.as_ref()),
            Some(&target)
        );
        app.open_chat(recipient.into());
        assert_eq!(app.reply_to, Some(target));
        assert_eq!(app.composer, "Private draft");
        app.reply_to = None;
        app.apply(Action::CloseChat, &ctx);
        assert!(!app.private_reply_drafts.contains_key(recipient));
        assert!(!app.attachment_drafts.contains_key(recipient));
        app.open_chat(recipient.into());
        assert!(app.reply_to.is_none());
    }

    /// Leaving or deleting one recipient must not erase another recipient's quote draft.
    #[test]
    fn leaving_a_chat_preserves_other_private_reply_drafts() {
        for delete in [false, true] {
            let mut app = app();
            let left = "15550001111@s.whatsapp.net";
            let other = "15550002222@s.whatsapp.net";
            let quote = ReplyTarget {
                id: "original".into(),
                chat: Some("12345@g.us".into()),
                destination_generation: 0,
            };
            app.chats
                .push(Chat::new(left.into(), "Left fixture".into()));
            app.chats
                .push(Chat::new(other.into(), "Other fixture".into()));
            app.open_chat = Some(left.into());
            app.reply_to = Some(quote.clone());
            app.private_reply_drafts.insert(left.into(), quote.clone());
            app.private_reply_drafts.insert(other.into(), quote.clone());
            if delete {
                app.forget_chat(left);
            } else {
                app.leave_chat(left);
            }
            assert!(app.open_chat.is_none());
            assert!(app.reply_to.is_none());
            assert!(!app.private_reply_drafts.contains_key(left));
            assert_eq!(app.private_reply_drafts.get(other), Some(&quote));
            app.open_chat(other.into());
            assert_eq!(app.reply_to, Some(quote));
        }
    }

    /// Only a visible incoming group message and an accessible sender can start a reply.
    #[test]
    fn private_reply_ignores_own_deleted_invalid_and_locked_targets() {
        for case in [
            "own",
            "deleted",
            "invalid",
            "locked",
            "locked-source",
            "stale",
        ] {
            let mut app = app();
            let group = "12345@g.us";
            let sender = "15550002222@s.whatsapp.net";
            let mut original = message(group, "original", 100);
            original.sender = sender.into();
            match case {
                "own" => original.from_me = true,
                "deleted" => original.content = Content::Revoked,
                "invalid" => original.sender = group.into(),
                "locked" => {
                    let mut chat = Chat::new(sender.into(), "Locked fixture".into());
                    chat.locked = true;
                    app.chats.push(chat);
                }
                "locked-source" => {
                    let mut chat = Chat::new(group.into(), "Locked group fixture".into());
                    chat.locked = true;
                    app.chats.push(chat);
                }
                _ => {}
            }
            app.open_chat = Some(if case == "stale" { sender } else { group }.into());
            let previous = app.open_chat.clone();
            app.conversations.insert(
                group.into(),
                Conversation {
                    messages: vec![original],
                    ..Default::default()
                },
            );
            app.apply(
                Action::ReplyPrivately {
                    chat: group.into(),
                    message: "original".into(),
                },
                &egui::Context::default(),
            );
            assert_eq!(app.open_chat, previous, "{case}");
            assert!(app.reply_to.is_none(), "{case}");
        }
    }

    /// Cached source content stays hidden after switching out of an authenticated folder.
    #[test]
    fn private_quote_previews_follow_source_lock_authorization() {
        let mut app = app();
        let group = "12345@g.us";
        let sender = "15550002222@s.whatsapp.net";
        let other = "15550003333@s.whatsapp.net";
        let mut source = Chat::new(group.into(), "Locked group fixture".into());
        source.locked = true;
        app.chats.extend([
            source,
            Chat::new(sender.into(), "Sender fixture".into()),
            Chat::new(other.into(), "Other fixture".into()),
        ]);
        let mut original = message(group, "original", 100);
        original.sender = sender.into();
        app.conversations.insert(
            group.into(),
            Conversation {
                messages: vec![original],
                requested: true,
                ..Default::default()
            },
        );
        app.settings.chat_lock_code_hash = Some("synthetic authenticated verifier".into());
        app.enter_locked_folder();
        app.open_chat = Some(group.into());
        app.apply(
            Action::ReplyPrivately {
                chat: group.into(),
                message: "original".into(),
            },
            &egui::Context::default(),
        );
        assert_eq!(app.open_chat.as_deref(), Some(sender));
        assert!(!app.locked_folder_open());
        let target = app.reply_to.clone().unwrap();
        assert!(app.conversations[group].message(&target).is_some());
        assert!(app.reply_preview(&target, sender).is_none());
        app.open_chat(other.into());
        app.open_chat(sender.into());
        assert_eq!(app.reply_to, Some(target.clone()));
        assert!(app.reply_preview(&target, sender).is_none());
        app.enter_locked_folder();
        assert!(app.reply_preview(&target, sender).is_some());
        app.close_locked_folder();
        assert!(app.reply_preview(&target, sender).is_none());
        let mut unlocked = app.chat(group).unwrap().clone();
        unlocked.locked = false;
        app.handle_chat_updated(unlocked, true);
        assert!(app.reply_preview(&target, sender).is_some());
    }

    /// Every send preserves hidden private quotes and drafts until their source is authorized.
    #[test]
    fn hidden_private_quotes_cannot_be_sent_by_any_composer_action() {
        for authorized in [false, true] {
            for kind in 0..7 {
                let mut app = app();
                let (backend, mut commands) = Backend::recording();
                app.backend = backend;
                let group = "12345@g.us";
                let recipient = "15550002222@s.whatsapp.net";
                let mut source = Chat::new(group.into(), "Locked source fixture".into());
                source.locked = true;
                app.chats.extend([
                    source,
                    Chat::new(recipient.into(), "Recipient fixture".into()),
                ]);
                app.open_chat = Some(recipient.into());
                let target = ReplyTarget {
                    id: "original".into(),
                    chat: Some(group.into()),
                    destination_generation: 0,
                };
                app.reply_to = Some(target.clone());
                app.settings.chat_lock_code_hash = Some("synthetic authenticated verifier".into());
                if authorized {
                    app.enter_locked_folder();
                }
                let action = match kind {
                    0 => Action::SendText {
                        chat: recipient.into(),
                        text: "Private draft".into(),
                        quoting: Some(target.clone()),
                    },
                    1 => {
                        app.pending
                            .push(Pending::File(PathBuf::from("synthetic-attachment.png")));
                        Action::SendPending {
                            account: app.account().id.clone(),
                            mentions: Vec::new(),
                            chat: recipient.into(),
                            caption: "Private draft".into(),
                        }
                    }
                    2 => Action::SendSticker(PathBuf::from("synthetic-sticker.webp")),
                    3 => Action::SendGif(Gif {
                        id: "fixture".into(),
                        still: None,
                        mp4: "https://example.invalid/fixture.mp4".into(),
                        width: 2,
                        height: 2,
                    }),
                    4 => {
                        app.unsent_voice = Some((recipient.into(), vec![0.0; 10]));
                        Action::SendRecording
                    }
                    5 => {
                        app.recording = Some(crate::audio::Recorder::recorded_fixture(vec![
                                0.0;
                                crate::voice::RATE
                                    as usize
                            ]));
                        Action::SendRecording
                    }
                    _ => {
                        app.sticker_draft = Some(crate::model::StickerDraft {
                            source: PathBuf::from("synthetic-sticker.png"),
                            width: 1,
                            height: 1,
                            transparent: false,
                            crop: crate::model::StickerCrop::centered(1, 1),
                            keep_transparent: false,
                            emojis: String::new(),
                        });
                        Action::MakeSticker { send: true }
                    }
                };
                app.apply(action, &egui::Context::default());
                let sent: Vec<_> = std::iter::from_fn(|| commands.try_recv().ok()).collect();
                if authorized {
                    assert!(
                        sent.iter().any(|command| matches!(command,
                            Command::SendText { quoting: Some(quote), .. } |
                            Command::SendFiles { quoting: Some(quote), .. } |
                            Command::SendSticker { quoting: Some(quote), .. } |
                            Command::SendGif { quoting: Some(quote), .. } |
                            Command::MakeSticker { quoting: Some(quote), .. } |
                            Command::SendVoice { quoting: Some(quote), .. } if quote == &target
                        )),
                        "kind {kind}"
                    );
                    if kind == 6 {
                        assert_eq!(app.reply_to, Some(target));
                    } else {
                        assert!(app.reply_to.is_none());
                    }
                } else {
                    assert!(
                        !sent.iter().any(|command| matches!(
                            command,
                            Command::SendText { .. }
                                | Command::SendFiles { .. }
                                | Command::SendImage { .. }
                                | Command::SendSticker { .. }
                                | Command::SendGif { .. }
                                | Command::SendVoice { .. }
                                | Command::MakeSticker { .. }
                        )),
                        "kind {kind}"
                    );
                    assert_eq!(app.reply_to, Some(target));
                    if kind <= 1 {
                        assert_eq!(app.composer, "Private draft");
                    }
                    if kind == 1 {
                        assert_eq!(app.pending.len(), 1);
                    }
                    if (4..=5).contains(&kind) {
                        assert!(app.unsent_voice.is_some());
                        assert!(app.recording.is_none());
                    }
                }
            }
        }
    }

    /// Checks that sticker dispatch consumes and forwards the pending quote exactly once.
    #[test]
    fn sending_a_sticker_consumes_the_pending_reply() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        app.open_chat = Some("fixture@s.whatsapp.net".into());
        app.reply_to = Some("quoted-message".into());

        app.apply(
            Action::SendSticker(std::path::PathBuf::from("sticker.webp")),
            &egui::Context::default(),
        );

        assert!(app.reply_to.is_none());
        assert!(matches!(
            commands.try_recv(),
            Ok(Command::SendSticker {
                chat,
                quoting: Some(id),
                ..
            }) if chat == "fixture@s.whatsapp.net" && id.id == "quoted-message"
        ));
    }

    /// Checks that oversized attachment clicks are refused before a download command is queued.
    #[test]
    fn clicking_an_oversized_attachment_does_not_start_a_download() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let chat = "peer@s.whatsapp.net";
        let mut attachment = message(chat, "picture", 1);
        attachment.content = Content::Image {
            motion: None,
            caption: None,
            media: Media {
                mime: "image/jpeg".into(),
                size: crate::model::MANUAL_DOWNLOAD_LIMIT + 1,
                width: None,
                height: None,
                album: None,
                path: None,
                state: MediaState::Idle,
            },
        };
        app.conversations
            .entry(chat.into())
            .or_default()
            .merge(vec![attachment], false);

        app.apply(
            Action::Download {
                card: None,
                chat: chat.into(),
                message: "picture".into(),
            },
            &egui::Context::default(),
        );

        assert!(commands.try_recv().is_err());
        assert!(matches!(
            app.media_of(chat, "picture").map(|media| &media.state),
            Some(MediaState::Failed(_))
        ));
    }

    #[test]
    fn history_fidelity_reload_repairs_loaded_order_and_content() {
        let mut conversation = Conversation::default();
        conversation.merge(
            vec![message("c", "third", 100), message("c", "first", 100)],
            false,
        );
        let refreshed = [("second", 2), ("first", 1), ("third", 3)]
            .into_iter()
            .map(|(id, order)| Message {
                history_order: Some(order),
                content: Content::text(format!("updated {id}")),
                edited: true,
                ..message("c", id, 100)
            })
            .collect::<Vec<_>>();
        conversation.merge(refreshed.clone(), true);
        conversation.merge(refreshed, true);
        assert_eq!(
            conversation
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["first", "second", "third"]
        );
        assert!(conversation.messages.iter().all(|m| m.edited));
        assert_eq!(
            conversation.messages[0].content,
            Content::text("updated first")
        );
    }

    #[test]
    fn conversations_merge_pages_without_duplicates() {
        let mut conversation = Conversation::default();
        conversation.merge(vec![message("c", "b", 2), message("c", "c", 3)], false);
        conversation.merge(vec![message("c", "a", 1), message("c", "b", 2)], true);
        let ids: Vec<&str> = conversation
            .messages
            .iter()
            .map(|m| m.id.as_str())
            .collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        conversation.merge(vec![message("c", "c", 3)], false);
        assert_eq!(conversation.messages.len(), 3);
    }

    #[test]
    fn a_jump_flash_rises_holds_and_fades_out() {
        assert_eq!(JumpHighlight::strength(-1.0), 0.0);
        assert_eq!(JumpHighlight::strength(0.0), 0.0);
        assert_eq!(JumpHighlight::strength(0.5), 1.0);
        let fading = JumpHighlight::strength(1.5);
        assert!(fading > 0.0 && fading < 1.0, "{fading}");
        assert_eq!(JumpHighlight::strength(JumpHighlight::DURATION), 0.0);
        assert_eq!(JumpHighlight::strength(10.0), 0.0);
    }

    #[test]
    fn quotes_and_search_hits_flash_their_message_until_the_chat_changes() {
        let mut app = app();
        let ctx = egui::Context::default();
        let (ada, bob) = ("1@s.whatsapp.net", "2@s.whatsapp.net");
        for chat in [ada, bob] {
            app.chats.push(Chat::new(chat.into(), "Chat".into()));
            app.conversations.insert(
                chat.into(),
                Conversation {
                    requested: true,
                    complete: true,
                    messages: vec![message(chat, "old", 10)],
                    ..Default::default()
                },
            );
        }
        app.apply(
            Action::OpenMessage {
                chat: ada.into(),
                message: "old".into(),
            },
            &ctx,
        );
        assert_eq!(
            app.jump_highlight,
            Some(JumpHighlight::new(ada.into(), "old".into()))
        );
        app.jump_highlight = None;
        app.apply(Action::ScrollTo("old".into()), &ctx);
        assert_eq!(
            app.jump_highlight,
            Some(JumpHighlight::new(ada.into(), "old".into()))
        );
        app.open_chat(bob.into());
        assert_eq!(app.jump_highlight, None);
    }

    #[test]
    fn a_search_hit_opens_its_chat_at_the_message() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        let conversation = Conversation {
            requested: true,
            complete: true,
            messages: vec![message(chat, "old", 10)],
            ..Default::default()
        };
        app.conversations.insert(chat.into(), conversation);
        app.apply(
            Action::OpenMessage {
                chat: chat.into(),
                message: "old".into(),
            },
            &ctx,
        );
        assert_eq!(app.open_chat.as_deref(), Some(chat));
        assert_eq!(app.scroll_anchor.as_deref(), Some("old"));
        assert!(!app.scroll_to_bottom, "aims at the hit, not the end");
    }

    #[test]
    fn ctrl_f_searches_the_open_chat_in_the_pane_and_ctrl_k_the_list() {
        let mut app = app();
        let ctx = egui::Context::default();
        // Without an open chat there is nothing to search inside.
        app.apply(Action::OpenChatSearch, &ctx);
        assert!(!app.chat_search_open);
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.apply(Action::OpenChatSearch, &ctx);
        assert!(app.chat_search_visible());
        assert!(app.focus_chat_search);
        assert!(!app.focus_composer, "the pane's field takes the keyboard");
        // Ctrl+K searches the chat list, and closes the pane.
        app.apply(Action::FocusSearch, &ctx);
        app.apply_actions(&ctx);
        assert!(app.focus_search);
        assert!(!app.chat_search_open);
    }

    #[test]
    fn the_pane_keeps_the_chat_list_search_and_gives_the_composer_back() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        app.open_chat = Some(chat.into());
        app.search = "list".into();
        app.apply(Action::OpenChatSearch, &ctx);
        assert!(app.chat_search_open);
        assert_eq!(app.search, "list", "the two searches are independent");
        app.search.clear();
        app.apply(Action::ChatSearch("engine".into()), &ctx);
        app.apply(Action::CloseChatSearch, &ctx);
        assert!(!app.chat_search_open);
        assert!(app.chat_search.is_empty());
        assert!(app.focus_composer, "closing hands the keyboard back");
    }

    #[test]
    fn the_pane_lists_only_the_answer_to_the_query_and_day_in_force() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.open_chat = Some(chat.into());
        app.apply(Action::OpenChatSearch, &ctx);
        let searches = |commands: &mut tokio::sync::mpsc::UnboundedReceiver<Command>| {
            std::iter::from_fn(|| commands.try_recv().ok())
                .filter_map(|command| match command {
                    Command::SearchChatMessages {
                        query, from, until, ..
                    } => Some((query, from.is_some() && until.is_some())),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert!(searches(&mut commands).is_empty(), "nothing to ask yet");
        app.apply(Action::ChatSearch("eng".into()), &ctx);
        app.apply(Action::ChatSearch(" engine ".into()), &ctx);
        assert_eq!(
            searches(&mut commands),
            [("eng".to_owned(), false), ("engine".to_owned(), false)]
        );
        assert!(app.chat_search_pending);
        let hits =
            |query: &str, from: Option<i64>, until: Option<i64>, ids: &[&str]| Event::ChatHits {
                chat: chat.into(),
                query: query.into(),
                from,
                until,
                messages: ids.iter().map(|id| message(chat, id, 1)).collect(),
                truncated: false,
            };
        // The answer to the query already replaced arrives too late.
        events.send(hits("eng", None, None, &["old"])).unwrap();
        app.handle_events();
        assert!(app.chat_search_hits.is_empty());
        assert!(app.chat_search_pending);
        events
            .send(hits("engine", None, None, &["m1", "m2"]))
            .unwrap();
        app.handle_events();
        assert_eq!(app.chat_search_hits.len(), 2);
        assert!(!app.chat_search_pending);
        // A day narrows the same query, and an answer without it is stale.
        let day = jiff::civil::Date::new(2026, 9, 23).expect("a date");
        app.apply(Action::SetChatSearchDay(Some(day)), &ctx);
        assert_eq!(searches(&mut commands), [("engine".to_owned(), true)]);
        assert_eq!(app.chat_search_day, Some(day));
        assert_eq!(app.chat_search_month, day, "the calendar follows the pick");
        assert!(!app.chat_search_calendar, "picking a day closes it");
        events.send(hits("engine", None, None, &["m3"])).unwrap();
        app.handle_events();
        assert_eq!(app.chat_search_hits.len(), 2, "still the earlier list");
        let (from, until) = app.chat_search_range();
        events.send(hits("engine", from, until, &["m1"])).unwrap();
        app.handle_events();
        assert_eq!(app.chat_search_hits.len(), 1);
        // A day on its own lists that day; with neither, nothing is asked.
        app.apply(Action::ChatSearch(String::new()), &ctx);
        assert_eq!(searches(&mut commands), [(String::new(), true)]);
        app.apply(Action::SetChatSearchDay(None), &ctx);
        assert!(searches(&mut commands).is_empty());
        assert!(app.chat_search_hits.is_empty());
        assert!(!app.chat_search_pending);
        assert_eq!(app.chat_search_range(), (None, None));
    }

    #[test]
    fn a_result_opened_from_the_pane_keeps_the_keyboard_there() {
        let mut app = app();
        let ctx = egui::Context::default();
        let chat = "1@s.whatsapp.net";
        app.chats.push(Chat::new(chat.into(), "Ada".into()));
        let conversation = Conversation {
            requested: true,
            complete: true,
            messages: vec![message(chat, "hit", 10)],
            ..Default::default()
        };
        app.conversations.insert(chat.into(), conversation);
        app.apply(Action::OpenChat(chat.into()), &ctx);
        app.apply(Action::OpenChatSearch, &ctx);
        app.apply(Action::ChatSearch("engine".into()), &ctx);
        app.apply(
            Action::OpenMessage {
                chat: chat.into(),
                message: "hit".into(),
            },
            &ctx,
        );
        assert!(app.chat_search_open, "the pane stays open");
        assert_eq!(app.chat_search, "engine", "with its query");
        assert!(!app.focus_composer);
        assert_eq!(
            app.jump_highlight
                .as_ref()
                .map(|jump| jump.message.as_str()),
            Some("hit"),
            "and the result flashes"
        );
        // Another chat's search does not follow the reader there.
        let other = "2@s.whatsapp.net";
        app.chats.push(Chat::new(other.into(), "Bo".into()));
        app.apply(Action::OpenChat(other.into()), &ctx);
        assert!(!app.chat_search_open);
        assert!(app.chat_search.is_empty());
    }

    #[test]
    fn clearing_the_search_clears_its_hits() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.search_hits.push(message("1@s.whatsapp.net", "m", 1));
        app.apply(Action::Search(String::new()), &ctx);
        assert!(app.search_hits.is_empty());
    }

    #[test]
    fn matching_contacts_are_people_not_yet_talked_to() {
        let mut app = app();
        app.me = Some("490000000000@s.whatsapp.net".into());
        let contact = |id: &str, name: &str| crate::model::Contact {
            id: id.into(),
            full_name: Some(name.into()),
            first_name: None,
            push_name: None,
        };
        // Exclude contacts that already have chats.
        app.contacts.insert(
            "491700000001@s.whatsapp.net".into(),
            contact("491700000001@s.whatsapp.net", "Ada Lovelace"),
        );
        app.chats.push(Chat::new(
            "491700000001@s.whatsapp.net".into(),
            "Ada Lovelace".into(),
        ));
        // Include contacts without chats.
        app.contacts.insert(
            "491700000002@s.whatsapp.net".into(),
            contact("491700000002@s.whatsapp.net", "Adele Goldberg"),
        );
        // Exclude groups and our own id.
        app.contacts
            .insert("12345@g.us".into(), contact("12345@g.us", "Adventurers"));
        app.contacts.insert(
            "490000000000@s.whatsapp.net".into(),
            contact("490000000000@s.whatsapp.net", "Adah Me"),
        );
        app.search = "ad".into();
        let names: Vec<&str> = app
            .matching_contacts()
            .iter()
            .filter_map(|contact| contact.display_name())
            .collect();
        assert_eq!(names, vec!["Adele Goldberg"]);
        // Match phone-number digits.
        app.search = "491700000002".into();
        assert_eq!(app.matching_contacts().len(), 1);
        app.search = String::new();
        assert!(app.matching_contacts().is_empty());
    }

    #[test]
    fn muting_all_channels_leaves_other_chats_alone() {
        let mut app = app();
        let (backend, mut commands) = Backend::recording();
        app.backend = backend;
        let ctx = egui::Context::default();
        app.chats = vec![
            Chat::new("1@newsletter".into(), "News".into()),
            Chat::new("2@newsletter".into(), "More news".into()),
            Chat::new("3@s.whatsapp.net".into(), "Ada".into()),
        ];
        app.apply(Action::MuteAllChannels(true), &ctx);
        app.apply_actions(&ctx);
        let muted: Vec<String> = std::iter::from_fn(|| commands.try_recv().ok())
            .filter_map(|command| match command {
                Command::SetMuted(chat, Some(0)) => Some(chat),
                _ => None,
            })
            .collect();
        assert_eq!(muted, ["1@newsletter", "2@newsletter"]);
    }

    #[test]
    fn the_unread_chip_finds_a_chat_marked_unread_by_hand() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut marked = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        marked.marked_unread = true;
        let read = Chat::new("2@s.whatsapp.net".into(), "Grace".into());
        app.chats = vec![marked, read];
        assert_eq!(app.unread_chats(ChatFilter::Unread), 1);
        app.apply(Action::SetChatFilter(ChatFilter::Unread), &ctx);
        let shown: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(shown, ["Ada"], "the chip finds the chat marked by hand");
        // The app badge counts it too, as WhatsApp does.
        assert_eq!(app.unread_chat_count(), 1);
    }

    #[test]
    fn the_app_badge_counts_unread_chats_not_messages() {
        let mut app = app();
        let mut busy = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        busy.unread = 7;
        let mut quiet = Chat::new("2@s.whatsapp.net".into(), "Grace".into());
        quiet.unread = 1;
        let mut archived = Chat::new("3@s.whatsapp.net".into(), "Old".into());
        archived.archived = true;
        archived.unread = 4;
        let mut muted = Chat::new("4@s.whatsapp.net".into(), "Loud".into());
        muted.muted_until = Some(i64::MAX);
        muted.unread = 9;
        let read = Chat::new("5@s.whatsapp.net".into(), "Done".into());
        app.chats = vec![busy, quiet, archived, muted, read];
        assert_eq!(app.unread_chat_count(), 2);
    }

    #[test]
    fn channels_have_their_own_chip_and_archived_chats_theirs() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut friend = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        friend.unread = 1;
        let mut channel = Chat::new("2@newsletter".into(), "News".into());
        channel.unread = 3;
        let mut archived = Chat::new("3@s.whatsapp.net".into(), "Old".into());
        archived.archived = true;
        archived.unread = 2;
        app.chats = vec![friend, channel, archived];
        let names = |app: &App| {
            app.visible_chats()
                .iter()
                .map(|chat| chat.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&app), ["Ada"], "All leaves channels out");
        assert_eq!(app.unread_chats(ChatFilter::Unread), 1);
        assert_eq!(app.unread_chats(ChatFilter::Channels), 1);
        app.apply(Action::SetChatFilter(ChatFilter::Channels), &ctx);
        assert_eq!(names(&app), ["News"]);
        app.apply(Action::ShowArchived(true), &ctx);
        assert_eq!(names(&app), ["Old"]);
        assert_eq!(app.archived_unread(), 1);
        app.apply(Action::SetChatFilter(ChatFilter::All), &ctx);
        assert!(!app.show_archived, "choosing a filter leaves the archive");
        assert_eq!(names(&app), ["Ada"]);
    }

    #[test]
    fn visible_chats_pin_first_and_filter() {
        let mut app = app();
        let mut a = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        a.last_activity = 10;
        let mut b = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        b.last_activity = 20;
        let mut c = Chat::new("3@s.whatsapp.net".into(), "Cy".into());
        c.last_activity = 5;
        c.pinned = true;
        let mut d = Chat::new("4@s.whatsapp.net".into(), "Dee".into());
        d.archived = true;
        app.chats = vec![b, a, c, d];
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Cy", "Bob", "Ada"]);
        app.search = "ad".into();
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Ada"]);
    }

    #[test]
    fn the_favorites_chip_keeps_the_phone_order_below_pins() {
        let mut app = app();
        let favorite = |id: &str, name: &str, position: u32, activity: i64| {
            let mut chat = Chat::new(id.into(), name.into());
            chat.favorite = true;
            chat.favorite_position = position;
            chat.last_activity = activity;
            chat
        };
        let mut pinned = favorite("3@s.whatsapp.net", "Cy", 2, 1);
        pinned.pinned = true;
        app.chats = vec![
            favorite("1@s.whatsapp.net", "Ada", 1, 30),
            favorite("2@s.whatsapp.net", "Bob", 0, 10),
            pinned,
            Chat::new("4@s.whatsapp.net".into(), "Dee".into()),
        ];
        app.chat_filter = ChatFilter::Favorites;
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, ["Cy", "Bob", "Ada"]);
        app.chat_filter = ChatFilter::All;
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(
            names,
            ["Cy", "Ada", "Bob", "Dee"],
            "other chips keep recency"
        );
    }

    #[test]
    fn the_chat_filter_narrows_the_main_list_only() {
        let mut app = app();
        let mut ada = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        ada.last_activity = 40;
        ada.unread = 2;
        let mut bob = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        bob.last_activity = 30;
        let mut club = Chat::new("3@g.us".into(), "Club".into());
        club.last_activity = 20;
        club.unread = 1;
        let mut news = Chat::new("4@newsletter".into(), "News".into());
        news.last_activity = 10;
        let mut old = Chat::new("5@g.us".into(), "Old group".into());
        old.archived = true;
        old.unread = 3;
        app.chats = vec![ada, bob, club, news, old];
        let names = |app: &App| -> Vec<String> {
            app.visible_chats()
                .iter()
                .map(|chat| chat.name.clone())
                .collect()
        };
        assert_eq!(
            names(&app),
            ["Ada", "Bob", "Club"],
            "channels have their own chip"
        );
        app.chat_filter = ChatFilter::Channels;
        assert_eq!(names(&app), ["News"]);
        app.chat_filter = ChatFilter::Unread;
        assert_eq!(names(&app), ["Ada", "Club"]);
        app.chat_filter = ChatFilter::Private;
        assert_eq!(names(&app), ["Ada", "Bob"], "no groups or broadcasts");
        app.chat_filter = ChatFilter::Groups;
        assert_eq!(names(&app), ["Club"], "archived groups stay in the archive");
        // Unread chats per chip, archived ones left out.
        assert_eq!(app.unread_chats(ChatFilter::Unread), 2);
        assert_eq!(app.unread_chats(ChatFilter::Private), 1);
        assert_eq!(app.unread_chats(ChatFilter::Groups), 1);
        // Search and the archive ignore the filter.
        app.search = "bob".into();
        assert_eq!(names(&app), ["Bob"]);
        app.search = String::new();
        app.show_archived = true;
        assert_eq!(names(&app), ["Old group"]);
    }

    #[test]
    fn the_unread_filter_keeps_the_open_chat_after_it_is_read() {
        let mut app = app();
        let mut ada = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        ada.unread = 1;
        let bob = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        app.chats = vec![ada, bob];
        let mut cy = Chat::new("3@s.whatsapp.net".into(), "Cy".into());
        cy.unread = 1;
        app.chats.push(cy);
        app.chat_filter = ChatFilter::Unread;
        let ctx = egui::Context::default();
        // Every chat opened from the list stays, not only the latest.
        for index in [0, 2] {
            let id = app.chats[index].id.clone();
            app.apply(Action::KeepUnread(id.clone()), &ctx);
            app.open_chat(id);
            app.chats[index].unread = 0;
        }
        assert_eq!(app.visible_chats().len(), 2, "both still listed once read");
        // Choosing a filter again forgets the kept chats.
        app.apply(Action::SetChatFilter(ChatFilter::Unread), &ctx);
        assert!(app.visible_chats().is_empty());
        // A chat opened from search or a notification is not kept.
        app.chats[0].unread = 1;
        app.open_chat("1@s.whatsapp.net".into());
        app.chats[0].unread = 0;
        assert!(app.visible_chats().is_empty());
        // Nothing is kept under another filter.
        app.apply(Action::SetChatFilter(ChatFilter::Private), &ctx);
        app.apply(Action::KeepUnread("2@s.whatsapp.net".into()), &ctx);
        app.apply(Action::SetChatFilter(ChatFilter::Unread), &ctx);
        assert!(app.visible_chats().is_empty());
    }

    #[test]
    fn leaving_the_locked_folder_closes_its_open_conversation() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut chat = Chat::new("fixture".into(), "Fixture".into());
        chat.locked = true;
        app.chats.push(chat);
        app.settings.set_chat_lock_code(Some("fixture-code"));
        app.search = "fixture-code".into();
        app.locked_folder = true;
        app.open_chat("fixture".into());
        assert!(app.current_chat().is_some());
        app.composer = "fixture draft".into();
        app.reply_to = Some("fixture-message".into());
        app.apply(Action::Search(String::new()), &ctx);
        assert!(app.current_chat().is_none());
        assert!(app.open_chat.is_none());
        assert!(app.composer.is_empty());
        assert!(app.reply_to.is_none());
        assert_eq!(app.drafts["fixture"], "fixture draft");
    }

    #[test]
    fn locked_tab_authenticates_without_using_search_and_relocks_on_filter_change() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut chat = Chat::new("fixture@g.us".into(), "Secret fixture".into());
        chat.locked = true;
        chat.archived = true;
        app.chats.push(chat);
        app.settings.set_chat_lock_code(Some("test-code"));
        app.apply(Action::OpenLockedFolder, &ctx);
        assert_eq!(app.dialog, Some(Dialog::UnlockLockedChats));
        assert!(app.visible_chats().is_empty());
        app.apply(Action::UnlockLockedFolder("wrong".into()), &ctx);
        assert!(app.chat_lock_error);
        assert!(!app.locked_folder_open());
        app.apply(Action::UnlockLockedFolder("test-code".into()), &ctx);
        assert!(app.locked_folder_open());
        assert!(app.search.is_empty());
        assert_eq!(
            app.visible_chats().len(),
            1,
            "includes archived locked chats"
        );
        app.apply(Action::Search("secret".into()), &ctx);
        assert_eq!(app.visible_chats().len(), 1);
        app.apply(Action::Search("unmatched".into()), &ctx);
        assert!(app.visible_chats().is_empty());
        assert!(app.locked_folder_open());
        app.open_chat("fixture@g.us".into());
        app.apply(Action::SetChatFilter(ChatFilter::All), &ctx);
        assert!(!app.locked_folder_open());
        assert!(app.current_chat().is_none());
        assert!(app.chat_lock_session.is_none());
        app.apply(Action::OpenLockedFolder, &ctx);
        assert_eq!(app.dialog, Some(Dialog::UnlockLockedChats));
    }

    #[test]
    fn code_setup_cannot_replace_an_existing_verifier_and_clears_prompt_state() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.apply(Action::CreateChatLockCode("fixture-code".into()), &ctx);
        assert!(app.locked_folder_open());
        app.apply(Action::CreateChatLockCode("replacement".into()), &ctx);
        assert!(app.settings.verifies_chat_lock_code("fixture-code"));
        app.apply(Action::CloseLockedFolder, &ctx);
        app.apply(Action::OpenLockedFolder, &ctx);
        app.chat_lock_entry = "partial".into();
        app.chat_lock_confirm = "partial".into();
        app.window_gone();
        assert!(app.chat_lock_entry.is_empty());
        assert!(app.chat_lock_confirm.is_empty());
        assert!(app.dialog.is_none());
    }

    #[test]
    fn our_own_chat_is_titled_and_found_by_our_name() {
        let mut app = app();
        let me = "15550000000@s.whatsapp.net";
        app.me = Some(me.into());
        let mut own = Chat::new(me.into(), "You".into());
        own.last_activity = 10;
        let mut ada = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        ada.last_activity = 20;
        app.chats = vec![own, ada];
        assert_eq!(app.chat_title(&app.chats[0].clone()), "You");
        app.me_name = Some("Carmine Paolino".into());
        assert_eq!(
            app.chat_title(&app.chats[0].clone()),
            "Carmine Paolino (You)"
        );
        for search in ["carmine", "you", "5550000"] {
            app.search = search.into();
            let ids: Vec<&str> = app
                .visible_chats()
                .iter()
                .map(|chat| chat.id.as_str())
                .collect();
            assert_eq!(ids, vec![me], "{search}");
        }
        app.locale = crate::i18n::Locale::German;
        assert_eq!(
            app.chat_title(&app.chats[0].clone()),
            "Carmine Paolino (Du)"
        );
    }

    #[test]
    fn contact_searches_offer_to_message_ourselves() {
        let mut app = app();
        assert!(!app.offers_self(""), "not before we know who we are");
        app.me = Some("15550000000@s.whatsapp.net".into());
        app.me_name = Some("Carmine".into());
        for needle in ["", "carm", "you", "message your", "555000"] {
            assert!(app.offers_self(needle), "{needle}");
        }
        assert!(!app.offers_self("ada"));
        app.locale = crate::i18n::Locale::Italian;
        assert!(app.offers_self("te stesso"), "the interface language");
        assert!(app.offers_self("yourself"), "English still works");
        let mut own = Chat::new(app.me.clone().unwrap(), "You".into());
        own.locked = true;
        app.chats = vec![own];
        assert!(
            !app.offers_self(""),
            "a locked chat stays in the locked list"
        );
    }

    #[test]
    fn message_yourself_creates_one_chat_and_respects_its_lock() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.me = Some("15550000000@s.whatsapp.net".into());
        app.dialog = Some(Dialog::NewChat);
        app.apply(Action::MessageYourself, &ctx);
        assert_eq!(app.open_chat, app.me);
        assert!(app.dialog.is_none());
        app.apply(Action::MessageYourself, &ctx);
        assert_eq!(app.chats.len(), 1);
        app.apply(Action::SetLocked(app.me.clone().unwrap(), true), &ctx);
        app.apply(Action::MessageYourself, &ctx);
        assert!(app.open_chat.is_none());
        assert_eq!(app.dialog, Some(Dialog::UnlockLockedChats));
    }

    #[test]
    fn quick_reaction_preferences_count_use_without_counting_removal_or_insertions() {
        let mut app = app();
        let ctx = egui::Context::default();
        for emoji in ["🦀", "🎉", "🦀", ""] {
            app.apply(
                Action::React {
                    chat: "fixture".into(),
                    message: "message".into(),
                    emoji: emoji.into(),
                },
                &ctx,
            );
        }
        app.remember_emoji("🔥");
        assert_eq!(
            app.settings.reaction_emoji,
            vec![("🦀".into(), 2), ("🎉".into(), 1)]
        );
        let saved = serde_json::to_string(&app.settings).unwrap();
        let loaded: Settings = serde_json::from_str(&saved).unwrap();
        assert_eq!(loaded.reaction_emoji, app.settings.reaction_emoji);
    }

    #[test]
    fn locked_conversations_close_on_back_code_changes_and_window_close() {
        for exit in ["back", "change-code", "clear-code", "window"] {
            let mut app = app();
            let ctx = egui::Context::default();
            let mut chat = Chat::new("fixture".into(), "Fixture".into());
            chat.locked = true;
            app.chats.push(chat);
            app.settings.set_chat_lock_code(Some("fixture-code"));
            app.search = "wrong-code".into();
            app.apply(Action::OpenLockedFolder, &ctx);
            assert!(!app.locked_folder);
            app.search = "fixture-code".into();
            app.apply(Action::OpenLockedFolder, &ctx);
            app.open_chat("fixture".into());
            assert!(app.current_chat().is_some());
            match exit {
                "back" => app.apply(Action::CloseLockedFolder, &ctx),
                "change-code" => app.apply(Action::SetChatLockCode(Some("new-code".into())), &ctx),
                "clear-code" => app.apply(Action::SetChatLockCode(None), &ctx),
                "window" => app.window_gone(),
                _ => unreachable!(),
            }
            assert!(!app.locked_folder, "{exit}");
            assert!(app.current_chat().is_none(), "{exit}");
            assert!(app.open_chat.is_none(), "{exit}");
            assert!(app.search.is_empty(), "{exit}");
        }
    }

    #[test]
    fn locked_chats_hide_everywhere_until_the_code_opens_the_folder() {
        let mut app = app();
        let mut a = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        a.last_activity = 10;
        a.unread = 3;
        let mut b = Chat::new("2@s.whatsapp.net".into(), "Bob".into());
        b.last_activity = 20;
        b.locked = true;
        b.unread = 5;
        app.chats = vec![b, a];
        app.settings.set_chat_lock_code(Some("1234"));

        // Hidden from the list, search, and the unread badge.
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Ada"]);
        app.search = "bob".into();
        assert!(app.visible_chats().is_empty());
        assert_eq!(app.unread_chat_count(), 1);
        assert_eq!(app.unread_chats(ChatFilter::All), 1);

        // Typing the code reveals the entry; opening the folder shows only
        // the locked chats; editing the search away hides them again.
        assert!(!app.secret_code_matched());
        app.search = "1234".into();
        assert!(app.secret_code_matched());
        app.locked_folder = true;
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Bob"]);
        assert_eq!(app.locked_count(), 1);
        app.search = "123".into();
        assert_eq!(
            app.visible_chats()
                .iter()
                .map(|chat| chat.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Ada"]
        );
        app.apply(Action::Search("123".into()), &egui::Context::default());
        assert!(!app.locked_folder);
        app.apply(Action::Search(String::new()), &egui::Context::default());
        let names: Vec<&str> = app
            .visible_chats()
            .iter()
            .map(|chat| chat.name.as_str())
            .collect();
        assert_eq!(names, vec!["Ada"]);
    }

    #[test]
    fn locked_chat_code_hint_is_shown_once() {
        let mut app = app();
        let mut chat = Chat::new("1@s.whatsapp.net".into(), "Ada".into());
        chat.locked = true;
        app.chats.push(chat);

        assert!(app.should_show_chat_lock_hint());
        app.apply(Action::DismissChatLockHint, &egui::Context::default());
        assert!(!app.should_show_chat_lock_hint());
    }

    #[test]
    fn locking_the_open_chat_closes_it() {
        let mut app = app();
        let id: ChatId = "2@s.whatsapp.net".into();
        app.chats = vec![Chat::new(id.clone(), "Bob".into())];
        app.open_chat = Some(id.clone());
        app.locked_folder = true;
        app.apply(Action::SetLocked(id, true), &egui::Context::default());
        assert!(app.open_chat.is_none());
        assert!(app.chats[0].locked);
    }

    #[test]
    fn a_remote_lock_closes_the_chat_and_hides_search_hits() {
        let mut app = app();
        let id: ChatId = "2@s.whatsapp.net".into();
        app.chats = vec![Chat::new(id.clone(), "Bob".into())];
        app.open_chat = Some(id.clone());
        app.search_hits.push(message(&id, "m", 1));
        app.composer = "Synthetic draft".into();
        let mut chat = app.chats[0].clone();
        chat.archived = true;
        chat.locked = true;
        app.handle_chat_updated(chat, true);
        assert!(app.open_chat.is_none());
        assert!(app.search_hits.is_empty());
        assert!(app.composer.is_empty());
        assert_eq!(
            app.drafts.get(&id).map(String::as_str),
            Some("Synthetic draft")
        );
        // A locked chat never contributes an archived row either.
        assert_eq!(app.archived_count(), 0);

        // Reopening it needs the folder open with the code typed.
        app.open_chat(id.clone());
        assert!(app.open_chat.is_none());
        app.settings.set_chat_lock_code(Some("1234"));
        app.search = "1234".into();
        app.locked_folder = true;
        app.open_chat(id.clone());
        assert_eq!(app.open_chat.as_deref(), Some(id.as_str()));
    }

    #[test]
    fn desktop_handlers_are_validated_even_for_archived_urls() {
        let mut app = app();
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(Default::default(), |_| {
            app.apply(Action::OpenUrl("file:///fixture.exe".into()), &ctx);
            app.apply(
                Action::OpenFile(PathBuf::from("/fixture/program.exe")),
                &ctx,
            );
        });
        output.textures_delta.clear();
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .all(|command| !matches!(command, egui::OutputCommand::OpenUrl(_)))
        );
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::OpenFolder(_)))
        );
    }

    #[test]
    fn locked_last_chat_is_not_restored_from_a_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings {
            last_chat: Some("locked".into()),
            ..Default::default()
        };
        let (mut app, events) = App::headless(AppDirs::under(root.path()), settings);
        let mut chat = Chat::new("locked".into(), "Fixture".into());
        chat.locked = true;
        events.send(Event::Chats(vec![chat])).unwrap();
        app.handle_events();
        assert!(app.open_chat.is_none());
        assert!(app.conversations.is_empty());
    }

    /// Disabling restoration leaves the saved chat intact and permits normal
    /// opening during the session and restoration after re-enabling it.
    #[test]
    fn startup_can_leave_the_remembered_chat_closed_until_reenabled() {
        let root = tempfile::tempdir().unwrap();
        let settings = Settings {
            last_chat: Some("chat".into()),
            restore_last_chat: false,
            ..Default::default()
        };
        let (mut app, events) = App::headless(AppDirs::under(root.path()), settings.clone());
        assert_eq!(app.page, Page::Chats);
        assert!(app.open_chat.is_none());
        assert_eq!(app.account().settings.last_chat.as_deref(), Some("chat"));
        events
            .send(Event::Chats(vec![Chat::new(
                "chat".into(),
                "Fixture".into(),
            )]))
            .unwrap();
        app.handle_events();
        assert!(app.open_chat.is_none());
        app.open_chat("chat".into());
        assert_eq!(app.open_chat.as_deref(), Some("chat"));

        let enabled = Settings {
            restore_last_chat: true,
            ..settings
        };
        let (app, _) = App::headless(AppDirs::under(root.path()), enabled);
        assert_eq!(app.open_chat.as_deref(), Some("chat"));
    }

    #[test]
    fn pinned_order_survives_new_messages_and_legacy_pin_ties() {
        let mut app = app();
        for (id, pin, activity) in [("a", 100, 999), ("b", 200, 1), ("c", 0, 0), ("d", 0, 900)] {
            let mut chat = Chat::new(id.into(), id.into());
            chat.pinned = true;
            chat.pinned_at = pin;
            chat.last_activity = activity;
            app.chats.push(chat);
        }
        let order = |app: &App| {
            app.visible_chats()
                .iter()
                .map(|chat| chat.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&app), ["b", "a", "c", "d"]);
        app.chats[0].last_activity = 10_000;
        app.chats[2].last_activity = 20_000;
        assert_eq!(order(&app), ["b", "a", "c", "d"]);
    }

    #[test]
    fn chat_and_contact_search_ignore_composed_and_decomposed_accents() {
        let mut app = app();
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ángel".into()));
        let contact = Contact {
            id: "2@s.whatsapp.net".into(),
            full_name: Some("A\u{301}ngel".into()),
            first_name: None,
            push_name: None,
        };
        app.contacts.insert(contact.id.clone(), contact);
        for query in ["angel", "ÁNGEL", "A\u{301}ngel"] {
            app.search = query.into();
            assert_eq!(app.visible_chats().len(), 1, "{query}");
            assert_eq!(app.matching_contacts().len(), 1, "{query}");
        }
        assert_eq!(app.chats[0].name, "Ángel");
        app.search = "bob".into();
        assert!(app.visible_chats().is_empty());
        assert!(app.matching_contacts().is_empty());
    }

    #[test]
    fn closing_a_chat_preserves_its_text_draft() {
        let mut app = app();
        let id = "1@s.whatsapp.net";
        app.chats.push(Chat::new(id.into(), "Ada".into()));
        app.open_chat(id.into());
        app.composer = "unfinished message".into();
        app.actions.push(Action::CloseChat);
        app.apply_actions(&egui::Context::default());
        assert!(app.open_chat.is_none());
        app.open_chat(id.into());
        assert_eq!(app.composer, "unfinished message");
    }

    #[test]
    fn a_saved_speed_between_choices_snaps_to_one() {
        let root = std::env::temp_dir().join(format!("zapfast-speed-{}", std::process::id()));
        let settings = Settings {
            voice_speed: 1.3,
            ..Settings::default()
        };
        let app = App::headless(AppDirs::under(&root), settings).0;
        assert_eq!(app.player.speed(), 1.25);
        assert_eq!(app.settings.voice_speed, 1.25);
    }

    #[test]
    fn direct_speed_selection_reaches_player_and_settings() {
        let mut app = app();
        let ctx = egui::Context::default();

        for speed in crate::audio::SPEEDS {
            app.apply(Action::SetVoiceSpeed(speed), &ctx);
            assert_eq!(app.player.speed(), speed);
            assert_eq!(app.settings.voice_speed, speed);
        }

        app.apply(Action::SetVoiceSpeed(4.0), &ctx);
        assert_eq!(app.settings.voice_speed, crate::audio::SPEEDS[4]);
    }

    #[test]
    fn video_menu_speeds_apply_without_changing_the_remembered_voice_speed() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.apply(Action::SetVoiceSpeed(1.5), &ctx);
        for speed in crate::audio::SPEEDS {
            app.apply(Action::SetVideoSpeed(speed), &ctx);
            assert_eq!(app.video.speed(), speed);
            assert_eq!(app.player.speed(), 1.5);
            assert_eq!(app.settings.voice_speed, 1.5);
        }
        app.apply(Action::SetVideoSpeed(1.25), &ctx);
        app.apply(Action::CycleVideoSpeed, &ctx);
        assert_eq!(app.video.speed(), 1.5);
        app.apply(Action::SetVideoSpeed(1.75), &ctx);
        app.apply(Action::CycleVideoSpeed, &ctx);
        assert_eq!(app.video.speed(), 2.0);
        app.apply(Action::CycleVideoSpeed, &ctx);
        assert_eq!(app.video.speed(), 1.0);
    }

    #[test]
    fn automatic_export_migration_persists_one_cutoff_and_keeps_full_archive_recovery() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(AppDirs::under(root.path()), Settings::default()).0;
        app.account_mut().settings.automatic_download_since = None;
        app.account_mut().settings.background_history = true;
        app.account_mut().settings.archive_media = true;
        app.report_background_history();
        let cutoff = app.account().settings.automatic_download_since.unwrap();
        let saved = crate::settings::AccountSettings::load(&app.account().dirs.settings_file());
        assert_eq!(saved.automatic_download_since, Some(cutoff));
        assert!(saved.background_history);
        assert!(saved.archive_media);
        app.report_background_history();
        assert_eq!(
            app.account().settings.automatic_download_since,
            Some(cutoff)
        );
        app.account_mut().settings.auto_download = false;
        app.report_background_history();
        assert_eq!(
            app.account().settings.automatic_download_since,
            Some(cutoff)
        );
        assert!(!app.account().reported_exports.unwrap().enabled);
        assert!(app.account().settings.archive_media);
    }

    #[test]
    fn opening_a_chat_keeps_drafts_apart() {
        let mut app = app();
        app.chats
            .push(Chat::new("1@s.whatsapp.net".into(), "Ada".into()));
        app.chats
            .push(Chat::new("2@s.whatsapp.net".into(), "Bob".into()));
        app.open_chat("1@s.whatsapp.net".into());
        app.composer = "hello ada".into();
        app.open_chat("2@s.whatsapp.net".into());
        assert_eq!(app.composer, "");
        app.open_chat("1@s.whatsapp.net".into());
        assert_eq!(app.composer, "hello ada");
        assert_eq!(app.settings.last_chat.as_deref(), Some("1@s.whatsapp.net"));
    }

    #[test]
    fn selected_mentions_become_wire_tokens_and_context_jids() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);
        app.composer_mentions.push(ComposerMention {
            id: member.into(),
            name: "Mira Example".into(),
        });

        let (text, mentions) = app.encode_composer_mentions(chat_id, "hello @Mira Example".into());

        assert_eq!(text, "hello @491702222222");
        assert_eq!(mentions, vec![member]);
    }

    #[test]
    fn existing_wire_mentions_survive_an_edit() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);

        let (text, mentions) = app.encode_composer_mentions(chat_id, "still @491702222222!".into());

        assert_eq!(text, "still @491702222222!");
        assert_eq!(mentions, vec![member]);
    }

    #[test]
    fn editing_a_selected_name_drops_its_mention() {
        let mut app = app();
        let chat_id = "123@g.us";
        let member = "491702222222@s.whatsapp.net";
        let mut chat = Chat::new(chat_id.into(), "Group".into());
        chat.participants.push(member.into());
        app.chats.push(chat);
        app.composer_mentions.push(ComposerMention {
            id: member.into(),
            name: "Mira".into(),
        });

        let (text, mentions) = app.encode_composer_mentions(chat_id, "hello @Miranda".into());

        assert_eq!(text, "hello @Miranda");
        assert!(mentions.is_empty());
    }

    #[test]
    fn dismissing_shortcut_hints_persists_and_focusing_keeps_the_draft() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.composer = "Unsent draft".into();
        app.focus_search = true;
        app.apply(Action::SetShortcutHints(false), &ctx);
        assert!(!app.settings.show_shortcut_hints);
        assert!(app.settings_dirty);
        app.apply(Action::SetShortcutHints(true), &ctx);
        assert!(
            app.settings.show_shortcut_hints,
            "the shortcuts dialog brings them back"
        );
        app.apply(Action::FocusComposer, &ctx);
        assert!(app.focus_composer);
        assert!(!app.focus_search);
        assert_eq!(app.composer, "Unsent draft");
    }

    #[test]
    fn returning_to_a_conversation_refocuses_the_composer() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.page = Page::Settings;

        app.apply(Action::Open(Page::Chats), &ctx);

        assert!(app.focus_composer);
    }

    #[test]
    fn recreating_the_window_refocuses_the_composer() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());

        app.attach(&ctx);

        assert!(app.focus_composer);
    }

    /// Plain egui text (a label, a tooltip) gets its emoji in colour from
    /// the plugin, while a line laid out with placeholders gets exactly one
    /// picture, its own, and no second one from the plugin.
    #[test]
    fn plain_text_emoji_are_coloured_without_painting_placeholders_twice() {
        use egui::epaint::{Shape, TextureId};
        fn pictures(shape: &Shape) -> usize {
            match shape {
                Shape::Vec(shapes) => shapes.iter().map(pictures).sum(),
                Shape::Mesh(mesh) if mesh.texture_id != TextureId::default() => 1,
                _ => 0,
            }
        }
        assert!(
            crate::emoji::available(),
            "the bundled font is always there"
        );
        let mut app = app();
        let ctx = egui::Context::default();
        app.attach(&ctx);
        let count = |draw: &dyn Fn(&mut egui::Ui)| {
            let mut total = 0;
            // A new area is measured in its first frame and drawn after.
            for _ in 0..2 {
                let mut output = ctx.run_ui(egui::RawInput::default(), |ui| draw(ui));
                output.textures_delta.clear();
                total = output
                    .shapes
                    .iter()
                    .map(|clipped| pictures(&clipped.shape))
                    .sum::<usize>();
            }
            total
        };
        assert_eq!(count(&|ui| drop(ui.label("plain 😀"))), 1, "a label");
        assert_eq!(
            count(&|ui| {
                egui::Area::new(egui::Id::new("tip"))
                    .order(egui::Order::Tooltip)
                    .show(ui.ctx(), |ui| ui.label("tip 👍🏽"));
            }),
            1,
            "a tooltip"
        );
        assert_eq!(
            count(&|ui| {
                let line = crate::ui::widgets::line(
                    ui,
                    "placeholder 😀",
                    egui::FontId::proportional(14.0),
                    egui::Color32::WHITE,
                    300.0,
                    1,
                );
                line.paint(ui, egui::pos2(0.0, 0.0), egui::Color32::WHITE);
            }),
            1,
            "a placeholder line"
        );
    }

    #[test]
    fn returning_to_a_conversation_does_not_interrupt_search() {
        let mut app = app();
        let ctx = egui::Context::default();
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.page = Page::Settings;
        app.search = "ada".into();

        app.apply(Action::Open(Page::Chats), &ctx);

        assert!(!app.focus_composer);

        app.search.clear();
        ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("chat-search")));
        app.dialog = Some(Dialog::About);
        app.apply(Action::CloseDialog, &ctx);
        assert!(!app.focus_composer);
    }

    #[test]
    fn names_fall_back_from_contacts_to_phones() {
        let mut app = app();
        app.contacts.insert(
            "1@s.whatsapp.net".into(),
            Contact {
                id: "1@s.whatsapp.net".into(),
                full_name: Some("Ada".into()),
                first_name: None,
                push_name: None,
            },
        );
        assert_eq!(app.display_name("1@s.whatsapp.net"), "Ada");
        assert_eq!(
            app.display_name("393331234567@s.whatsapp.net"),
            "+39 333 123 4567"
        );
        assert_eq!(app.display_name("42@lid"), "Unknown");
        app.contacts.insert(
            "42@lid".into(),
            Contact {
                id: "42@lid".into(),
                full_name: None,
                first_name: None,
                push_name: Some("Bob".into()),
            },
        );
        assert_eq!(app.display_name("42@lid"), "~Bob");
        app.me = Some("42@lid".into());
        assert_eq!(app.display_name("42@lid"), "You");
    }
    /// Private-reply refusals preserve the owning account's quote, text and voice clip.
    #[test]
    fn hidden_account_private_reply_recovery_keeps_the_visible_composer_unchanged() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let chat = "recipient@s.whatsapp.net";
        for account in &mut app.accounts {
            account
                .chats
                .push(Chat::new(chat.into(), "Synthetic recipient".into()));
            account.open_chat = Some(chat.into());
        }
        let (backend, events) = Backend::detached();
        app.accounts[1].backend = backend;
        let quote = crate::model::ReplyTarget {
            id: "source-message".into(),
            chat: Some("source@g.us".into()),
            destination_generation: 0,
        };
        for unsent in [
            Unsent::Text("Hidden correction".into()),
            Unsent::Voice(vec![0.25]),
        ] {
            events
                .send(Event::SendRefused {
                    chat: chat.into(),
                    quoting: Some(quote.clone()),
                    unsent,
                    reason: Refusal::QuoteUnavailable,
                    mentions: Vec::new(),
                })
                .unwrap();
        }
        app.handle_events();
        assert!(app.composer.is_empty());
        assert!(app.reply_to.is_none());
        assert!(app.unsent_voice.is_none());
        assert_eq!(
            app.accounts[1]
                .unsent_voice
                .as_ref()
                .map(|(id, _)| id.as_str()),
            Some(chat)
        );
        let second = app.accounts[1].id.clone();
        app.switch_account(&second);
        assert_eq!(app.composer, "Hidden correction");
        assert_eq!(app.reply_to, Some(quote));
        assert_eq!(app.unsent_voice, Some((chat.into(), vec![0.25])));
    }
    fn browser_link(text: &str) -> crate::chat_link::ChatLink {
        crate::chat_link::ChatLink::parse(&format!("whatsapp://send?phone=15550100123&text={text}"))
            .unwrap()
    }
    #[test]
    fn a_browser_link_waits_for_the_archive_and_never_sends_the_draft() {
        let mut app = app();
        let (backend, mut commands, events) = Backend::recording_with_events();
        app.backend = backend;
        let ctx = egui::Context::default();
        app.open_chat = Some("15550100124@s.whatsapp.net".into());
        app.composer = "Existing draft in another chat".into();
        app.open_chat_link(browser_link(
            "Hola%2C+informaci%C3%B3n+%26+precio+%F0%9F%98%8A",
        ));
        app.handle_chat_links(&ctx);
        assert_eq!(app.composer, "Existing draft in another chat");
        assert_eq!(app.pending_chat_links.len(), 1);

        events
            .send(Event::Chats(vec![Chat::new(
                "15550100124@s.whatsapp.net".into(),
                "Previous chat".into(),
            )]))
            .unwrap();
        app.handle_events();
        app.handle_chat_links(&ctx);
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat.as_deref(), Some("15550100123@s.whatsapp.net"));
        assert_eq!(app.composer, "Hola, información & precio 😊");
        assert_eq!(
            app.drafts["15550100124@s.whatsapp.net"],
            "Existing draft in another chat"
        );
        assert!(app.pending_chat_links.is_empty());
        let mut saved = false;
        while let Ok(command) = commands.try_recv() {
            assert!(!matches!(
                command,
                Command::SendText { .. } | Command::EditText { .. }
            ));
            if let Command::SaveDraft { chat, text } = command
                && chat == "15550100123@s.whatsapp.net"
            {
                assert_eq!(text, app.composer);
                saved = true;
            }
        }
        assert!(saved, "the prepared text is persisted only as a draft");
    }
    #[test]
    fn browser_links_preserve_existing_drafts_for_the_same_recipient() {
        let mut app = app();
        app.chats_loaded = true;
        app.chats.push(Chat::new(
            "15550100123@s.whatsapp.net".into(),
            "Peer".into(),
        ));
        app.open_chat = Some("15550100123@s.whatsapp.net".into());
        app.composer = "Already typed".into();
        let ctx = egui::Context::default();
        app.open_chat_link(browser_link("Link+draft"));
        app.handle_chat_links(&ctx);
        assert_eq!(app.composer, "Already typed\nLink draft");
        assert_eq!(app.chats.len(), 1);
        app.open_chat_link(browser_link(""));
        app.handle_chat_links(&ctx);
        assert_eq!(app.composer, "Already typed\nLink draft");
    }
    #[test]
    fn browser_drafts_do_not_edit_or_quote_an_existing_message() {
        let mut app = app();
        app.chats_loaded = true;
        app.chats.push(Chat::new(
            "15550100123@s.whatsapp.net".into(),
            "Peer".into(),
        ));
        app.open_chat = Some("15550100123@s.whatsapp.net".into());
        app.editing = Some("message-id".into());
        app.reply_to = Some("quoted-id".into());
        app.composer = "Message being edited".into();
        app.open_chat_link(browser_link("Fresh+draft"));
        app.handle_chat_links(&egui::Context::default());
        assert_eq!(app.composer, "Fresh draft");
        assert!(app.editing.is_none());
        assert!(app.reply_to.is_none());
    }
    #[test]
    fn browser_links_wait_for_the_app_unlock_and_cannot_open_locked_chats() {
        let mut app = app();
        app.chats_loaded = true;
        app.app_lock.lock();
        let ctx = egui::Context::default();
        app.open_chat_link(browser_link("Waiting+draft"));
        app.handle_chat_links(&ctx);
        assert!(app.open_chat.is_none());
        assert!(app.composer.is_empty());
        assert_eq!(app.pending_chat_links.len(), 1);
        app.app_lock.release();
        app.handle_chat_links(&ctx);
        assert_eq!(app.composer, "Waiting draft");

        app.chats[0].locked = true;
        app.open_chat = None;
        app.composer.clear();
        app.open_chat_link(browser_link("Hidden+draft"));
        app.handle_chat_links(&ctx);
        assert!(app.open_chat.is_none());
        assert!(app.composer.is_empty());
    }
    #[test]
    fn a_waiting_browser_link_keeps_the_account_it_was_opened_on() {
        let directory = tempfile::tempdir().unwrap();
        let (mut app, _) = two_accounts(directory.path());
        let ctx = egui::Context::default();
        let second = app.accounts[1].id.clone();
        app.switch_account(&second);
        app.open_chat_link(browser_link("Second+account"));
        app.switch_account(&AccountId::first());
        app.accounts[1].chats_loaded = true;
        app.handle_chat_links(&ctx);
        assert_eq!(app.account().id, second);
        assert_eq!(app.composer, "Second account");
        assert!(app.accounts[0].chats.is_empty());
    }
}

#[cfg(test)]
mod name_tests {
    use super::*;
    use crate::model::{Contact, Content, Delivery, MentionRef};

    fn app() -> App {
        let root = std::env::temp_dir().join(format!("zapfast-names-{}", std::process::id()));
        let (mut app, _events) = App::headless(AppDirs::under(&root), Settings::default());
        app.me = Some("15550001111@s.whatsapp.net".into());
        app.me_name = Some("Carmine".into());
        app.contacts.insert(
            "1@s.whatsapp.net".into(),
            Contact {
                id: "1@s.whatsapp.net".into(),
                full_name: Some("Ada Lovelace".into()),
                first_name: None,
                push_name: Some("Ada".into()),
            },
        );
        app.contacts.insert(
            "2@s.whatsapp.net".into(),
            Contact {
                id: "2@s.whatsapp.net".into(),
                full_name: None,
                first_name: None,
                push_name: Some("Bob".into()),
            },
        );
        app
    }

    #[test]
    fn group_members_go_by_their_whole_saved_first_name() {
        let mut app = app();
        let mut chat = Chat::new("fixture@g.us".into(), "Group".into());
        for (index, full_name, first_name) in [
            (0, "My Dih", Some("My Dih")),
            (1, "Grace Hopper", Some("Grace")),
            (2, "Mary Ann Evans", None),
            (3, "Stray Name", Some("")),
        ] {
            let id = format!("1555000001{index}@s.whatsapp.net");
            app.contacts.insert(
                id.clone(),
                Contact {
                    id: id.clone(),
                    full_name: Some(full_name.into()),
                    first_name: first_name.map(Into::into),
                    push_name: None,
                },
            );
            chat.participants.push(id);
        }
        // Without a saved first name (a profile name, or a contact synced
        // before first names were kept) the line keeps the first word.
        chat.participants.push("2@s.whatsapp.net".into());
        app.contacts.get_mut("2@s.whatsapp.net").unwrap().push_name = Some("Bob Builder".into());
        assert_eq!(
            app.participant_names(&chat),
            "Bob, Grace, Mary, My Dih, Stray"
        );
        // The sender before a group's last message goes by the same name.
        assert_eq!(
            app.short_name("15550000010@s.whatsapp.net", "My Dih"),
            "My Dih"
        );
        assert_eq!(
            app.short_name("15550000011@s.whatsapp.net", "Grace Hopper"),
            "Grace"
        );
        assert_eq!(app.short_name("2@s.whatsapp.net", "~Bob Builder"), "~Bob");
        assert_eq!(
            app.short_name("3@s.whatsapp.net", "+1 555 0100"),
            "+1 555 0100"
        );
        let me = app.me.clone().unwrap();
        assert_eq!(app.short_name(&me, "You"), "You");
    }

    #[test]
    fn the_contact_editor_opens_with_the_saved_first_name_whole() {
        let mut app = app();
        let ctx = egui::Context::default();
        let saved = |app: &mut App, id: &str, full: &str, first: Option<&str>| {
            app.contacts.insert(
                id.into(),
                Contact {
                    id: id.into(),
                    full_name: Some(full.into()),
                    first_name: first.map(Into::into),
                    push_name: None,
                },
            );
        };
        let edit = |app: &mut App, id: &str, name: &str| {
            app.apply(
                Action::EditContact {
                    id: id.into(),
                    name: name.into(),
                },
                &ctx,
            );
            app.contact_edit.take().expect("the editor opens")
        };
        let pair = |first: &str, last: &str| (first.to_owned(), last.to_owned());
        // #314: a first name of two words, without and with a last name.
        saved(
            &mut app,
            "15550000020@s.whatsapp.net",
            "first second",
            Some("first second"),
        );
        assert_eq!(
            edit(&mut app, "15550000020@s.whatsapp.net", "first second"),
            pair("first second", "")
        );
        saved(
            &mut app,
            "15550000021@s.whatsapp.net",
            "first second third",
            Some("first second"),
        );
        assert_eq!(
            edit(&mut app, "15550000021@s.whatsapp.net", "first second third"),
            pair("first second", "third")
        );
        // Without a saved first name the whole name stays first, so saving
        // it unchanged cannot shorten the first name to one word.
        saved(&mut app, "15550000022@s.whatsapp.net", "My Dih", None);
        assert_eq!(
            edit(&mut app, "15550000022@s.whatsapp.net", "My Dih"),
            pair("My Dih", "")
        );
        assert_eq!(
            compose_name("My Dih", ""),
            (Some("My Dih".into()), Some("My Dih".into()))
        );
    }

    #[test]
    fn unnamed_and_cached_group_titles_share_counted_participant_names() {
        let mut app = app();
        let mut chat = Chat::new("fixture@g.us".into(), "Group".into());
        for (index, name) in [
            "Andrea North",
            "Andrea South",
            "Andrea West",
            "Giacomo East",
        ]
        .iter()
        .enumerate()
        {
            let id = format!("1555000000{index}@s.whatsapp.net");
            app.contacts.insert(
                id.clone(),
                Contact {
                    id: id.clone(),
                    full_name: Some((*name).into()),
                    first_name: name.split(' ').next().map(Into::into),
                    push_name: Some((*name).into()),
                },
            );
            chat.participants.push(id);
        }
        // Duplicate entries for the same identity must not inflate the count.
        chat.participants.push(chat.participants[0].clone());
        chat.participants.push(app.me.clone().unwrap());
        assert_eq!(app.participant_names(&chat), "Andrea x3, Giacomo, You");
        assert_eq!(app.chat_title(&chat), app.participant_names(&chat));
        chat.name.clear();
        assert_eq!(app.chat_title(&chat), app.participant_names(&chat));
        chat.name = "Group".into();
        chat.group_subject_known = true;
        assert_eq!(
            app.chat_title(&chat),
            "Group",
            "an authoritative title is not a placeholder"
        );
        chat.name = "Weekend plans".into();
        assert_eq!(app.chat_title(&chat), "Weekend plans");
        chat.name.clear();
        chat.participants.clear();
        assert_eq!(
            app.chat_title(&chat),
            "Group",
            "no invented members while metadata is missing"
        );
    }

    #[test]
    fn saved_names_come_first_and_profile_names_fill_in() {
        let app = app();
        assert_eq!(app.display_name("1@s.whatsapp.net"), "Ada Lovelace");
        assert_eq!(app.display_name("2@s.whatsapp.net"), "~Bob");
        assert_eq!(
            app.display_name_or("3@s.whatsapp.net", Some("Cy")),
            "~Cy",
            "a name the message carried, for someone unknown"
        );
    }

    /// Checks that mention names and preview tokens resolve consistently in synthetic messages.
    #[test]
    fn mentions_use_our_own_name_and_previews_resolve_tokens() {
        let app = app();
        assert_eq!(app.mention_name("15550001111@s.whatsapp.net"), "Carmine");
        assert_eq!(app.display_name("15550001111@s.whatsapp.net"), "You");
        assert_eq!(
            app.resolve_mention_tokens("palestra oggi? @15550001111 e @1 ?"),
            "palestra oggi? @Carmine e @1 ?",
            "a short number is not a mention"
        );
        let message = Message {
            id: "m".into(),
            chat: "1@s.whatsapp.net".into(),
            sender: "1@s.whatsapp.net".into(),
            sender_name: None,
            from_me: false,
            timestamp: 0,
            content: Content::text("ciao @15550001111"),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            history_order: None,
            edited: false,
            mentions: vec![MentionRef {
                user: "15550001111".into(),
                id: "15550001111@s.whatsapp.net".into(),
            }],
            forwarded: false,
            thumbnail: None,
        };
        assert_eq!(app.message_text(&message), "ciao @Carmine");

        // A caption mentioning a privacy id is named through the message's
        // mentions, which carry the canonical id, in notifications and
        // search results alike.
        let photo = Message {
            content: Content::Image {
                motion: None,
                caption: Some("@987654321012345 looks sharp".into()),
                media: Media {
                    mime: "image/jpeg".into(),
                    size: 100,
                    width: None,
                    height: None,
                    path: None,
                    state: MediaState::Idle,
                    album: None,
                },
            },
            mentions: vec![MentionRef {
                user: "987654321012345".into(),
                id: "15550001111@s.whatsapp.net".into(),
            }],
            ..message
        };
        assert_eq!(app.message_text(&photo), "Photo: @Carmine looks sharp");
        assert_eq!(
            app.preview_line("@987654321012345 looks sharp", &photo),
            "@Carmine looks sharp"
        );
    }
}

#[cfg(test)]
mod app_lock_tests {
    use super::*;
    use crate::app_lock::{Clock, Form, FormError, FormMode};
    use std::sync::{Arc, Mutex};

    const PASSWORD: &str = "open-sesame";
    const CHAT: &str = "1@s.whatsapp.net";

    fn settings(password: Option<&str>) -> Settings {
        Settings {
            app_lock_hash: password.map(crate::app_lock::verifier),
            ..Settings::default()
        }
    }

    fn app_with(settings: Settings) -> App {
        let root = std::env::temp_dir().join(format!(
            "zapfast-app-lock-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        App::headless(AppDirs::under(&root), settings).0
    }

    /// An app with the lock set but not locked, as after unlocking.
    fn unlocked_app() -> App {
        let mut app = app_with(settings(Some(PASSWORD)));
        app.app_lock.release();
        app
    }

    fn manual_clock(app: &mut App) -> Arc<Mutex<Instant>> {
        let now = Arc::new(Mutex::new(Instant::now()));
        let read = Arc::clone(&now);
        let clock: Clock = Arc::new(move || *read.lock().unwrap());
        app.app_lock.set_clock(clock);
        now
    }

    fn advance(now: &Mutex<Instant>, by: Duration) {
        *now.lock().unwrap() += by;
    }

    /// Waits for the password thread and applies its answer.
    fn finish(app: &mut App, ctx: &egui::Context) {
        let outcome = app.app_lock.wait().expect("a finished check");
        app.app_lock_outcome(outcome);
        app.apply_actions(ctx);
    }

    fn try_password(app: &mut App, ctx: &egui::Context, password: &str) {
        app.app_lock.entry = password.into();
        app.apply(Action::UnlockApp, ctx);
        finish(app, ctx);
    }

    fn incoming(app: &mut App) -> Message {
        let mut chat = Chat::new(CHAT.into(), "Ada Lovelace".into());
        chat.unread = 1;
        app.chats = vec![chat];
        Message {
            id: "m1".into(),
            chat: CHAT.into(),
            sender: CHAT.into(),
            sender_name: None,
            from_me: false,
            timestamp: crate::util::now(),
            content: Content::text("the engine is ready"),
            status: Delivery::None,
            delivered_at: None,
            read_at: None,
            quoted: None,
            reactions: Vec::new(),
            history_order: None,
            edited: false,
            mentions: Vec::new(),
            forwarded: false,
            thumbnail: None,
        }
    }

    #[test]
    fn in_app_notifications_share_suppression_privacy_expiry_and_account_routing() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        let mut message = incoming(&mut app);
        app.window_hidden = false;
        app.window_focused = true;
        app.open_chat = None;
        app.maybe_notify(CHAT, &message);
        assert!(app.in_app_notifications.is_empty(), "default off");
        app.account_mut().settings.in_app_notifications = true;
        app.chats[0].muted_until = Some(crate::util::now() + 3600);
        app.maybe_notify(CHAT, &message);
        assert!(app.in_app_notifications.is_empty());
        app.chats[0].muted_until = None;
        app.chats[0].locked = true;
        app.maybe_notify(CHAT, &message);
        assert!(app.in_app_notifications.is_empty());
        app.chats[0].locked = false;
        app.open_chat = Some(CHAT.into());
        app.maybe_notify(CHAT, &message);
        assert!(
            app.in_app_notifications.is_empty(),
            "the chat being read stays quiet"
        );
        app.open_chat = None;
        app.account_mut().settings.notification_sender = false;
        for index in 0..6 {
            message.id = format!("synthetic-{index}");
            app.maybe_notify(CHAT, &message);
        }
        assert_eq!(
            app.in_app_notifications.len(),
            crate::ui::notification_overlay::LIMIT
        );
        assert!(
            app.in_app_notifications
                .iter()
                .all(|notification| !notification.title.contains("Ada")
                    && !notification.body.contains("engine"))
        );
        let expired = app.in_app_notifications[0].id;
        app.in_app_notifications[0].expires = std::time::Instant::now() - Duration::from_secs(1);
        app.apply(Action::OpenInAppNotification(expired), &ctx);
        assert!(app.actions.is_empty(), "expired cards cannot route an open");
        let live = app.in_app_notifications.last().unwrap().id;
        let origin = app.account().id.clone();
        app.add_account();
        assert_ne!(app.account().id, origin);
        app.apply(Action::OpenInAppNotification(live), &ctx);
        assert_eq!(app.account().id, origin);
        assert!(app.actions.iter().any(|action| matches!(action, Action::OpenMessage { chat, message } if chat == CHAT && message == "synthetic-5")));
        app.in_app_notifications.clear();
        app.lock_app();
        app.maybe_notify(CHAT, &message);
        assert!(
            app.in_app_notifications.is_empty(),
            "app lock never queues private cards"
        );
    }

    #[test]
    fn it_starts_locked_only_with_a_password() {
        assert!(app_with(settings(Some(PASSWORD))).app_lock.is_locked());
        assert!(!app_with(settings(None)).app_lock.is_locked());
    }

    #[test]
    fn inactivity_locks_after_the_chosen_time_even_while_hidden() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        app.settings.app_lock_after = crate::settings::AutoLock::OneMinute;
        let now = manual_clock(&mut app);
        advance(&now, Duration::from_secs(59));
        app.background_frame(&ctx);
        assert!(!app.app_lock.is_locked());
        // Input restarts the count.
        app.app_lock.note_input();
        advance(&now, Duration::from_secs(59));
        app.background_frame(&ctx);
        assert!(!app.app_lock.is_locked());
        // Hidden in the tray, only the headless loop runs, and it locks.
        app.window_gone();
        advance(&now, Duration::from_secs(2));
        app.background_frame(&ctx);
        assert!(app.app_lock.is_locked());
    }

    #[test]
    fn a_longer_choice_waits_longer_and_no_password_never_locks() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        app.apply(
            Action::SetAutoLock(crate::settings::AutoLock::OneHour),
            &ctx,
        );
        let now = manual_clock(&mut app);
        advance(&now, Duration::from_secs(59 * 60));
        app.background_frame(&ctx);
        assert!(!app.app_lock.is_locked());
        advance(&now, Duration::from_secs(60));
        app.background_frame(&ctx);
        assert!(app.app_lock.is_locked());

        let mut app = app_with(settings(None));
        let now = manual_clock(&mut app);
        advance(&now, Duration::from_secs(24 * 60 * 60));
        app.background_frame(&ctx);
        app.apply(Action::LockApp, &ctx);
        assert!(!app.app_lock.is_locked());
    }

    #[test]
    fn window_input_counts_as_use() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        app.settings.app_lock_after = crate::settings::AutoLock::OneMinute;
        app.attach(&ctx);
        let now = manual_clock(&mut app);
        advance(&now, Duration::from_secs(50));
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(egui::pos2(10.0, 10.0))],
                ..Default::default()
            },
            |ui| app.frame_ui(ui),
        );
        output.textures_delta.clear();
        advance(&now, Duration::from_secs(50));
        app.background_frame(&ctx);
        assert!(!app.app_lock.is_locked());
    }

    #[test]
    fn lock_now_stops_media_and_closes_the_locked_folder() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        app.locked_folder = true;
        app.chat_lock_entry = "half typed".into();
        app.window_focused = true;
        app.apply(Action::LockApp, &ctx);
        assert!(app.app_lock.is_locked());
        assert!(!app.locked_folder);
        assert!(app.chat_lock_entry.is_empty());
        assert!(!app.window_focused);
    }

    #[test]
    fn a_right_password_unlocks_and_wrong_ones_back_off() {
        let ctx = egui::Context::default();
        let mut app = app_with(settings(Some(PASSWORD)));
        let now = manual_clock(&mut app);
        for _ in 0..3 {
            try_password(&mut app, &ctx, "not the password");
            assert!(app.app_lock.is_locked());
            assert!(app.app_lock.wrong);
        }
        assert_eq!(app.app_lock.wait_left(), Some(Duration::from_secs(1)));
        // A try during the wait is not made at all.
        app.app_lock.entry = PASSWORD.into();
        app.apply(Action::UnlockApp, &ctx);
        assert!(!app.app_lock.checking());
        advance(&now, Duration::from_secs(1));
        try_password(&mut app, &ctx, PASSWORD);
        assert!(!app.app_lock.is_locked());
        assert!(app.app_lock.entry.is_empty());
    }

    #[test]
    fn while_locked_actions_do_nothing_and_a_clicked_notification_waits() {
        let ctx = egui::Context::default();
        let mut app = app_with(settings(Some(PASSWORD)));
        app.chats = vec![Chat::new(CHAT.into(), "Ada".into())];
        app.open_chat = None;
        app.apply(Action::OpenChat(CHAT.into()), &ctx);
        app.apply(Action::Open(Page::Settings), &ctx);
        app.apply(Action::ShowDialog(Dialog::NewChat), &ctx);
        app.apply(Action::Unlink, &ctx);
        assert_eq!(app.open_chat, None);
        assert_eq!(app.page, Page::Chats);
        assert_eq!(app.dialog, None);

        app.notification_opens
            .lock()
            .unwrap()
            .push(crate::notify::NotificationTarget {
                account: crate::model::AccountId::first(),
                chat: CHAT.into(),
                message: "m1".into(),
            });
        app.handle_notification_opens();
        app.apply_actions(&ctx);
        assert_eq!(app.open_chat, None, "not while locked");
        try_password(&mut app, &ctx, PASSWORD);
        assert_eq!(app.open_chat.as_deref(), Some(CHAT), "opened once unlocked");
    }

    fn attention_requested(ctx: &egui::Context) -> bool {
        ctx.viewport(|viewport| {
            viewport
                .commands
                .iter()
                .any(|command| matches!(command, egui::ViewportCommand::RequestUserAttention(_)))
        })
    }

    /// A notification highlights the taskbar entry of an open, unfocused
    /// window, once; a hidden window has no entry to highlight.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_notification_highlights_an_unfocused_window_in_the_taskbar() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.window_hidden = false;
        app.window_focused = false;
        app.maybe_notify(CHAT, &message);
        app.request_attention_on(&ctx, false);
        assert!(attention_requested(&ctx));

        let ctx = egui::Context::default();
        app.request_attention_on(&ctx, false);
        assert!(!attention_requested(&ctx), "only once per notification");

        app.window_hidden = true;
        app.maybe_notify(CHAT, &message);
        app.request_attention_on(&ctx, false);
        assert!(!attention_requested(&ctx));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn gnome_gets_no_duplicate_window_attention_notification() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.window_hidden = false;
        app.window_focused = false;
        app.maybe_notify(CHAT, &message);
        app.request_attention_on(&ctx, true);
        assert!(!attention_requested(&ctx));
        app.request_attention_on(&ctx, false);
        assert!(
            !attention_requested(&ctx),
            "the suppressed request is spent"
        );
    }

    /// Behind the app lock `window_focused` stays false, but a lock screen
    /// that has the focus is no window to point at.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_focused_lock_screen_is_not_highlighted() {
        let ctx = egui::Context::default();
        let mut input = egui::RawInput::default();
        input.viewports.insert(
            egui::ViewportId::ROOT,
            egui::ViewportInfo {
                focused: Some(true),
                ..Default::default()
            },
        );
        ctx.run_ui(input, |_| {}).textures_delta.clear();
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.window_hidden = false;
        app.lock_app();
        assert!(!app.window_focused);
        app.maybe_notify(CHAT, &message);
        app.request_attention(&ctx);
        assert!(!attention_requested(&ctx));
    }

    #[test]
    fn reading_the_chat_asks_for_no_attention() {
        let ctx = egui::Context::default();
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.window_hidden = false;
        app.window_focused = true;
        app.page = Page::Chats;
        app.open_chat = Some(CHAT.into());
        app.maybe_notify(CHAT, &message);
        app.request_attention(&ctx);
        assert!(!attention_requested(&ctx));
    }

    #[test]
    fn notifications_while_locked_carry_no_chat_sender_text_or_picture() {
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.chats[0].notification_sound = Some(crate::settings::NotificationSound::Alert);
        app.maybe_notify(CHAT, &message);
        let shown = app.notifications.shown.last().unwrap().clone();
        assert_eq!(shown.title, "Ada Lovelace");
        assert_eq!(shown.body, "the engine is ready");

        app.lock_app();
        app.maybe_notify(CHAT, &message);
        let shown = app.notifications.shown.last().unwrap();
        assert_eq!(shown.title, "Zap Faster");
        assert_eq!(shown.body, "New message");
        assert_eq!(shown.picture, None);
        assert_eq!(
            shown.sound, app.settings.message_sound,
            "the chat's own sound would name it"
        );
    }

    #[test]
    fn limited_direct_chats_notify_once_until_the_chat_is_opened() {
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.maybe_notify(CHAT, &message);
        app.maybe_notify(CHAT, &message);
        assert_eq!(app.notifications.shown.len(), 2, "off by default");

        app.account_mut().settings.limit_notifications = crate::settings::NotificationLimit::Direct;
        app.notifications.shown.clear();
        app.maybe_notify(CHAT, &message);
        app.maybe_notify(CHAT, &message);
        assert_eq!(app.notifications.shown.len(), 1);

        app.open_chat(CHAT.into());
        app.chats[0].unread = 1;
        app.maybe_notify(CHAT, &message);
        assert_eq!(app.notifications.shown.len(), 2, "opening reset the wait");
        app.maybe_notify(CHAT, &message);
        assert_eq!(app.notifications.shown.len(), 2);
    }

    #[test]
    fn the_direct_and_group_limits_are_independent() {
        const GROUP: &str = "2@g.us";
        let mut app = unlocked_app();
        let direct = incoming(&mut app);
        let mut group_chat = Chat::new(GROUP.into(), "Engineers".into());
        group_chat.unread = 1;
        app.chats.push(group_chat);
        let group = Message {
            id: "m2".into(),
            chat: GROUP.into(),
            sender: CHAT.into(),
            ..direct.clone()
        };

        app.account_mut().settings.limit_notifications = crate::settings::NotificationLimit::Group;
        app.maybe_notify(CHAT, &direct);
        app.maybe_notify(CHAT, &direct);
        app.maybe_notify(GROUP, &group);
        app.maybe_notify(GROUP, &group);
        assert_eq!(
            app.notifications.shown.len(),
            3,
            "direct chats are unlimited, the group notified once"
        );

        app.account_mut().settings.limit_notifications = crate::settings::NotificationLimit::Direct;
        app.notifications.shown.clear();
        app.maybe_notify(GROUP, &group);
        app.maybe_notify(GROUP, &group);
        app.maybe_notify(CHAT, &direct);
        app.maybe_notify(CHAT, &direct);
        assert_eq!(app.notifications.shown.len(), 3);
    }

    #[test]
    fn mentions_and_replies_to_us_bypass_the_group_limit() {
        const GROUP: &str = "2@g.us";
        let mut app = unlocked_app();
        app.me = Some("15550001111@s.whatsapp.net".into());
        let direct = incoming(&mut app);
        let mut group_chat = Chat::new(GROUP.into(), "Engineers".into());
        group_chat.unread = 1;
        app.chats.push(group_chat);
        let plain = Message {
            id: "m2".into(),
            chat: GROUP.into(),
            sender: CHAT.into(),
            ..direct
        };
        let mut mention = plain.clone();
        mention.mentions = vec![crate::model::MentionRef {
            user: "15550001111".into(),
            id: "15550001111@s.whatsapp.net".into(),
        }];
        let mut reply = plain.clone();
        reply.quoted = Some(crate::model::Quoted {
            chat: None,
            id: "earlier".into(),
            sender: "15550001111@s.whatsapp.net".into(),
            sender_name: None,
            summary: "earlier".into(),
            mentions: Vec::new(),
        });

        app.account_mut().settings.limit_notifications = crate::settings::NotificationLimit::Group;
        app.maybe_notify(GROUP, &mention);
        app.maybe_notify(GROUP, &mention);
        app.maybe_notify(GROUP, &reply);
        assert_eq!(app.notifications.shown.len(), 3, "addressed to us");

        // They neither start nor use the wait: the first plain message
        // still notifies, and the second does not.
        app.maybe_notify(GROUP, &plain);
        app.maybe_notify(GROUP, &plain);
        assert_eq!(app.notifications.shown.len(), 4);
        app.maybe_notify(GROUP, &mention);
        assert_eq!(app.notifications.shown.len(), 5, "still addressed to us");
    }

    #[test]
    fn a_limited_chat_is_limited_while_the_app_is_locked_too() {
        let mut app = unlocked_app();
        let message = incoming(&mut app);
        app.account_mut().settings.limit_notifications = crate::settings::NotificationLimit::Direct;
        app.maybe_notify(CHAT, &message);
        app.lock_app();
        app.maybe_notify(CHAT, &message);
        assert_eq!(app.notifications.shown.len(), 1);
    }

    #[test]
    fn messages_arriving_while_locked_stay_unread() {
        let ctx = egui::Context::default();
        let mut app = app_with(settings(Some(PASSWORD)));
        app.open_chat = Some(CHAT.into());
        app.window_focused = true;
        app.background_frame(&ctx);
        let mut chat = Chat::new(CHAT.into(), "Ada".into());
        chat.unread = 1;
        app.handle_chat_updated(chat, true);
        assert_eq!(app.chat(CHAT).unwrap().unread, 1);
    }

    #[test]
    fn setting_changing_and_turning_off_the_password() {
        let ctx = egui::Context::default();
        let mut app = app_with(settings(None));
        // Changing needs a password to change.
        app.apply(Action::AppLockForm(Some(FormMode::Change)), &ctx);
        assert_eq!(app.app_lock.form, None);
        app.apply(Action::AppLockForm(Some(FormMode::Set)), &ctx);
        let form = app.app_lock.form.as_mut().unwrap();
        form.new = "first-password".into();
        form.confirm = "first-password".into();
        app.apply(Action::SubmitAppLockForm, &ctx);
        finish(&mut app, &ctx);
        assert_eq!(app.app_lock.form, None);
        let first = app.settings.app_lock_hash.clone().unwrap();
        assert!(crate::app_lock::verifies(&first, "first-password"));
        assert!(
            !app.app_lock.is_locked(),
            "setting a password does not lock"
        );

        // A second Set cannot replace it without the current password.
        app.apply(Action::AppLockForm(Some(FormMode::Set)), &ctx);
        assert_eq!(app.app_lock.form, None);

        let mut change = Form::new(FormMode::Change);
        change.current = "wrong".into();
        change.new = "second-password".into();
        change.confirm = "second-password".into();
        app.app_lock.form = Some(change.clone());
        app.apply(Action::SubmitAppLockForm, &ctx);
        finish(&mut app, &ctx);
        let form = app.app_lock.form.as_ref().unwrap();
        assert_eq!(form.error, Some(FormError::WrongCurrent));
        assert!(!form.busy);
        assert_eq!(app.settings.app_lock_hash.as_deref(), Some(first.as_str()));

        change.current = "first-password".into();
        app.app_lock.form = Some(change);
        app.apply(Action::SubmitAppLockForm, &ctx);
        finish(&mut app, &ctx);
        let second = app.settings.app_lock_hash.clone().unwrap();
        assert!(crate::app_lock::verifies(&second, "second-password"));

        // Turning off requires the password too.
        app.apply(Action::AppLockForm(Some(FormMode::TurnOff)), &ctx);
        app.app_lock.form.as_mut().unwrap().current = "first-password".into();
        app.apply(Action::SubmitAppLockForm, &ctx);
        finish(&mut app, &ctx);
        assert!(app.settings.app_lock_hash.is_some());
        app.app_lock.form.as_mut().unwrap().current = "second-password".into();
        app.apply(Action::SubmitAppLockForm, &ctx);
        finish(&mut app, &ctx);
        assert_eq!(app.settings.app_lock_hash, None);
        assert_eq!(app.app_lock.form, None);
    }

    #[test]
    fn forgetting_the_password_unlocks_only_after_unlinking() {
        let ctx = egui::Context::default();
        let mut app = app_with(settings(Some(PASSWORD)));
        app.apply(Action::ForgotAppPassword(true), &ctx);
        assert_eq!(
            app.app_lock.forgetting,
            crate::app_lock::Forgetting::Confirming
        );
        app.apply(Action::UnlinkLockedApp, &ctx);
        assert_eq!(
            app.app_lock.forgetting,
            crate::app_lock::Forgetting::Unlinking
        );
        assert!(app.app_lock.is_locked(), "still locked until unlinked");
        app.handle_link(LinkStatus::LoggedOut, true);
        assert!(!app.app_lock.is_locked());
        assert_eq!(app.settings.app_lock_hash, None);
    }

    #[test]
    fn the_lock_shortcut_needs_a_password() {
        let ctx = egui::Context::default();
        let press = |app: &mut App| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::L,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::COMMAND | egui::Modifiers::SHIFT,
                    }],
                    ..Default::default()
                },
                |ui| crate::ui::keys::handle(app, ui.ctx()),
            );
            output.textures_delta.clear();
            std::mem::take(&mut app.actions)
        };
        let mut app = unlocked_app();
        let actions = press(&mut app);
        assert!(actions.contains(&Action::LockApp), "{actions:?}");
        assert!(!actions.contains(&Action::FocusComposer));
        let mut app = app_with(settings(None));
        assert!(!press(&mut app).contains(&Action::LockApp));
    }
}

/// Localizes edit-refusal reasons and appends protocol details only for send failures.
fn edit_failure_message(locale: crate::i18n::Locale, error: &EditFailure) -> String {
    match error {
        EditFailure::Expired => {
            crate::i18n::gettext(locale, "This message can no longer be edited").into_owned()
        }
        EditFailure::Offline => {
            crate::i18n::gettext(locale, "Not connected to WhatsApp").into_owned()
        }
        EditFailure::Send(detail) => format!(
            "{}: {detail}",
            crate::i18n::gettext(locale, "Could not send the edit")
        ),
        EditFailure::Save => {
            crate::i18n::gettext(locale, "The edit was sent but could not be saved locally")
                .into_owned()
        }
    }
}

#[cfg(test)]
mod restored_account_tests {
    use super::*;

    #[test]
    fn resume_persists_explicit_choice_and_requires_the_target_session() {
        let root = tempfile::tempdir().unwrap();
        let (mut app, _events) = App::headless(AppDirs::under(root.path()), Settings::default());
        let id = app.account().id.clone();
        app.account_mut().settings.archive_only = true;
        let (backend, mut commands) = Backend::recording();
        app.account_mut().backend = backend;
        app.apply(
            Action::ResumeRestoredAccount(id.clone()),
            &egui::Context::default(),
        );
        assert!(app.account().settings.archive_only);
        assert!(commands.try_recv().is_err());
        std::fs::write(
            app.account().dirs.session_db(),
            b"synthetic session fixture",
        )
        .unwrap();
        app.apply(Action::ResumeRestoredAccount(id), &egui::Context::default());
        assert!(!app.account().settings.archive_only);
        let persisted = crate::settings::AccountSettings::load(&app.account().dirs.settings_file());
        assert!(!persisted.archive_only);
        assert!(matches!(
            commands.try_recv().unwrap(),
            Command::ResumeRestoredAccount
        ));
    }

    #[test]
    fn cleanup_refuses_an_unrelated_account_directory() {
        let root = tempfile::tempdir().unwrap();
        let dirs = crate::paths::AccountDirs {
            id: AccountId::first(),
            state: root.path().join("unrelated"),
            cache: root.path().join("unrelated-cache"),
        };
        dirs.ensure().unwrap();
        std::fs::write(dirs.session_db(), b"preserved synthetic fixture").unwrap();
        assert!(discard_unregistered_archive_restore(&dirs).is_err());
        assert_eq!(
            std::fs::read(dirs.session_db()).unwrap(),
            b"preserved synthetic fixture"
        );
    }
}
