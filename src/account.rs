//! One linked WhatsApp number inside the process.
//!
//! The window's [`crate::app::App`] owns a list of these. Views draw the
//! active account through `App`'s `Deref`. Each account has its own backend
//! thread, archive, and folders.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

use crate::app::{AttachmentDraft, Conversation};
use crate::backend::{Backend, LinkStatus, Waker};
use crate::model::ComposerMention;
use crate::model::{
    AccountId, Chat, ChatFilter, ChatId, Contact, Label, Message, PollDraft, StickerPack,
};
use crate::paths::{AccountDirs, AppDirs};
use crate::settings::AccountSettings;

/// One WhatsApp account's runtime state.
pub struct Account {
    pub id: AccountId,
    pub dirs: AccountDirs,
    pub settings: AccountSettings,
    pub(crate) settings_dirty: bool,
    pub backend: Backend,
    pub link: LinkStatus,
    pub syncing: bool,
    pub sync_percent: Option<u32>,
    pub call: Option<crate::calls::CallUpdate>,
    pub call_preparing: bool,
    pub(crate) pending_call_request: Option<crate::calls::CallRequest>,
    pub call_devices: crate::calls::DeviceList,
    pub call_records: Vec<crate::archive::CallRecord>,
    pub statuses: Vec<Message>,
    pub call_log_open: bool,
    pub me: Option<String>,
    pub me_lid: Option<String>,
    pub me_name: Option<String>,
    pub me_about: Option<String>,
    pub chats: Vec<Chat>,
    /// The archive's initial chat list has arrived, including offline.
    pub(crate) chats_loaded: bool,
    pub contacts: HashMap<String, Contact>,
    pub conversations: HashMap<ChatId, Conversation>,
    pub open_chat: Option<ChatId>,
    pub scroll_chat_into_view: Option<ChatId>,
    pub drafts: HashMap<ChatId, String>,
    /// Whether the open chat had a draft when opened. Keep its list priority
    /// stable while the reader types, clears, or sends that draft.
    pub(crate) open_draft_priority: bool,
    /// of claiming the archive is empty.
    pub storage_stats: Option<crate::model::StorageStats>,
    /// When the last reading was asked for, so a stale one is asked again.
    pub(crate) storage_stats_at: Option<std::time::Instant>,
    /// Whether the message count has been read for the current visit to
    /// Settings. The refresh reads the sizes alone.
    pub(crate) storage_stats_counted: bool,
    /// Whether a reading is already on its way to the worker.
    pub(crate) storage_stats_asked: bool,
    /// Which storage-stats ask is current. An answer for an older one is
    /// dropped: it would otherwise put its numbers over the newer read and
    /// date them now.
    pub(crate) storage_stats_token: u64,
    pub(crate) refused_edits: Vec<(ChatId, String, crate::backend::EditDraft)>,
    pub(crate) attachment_drafts: HashMap<ChatId, AttachmentDraft>,
    pub(crate) private_reply_drafts: HashMap<ChatId, crate::model::ReplyTarget>,
    pub(crate) private_reply_generations: HashMap<ChatId, u64>,
    pub(crate) private_reply_generation: u64,
    pub(crate) unsent_voice: Option<(ChatId, Vec<f32>)>,
    pub(crate) draft_mentions: HashMap<ChatId, Vec<ComposerMention>>,
    pub search: String,
    pub search_selected: Option<ChatId>,
    pub search_hits: Vec<Message>,
    pub typing: HashMap<ChatId, Vec<(String, Instant)>>,
    pub presence: HashMap<String, crate::app::Presence>,
    pub account_receipts_off: bool,
    pub(crate) local_avatars: HashMap<String, PathBuf>,
    pub(crate) avatars: HashMap<String, Option<PathBuf>>,
    pub(crate) avatar_requests: HashSet<String>,
    pub(crate) avatars_full: HashMap<String, Option<PathBuf>>,
    pub(crate) avatar_full_requests: HashSet<String>,
    pub(crate) played_told: HashSet<String>,
    pub stickers: Vec<PathBuf>,
    pub stickers_received: Vec<PathBuf>,
    pub stickers_saved: Vec<PathBuf>,
    pub sticker_packs: Vec<StickerPack>,
    pub stickers_pending: bool,
    pub sticker_import_pending: bool,
    pub sticker_link: String,
    pub poll_draft: PollDraft,
    pub poll_creating: bool,
    pub poll_voting: HashSet<(ChatId, String)>,
    pub contact_edit: Option<(String, String)>,
    pub new_contact_phone: String,
    pub new_contact_name: String,
    pub new_contact_last: String,
    pub new_contact_pending: bool,
    pub(crate) new_contact_sequence: u64,
    pub(crate) new_contact_request: Option<u64>,
    pub(crate) new_contact_dialog_request: Option<u64>,
    pub pair_phone: String,
    pub archive_progress: Option<crate::model::ArchiveRecoveryProgress>,
    pub backup_busy: bool,
    pub backup_status: Option<String>,
    pub backup_merge_preview: Option<crate::archive::merge::MergePreview>,
    pub show_archived: bool,
    /// Expansion belongs to this account; it never changes phone membership.
    pub collapsed_communities: HashSet<ChatId>,
    /// Starred messages, newest star first, and whether the left panel lists
    /// them instead of the chats.
    pub starred: Vec<crate::archive::Starred>,
    pub show_starred: bool,
    pub starred_more: bool,
    /// Ids of the starred messages of each chat, for the mark in the
    /// conversation. Filled when a chat opens and on every confirmed star.
    pub stars: HashMap<ChatId, HashSet<String>>,
    /// Ids of each chat's pinned messages, for its menu and its bubble mark.
    pub pins: HashMap<ChatId, HashSet<String>>,
    /// Active pins of each chat, for the line under its header.
    pub chat_pins: HashMap<ChatId, Vec<crate::archive::Pinned>>,
    pub pin_notices: HashMap<ChatId, Vec<crate::archive::Pinned>>,
    /// Demo/test: keep this starred row's context menu open.
    #[cfg(any(test, feature = "demo"))]
    pub open_list_menu: Option<(ChatId, String)>,
    /// A starred row asked to reply: the quote starts once the message it
    /// points at is loaded, because the composer drops a quote whose message
    /// has not arrived yet.
    pub reply_when_loaded: Option<(ChatId, String)>,
    pub chat_filter: ChatFilter,
    pub label_filter: Option<String>,
    pub labels: Vec<Label>,
    pub account_privacy: crate::privacy::Snapshot,
    pub gallery: crate::model::GalleryState,
    pub uploads: HashMap<u64, (ChatId, u64, u64)>,
    pub interactive_sending: HashSet<(ChatId, String)>,
    pub group_saving: HashSet<ChatId>,
    pub(crate) reported_online: Option<bool>,
    pub(crate) reported_history: Option<(bool, bool, bool, u32, u32, Option<crate::model::ChatId>)>,
    pub(crate) reported_exports: Option<crate::settings::AutomaticExportPolicy>,
    /// Chats this account may pin; WhatsApp Plus raises it once known.
    pub pin_limit: usize,
}

impl Account {
    /// Opens account-owned settings, backend and persisted chat state.
    pub fn new(
        id: AccountId,
        dirs: AccountDirs,
        settings: AccountSettings,
        backend: Backend,
    ) -> Self {
        let local_avatars = crate::local_avatars::load(&dirs.state).unwrap_or_else(|error| {
            log::warn!("could not load local picture preferences: {error}");
            HashMap::new()
        });
        let open_chat = settings.last_chat.clone();
        let chat_filter = settings.last_filter;
        let label_filter = settings.last_label.clone();
        backend.send(crate::backend::Command::SetCallMicrophone(
            settings.call_microphone.clone(),
        ));
        backend.send(crate::backend::Command::SetCallSpeaker(
            settings.call_speaker.clone(),
        ));
        backend.send(crate::backend::Command::SetCallCamera(
            settings.call_camera.clone(),
        ));
        backend.send(crate::backend::Command::SetCacheRetention(
            settings.cache_retention_days,
        ));
        Self {
            id,
            dirs,
            settings,
            settings_dirty: false,
            backend,
            link: LinkStatus::Starting,
            syncing: false,
            sync_percent: None,
            call: None,
            call_preparing: false,
            pending_call_request: None,
            call_devices: Default::default(),
            call_records: Vec::new(),
            statuses: Vec::new(),
            call_log_open: false,
            me: None,
            me_lid: None,
            me_name: None,
            me_about: None,
            chats: Vec::new(),
            chats_loaded: false,
            contacts: HashMap::new(),
            conversations: HashMap::new(),
            open_chat,
            scroll_chat_into_view: None,
            drafts: HashMap::new(),
            open_draft_priority: false,
            storage_stats: None,
            storage_stats_at: None,
            storage_stats_asked: false,
            storage_stats_token: 0,
            storage_stats_counted: false,
            refused_edits: Vec::new(),
            private_reply_drafts: HashMap::new(),
            private_reply_generations: HashMap::new(),
            private_reply_generation: 0,
            unsent_voice: None,
            draft_mentions: HashMap::new(),
            attachment_drafts: HashMap::new(),
            search: String::new(),
            search_selected: None,
            search_hits: Vec::new(),
            typing: HashMap::new(),
            presence: HashMap::new(),
            account_receipts_off: false,
            local_avatars,
            avatars: HashMap::new(),
            avatar_requests: HashSet::new(),
            avatars_full: HashMap::new(),
            avatar_full_requests: HashSet::new(),
            played_told: HashSet::new(),
            stickers: Vec::new(),
            stickers_received: Vec::new(),
            stickers_saved: Vec::new(),
            sticker_packs: Vec::new(),
            stickers_pending: false,
            sticker_import_pending: false,
            sticker_link: String::new(),
            poll_draft: Default::default(),
            poll_creating: false,
            poll_voting: HashSet::new(),
            contact_edit: None,
            new_contact_phone: String::new(),
            new_contact_name: String::new(),
            new_contact_last: String::new(),
            new_contact_pending: false,
            new_contact_sequence: 0,
            new_contact_request: None,
            new_contact_dialog_request: None,
            pair_phone: String::new(),
            archive_progress: None,
            backup_busy: false,
            backup_status: None,
            backup_merge_preview: None,
            show_archived: false,
            collapsed_communities: HashSet::new(),
            starred: Vec::new(),
            show_starred: false,
            starred_more: false,
            stars: HashMap::new(),
            pins: HashMap::new(),
            chat_pins: HashMap::new(),
            pin_notices: HashMap::new(),
            #[cfg(any(test, feature = "demo"))]
            open_list_menu: None,
            reply_when_loaded: None,
            chat_filter,
            label_filter,
            labels: Vec::new(),
            account_privacy: crate::privacy::Snapshot::default(),
            gallery: Default::default(),
            uploads: HashMap::new(),
            interactive_sending: HashSet::new(),
            group_saving: HashSet::new(),
            reported_online: None,
            reported_history: None,
            reported_exports: None,
            pin_limit: crate::backend::PINNED_CHATS,
        }
    }

    /// Opens a live account from disk, migrating older global fields onto account 1.
    pub fn spawn(
        app_dirs: &AppDirs,
        id: AccountId,
        legacy: &crate::settings::Settings,
        waker: &Waker,
    ) -> std::io::Result<Self> {
        let dirs = app_dirs.account(&id);
        dirs.ensure()?;
        let path = dirs.settings_file();
        let mut settings = if path.exists() {
            AccountSettings::load(&path)
        } else if id.as_str() == "1" {
            let migrated = AccountSettings::from_legacy(legacy);
            migrated.save(&path)?;
            migrated
        } else {
            AccountSettings::default()
        };
        resolve_wallpaper_path(&dirs, &mut settings);
        let backend = Backend::spawn(dirs.clone(), waker.clone());
        Ok(Self::new(id, dirs, settings, backend))
    }

    pub fn detached(
        app_dirs: &AppDirs,
        id: AccountId,
        settings: AccountSettings,
    ) -> std::io::Result<(Self, std::sync::mpsc::Sender<crate::backend::Event>)> {
        let dirs = app_dirs.account(&id);
        dirs.ensure()?;
        let (backend, events) = Backend::detached();
        Ok((Self::new(id, dirs, settings, backend), events))
    }

    /// Whether the device has linked data, including while offline.
    pub fn is_linked(&self) -> bool {
        self.settings.archive_only
            || matches!(
                self.link,
                LinkStatus::Connected | LinkStatus::Connecting | LinkStatus::Disconnected { .. }
            )
            || (!self.chats.is_empty() && !matches!(self.link, LinkStatus::LoggedOut))
    }

    /// The taskbar count: unarchived, unmuted, unlocked chats that look
    /// unread. WhatsApp counts chats here, not the messages inside them.
    pub fn unread_chat_count(&self) -> u32 {
        let now = crate::util::now();
        let chats = self
            .chats
            .iter()
            .filter(|chat| {
                !chat.archived && !chat.locked && !chat.muted(now) && chat.looks_unread()
            })
            .count();
        u32::try_from(chats).unwrap_or(u32::MAX)
    }

    /// The name the switcher shows: the profile name, else the number,
    /// else "Account" and its number here.
    pub fn display_label(&self, locale: crate::i18n::Locale) -> String {
        if self.settings.archive_only {
            return crate::i18n::gettext(locale, "Offline archive {id}")
                .replace("{id}", self.id.as_str());
        }
        if let Some(name) = self
            .me_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
        {
            return name.trim().to_owned();
        }
        if let Some(phone) = self.phone() {
            return phone;
        }
        crate::i18n::gettext(locale, "Account {id}").replace("{id}", self.id.as_str())
    }

    /// Returns or requests a cached profile picture from this account.
    pub fn avatar(&mut self, id: &str) -> Option<PathBuf> {
        if let Some(path) = self.local_avatars.get(&crate::local_avatars::key(id)) {
            return Some(path.clone());
        }
        if let Some(known) = self.avatars.get(id) {
            return known.clone();
        }
        if self.avatar_requests.insert(id.to_owned()) {
            self.backend.send(crate::backend::Command::FetchAvatar {
                id: id.to_owned(),
                full: false,
            });
        }
        None
    }

    /// Our phone number, formatted, once WhatsApp has told us.
    pub fn phone(&self) -> Option<String> {
        let me = self.me.as_deref()?;
        crate::model::phone_of(me).map(crate::util::phone)
    }

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
    }

    pub fn save_settings(&mut self) {
        self.settings_dirty = false;
        if let Err(error) = self.settings.save(&self.dirs.settings_file()) {
            log::warn!("could not save account settings: {error}");
        }
    }
}

fn resolve_wallpaper_path(dirs: &AccountDirs, settings: &mut AccountSettings) {
    if settings
        .wallpaper_image
        .as_ref()
        .is_some_and(|path| path.exists())
    {
        return;
    }
    for extension in ["jpg", "png", "webp", "gif"] {
        let path = dirs.wallpaper_file(extension);
        if path.exists() {
            settings.wallpaper_image = Some(path);
            return;
        }
    }
    if settings
        .wallpaper_image
        .as_ref()
        .is_some_and(|path| !path.exists())
    {
        settings.wallpaper_image = None;
    }
}
