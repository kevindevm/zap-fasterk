//! Desktop notifications when the app is hidden, unfocused, or on another chat.
//!
//! Delivery uses the platform notification service. Each notification runs on
//! its own thread because delivery and click handling can block. A click hands
//! back the chat and the message it announced.

use crate::settings::NotificationSound;
#[cfg(any(target_os = "macos", test))]
mod native_sound;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[cfg(target_os = "windows")]
mod badge_windows;
#[cfg(target_os = "windows")]
mod popups_windows;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use badge_windows::{Badge, Taskbar};
#[cfg(target_os = "windows")]
pub(crate) use popups_windows::NativePopups;

/// Chat and message a clicked notification opens. The message id travels with
/// the click, so the reader lands on what was announced instead of on the end
/// of the chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotificationTarget {
    pub account: crate::model::AccountId,
    pub chat: String,
    pub message: String,
}

/// Notifications that may still wait for a click. Each one waiting holds a
/// thread, a runtime, and a D-Bus connection, about five file descriptors,
/// and the desktop may never say it closed: KDE Plasma files notifications
/// that arrive during Do Not Disturb (on by default while sharing the screen)
/// straight into its history without a signal. Without a limit they piled up
/// until the process ran out of descriptors and aborted.
const WAITING_LIMIT: usize = 32;

/// How long a chat stays quiet after a notification when its notifications
/// are limited. Opening the chat ends the wait early.
const THROTTLE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// How long any chat stays quiet after a notification, so several messages in
/// quick succession make one sound and one banner, not one each. Opening the
/// chat ends the wait early.
pub(crate) const BURST_WINDOW: Duration = Duration::from_secs(5);

/// How long a chat stays quiet after it shows a notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quiet {
    /// Not at all: a mention of us or a reply to us in a group, which neither
    /// starts nor uses a wait.
    Never,
    /// For a burst of messages.
    Burst,
    /// For `THROTTLE_WINDOW`: the chat's repeat notifications are limited.
    Limited,
}

impl Quiet {
    fn window(self) -> Option<Duration> {
        match self {
            Self::Never => None,
            Self::Burst => Some(BURST_WINDOW),
            Self::Limited => Some(THROTTLE_WINDOW),
        }
    }
}

type Throttle = Arc<Mutex<std::collections::HashMap<(String, String), Instant>>>;

type PendingByAccountChat =
    std::collections::HashMap<(String, String), Vec<(u64, tokio::sync::oneshot::Sender<Stop>)>>;

/// How a notification stops waiting for a click.
#[derive(Debug, PartialEq, Eq)]
enum Stop {
    /// The chat was read: take the notification off the desktop.
    Close,
    /// Newer notifications took its place: leave it on the desktop, but stop
    /// waiting, so a click on it no longer opens the chat.
    Release,
}

/// What a notification still to be shown was told meanwhile: `None` when its
/// chat was read (don't show it), else whether it may wait for a click. A
/// notification released in a burst before it appeared is still shown.
fn before_showing(cancelled: &mut tokio::sync::oneshot::Receiver<Stop>) -> Option<bool> {
    match cancelled.try_recv() {
        Err(tokio::sync::oneshot::error::TryRecvError::Empty) => Some(true),
        Ok(Stop::Release) => Some(false),
        Ok(Stop::Close) | Err(tokio::sync::oneshot::error::TryRecvError::Closed) => None,
    }
}

/// Cancellation is registered before delivery starts, so reading a chat while
/// its notification is still being delivered cannot leave a stale notification.
#[derive(Default)]
pub struct Notifications {
    pending: PendingByAccountChat,
    /// Order of registration, so the oldest waiting notification is released first.
    registered: u64,
    /// When each chat last showed a notification. Shared so a failed
    /// delivery, which happens on the notification's own thread, can undo it.
    throttle: Throttle,
    /// What unit tests would have shown, recorded instead of shown on the
    /// desktop. Always empty outside tests.
    pub shown: Vec<Shown>,
}

/// A notification a unit test asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    pub title: String,
    pub body: String,
    pub picture: Option<PathBuf>,
    pub sound: NotificationSound,
    pub sound_output: Option<String>,
}

impl Notifications {
    fn register(
        &mut self,
        account: &crate::model::AccountId,
        chat: &str,
    ) -> tokio::sync::oneshot::Receiver<Stop> {
        self.pending.retain(|_, entries| {
            entries.retain(|(_, entry)| !entry.is_closed());
            !entries.is_empty()
        });
        while self.pending.values().map(Vec::len).sum::<usize>() >= WAITING_LIMIT {
            self.release_oldest();
        }
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        self.registered += 1;
        self.pending
            .entry((account.as_str().to_owned(), chat.to_owned()))
            .or_default()
            .push((self.registered, cancel));
        cancelled
    }

    fn release_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .flat_map(|(key, entries)| entries.iter().map(move |(order, _)| (*order, key)))
            .min()
            .map(|(order, key)| (order, key.clone()));
        let Some((order, key)) = oldest else {
            return;
        };
        if let Some(entries) = self.pending.get_mut(&key) {
            if let Some(index) = entries.iter().position(|(entry, _)| *entry == order) {
                let (_, cancel) = entries.remove(index);
                let _ = cancel.send(Stop::Release);
            }
            if entries.is_empty() {
                self.pending.remove(&key);
            }
        }
    }

    pub fn clear(&mut self, account: &crate::model::AccountId, chat: &str) {
        if let Some(entries) = self
            .pending
            .remove(&(account.as_str().to_owned(), chat.to_owned()))
        {
            for (_, cancel) in entries {
                let _ = cancel.send(Stop::Close);
            }
        }
    }

    /// Whether a notification for this chat may be shown at `now`. Unless
    /// `quiet` is [`Quiet::Never`], the chat is allowed once, then refused for
    /// the quiet window counted from the message that was allowed;
    /// refused messages do not extend the wait. The wait starts here, so a
    /// burst of messages shows one notification; [`Self::show`] ends it again
    /// if the desktop then fails to show that notification.
    pub fn allow(
        &self,
        account: &crate::model::AccountId,
        chat: &str,
        now: Instant,
        quiet: Quiet,
    ) -> bool {
        let Some(window) = quiet.window() else {
            return true;
        };
        let key = (account.as_str().to_owned(), chat.to_owned());
        let mut throttle = self.throttle.lock().unwrap_or_else(|p| p.into_inner());
        // Every chat that notifies leaves an entry. None can matter past the
        // longest window, whichever window the chat's next message uses.
        throttle.retain(|_, last| now.saturating_duration_since(*last) < THROTTLE_WINDOW);
        if throttle
            .get(&key)
            .is_some_and(|last| now.saturating_duration_since(*last) < window)
        {
            return false;
        }
        throttle.insert(key, now);
        true
    }

    /// Lets the chat's next message notify at once, because the chat was looked at.
    pub fn reset_throttle(&self, account: &crate::model::AccountId, chat: &str) {
        self.throttle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&(account.as_str().to_owned(), chat.to_owned()));
    }

    /// Ends the wait the chat has now, unless a newer one has replaced it by
    /// the time the returned function runs.
    fn undo_throttle(
        &self,
        account: &crate::model::AccountId,
        chat: &str,
    ) -> Box<dyn FnOnce() + Send> {
        let key = (account.as_str().to_owned(), chat.to_owned());
        let throttle = Arc::clone(&self.throttle);
        let started = throttle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&key)
            .copied();
        Box::new(move || {
            let mut throttle = throttle.lock().unwrap_or_else(|p| p.into_inner());
            if started.is_some() && throttle.get(&key).copied() == started {
                throttle.remove(&key);
            }
        })
    }

    /// Moves every wait back by `by`, as if that much time had passed.
    #[cfg(test)]
    pub fn elapse(&self, by: Duration) {
        for last in self
            .throttle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values_mut()
        {
            *last = last.checked_sub(by).unwrap_or(*last);
        }
    }

    pub fn clear_account(&mut self, account: &crate::model::AccountId) {
        let account = account.as_str();
        self.throttle
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(id, _), _| id != account);
        let keys: Vec<_> = self
            .pending
            .keys()
            .filter(|(id, _)| id == account)
            .cloned()
            .collect();
        for key in keys {
            if let Some(entries) = self.pending.remove(&key) {
                for (_, cancel) in entries {
                    let _ = cancel.send(Stop::Close);
                }
            }
        }
    }

    pub fn clear_all(&mut self) {
        self.pending.clear();
    }

    /// Shows a notification; platform delivery runs outside the interface thread.
    #[expect(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        title: String,
        body: String,
        picture: Option<PathBuf>,
        sound: NotificationSound,
        target: NotificationTarget,
        opened: Arc<Mutex<Vec<NotificationTarget>>>,
        wake: impl Fn() + Send + 'static,
        waits: bool,
    ) {
        self.show_on(
            title, body, picture, sound, None, target, opened, wake, waits,
        );
    }

    /// Custom sounds follow the originating account's playback device. Native
    /// system sounds remain managed by the desktop, including macOS Focus.
    #[expect(clippy::too_many_arguments)]
    pub fn show_on(
        &mut self,
        title: String,
        body: String,
        picture: Option<PathBuf>,
        sound: NotificationSound,
        sound_output: Option<String>,
        target: NotificationTarget,
        opened: Arc<Mutex<Vec<NotificationTarget>>>,
        wake: impl Fn() + Send + 'static,
        waits: bool,
    ) {
        let cancelled = self.register(&target.account, &target.chat);
        // The chat's wait ends again if the desktop does not show this.
        let failed: Box<dyn FnOnce() + Send> = if waits {
            self.undo_throttle(&target.account, &target.chat)
        } else {
            Box::new(|| {})
        };
        if cfg!(test) {
            self.shown.push(Shown {
                title,
                body,
                picture,
                sound,
                sound_output,
            });
            return;
        }
        let undo = Arc::new(Mutex::new(Some(failed)));
        let undo_spawn = Arc::clone(&undo);
        let spawned = std::thread::Builder::new()
            .name("notification".into())
            .spawn(move || {
                let failed = move || {
                    if let Some(undo) = undo_spawn.lock().unwrap_or_else(|p| p.into_inner()).take()
                    {
                        undo();
                    }
                };
                let system_sound = sound == NotificationSound::System;
                let native_sound: Option<String> = None;
                play_sound_on(sound, sound_output);
                deliver(
                    &title,
                    &body,
                    picture.as_deref(),
                    system_sound,
                    native_sound.as_deref(),
                    target,
                    opened,
                    wake,
                    cancelled,
                    failed,
                )
            });
        if let Err(error) = spawned {
            log::debug!("no thread for a notification: {error}");
            if let Some(undo) = undo.lock().unwrap_or_else(|p| p.into_inner()).take() {
                undo();
            }
        }
    }
}

/// Pidgin's message and alert sounds (GPL-2.0, see `assets/sounds/README.md`).
const RECEIVE: &[u8] = include_bytes!("../assets/sounds/receive.wav");
const ALERT: &[u8] = include_bytes!("../assets/sounds/alert.wav");
/// Zap Fasterk's own synthesized sounds (MIT, see `assets/sounds/README.md`).
const DING: &[u8] = include_bytes!("../assets/sounds/ding.wav");
const POP: &[u8] = include_bytes!("../assets/sounds/pop.wav");
const BUBBLE: &[u8] = include_bytes!("../assets/sounds/bubble.wav");
const CHORD: &[u8] = include_bytes!("../assets/sounds/chord.wav");

/// Plays a notification sound on its own thread, for notifications and
/// their preview in Settings. System sounds and silence play nothing here.
pub fn play_sound(sound: NotificationSound) {
    play_sound_on(sound, None);
}

pub fn play_sound_on(sound: NotificationSound, preferred: Option<String>) {
    let source: Box<dyn Fn() -> std::io::Result<Box<dyn ReadSeek>> + Send> = match sound {
        NotificationSound::Receive => Box::new(|| Ok(Box::new(std::io::Cursor::new(RECEIVE)))),
        NotificationSound::Alert => Box::new(|| Ok(Box::new(std::io::Cursor::new(ALERT)))),
        NotificationSound::Ding => Box::new(|| Ok(Box::new(std::io::Cursor::new(DING)))),
        NotificationSound::Pop => Box::new(|| Ok(Box::new(std::io::Cursor::new(POP)))),
        NotificationSound::Bubble => Box::new(|| Ok(Box::new(std::io::Cursor::new(BUBBLE)))),
        NotificationSound::Chord => Box::new(|| Ok(Box::new(std::io::Cursor::new(CHORD)))),
        NotificationSound::Custom(path) => Box::new(move || {
            Ok(Box::new(std::io::BufReader::new(std::fs::File::open(
                &path,
            )?)))
        }),
        NotificationSound::System | NotificationSound::None => return,
    };
    let spawned = std::thread::Builder::new()
        .name("notification-sound".into())
        .spawn(move || {
            let played = (|| -> Result<(), String> {
                let reader = source().map_err(|error| error.to_string())?;
                let decoder = rodio::Decoder::new(reader).map_err(|error| error.to_string())?;
                let device = crate::audio::open_output_on(preferred.as_deref())
                    .map_err(|error| error.to_string())?;
                let player = rodio::Player::connect_new(device.mixer());
                player.append(decoder);
                player.sleep_until_end();
                Ok(())
            })();
            if let Err(error) = played {
                log::debug!("notification sound not played: {error}");
            }
        });
    if let Err(error) = spawned {
        log::debug!("no thread for a notification sound: {error}");
    }
}

trait ReadSeek: std::io::Read + std::io::Seek + Send + Sync {}
impl<T: std::io::Read + std::io::Seek + Send + Sync> ReadSeek for T {}

/// The title and body of a notification while the app lock is on, which
/// name neither the chat nor the sender and carry none of the message.
pub fn locked_lines(locale: crate::i18n::Locale) -> (String, String) {
    (
        "Zap Fasterk".to_owned(),
        crate::i18n::gettext(locale, "New message").into_owned(),
    )
}

/// Sender masking also masks message text and pictures to avoid identity leaks.
pub fn preview_lines(
    locale: crate::i18n::Locale,
    show_sender: bool,
    show_content: bool,
    chat_name: &str,
    is_group: bool,
    sender: &str,
    summary: &str,
) -> (String, String) {
    if !show_sender {
        return locked_lines(locale);
    }
    if !show_content {
        return (
            chat_name.to_owned(),
            crate::i18n::gettext(locale, "New message").into_owned(),
        );
    }
    lines(chat_name, is_group, sender, summary)
}

/// Builds the notification title and body, including the group sender.
pub fn lines(chat_name: &str, is_group: bool, sender: &str, summary: &str) -> (String, String) {
    let body = if is_group {
        format!("{sender}: {summary}")
    } else {
        summary.to_owned()
    };
    (chat_name.to_owned(), body)
}

#[cfg(target_os = "windows")]
#[expect(clippy::too_many_arguments)]
fn deliver(
    title: &str,
    body: &str,
    picture: Option<&std::path::Path>,
    system_sound: bool,
    _native_sound: Option<&str>,
    target: NotificationTarget,
    opened: Arc<Mutex<Vec<NotificationTarget>>>,
    wake: impl Fn() + Send + 'static,
    mut cancelled: tokio::sync::oneshot::Receiver<Stop>,
    failed: impl FnOnce(),
) {
    if before_showing(&mut cancelled).is_some()
        && let Err(error) = windows::show(title, body, picture, system_sound, target, opened, wake)
    {
        log::debug!("no Windows notification: {error}");
        failed();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_sounds_decode_without_an_audio_device() {
        for bytes in [RECEIVE, ALERT, DING, POP, BUBBLE, CHORD] {
            let decoded = rodio::Decoder::new(std::io::Cursor::new(bytes)).unwrap();
            assert!(decoded.count() > 1000, "a bundled sound plays samples");
        }
    }
    #[test]
    fn notification_sound_retains_the_originating_accounts_selected_output() {
        let mut notifications = Notifications::default();
        notifications.show_on(
            "Synthetic notification".into(),
            "Invented message".into(),
            None,
            NotificationSound::Receive,
            Some("Synthetic headset".into()),
            NotificationTarget {
                account: "1".into(),
                chat: "fixture".into(),
                message: "fixture".into(),
            },
            Arc::new(Mutex::new(Vec::new())),
            || {},
            false,
        );
        assert_eq!(
            notifications.shown[0].sound_output.as_deref(),
            Some("Synthetic headset")
        );
    }
    use super::*;
    use crate::model::AccountId;

    #[test]
    fn notification_preview_controls_mask_identity_content_and_group_sender_consistently() {
        use crate::i18n::Locale;
        for (sender, content) in [(false, false), (false, true)] {
            assert_eq!(
                preview_lines(
                    Locale::English,
                    sender,
                    content,
                    "Synthetic group",
                    true,
                    "Ada",
                    "Private fixture"
                ),
                locked_lines(Locale::English)
            );
        }
        assert_eq!(
            preview_lines(
                Locale::English,
                true,
                false,
                "Synthetic group",
                true,
                "Ada",
                "Private fixture"
            ),
            ("Synthetic group".into(), "New message".into())
        );
        assert_eq!(
            preview_lines(
                Locale::English,
                true,
                true,
                "Synthetic group",
                true,
                "Ada",
                "Synthetic text"
            ),
            ("Synthetic group".into(), "Ada: Synthetic text".into())
        );
    }

    fn account(id: &str) -> AccountId {
        AccountId(id.into())
    }

    #[test]
    fn reading_cancels_delivered_and_pending_notifications_for_only_that_chat() {
        let mut notifications = Notifications::default();
        let one = account("1");
        let mut first = notifications.register(&one, "a");
        let mut second = notifications.register(&one, "a");
        let mut other = notifications.register(&one, "b");
        notifications.clear(&one, "a");
        assert_eq!(first.try_recv(), Ok(Stop::Close));
        assert_eq!(second.try_recv(), Ok(Stop::Close));
        assert_eq!(
            other.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        let mut next = notifications.register(&one, "a");
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        notifications.clear_all();
        assert!(other.try_recv().is_err());
        assert_eq!(
            next.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        );
    }

    #[test]
    fn a_limited_chat_notifies_once_then_waits_ten_minutes() {
        let notifications = Notifications::default();
        let one = account("1");
        let start = Instant::now();
        assert!(notifications.allow(&one, "a", start, Quiet::Limited));
        assert!(!notifications.allow(&one, "a", start + Duration::from_secs(1), Quiet::Limited));
        // Refused messages do not push the end of the wait back.
        assert!(!notifications.allow(&one, "a", start + Duration::from_secs(599), Quiet::Limited));
        assert!(notifications.allow(&one, "a", start + THROTTLE_WINDOW, Quiet::Limited));
        assert!(!notifications.allow(&one, "a", start + THROTTLE_WINDOW, Quiet::Limited));
    }

    #[test]
    fn a_notification_that_was_not_shown_does_not_start_the_wait() {
        let notifications = Notifications::default();
        let one = account("1");
        let start = Instant::now();
        assert!(notifications.allow(&one, "a", start, Quiet::Limited));
        let undo = notifications.undo_throttle(&one, "a");
        undo();
        assert!(notifications.allow(&one, "a", start, Quiet::Limited));

        // A wait that has since been replaced is left alone.
        let undo = notifications.undo_throttle(&one, "a");
        notifications.reset_throttle(&one, "a");
        let later = start + THROTTLE_WINDOW;
        assert!(notifications.allow(&one, "a", later, Quiet::Limited));
        undo();
        assert!(!notifications.allow(&one, "a", later, Quiet::Limited));
    }

    #[test]
    fn unlimited_chats_always_notify_and_leave_no_state() {
        let notifications = Notifications::default();
        let one = account("1");
        let start = Instant::now();
        assert!(notifications.allow(&one, "a", start, Quiet::Never));
        assert!(notifications.allow(&one, "a", start, Quiet::Never));
        assert!(notifications.throttle.lock().unwrap().is_empty());
    }

    #[test]
    fn opening_a_chat_lets_its_next_message_notify() {
        let notifications = Notifications::default();
        let one = account("1");
        let start = Instant::now();
        assert!(notifications.allow(&one, "a", start, Quiet::Limited));
        assert!(notifications.allow(&one, "b", start, Quiet::Limited));
        notifications.reset_throttle(&one, "a");
        assert!(notifications.allow(&one, "a", start, Quiet::Limited));
        assert!(!notifications.allow(&one, "b", start, Quiet::Limited));
    }

    #[test]
    fn the_wait_is_kept_per_account_and_chat() {
        let mut notifications = Notifications::default();
        let start = Instant::now();
        assert!(notifications.allow(&account("1"), "a", start, Quiet::Limited));
        assert!(notifications.allow(&account("2"), "a", start, Quiet::Limited));
        assert!(notifications.allow(&account("1"), "b", start, Quiet::Limited));
        assert!(!notifications.allow(&account("1"), "a", start, Quiet::Limited));
        notifications.clear_account(&account("1"));
        assert!(notifications.allow(&account("1"), "a", start, Quiet::Limited));
        assert!(!notifications.allow(&account("2"), "a", start, Quiet::Limited));
        // Locking the app closes notifications but keeps the waits.
        notifications.clear_all();
        assert!(!notifications.allow(&account("2"), "a", start, Quiet::Limited));
    }

    #[test]
    fn a_wait_past_the_longest_window_is_forgotten() {
        let notifications = Notifications::default();
        let one = account("1");
        let start = Instant::now();
        assert!(notifications.allow(&one, "a", start, Quiet::Burst));
        assert!(notifications.allow(&one, "b", start + Duration::from_secs(1), Quiet::Limited));
        assert!(notifications.allow(&one, "c", start + THROTTLE_WINDOW, Quiet::Burst));
        let throttle = notifications.throttle.lock().unwrap();
        assert!(!throttle.contains_key(&("1".into(), "a".into())));
        assert!(throttle.contains_key(&("1".into(), "b".into())));
        assert!(throttle.contains_key(&("1".into(), "c".into())));
    }

    #[test]
    fn expired_notifications_do_not_accumulate() {
        let mut notifications = Notifications::default();
        drop(notifications.register(&account("1"), "a"));
        let _next = notifications.register(&account("1"), "b");
        assert!(
            !notifications
                .pending
                .contains_key(&("1".into(), "a".into()))
        );
    }

    #[test]
    fn the_oldest_waiting_notification_is_released_without_closing_it() {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut notifications = Notifications::default();
        let mut waiting: Vec<_> = (0..WAITING_LIMIT)
            .map(|index| notifications.register(&account("1"), &format!("chat {}", index % 3)))
            .collect();
        let mut newest = notifications.register(&account("1"), "chat 0");
        assert_eq!(waiting[0].try_recv(), Ok(Stop::Release));
        for later in &mut waiting[1..] {
            assert_eq!(later.try_recv(), Err(TryRecvError::Empty));
        }
        assert_eq!(newest.try_recv(), Err(TryRecvError::Empty));
        let count: usize = notifications.pending.values().map(Vec::len).sum();
        assert_eq!(count, WAITING_LIMIT);

        // Reading a chat still closes what is left of it.
        notifications.clear(&account("1"), "chat 0");
        assert_eq!(newest.try_recv(), Ok(Stop::Close));
        assert_eq!(waiting[3].try_recv(), Ok(Stop::Close));
        assert_eq!(waiting[1].try_recv(), Err(TryRecvError::Empty));
    }

    /// A burst past the limit releases notifications that have not appeared
    /// yet: they are still shown, only not waited on. A read chat's are not.
    #[test]
    fn a_notification_released_before_it_appears_is_still_shown() {
        let mut notifications = Notifications::default();
        let one = account("1");
        let mut first = notifications.register(&one, "a");
        let mut read = notifications.register(&one, "b");
        let _waiting: Vec<_> = (0..WAITING_LIMIT - 1)
            .map(|index| notifications.register(&one, &format!("chat {index}")))
            .collect();
        notifications.clear(&one, "b");
        assert_eq!(before_showing(&mut first), Some(false));
        assert_eq!(before_showing(&mut read), None);
        let mut fresh = notifications.register(&one, "c");
        assert_eq!(before_showing(&mut fresh), Some(true));
    }

    #[test]
    fn finished_notifications_do_not_count_against_the_limit() {
        let mut notifications = Notifications::default();
        for index in 0..WAITING_LIMIT * 2 {
            drop(notifications.register(&account("1"), &format!("chat {index}")));
        }
        let mut waiting: Vec<_> = (0..WAITING_LIMIT)
            .map(|index| notifications.register(&account("1"), &format!("chat {index}")))
            .collect();
        for receiver in &mut waiting {
            assert_eq!(
                receiver.try_recv(),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty)
            );
        }
    }

    /// Shows a test notification with an optional cached picture:
    /// `cargo test --all-features shows_one -- --ignored --nocapture`.
    #[test]
    #[ignore = "shows a real notification"]
    fn shows_one_on_this_desktop() {
        let picture = std::fs::read_dir(crate::paths::AppDirs::discover().avatar_cache_dir())
            .ok()
            .and_then(|entries| entries.flatten().map(|entry| entry.path()).next());
        let mut notifications = Notifications::default();
        notifications.show(
            "Ada Lovelace".into(),
            "A test from Zap Fasterk, with a picture".into(),
            picture,
            NotificationSound::System,
            NotificationTarget {
                account: crate::model::AccountId::first(),
                chat: "test".into(),
                message: "test-message".into(),
            },
            Default::default(),
            || {},
            false,
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    #[test]
    fn reading_one_account_leaves_the_same_chat_on_another() {
        let mut notifications = Notifications::default();
        let first = account("1");
        let second = account("2");
        let mut theirs = notifications.register(&second, "shared");
        notifications.clear(&first, "shared");
        assert_eq!(
            theirs.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        );
        notifications.clear(&second, "shared");
        assert_eq!(theirs.try_recv(), Ok(Stop::Close));
    }

    #[test]
    fn a_group_names_the_sender_and_a_chat_does_not() {
        assert_eq!(
            lines("Rust Berlin", true, "Mira", "Save me a seat"),
            ("Rust Berlin".to_owned(), "Mira: Save me a seat".to_owned())
        );
        assert_eq!(
            lines("Ada Lovelace", false, "Ada Lovelace", "Photo"),
            ("Ada Lovelace".to_owned(), "Photo".to_owned())
        );
    }
}
