//! Keyboard shortcuts.

use egui::{Key, Modifiers};

use crate::app::App;
use crate::model::{Action, Chat, Dialog, Page, PickerTab, Scroll};

/// Whether Home, End and Ctrl+End scroll the open chat rather than move the
/// cursor of the focused text field (#456).
fn jump_keys_reach_chat(editing_text: bool, composer_focused: bool, composer_empty: bool) -> bool {
    !editing_text || (composer_focused && composer_empty)
}

/// Shared by the keyboard and macOS native menu, preserving the window default.
pub(crate) fn close_shortcut_action(app: &App, ctx: &egui::Context) -> Option<Action> {
    if !app.settings.close_chat_with_ctrl_w {
        return Some(Action::CloseWindow);
    }
    (!app.app_lock.is_locked()
        && !app.window_hidden
        && app.page == Page::Chats
        && app.open_chat.is_some()
        && app.dialog.is_none()
        && !app.show_update
        && !egui::Popup::is_any_open(ctx)
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && app.picture_edit.is_none()
        && app.image_preview.is_none()
        && !app.video_expanded
        && app.sticker_view.is_none())
    .then_some(Action::CloseChat)
}

/// The focused message's items: Left and Right walk them, Space and Enter
/// act on the current one as clicking it does.
fn message_item_keys(app: &mut App, ctx: &egui::Context, chat: &str, row: &str) {
    use crate::ui::conversation as view;
    // The row's messages; the keys act on the album item the cursor is on.
    // An item that is gone leaves the cursor lost until it moves again.
    struct Setup {
        owners: Vec<crate::model::Message>,
        targets: Vec<crate::a11y::KeyTarget>,
        owner_ids: Vec<String>,
        current: usize,
        lost: bool,
    }
    let setup = (|| {
        let conversation = app.conversations.get(chat)?;
        let index = conversation
            .messages
            .iter()
            .position(|message| message.id == row)?;
        let members = view::row_members(&conversation.messages, false, index);
        let (targets, owners) = view::row_targets(members, &app.contacts);
        if targets.is_empty() {
            return None;
        }
        let owner_id = |index: usize| members[owners[index]].id.clone();
        let owner_ids = (0..targets.len()).map(owner_id).collect::<Vec<_>>();
        let (current, lost) = match app
            .message_item
            .as_ref()
            .filter(|(item_chat, item_row, ..)| item_chat == chat && *item_row == row)
        {
            None => (0, false),
            Some((.., index, owner, item))
                if targets.get(*index) == Some(item) && owner_ids[*index] == *owner =>
            {
                (*index, false)
            }
            Some((.., owner, item)) => match targets
                .iter()
                .enumerate()
                .position(|(index, target)| target == item && owner_ids[index] == *owner)
            {
                Some(index) => (index, false),
                None => (0, true),
            },
        };
        let owners = (0..targets.len())
            .map(|index| members[owners[index]].clone())
            .collect::<Vec<_>>();
        Some(Setup {
            owners,
            targets,
            owner_ids,
            current,
            lost,
        })
    })();
    let Some(setup) = setup else {
        return;
    };
    let owner = &setup.owners[setup.current];
    let voting = view::Voting {
        connected: app.link.is_connected(),
        pending: &app.poll_voting,
    };
    let primary = view::target_actions(chat, owner, setup.targets.get(setup.current), &voting);
    let option = match setup.targets.get(setup.current) {
        Some(crate::a11y::KeyTarget::PollOption(option)) => Some(*option),
        _ => None,
    };
    let blocked = option.and_then(|option| view::option_blocked(owner, option, &voting));
    let phone_menu = match setup.targets.get(setup.current) {
        Some(crate::a11y::KeyTarget::Phone { url, .. }) => Some(url.clone()),
        _ => None,
    };
    enum ItemKey {
        Activate,
        Move(isize),
    }
    let pressed = ctx.input_mut(|input| {
        if take_plain(input, Key::Space) || take_plain(input, Key::Enter) {
            Some(ItemKey::Activate)
        } else if take_plain(input, Key::ArrowLeft) {
            Some(ItemKey::Move(-1))
        } else if take_plain(input, Key::ArrowRight) {
            Some(ItemKey::Move(1))
        } else {
            None
        }
    });
    let mut actions = Vec::new();
    match pressed {
        Some(ItemKey::Move(delta)) => {
            let index = setup
                .current
                .saturating_add_signed(delta)
                .min(setup.targets.len().saturating_sub(1));
            if let Some(target) = setup.targets.get(index) {
                let text = view::target_label(
                    app.locale,
                    &setup.owners[index],
                    &setup.targets,
                    index,
                    &voting,
                );
                // The list stays on this row while its items are
                // walked: a message arriving meanwhile does not
                // become the current one.
                app.message_cursor.insert(chat.to_owned(), row.to_owned());
                app.reveal_cursor = true;
                app.message_item = Some((
                    chat.to_owned(),
                    row.to_owned(),
                    index,
                    setup.owner_ids[index].clone(),
                    target.clone(),
                ));
                app.announce_accessibility(&text);
            }
        }
        Some(ItemKey::Activate) if setup.lost && !setup.targets.is_empty() => {
            // The item heard last is gone: say where the cursor is
            // now rather than act on something else.
            if let Some(target) = setup.targets.first() {
                let text = view::target_label(app.locale, owner, &setup.targets, 0, &voting);
                app.message_cursor.insert(chat.to_owned(), row.to_owned());
                app.reveal_cursor = true;
                app.message_item = Some((
                    chat.to_owned(),
                    row.to_owned(),
                    0,
                    setup.owner_ids[0].clone(),
                    target.clone(),
                ));
                app.announce_accessibility(&text);
            }
        }
        Some(ItemKey::Activate) => {
            if let Some(url) = phone_menu {
                actions.push(Action::OpenPhoneMenu {
                    message: owner.id.clone(),
                    url,
                });
            } else if !primary.is_empty() {
                // A download Space or Enter asked for is spoken if it
                // fails. Play and pause are not announced: the
                // player's own state is the feedback.
                if let Some((message, card)) = primary.iter().find_map(|action| match action {
                    Action::Download { message, card, .. } => Some((message.clone(), *card)),
                    _ => None,
                }) {
                    app.download_heard.insert((chat.to_owned(), message, card));
                }
                actions.extend(primary.iter().cloned());
                if let (Some(option), Some(Action::VotePoll { choices, .. })) =
                    (option, primary.first())
                {
                    // A vote says the choice it sends, as the ballot
                    // will show it once the vote lands.
                    let text = view::vote_said(app.locale, owner, option, choices);
                    app.announce_accessibility(&text);
                }
            } else if let Some(reason) = blocked {
                let text = crate::a11y::fill(
                    &crate::i18n::gettext(app.locale, "unavailable: {reason}"),
                    &[("reason", &view::blocker_said(app.locale, reason))],
                );
                app.announce_accessibility(&text);
            }
        }
        None => {}
    }
    app.actions.extend(actions);
}

/// Routes keyboard commands to actions, respecting overlays and text focus.
pub fn handle(app: &mut App, ctx: &egui::Context) {
    if app.picture_edit.is_some() {
        editor_keys(app, ctx);
        return;
    }
    if app.image_preview.is_some() {
        preview_keys(app, ctx);
        return;
    }
    if app.video_expanded {
        video_keys(app, ctx);
        return;
    }
    if app.sticker_view.is_some() {
        sticker_view_keys(app, ctx);
        return;
    }
    if matches!(app.dialog, Some(Dialog::Statuses))
        && app.status_shown.is_some()
        && app.picker.is_none()
    {
        status_keys(app, ctx);
        return;
    }
    let editing_text = ctx.text_edit_focused();
    accessibility_shortcuts(app, ctx);
    let find = find_action(app);
    let archive = archive_action(app);
    let next_account = next_account_action(app);
    let close = close_shortcut_action(app, ctx);
    let composer_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("composer-text")));
    // Home/End (and Ctrl+End) move the text cursor in a field with text in
    // it, as in any editor; they scroll the chat only from an empty composer
    // or with no field focused.
    let jump_allowed =
        jump_keys_reach_chat(editing_text, composer_focused, app.composer.is_empty());
    // Ctrl+. and Ctrl+; open the emoji picker only where the smiley would:
    // beside the focused message field. A selection, a block or a
    // read-only chat replaces the field.
    let emoji_keys_allowed = composer_focused
        && app.page == Page::Chats
        && app
            .current_chat()
            .is_some_and(|chat| chat.can_send() && !chat.locked && !app.selecting_in(&chat.id))
        && app.editing.is_none()
        && app.dialog.is_none()
        && !app.show_update
        && app.picker != Some(PickerTab::Emoji)
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && app.call.is_none();
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        // The message list's key counts only with its modifier alone:
        // `consume_key` would also take Alt+Shift+2.
        let messages_key_free = input.events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Key {
                    key: Key::Num2,
                    pressed: true,
                    modifiers,
                    ..
                } if messages_modifier_alone(*modifiers)
            )
        });
        let mut key = |modifiers: Modifiers, key: Key, action: Action| {
            if input.consume_key(modifiers, key) {
                actions.push(action);
            }
        };
        // Checked first: a plain Ctrl+F binding also matches Ctrl+Shift+F.
        key(
            Modifiers::COMMAND | Modifiers::SHIFT,
            Key::F,
            Action::FocusSearch,
        );
        key(Modifiers::COMMAND, Key::F, find);
        // Before Ctrl+L, which would also match it with Shift held.
        if app.settings.app_lock_hash.is_some() {
            key(
                Modifiers::COMMAND | Modifiers::SHIFT,
                Key::L,
                Action::LockApp,
            );
        }
        key(Modifiers::COMMAND, Key::K, Action::FocusSearch);
        if let Some(next_account) = next_account {
            key(Modifiers::COMMAND | Modifiers::SHIFT, Key::A, next_account);
        }
        if app.is_linked() {
            key(
                Modifiers::COMMAND,
                Key::N,
                Action::ShowDialog(Dialog::NewChat),
            );
        }
        if !editing_text {
            key(
                Modifiers::NONE,
                Key::Questionmark,
                Action::ShowDialog(Dialog::Shortcuts),
            );
        }
        if app.page == Page::Chats
            && app.open_chat.is_some()
            && app.dialog.is_none()
            && !app.show_update
            && app.picker.is_none()
            && app.reaction_target.is_none()
            && app.recording.is_none()
        {
            key(Modifiers::COMMAND, Key::L, Action::FocusComposer);
        }
        if let Some(archive) = archive {
            key(Modifiers::COMMAND, Key::E, archive);
        }
        key(Modifiers::COMMAND, Key::B, Action::ToggleSidebar);
        key(Modifiers::COMMAND, Key::Comma, Action::ToggleSettings);
        key(Modifiers::COMMAND, Key::Q, Action::Quit);
        if let Some(close) = close {
            key(Modifiers::COMMAND, Key::W, close);
        }
        key(
            Modifiers::COMMAND,
            Key::Slash,
            Action::ShowDialog(Dialog::Shortcuts),
        );
        key(Modifiers::COMMAND, Key::Plus, Action::ZoomBy(0.1));
        key(Modifiers::COMMAND, Key::Equals, Action::ZoomBy(0.1));
        key(Modifiers::COMMAND, Key::Minus, Action::ZoomBy(-0.1));
        key(Modifiers::COMMAND, Key::Num0, Action::ResetZoom);
        if jump_allowed {
            key(Modifiers::COMMAND, Key::End, Action::ScrollToBottom);
        }
        key(
            Modifiers::COMMAND | Modifiers::SHIFT,
            Key::H,
            Action::ToggleScreenPrivacy,
        );
        // Alt+2 goes to the message list. On macOS it is Control+2:
        // Command+1 to 9 open chats there, and Option with a digit types
        // characters some keyboard layouts need, such as # and €.
        if app.dialog.is_none()
            && !app.show_update
            && app.picker.is_none()
            && app.reaction_target.is_none()
            && app.recording.is_none()
            && messages_key_free
        {
            key(Modifiers::ALT, Key::Num2, Action::FocusMessages);
        }
        // Ctrl+. and Ctrl+; open the emoji picker with its search focused.
        if emoji_keys_allowed && take_emoji_key(input) {
            actions.push(Action::TogglePicker(PickerTab::Emoji));
        }
    });
    drop_shortcut_text(ctx);
    // Escape cancels the topmost state. Menus handle Escape themselves.
    let menu_open = egui::Popup::is_any_open(ctx);
    // Ctrl+Tab switches between the two most recent chats. Leave plain Tab
    // and all other modified Tabs for focus navigation and the platform.
    if app.page == Page::Chats
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && !menu_open
        && app.call.is_none()
        && ctx.input_mut(take_recent_chat)
    {
        ctx.data_mut(|data| data.insert_temp(crate::theme::keyboard_focus_id(), true));
        actions.push(Action::SwitchRecentChat);
    }
    let search_focused = ctx.memory(|memory| memory.has_focus(egui::Id::new("chat-search")));
    if app.page == Page::Chats
        && app.selection.as_ref().is_some_and(|(chat, selected)| {
            app.open_chat.as_ref() == Some(chat) && !selected.is_empty()
        })
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && !app.chat_search_calendar
        && !menu_open
        // The selection bar replaces the composer. Its previous frame's
        // focus must not swallow Copy before egui drops that hidden field.
        && (!editing_text || composer_focused)
        && ctx.input(|input| input.events.iter().any(is_copy_event))
        && let Some(text) = super::conversation::selected_text(app)
    {
        // Native clipboard commands arrive as Event::Copy, including Copy
        // from the macOS menu. Consume only when we have text to export;
        // otherwise leave Copy available to egui's text-selection hook.
        ctx.input_mut(|input| {
            input.events.retain(|event| !is_copy_event(event));
        });
        actions.push(Action::CopyText(text));
    }
    // The focused message's items: Left and Right walk the message's links,
    // cards and files, Space and Enter act on the current one as clicking
    // it does.
    if app.page == Page::Chats
        && app.open_chat.is_some()
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && !menu_open
        && app.call.is_none()
        && let Some(chat) = app.open_chat.clone()
        && ctx.memory(|memory| memory.has_focus(crate::ui::conversation::messages_list_id(&chat)))
        && !app.selecting_in(&chat)
        && let Some(row) = app
            .current_message(&chat)
            .and_then(|id| app.row_message(&chat, &id))
    {
        message_item_keys(app, ctx, &chat, &row);
    }
    if app.page == Page::Chats
        && app.open_chat.is_some()
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && !menu_open
        && let Some(chat) = app.open_chat.clone()
        && ctx.memory(|memory| memory.has_focus(crate::ui::conversation::messages_list_id(&chat)))
        && let Some(step) =
            crate::ui::list_nav::take_step(ctx, || app.message_page_rows(ctx, &chat))
    {
        actions.push(Action::MessageCursor(step));
    }
    // PgUp/PgDn/Home/End scroll the open chat. PgUp/PgDn also work while the
    // composer has focus, since egui's TextEdit does not handle them itself;
    // Home/End keep moving the text cursor in a non-empty field, as ↑ keeps
    // its normal meaning outside an empty composer.
    if app.page == Page::Chats
        && app.open_chat.is_some()
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && !menu_open
        && !super::accessibility::list_focused(ctx)
    {
        ctx.input_mut(|input| {
            if take_plain(input, Key::PageUp) {
                actions.push(Action::ScrollPage(Scroll::PageUp));
            }
            if take_plain(input, Key::PageDown) {
                actions.push(Action::ScrollPage(Scroll::PageDown));
            }
            if jump_allowed {
                if take_plain(input, Key::Home) {
                    actions.push(Action::ScrollPage(Scroll::Top));
                }
                if take_plain(input, Key::End) {
                    actions.push(Action::ScrollPage(Scroll::Bottom));
                }
            }
        });
    }
    let escape = (!menu_open || app.reaction_target.is_some())
        && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
    if escape {
        if app.show_update {
            actions.push(Action::CloseUpdate);
        } else if app.dialog.is_some() && app.group_name_edit.is_some() {
            // Cancels the group rename and keeps the dialog open.
            actions.push(Action::CloseGroupName);
        } else if matches!(app.dialog, Some(crate::model::Dialog::ChatInfo(_)))
            && app.gallery.tab.is_some()
        {
            actions.push(Action::GalleryTab(None));
        } else if matches!(app.dialog, Some(crate::model::Dialog::Statuses))
            && app.status_shown.is_some()
        {
            // The first Escape leaves the viewer for its authors' list.
            actions.push(Action::StatusClose);
        } else if app.dialog.is_some() {
            actions.push(Action::CloseDialog);
        } else if app.recording.is_some() {
            actions.push(Action::CancelRecording);
        } else if app.picker.is_some() || app.reaction_target.is_some() {
            actions.push(Action::ClosePicker);
        } else if app.selection.is_some() {
            // Taken here, the key never reaches the selection bar, and
            // would otherwise fall through to closing the chat.
            actions.push(Action::CancelSelection);
        } else if app.chat_search_visible() && app.chat_search_calendar {
            // The day filter first, then the pane it hangs from.
            app.chat_search_calendar = false;
        } else if app.chat_search_visible() && !composer_focused {
            actions.push(Action::CloseChatSearch);
        } else if app.show_starred {
            actions.push(Action::ToggleStarred);
        } else if app.emoji_start.is_some() {
            actions.push(Action::CloseEmojiSuggestions);
        } else if app.mention_start.is_some() {
            actions.push(Action::CloseMentions);
        } else if !app.pending.is_empty() {
            actions.push(Action::ClearPending);
        } else if app.editing.is_some() {
            actions.push(Action::CancelEdit);
        } else if app.reply_to.is_some() {
            actions.push(Action::CancelReply);
        } else if app.page == Page::Wallpaper {
            actions.push(Action::Open(Page::Settings));
        } else if app.page == Page::Settings && !app.settings_search.is_empty() {
            actions.push(Action::SearchSettings(String::new()));
        } else if app.page == Page::Settings {
            actions.push(Action::Open(Page::Chats));
        } else if search_focused || !app.search.is_empty() {
            if !app.search.is_empty() {
                actions.push(Action::Search(String::new()));
            }
            if app.open_chat.is_some() {
                actions.push(Action::FocusComposer);
            }
        } else if app.chat_search_visible() {
            actions.push(Action::CloseChatSearch);
        } else if app.open_chat.is_some() {
            actions.push(Action::CloseChat);
        } else if app.locked_folder {
            actions.push(Action::CloseLockedFolder);
        }
    }
    if escape {
        keep_focus_on_escape(app, ctx);
    }
    // Enter sends a recording because the text field is hidden.
    if app.recording.is_some()
        && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter))
    {
        actions.push(Action::SendRecording);
    }
    // Alt+Up/Down switches chats without leaving the composer, and so do
    // Ctrl+Shift+[ and Ctrl+Shift+], WhatsApp's own keys. With Shift held,
    // US-style layouts report the brackets as braces, so both spellings
    // count; a layout with another character on the shifted key has Alt.
    let step = ctx.input_mut(|input| {
        let brackets = Modifiers::COMMAND | Modifiers::SHIFT;
        let mut pressed = |modifiers: Modifiers, keys: &[Key]| {
            keys.iter().any(|key| input.consume_key(modifiers, *key))
        };
        if pressed(Modifiers::ALT, &[Key::ArrowDown])
            || pressed(brackets, &[Key::CloseBracket, Key::CloseCurlyBracket])
        {
            1
        } else if pressed(Modifiers::ALT, &[Key::ArrowUp])
            || pressed(brackets, &[Key::OpenBracket, Key::OpenCurlyBracket])
        {
            -1
        } else {
            0
        }
    });
    // Numbered shortcuts use the same ordering and filters as the sidebar.
    let number = if app.page == Page::Chats
        && app.dialog.is_none()
        && !app.show_update
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.recording.is_none()
        && !menu_open
    {
        ctx.input_mut(take_chat_number)
    } else {
        None
    };
    if step != 0 || number.is_some() {
        // Switching chats from the keyboard is keyboard navigation, as Tab
        // is: a chat without a message field then hands its focus to the
        // message list rather than to nothing.
        ctx.data_mut(|data| data.insert_temp(crate::theme::keyboard_focus_id(), true));
        let visible = app.visible_chats();
        if !visible.is_empty() {
            let current = app
                .open_chat
                .as_ref()
                .and_then(|open| visible.iter().position(|chat| chat.id == *open));
            let next = number.unwrap_or_else(|| match current {
                Some(index) => (index as i64 + step).rem_euclid(visible.len() as i64) as usize,
                None => 0,
            });
            if let Some(next) = visible.get(next).map(|chat| chat.id.clone()) {
                if number.is_some() {
                    app.search_selected = None;
                }
                // Stepping through the Unread list must not shift it underfoot.
                if app.search.trim().is_empty() && !app.show_archived {
                    actions.push(Action::KeepUnread(next.clone()));
                }
                app.scroll_chat_into_view = Some(next.clone());
                actions.push(Action::OpenChat(next));
            }
        }
    }
    // Arrow Up in an empty, focused composer edits the user's most recent
    // message, as WhatsApp does. The key keeps its normal meaning everywhere
    // else: it navigates open overlays and moves the cursor in a non-empty
    // field.
    let edit_previous = composer_focused
        && app.page == Page::Chats
        && app.open_chat.is_some()
        && app
            .open_chat
            .as_deref()
            .and_then(|id| app.chat(id))
            .is_some_and(Chat::can_send)
        && app.composer.is_empty()
        && app.pending.is_empty()
        && app.editing.is_none()
        && app.picker.is_none()
        && app.reaction_target.is_none()
        && app.dialog.is_none()
        && !app.show_update
        && app.recording.is_none()
        && !menu_open
        && ctx.input_mut(|input| take_plain(input, Key::ArrowUp));
    if let Some(id) = edit_previous.then(|| app.previous_own_editable()).flatten() {
        actions.push(Action::Edit(id));
    }
    app.actions.extend(actions);
}

fn accessibility_shortcuts(app: &mut App, ctx: &egui::Context) {
    if app.page != Page::Chats
        || (app.dialog.is_some() && !super::dialogs::uses_side_panel(app, ctx))
        || app.show_update
        || app.picker.is_some()
        || app.reaction_target.is_some()
        || egui::Popup::is_any_open(ctx)
    {
        return;
    }
    let modifiers = Modifiers::ALT;
    let section = ctx.input_mut(|input| {
        // Alt+2 (Control+2 on macOS) belongs to the message list: it focuses
        // the list rather than opening its newest message.
        [Key::Num1, Key::Num3]
            .into_iter()
            .position(|key| take_exact(input, modifiers, key))
    });
    if section == Some(0) {
        if let Some(chat) = app.visible_chats().first().map(|chat| chat.id.clone()) {
            app.scroll_chat_into_view = Some(chat.clone());
            super::accessibility::request_item(ctx, super::chats::chat_row_id(&chat).with("focus"));
        }
    } else if let Some(section) = section
        && let Some(chat) = app.open_chat.clone()
        && let Some(conversation) = app.conversations.get(&chat)
    {
        let message = if section == 1 {
            app.unread_divider
                .as_ref()
                .filter(|divider| divider.chat == chat)
                .and_then(|divider| {
                    conversation
                        .messages
                        .iter()
                        .rev()
                        .filter(|message| !message.from_me)
                        .nth(divider.count.saturating_sub(1) as usize)
                })
        } else {
            conversation.messages.last()
        };
        if let Some(message) = message {
            let id = message.id.clone();
            super::accessibility::request_item(ctx, super::conversation::bubble_id(&chat, &id));
            app.actions.push(Action::OpenMessage { chat, message: id });
        }
    }
    if app.recording.is_none()
        && app.current_chat().is_some_and(Chat::can_send)
        && ctx.input_mut(|input| take_exact(input, Modifiers::COMMAND | Modifiers::SHIFT, Key::R))
    {
        app.actions.push(Action::StartRecording);
    }
}

pub(crate) fn take_exact(input: &mut egui::InputState, expected: Modifiers, key: Key) -> bool {
    let mut taken = false;
    input.events.retain(|event| {
        if matches!(event, egui::Event::Key { key: actual, pressed: true, modifiers, .. }
            if *actual == key && modifiers.shift == expected.shift && modifiers.alt == expected.alt
                && if expected.command { modifiers.command } else { modifiers.ctrl == expected.ctrl }) {
            taken = true;
            false
        } else { true }
    });
    taken
}

/// Recognizes native Copy and plain Command/Ctrl+C presses.
fn is_copy_event(event: &egui::Event) -> bool {
    matches!(event, egui::Event::Copy)
        || matches!(event, egui::Event::Key {
            key: Key::C, pressed: true, modifiers, ..
        } if (modifiers.command || modifiers.ctrl)
            && !modifiers.alt && !modifiers.shift)
}

/// Takes an exact Command/Ctrl+1..9 press, leaving modified number keys alone.
fn take_chat_number(input: &mut egui::InputState) -> Option<usize> {
    let keys = [
        Key::Num1,
        Key::Num2,
        Key::Num3,
        Key::Num4,
        Key::Num5,
        Key::Num6,
        Key::Num7,
        Key::Num8,
        Key::Num9,
    ];
    let mut number = None;
    input.events.retain(|event| {
        if number.is_none()
            && let egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } = event
            && modifiers.matches_exact(Modifiers::COMMAND)
            && let Some(index) = keys.iter().position(|candidate| candidate == key)
        {
            number = Some(index);
            return false;
        }
        true
    });
    number
}

/// Takes this frame's first Ctrl+. or Ctrl+;, the emoji picker's keys.
fn take_emoji_key(input: &mut egui::InputState) -> bool {
    let mut taken = false;
    input.events.retain(|event| {
        let matches = !taken
            && matches!(
                event,
                egui::Event::Key {
                    key: Key::Period | Key::Semicolon,
                    pressed: true,
                    modifiers,
                    ..
                } if emoji_modifiers(*modifiers)
            );
        taken |= matches;
        !matches
    });
    taken
}

/// Control held with no other modifier, as the emoji keys want it.
fn emoji_modifiers(modifiers: Modifiers) -> bool {
    modifiers.matches_exact(Modifiers::CTRL)
}

/// Takes Ctrl+Tab alone, leaving plain Tab and every other modified Tab for
/// focus navigation and the platform.
fn take_recent_chat(input: &mut egui::InputState) -> bool {
    let mut taken = false;
    input.events.retain(|event| {
        let matches = matches!(
            event,
            egui::Event::Key {
                key: Key::Tab,
                pressed: true,
                modifiers,
                ..
            } if modifiers.ctrl && !modifiers.alt && !modifiers.shift && !modifiers.mac_cmd
        );
        taken |= matches;
        !matches
    });
    taken
}

/// Removes this frame's first plain (unmodified) press of `key`, if any, and
/// reports whether one was found. `consume_key` is unsuitable here: it also
/// matches the key with Shift or Alt held, and a plain binding must leave
/// those combinations, such as Shift+Home for text selection, alone.
pub(crate) fn take_plain(input: &mut egui::InputState, key: Key) -> bool {
    let mut taken = false;
    input.events.retain(|event| {
        if taken {
            return true;
        }
        let matches = matches!(
            event,
            egui::Event::Key {
                key: found,
                pressed: true,
                modifiers,
                ..
            } if *found == key && *modifiers == Modifiers::NONE
        );
        taken |= matches;
        !matches
    });
    taken
}

/// Handles keys while the image preview is open. No chat shortcut runs, and
/// typing and clipboard input are swallowed; Left/Right browse photos, while
/// Tab, Enter, Space and Up/Down stay for the preview's own controls.
fn preview_keys(app: &mut App, ctx: &egui::Context) {
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        if input.consume_key(Modifiers::NONE, Key::Escape) {
            actions.push(Action::CloseImagePreview);
        }
        let copy_shortcut = input.consume_key(Modifiers::COMMAND, Key::C)
            || input.consume_key(Modifiers::CTRL, Key::C);
        if copy_shortcut && let Some(preview) = &app.image_preview {
            actions.push(Action::CopyImage(preview.path().to_owned()));
        }
        let mut event_actions = Vec::new();
        for event in &input.events {
            if let egui::Event::Key {
                key,
                modifiers,
                pressed: true,
                ..
            } = event
            {
                event_actions.extend(crate::image_preview::preview_action(*key, *modifiers));
            }
        }
        actions.extend(event_actions);
        input
            .events
            .retain(|event| !crate::image_preview::consumes_key(event));
    });
    app.actions.extend(actions);
}

/// Keeps keyboard focus where it was on an Escape in the window itself,
/// anywhere in the app, after keyboard navigation or with a screen reader
/// attached; a dialog or the update notice keep their own handling of it.
/// egui lets go of focus on a plain Escape at the start of the pass, before
/// anything is drawn, unless the focused widget claims the key, and a screen
/// reader then hears focus fall to the window. Given back in this pass,
/// which draws the widget again, focus stays on it while Escape still does
/// what `handle` gives it to do: it closes or clears the topmost thing, up
/// to the open chat itself. When that takes the focused widget away, as
/// closing the chat does, focus goes with it once the widget is no longer
/// drawn, as before; with nothing to close, focus stays where it was.
fn keep_focus_on_escape(app: &App, ctx: &egui::Context) {
    if app.dialog.is_some() || app.show_update || ctx.memory(|memory| memory.focused().is_some()) {
        return;
    }
    // Only for the keyboard: a pointer user who clicked into a field sees
    // it let go on Escape, as egui does.
    if escape_keeps_focus(ctx)
        && let Some(id) = focus_before_escape(ctx)
    {
        ctx.memory_mut(|memory| memory.request_focus(id));
    }
}

/// The widget that had focus when this pass began, among those the last
/// pass drew.
fn focus_before_escape(ctx: &egui::Context) -> Option<egui::Id> {
    let drawn: Vec<egui::Id> = ctx.viewport(|viewport| {
        viewport
            .prev_pass
            .widgets
            .layers()
            .flat_map(|(_, widgets)| widgets.iter().map(|widget| widget.id))
            .collect()
    });
    ctx.memory(|memory| {
        drawn
            .into_iter()
            .find(|id| memory.had_focus_last_frame(*id))
    })
}

/// Whether Escape keeps focus where it was: after keyboard navigation, or
/// with a screen reader attached, which always hears where focus goes.
fn escape_keeps_focus(ctx: &egui::Context) -> bool {
    let keyboard = ctx.data(|data| {
        data.get_temp::<bool>(crate::theme::keyboard_focus_id())
            .unwrap_or(false)
    });
    // A node builder exists only while AccessKit is on.
    let screen_reader = ctx
        .accesskit_node_builder(egui::accesskit_root_id(), |_| ())
        .is_some();
    keyboard || screen_reader
}

/// Drops the text of a digit shortcut taken this frame: a digit with Alt,
/// or with Control on macOS, such as Alt+2. Windows delivers Alt+2 as the
/// key and as the text "2": with the key taken by its shortcut, the text
/// would still reach a focused field and be typed there. Control with a
/// digit types nothing.
fn drop_shortcut_text(ctx: &egui::Context) {
    let held = |modifiers: &Modifiers| modifiers.alt;
    ctx.input_mut(|input| {
        // Taken: in this frame's input, but no longer among its events.
        let taken: Vec<&str> = input
            .raw
            .events
            .iter()
            .filter(|event| !input.events.contains(event))
            .filter_map(|event| match event {
                egui::Event::Key {
                    key:
                        key @ (Key::Num0
                        | Key::Num1
                        | Key::Num2
                        | Key::Num3
                        | Key::Num4
                        | Key::Num5
                        | Key::Num6
                        | Key::Num7
                        | Key::Num8
                        | Key::Num9),
                    pressed: true,
                    modifiers,
                    ..
                } if held(modifiers) => Some(key.name()),
                _ => None,
            })
            .collect();
        for digit in taken {
            if let Some(index) = input
                .events
                .iter()
                .position(|event| matches!(event, egui::Event::Text(text) if text == digit))
            {
                input.events.remove(index);
            }
        }
    });
}

/// Handles keys while the cropper covers the window: Escape leaves the
/// staged picture alone, Enter keeps the crop.
/// Handles keys while the editor covers the window: Escape leaves the
/// staged picture alone, Enter keeps the edit, Ctrl+Z takes the latest mark
/// back, and Ctrl+Shift+Z or Ctrl+Y puts it back. Enter on a focused button
/// presses that button instead, as anywhere else.
fn editor_keys(app: &mut App, ctx: &egui::Context) {
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        if input.consume_key(Modifiers::NONE, Key::Escape) {
            actions.push(Action::CancelPictureEdit);
        }
        if input.consume_key(Modifiers::NONE, Key::Enter) {
            actions.push(Action::ApplyPictureEdit);
        }
    });
    /// A step through the editor's marks.
    enum Step {
        Undo,
        Redo,
    }
    let mut history = None;
    ctx.input_mut(|input| {
        // A pattern without Shift matches with Shift held too, so the redo
        // spelling is asked for first.
        if input.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::Z)
            || input.consume_key(Modifiers::COMMAND, Key::Y)
        {
            history = Some(Step::Redo);
        } else if input.consume_key(Modifiers::COMMAND, Key::Z) {
            history = Some(Step::Undo);
        }
    });
    if let Some(edit) = app
        .picture_edit
        .as_mut()
        .filter(|edit| edit.drawing.is_none())
    {
        match history {
            Some(Step::Undo) => edit.undo(),
            Some(Step::Redo) => edit.redo(),
            None => {}
        }
    }
    app.actions.extend(actions);
}

/// Handles keys while a video covers the window: Escape puts it back, Space
/// plays or pauses, M mutes, S changes the speed, and the arrows jump five
/// seconds. No chat shortcut runs and nothing is typed into the composer
/// under it.
fn video_keys(app: &mut App, ctx: &egui::Context) {
    let Some((message, path)) = app
        .video
        .message()
        .map(str::to_owned)
        .zip(app.video.path().map(std::path::Path::to_owned))
    else {
        return;
    };
    let status = app.video.status(&message);
    let jump = |seconds: f32| {
        let status = status.as_ref()?;
        let total = status.total.as_secs_f32();
        (total > 0.0).then(|| Action::SeekVideo {
            message: message.clone(),
            fraction: ((status.position.as_secs_f32() + seconds) / total).clamp(0.0, 1.0),
        })
    };
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        if input.consume_key(Modifiers::NONE, Key::Escape) {
            actions.push(Action::CollapseVideo);
        }
        if input.consume_key(Modifiers::NONE, Key::Space) {
            actions.push(Action::PlayVideo {
                message: message.clone(),
                path: path.clone(),
            });
        }
        if input.consume_key(Modifiers::NONE, Key::M) {
            actions.push(Action::ToggleVideoSound);
        }
        if input.consume_key(Modifiers::NONE, Key::S) {
            actions.push(Action::CycleVideoSpeed);
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowLeft) {
            actions.extend(jump(-5.0));
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowRight) {
            actions.extend(jump(5.0));
        }
        input.events.retain(|event| {
            !matches!(
                event,
                egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Paste(_)
            )
        });
    });
    app.actions.extend(actions);
}

/// While a status update is open, Left and Right step through its author's
/// updates, and Escape goes back to the authors' list. The viewer has no
/// text fields, so no caret loses its arrows; the picker showing over it
/// keeps them.
fn status_keys(app: &mut App, ctx: &egui::Context) {
    let mut actions = Vec::new();
    ctx.input_mut(|input| {
        if input.consume_key(Modifiers::NONE, Key::Escape) {
            actions.push(Action::StatusClose);
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowLeft) {
            actions.push(Action::StatusStep { back: true });
        }
        if input.consume_key(Modifiers::NONE, Key::ArrowRight) {
            actions.push(Action::StatusStep { back: false });
        }
    });
    app.actions.extend(actions);
}

/// While the sticker viewer is open, do not type into the chat behind it.
fn sticker_view_keys(app: &mut App, ctx: &egui::Context) {
    let close = ctx.input_mut(|input| {
        let close = input.consume_key(Modifiers::NONE, Key::Escape);
        input
            .events
            .retain(|event| !crate::image_preview::consumes_key(event));
        close
    });
    if close {
        app.actions.push(Action::CloseStickerPreview);
    }
}

/// Ctrl+F searches the open chat, as in WhatsApp, the chat list when no
/// chat is open, and the settings on the Settings page.
fn find_action(app: &App) -> Action {
    match app.page {
        Page::Settings => Action::FocusSettingsSearch,
        Page::Chats if app.open_chat.is_some() => Action::OpenChatSearch,
        _ => Action::FocusSearch,
    }
}

/// Ctrl+E archives the open chat, or unarchives an archived one, as in
/// WhatsApp. Archiving closes the chat, as the chat menus do.
fn archive_action(app: &App) -> Option<Action> {
    if app.page != Page::Chats
        || app.dialog.is_some()
        || app.show_update
        || app.picker.is_some()
        || app.reaction_target.is_some()
        || app.recording.is_some()
    {
        return None;
    }
    let chat = app.chat(app.open_chat.as_deref()?)?;
    Some(Action::SetArchived(chat.id.clone(), !chat.archived))
}

fn next_account_action(app: &App) -> Option<Action> {
    if !app.has_several_accounts()
        || app.adding_account
        || app.dialog.is_some()
        || app.show_update
        || app.picker.is_some()
        || app.reaction_target.is_some()
        || app.recording.is_some()
    {
        return None;
    }
    let next = (app.active + 1) % app.accounts.len();
    Some(Action::SwitchAccount(app.accounts[next].id.clone()))
}

/// The message list's modifier held alone: Control on macOS, Alt elsewhere.
fn messages_modifier_alone(modifiers: Modifiers) -> bool {
    modifiers.matches_exact(Modifiers::ALT)
}

/// The message list's shortcut: Alt+2, but Control+2 on macOS, where
/// Command with a digit opens a chat. `label` shows "Control" as ⌃ there.
const MESSAGES_KEYS: &str = "Alt+2";

/// Shortcuts shown in the help dialog.
pub const SHORTCUTS: &[(&str, &str)] = &[
    (
        "Alt+1 / 2 / 3",
        "Chat list / messages / first unread (Ctrl on macOS)",
    ),
    (
        "Home / End, PgUp / PgDn",
        "First / last / page of focused list items",
    ),
    ("← / →", "Move between controls in a toolbar"),
    (
        "R / E / C / F",
        "Reply / react / copy / forward the focused message",
    ),
    (
        "Delete / Space / O / V / G",
        "Delete / play / open / poll options / quoted message",
    ),
    ("Ctrl+← / Ctrl+→", "Move between a message's controls"),
    (
        "Ctrl+Shift+R",
        "Start a voice recording (Enter sends, Escape discards)",
    ),
    ("Ctrl+K / Ctrl+Shift+F", "Search chats"),
    ("Ctrl+F", "Search the open chat or the settings"),
    (
        "↑ / ↓, Enter",
        "Walk the open chat's search results and jump to one",
    ),
    ("Ctrl+L", "Focus the message input"),
    ("Ctrl+. / Ctrl+;", "Emoji picker (in the message input)"),
    ("Alt+↑ / Alt+↓", "Previous / next chat"),
    ("Ctrl+Tab", "Switch between the two most recent chats"),
    (
        "Space / Enter",
        "Play or pause, open or vote for the selected item, downloading it first when needed (in the message list)",
    ),
    (
        "← / →",
        "Move between the items of a message: its file, links, phone numbers, cards or poll options (in the message list)",
    ),
    ("Ctrl+Shift+[ / ]", "Previous / next chat, as in WhatsApp"),
    ("Ctrl+1..9", "Open a chat by its position in the chat list"),
    ("↑", "Edit the previous message (when the input is empty)"),
    ("Enter", "Send (Shift+Enter for a new line)"),
    (
        "Escape",
        "Dismiss the current action, return from search, or close the chat",
    ),
    ("Ctrl+N", "New chat or message yourself"),
    ("Ctrl+E", "Archive or unarchive the open chat"),
    ("Ctrl+Shift+A", "Switch to the next linked number"),
    (
        "Ctrl+V",
        "Paste text, or stage a picture from the clipboard",
    ),
    ("Ctrl+B", "Collapse or expand the chat list"),
    ("Ctrl+Shift+H", "Toggle screen privacy"),
    (
        "Ctrl+End",
        "Jump to the newest message (when the input is empty)",
    ),
    (MESSAGES_KEYS, "Go to the message list"),
    (
        "↑ / ↓, Home / End, PgUp / PgDn",
        "Move through messages (in the message list)",
    ),
    ("PgUp / PgDn", "Scroll the open chat by a page"),
    (
        "Home / End",
        "Top / bottom of the open chat (when the input is empty)",
    ),
    ("Ctrl+,", "Settings"),
    ("Ctrl++ / Ctrl+-", "Zoom in / out"),
    ("Ctrl+0", "Reset zoom"),
    ("? / Ctrl+/", "Keyboard shortcuts (? when not typing)"),
    (
        "Ctrl+Shift+L",
        "Lock Zap Fasterk (with an app lock password)",
    ),
    ("Ctrl+W", "Close the window or chat (choose in Settings)"),
    ("Ctrl+Q", "Quit"),
];

/// Uses Command and Option labels on macOS.
pub fn label(keys: &str) -> String {
    keys.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_shortcuts_leave_numbered_chats_and_text_input_alone() {
        let (_root, mut app, ids) = app_with_chats(2);
        let ctx = egui::Context::default();
        let modifiers = Modifiers::ALT;
        assert!(!press(&mut app, &ctx, Key::Num1, modifiers));
        assert!(
            app.actions
                .iter()
                .all(|action| !matches!(action, Action::OpenChat(_)))
        );
        assert_eq!(app.scroll_chat_into_view.as_ref(), Some(&ids[0]));
        app.actions.clear();
        assert!(!press(&mut app, &ctx, Key::Num2, Modifiers::COMMAND));
        assert!(
            app.actions
                .iter()
                .any(|action| matches!(action, Action::OpenChat(id) if *id == ids[1]))
        );
    }

    #[test]
    fn close_shortcut_preserves_default_and_uses_platform_command_for_chat_choice() {
        let (_root, mut app, ids) = app_with_chats(1);
        let ctx = egui::Context::default();
        app.open_chat = Some(ids[0].clone());
        assert!(!press(&mut app, &ctx, Key::W, Modifiers::COMMAND));
        assert_eq!(app.actions, [Action::CloseWindow]);
        app.settings.close_chat_with_ctrl_w = true;
        for modifiers in [
            Modifiers {
                ctrl: true,
                command: true,
                ..Modifiers::NONE
            },
            Modifiers {
                mac_cmd: true,
                command: true,
                ..Modifiers::NONE
            },
        ] {
            app.actions.clear();
            assert!(!press(&mut app, &ctx, Key::W, modifiers));
            assert_eq!(app.actions, [Action::CloseChat]);
        }
        app.actions.clear();
        assert!(press(&mut app, &ctx, Key::W, Modifiers::NONE));
        assert!(app.actions.is_empty());
    }

    #[test]
    fn close_chat_shortcut_does_not_close_window_or_hidden_chat() {
        let (_root, mut app, ids) = app_with_chats(1);
        app.settings.close_chat_with_ctrl_w = true;
        assert!(close_shortcut_action(&app, &egui::Context::default()).is_none());
        app.open_chat = Some(ids[0].clone());
        app.dialog = Some(Dialog::Shortcuts);
        assert!(close_shortcut_action(&app, &egui::Context::default()).is_none());
        app.dialog = None;
        app.page = Page::Settings;
        assert!(close_shortcut_action(&app, &egui::Context::default()).is_none());
        app.page = Page::Chats;
        app.window_hidden = true;
        assert!(close_shortcut_action(&app, &egui::Context::default()).is_none());
        app.window_hidden = false;
        app.app_lock.lock();
        assert!(close_shortcut_action(&app, &egui::Context::default()).is_none());
        app.settings.close_chat_with_ctrl_w = false;
        assert_eq!(
            close_shortcut_action(&app, &egui::Context::default()),
            Some(Action::CloseWindow)
        );
    }

    /// Ctrl+. and Ctrl+; open the emoji picker from the focused message
    /// field, as they open GTK's emoji chooser. Elsewhere, while editing,
    /// or with the emoji tab already open, the keys are left for the views.
    #[test]
    fn ctrl_period_and_semicolon_open_the_emoji_picker_from_the_composer() {
        let (_root, mut app, ids) = app_with_chats(1);
        app.open_chat = Some(ids[0].clone());
        let ctx = egui::Context::default();
        // What a real Control press carries on Windows: Ctrl and Command.
        let control = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::NONE
        };
        let composer = egui::Id::new("composer-text");
        for key in [Key::Period, Key::Semicolon] {
            app.actions.clear();
            ctx.memory_mut(|memory| memory.request_focus(composer));
            press(&mut app, &ctx, key, control);
            assert!(
                app.actions
                    .contains(&Action::TogglePicker(PickerTab::Emoji)),
                "{key:?} opened the picker"
            );
        }
        // With the emoji tab open the keys pass through to the views.
        app.actions.clear();
        app.picker = Some(PickerTab::Emoji);
        ctx.memory_mut(|memory| memory.request_focus(composer));
        assert!(press(&mut app, &ctx, Key::Period, control));
        assert!(app.actions.is_empty());
        // A dialog keeps them too.
        app.actions.clear();
        app.picker = None;
        app.dialog = Some(Dialog::Shortcuts);
        ctx.memory_mut(|memory| memory.request_focus(composer));
        assert!(press(&mut app, &ctx, Key::Period, control));
        assert!(app.actions.is_empty());
    }

    #[test]
    fn only_control_tab_switches_recent_chats() {
        let (_root, mut app, _) = app_with_chats(0);
        app.page = Page::Chats;
        let ctx = egui::Context::default();
        for modifiers in [Modifiers::CTRL, Modifiers::CTRL | Modifiers::COMMAND] {
            app.actions.clear();
            assert!(!press(&mut app, &ctx, Key::Tab, modifiers));
            assert!(app.actions.contains(&Action::SwitchRecentChat));
        }
        for modifiers in [
            Modifiers::NONE,
            Modifiers::SHIFT,
            Modifiers::CTRL | Modifiers::SHIFT,
            Modifiers::CTRL | Modifiers::ALT,
            Modifiers::MAC_CMD,
        ] {
            app.actions.clear();
            press(&mut app, &ctx, Key::Tab, modifiers);
            assert!(!app.actions.contains(&Action::SwitchRecentChat));
        }
        app.dialog = Some(Dialog::Shortcuts);
        app.actions.clear();
        press(&mut app, &ctx, Key::Tab, Modifiers::CTRL);
        assert!(!app.actions.contains(&Action::SwitchRecentChat));
        app.dialog = None;
        app.page = Page::Settings;
        app.actions.clear();
        press(&mut app, &ctx, Key::Tab, Modifiers::CTRL);
        assert!(!app.actions.contains(&Action::SwitchRecentChat));
    }

    #[test]
    fn ctrl_f_searches_the_open_chat_the_list_or_the_settings() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        assert!(matches!(find_action(&app), Action::FocusSearch));
        app.open_chat = Some("1@s.whatsapp.net".into());
        app.page = Page::Chats;
        assert!(matches!(find_action(&app), Action::OpenChatSearch));
        app.page = Page::Settings;
        assert!(matches!(find_action(&app), Action::FocusSettingsSearch));
        app.page = Page::Wallpaper;
        assert!(matches!(find_action(&app), Action::FocusSearch));
    }

    #[test]
    fn escape_clears_the_settings_search_before_leaving_settings() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.page = Page::Settings;
        app.settings_search = "sound".into();
        let ctx = egui::Context::default();
        escape(&mut app, &ctx);
        assert!(
            matches!(app.actions.as_slice(), [Action::SearchSettings(text)] if text.is_empty())
        );
        app.actions.clear();
        app.settings_search.clear();
        escape(&mut app, &ctx);
        assert!(matches!(
            app.actions.as_slice(),
            [Action::Open(Page::Chats)]
        ));
    }

    /// Ctrl+Z takes the latest mark back, and Ctrl+Shift+Z and Ctrl+Y put
    /// it back: the Shift spelling must not undo again.
    #[test]
    fn the_editors_undo_and_redo_keys_step_through_its_marks() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let mut edit = crate::model::PictureEdit::new(
            0,
            crate::model::PictureSource::File("photo.png".into()),
            100,
            100,
        );
        for at in [10.0, 30.0] {
            edit.begin_mark(crate::model::Brush::default(), [at, at]);
            edit.finish_mark();
        }
        app.picture_edit = Some(edit);
        let ctx = egui::Context::default();
        let press = |app: &mut App, key: Key, modifiers: Modifiers| {
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers,
                    }],
                    ..Default::default()
                },
                |ui| handle(app, ui.ctx()),
            );
            output.textures_delta.clear();
            app.picture_edit.as_ref().map_or(0, |edit| edit.marks.len())
        };
        assert_eq!(press(&mut app, Key::Z, Modifiers::COMMAND), 1);
        assert_eq!(press(&mut app, Key::Z, Modifiers::COMMAND), 0);
        let redo = Modifiers::COMMAND | Modifiers::SHIFT;
        assert_eq!(press(&mut app, Key::Z, redo), 1);
        assert_eq!(press(&mut app, Key::Y, Modifiers::COMMAND), 2);
        assert_eq!(
            press(&mut app, Key::Z, Modifiers::NONE),
            2,
            "Z alone does nothing"
        );
        assert!(app.actions.is_empty());
    }

    fn escape(app: &mut App, ctx: &egui::Context) {
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| handle(app, ui.ctx()),
        );
        output.textures_delta.clear();
    }

    #[test]
    fn escape_folds_the_day_filter_then_the_search_pane_then_the_chat() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.page = Page::Chats;
        app.open_chat = Some("fixture".into());
        app.chat_search_open = true;
        app.chat_search_calendar = true;
        let ctx = egui::Context::default();
        escape(&mut app, &ctx);
        assert!(!app.chat_search_calendar);
        assert!(app.actions.is_empty(), "the pane stays for now");
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CloseChatSearch]));
        app.actions.clear();
        // Settings hide the pane, so Escape leaves them instead.
        app.page = Page::Settings;
        escape(&mut app, &ctx);
        assert!(matches!(
            app.actions.as_slice(),
            [Action::Open(Page::Chats)]
        ));
    }

    #[test]
    fn escape_leaves_the_status_viewer_before_closing_its_dialog() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let ctx = egui::Context::default();
        app.dialog = Some(Dialog::Statuses);
        app.status_shown = Some("ana:new".into());
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::StatusClose]));
        app.actions.clear();
        app.status_shown = None;
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CloseDialog]));
    }

    fn press_consumed(app: &mut App, ctx: &egui::Context, key: Key) {
        assert!(
            !press(app, ctx, key, Modifiers::NONE),
            "the viewer takes the arrow key"
        );
    }

    #[test]
    fn status_viewer_steps_with_the_arrow_keys() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let ctx = egui::Context::default();
        app.dialog = Some(Dialog::Statuses);
        app.status_shown = Some("ana:new".into());
        press_consumed(&mut app, &ctx, Key::ArrowLeft);
        assert!(matches!(
            app.actions.as_slice(),
            [Action::StatusStep { back: true }]
        ));
        app.actions.clear();
        press_consumed(&mut app, &ctx, Key::ArrowRight);
        assert!(matches!(
            app.actions.as_slice(),
            [Action::StatusStep { back: false }]
        ));
        app.actions.clear();
        // Back at the authors' list, the arrows are left alone.
        app.status_shown = None;
        assert!(press(&mut app, &ctx, Key::ArrowLeft, Modifiers::NONE));
        assert!(app.actions.is_empty());
    }

    #[test]
    fn focus_input_shortcut_only_targets_an_available_composer() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let ctx = egui::Context::default();
        for (page, chat, dialog, expected) in [
            (Page::Chats, Some("fixture"), None, true),
            (Page::Chats, None, None, false),
            (Page::Settings, Some("fixture"), None, false),
            (Page::Chats, Some("fixture"), Some(Dialog::Shortcuts), false),
        ] {
            app.page = page;
            app.open_chat = chat.map(str::to_owned);
            app.dialog = dialog;
            app.actions.clear();
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: Key::L,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::COMMAND,
                    }],
                    ..Default::default()
                },
                |ui| handle(&mut app, ui.ctx()),
            );
            output.textures_delta.clear();
            assert_eq!(
                matches!(app.actions.as_slice(), [Action::FocusComposer]),
                expected
            );
        }
    }

    #[test]
    fn escape_returns_from_search_and_reply_before_closing_the_chat() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.page = Page::Chats;
        app.open_chat = Some("fixture".into());
        let ctx = egui::Context::default();
        app.reply_to = Some("reply".into());
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CancelReply]));
        app.actions.clear();
        app.reply_to = None;
        app.search = "Ada".into();
        escape(&mut app, &ctx);
        assert!(
            matches!(app.actions.as_slice(), [Action::Search(text), Action::FocusComposer] if text.is_empty())
        );
        app.actions.clear();
        app.search.clear();
        escape(&mut app, &ctx);
        assert!(matches!(app.actions.as_slice(), [Action::CloseChat]));
    }

    #[test]
    fn escape_closes_the_update_before_touching_an_unfinished_message() {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        app.show_update = true;
        app.page = Page::Settings;
        app.reply_to = Some("reply-fixture".into());
        app.pending
            .push(crate::app::Pending::File("unsent.png".into()));
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: Key::Escape,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| handle(&mut app, ui.ctx()),
        );
        output.textures_delta.clear();
        assert!(matches!(app.actions.as_slice(), [Action::CloseUpdate]));
    }

    fn app_with_chats(count: usize) -> (tempfile::TempDir, App, Vec<String>) {
        let root = tempfile::tempdir().unwrap();
        let mut app = App::headless(
            crate::paths::AppDirs::under(root.path()),
            crate::settings::Settings::default(),
        )
        .0;
        let ids: Vec<String> = (0..count)
            .map(|index| format!("49170000{index:04}@s.whatsapp.net"))
            .collect();
        for (index, id) in ids.iter().enumerate() {
            let mut chat = Chat::new(id.clone(), format!("Chat {index:02}"));
            chat.last_activity = 100 - index as i64;
            app.chats.push(chat);
        }
        app.page = Page::Chats;
        (root, app, ids)
    }

    /// Runs `handle` on one key press and reports whether the press was left
    /// for the views, as every press that is not a shortcut must be.
    fn press(app: &mut App, ctx: &egui::Context, key: Key, modifiers: Modifiers) -> bool {
        let mut survived = false;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers,
                }],
                ..Default::default()
            },
            |ui| {
                handle(app, ui.ctx());
                survived = ui.ctx().input(|input| {
                    input.events.iter().any(|event| {
                        matches!(
                            event,
                            egui::Event::Key {
                                key: found,
                                pressed: true,
                                ..
                            } if *found == key
                        )
                    })
                });
            },
        );
        output.textures_delta.clear();
        survived
    }

    /// The modifiers a real Ctrl+Shift press carries on Linux and Windows,
    /// and a real Cmd+Shift press on macOS.
    fn ctrl_shift() -> [Modifiers; 3] {
        [
            Modifiers::COMMAND | Modifiers::SHIFT,
            Modifiers {
                ctrl: true,
                command: true,
                shift: true,
                ..Modifiers::NONE
            },
            Modifiers {
                mac_cmd: true,
                command: true,
                shift: true,
                ..Modifiers::NONE
            },
        ]
    }

    #[test]
    fn bracket_shortcuts_step_to_the_previous_and_next_chat() {
        let (_root, mut app, ids) = app_with_chats(3);
        app.open_chat = Some(ids[1].clone());
        let ctx = egui::Context::default();
        // With Shift held, US-style layouts report the brackets as braces.
        for modifiers in ctrl_shift() {
            for (key, expected) in [
                (Key::CloseBracket, &ids[2]),
                (Key::CloseCurlyBracket, &ids[2]),
                (Key::OpenBracket, &ids[0]),
                (Key::OpenCurlyBracket, &ids[0]),
            ] {
                app.actions.clear();
                app.scroll_chat_into_view = None;
                let survived = press(&mut app, &ctx, key, modifiers);
                assert!(!survived, "{key:?} with {modifiers:?} reached the views");
                assert!(
                    app.actions.contains(&Action::OpenChat(expected.clone())),
                    "{key:?} with {modifiers:?} opens {expected}: {:?}",
                    app.actions
                );
                assert_eq!(app.scroll_chat_into_view.as_ref(), Some(expected));
            }
        }
    }

    #[test]
    fn numbered_shortcuts_open_each_position_with_platform_command_modifiers() {
        let (_root, mut app, ids) = app_with_chats(9);
        let ctx = egui::Context::default();
        for modifiers in ctrl_shift().map(|modifiers| Modifiers {
            shift: false,
            ..modifiers
        }) {
            for (key, expected) in [
                Key::Num1,
                Key::Num2,
                Key::Num3,
                Key::Num4,
                Key::Num5,
                Key::Num6,
                Key::Num7,
                Key::Num8,
                Key::Num9,
            ]
            .into_iter()
            .zip(&ids)
            {
                app.actions.clear();
                app.scroll_chat_into_view = None;
                assert!(!press(&mut app, &ctx, key, modifiers));
                assert!(app.actions.contains(&Action::OpenChat(expected.clone())));
                assert_eq!(app.scroll_chat_into_view.as_ref(), Some(expected));
            }
        }
    }

    #[test]
    fn numbered_shortcuts_follow_pins_filters_search_and_archived_order() {
        let (_root, mut app, ids) = app_with_chats(4);
        let ctx = egui::Context::default();
        app.chats[3].pinned = true;
        app.chats[2].archived = true;
        app.chats[1].locked = true;
        assert!(!press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND));
        assert!(app.actions.contains(&Action::OpenChat(ids[3].clone())));
        app.actions.clear();
        assert!(!press(&mut app, &ctx, Key::Num2, Modifiers::COMMAND));
        assert!(app.actions.contains(&Action::OpenChat(ids[0].clone())));

        app.chat_filter = crate::model::ChatFilter::Unread;
        app.chats[0].unread = 1;
        app.actions.clear();
        press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND);
        assert!(app.actions.contains(&Action::OpenChat(ids[0].clone())));
        assert!(app.actions.contains(&Action::KeepUnread(ids[0].clone())));

        app.search = "Chat 02".into();
        app.search_selected = Some(ids[0].clone());
        app.actions.clear();
        press(&mut app, &ctx, Key::Num9, Modifiers::COMMAND);
        assert!(app.actions.is_empty());
        assert_eq!(app.search_selected.as_ref(), Some(&ids[0]));
        press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND);
        assert_eq!(app.actions, [Action::OpenChat(ids[2].clone())]);
        assert!(app.search_selected.is_none());

        app.search.clear();
        app.show_archived = true;
        app.actions.clear();
        press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND);
        assert_eq!(app.actions, [Action::OpenChat(ids[2].clone())]);
    }

    #[test]
    fn numbered_shortcuts_do_not_wrap_missing_positions_or_change_zero_zoom() {
        let (_root, mut app, ids) = app_with_chats(2);
        let ctx = egui::Context::default();
        app.open_chat = Some(ids[0].clone());
        for key in [Key::Num3, Key::Num9] {
            assert!(!press(&mut app, &ctx, key, Modifiers::COMMAND));
            assert!(app.actions.is_empty());
            assert!(app.scroll_chat_into_view.is_none());
        }
        app.chats.clear();
        press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND);
        assert!(app.actions.is_empty());
        press(&mut app, &ctx, Key::Num0, Modifiers::COMMAND);
        assert_eq!(app.actions, [Action::ResetZoom]);
    }

    #[test]
    fn numbered_shortcuts_leave_unbound_plain_and_modified_numbers_alone() {
        let (_root, mut app, _ids) = app_with_chats(2);
        let ctx = egui::Context::default();
        for modifiers in [
            Modifiers::NONE,
            Modifiers::SHIFT,
            Modifiers::ALT,
            Modifiers::COMMAND | Modifiers::SHIFT,
            Modifiers::COMMAND | Modifiers::ALT,
        ] {
            let modifiers = if modifiers.command {
                Modifiers {
                    ctrl: true,
                    mac_cmd: false,
                    ..modifiers
                }
            } else {
                modifiers
            };
            for key in [Key::Num1, Key::Num4] {
                let unconsumed = press(&mut app, &ctx, key, modifiers);
                let section_shortcut = modifiers == Modifiers::ALT && key == Key::Num1;
                assert_eq!(unconsumed, !section_shortcut, "{key:?}, {modifiers:?}");
                assert!(app.actions.is_empty());
            }
        }
    }

    #[test]
    fn numbered_shortcuts_leave_settings_and_overlays_alone() {
        let (_root, mut app, _ids) = app_with_chats(2);
        let ctx = egui::Context::default();
        app.page = Page::Settings;
        assert!(press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND));
        app.page = Page::Chats;
        app.dialog = Some(Dialog::Shortcuts);
        assert!(press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND));
        app.dialog = None;
        app.show_update = true;
        assert!(press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND));
        app.show_update = false;
        app.reaction_target = Some(("chat-fixture".into(), "message-fixture".into()));
        assert!(press(&mut app, &ctx, Key::Num1, Modifiers::COMMAND));
        assert!(app.actions.is_empty());
    }

    /// One number on screen and a second linked beside it.
    fn app_with_two_numbers() -> (tempfile::TempDir, App) {
        let root = tempfile::tempdir().unwrap();
        let dirs = crate::paths::AppDirs::under(root.path());
        let mut app = App::headless(dirs.clone(), crate::settings::Settings::default()).0;
        let second = crate::account::Account::detached(
            &dirs,
            crate::model::AccountId::parse("2").unwrap(),
            crate::settings::AccountSettings::default(),
        )
        .unwrap()
        .0;
        app.accounts.push(second);
        app.active = 0;
        app.page = Page::Chats;
        (root, app)
    }

    #[test]
    fn ctrl_shift_a_steps_through_linked_numbers_and_wraps() {
        let (_root, mut app) = app_with_two_numbers();
        let ids: Vec<_> = app
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        let ctx = egui::Context::default();
        for modifiers in ctrl_shift() {
            for (active, expected) in [(0, &ids[1]), (1, &ids[0])] {
                app.active = active;
                app.actions.clear();
                assert!(!press(&mut app, &ctx, Key::A, modifiers));
                assert_eq!(app.actions, [Action::SwitchAccount(expected.clone())]);
            }
        }
    }

    #[test]
    fn ctrl_shift_a_needs_a_second_number_that_is_not_being_added() {
        let (_root, mut single, _ids) = app_with_chats(1);
        let ctx = egui::Context::default();
        let ctrl_shift_a = Modifiers::COMMAND | Modifiers::SHIFT;
        assert!(press(&mut single, &ctx, Key::A, ctrl_shift_a));
        assert!(
            single.actions.is_empty(),
            "one number has nothing to switch to"
        );
        let (_root, mut app) = app_with_two_numbers();
        app.adding_account = true;
        assert!(press(&mut app, &ctx, Key::A, ctrl_shift_a));
        assert!(
            app.actions.is_empty(),
            "a number being added keeps its Cancel"
        );
    }

    #[test]
    fn ctrl_shift_a_leaves_dialogs_and_other_combinations_alone() {
        let (_root, mut app) = app_with_two_numbers();
        let ctx = egui::Context::default();
        app.dialog = Some(Dialog::Shortcuts);
        assert!(press(
            &mut app,
            &ctx,
            Key::A,
            Modifiers::COMMAND | Modifiers::SHIFT
        ));
        app.dialog = None;
        for modifiers in [Modifiers::NONE, Modifiers::SHIFT, Modifiers::COMMAND] {
            assert!(press(&mut app, &ctx, Key::A, modifiers));
        }
        assert!(
            !app.actions
                .iter()
                .any(|action| matches!(action, Action::SwitchAccount(_))),
            "{:?}",
            app.actions
        );
    }

    #[test]
    fn ctrl_e_archives_and_unarchives_the_open_chat() {
        let (_root, mut app, ids) = app_with_chats(2);
        app.open_chat = Some(ids[1].clone());
        let ctx = egui::Context::default();
        for modifiers in ctrl_shift().map(|modifiers| Modifiers {
            shift: false,
            ..modifiers
        }) {
            app.chats[1].archived = false;
            app.actions.clear();
            assert!(!press(&mut app, &ctx, Key::E, modifiers));
            assert_eq!(app.actions, [Action::SetArchived(ids[1].clone(), true)]);
            app.chats[1].archived = true;
            app.actions.clear();
            assert!(!press(&mut app, &ctx, Key::E, modifiers));
            assert_eq!(app.actions, [Action::SetArchived(ids[1].clone(), false)]);
        }
    }

    #[test]
    fn ctrl_e_needs_an_open_chat_and_no_overlay() {
        let (_root, mut app, ids) = app_with_chats(1);
        let ctx = egui::Context::default();
        assert!(press(&mut app, &ctx, Key::E, Modifiers::COMMAND));
        assert!(app.actions.is_empty(), "no chat is open");
        app.open_chat = Some(ids[0].clone());
        app.dialog = Some(Dialog::Shortcuts);
        assert!(press(&mut app, &ctx, Key::E, Modifiers::COMMAND));
        app.dialog = None;
        app.page = Page::Settings;
        assert!(press(&mut app, &ctx, Key::E, Modifiers::COMMAND));
        app.page = Page::Chats;
        assert!(press(&mut app, &ctx, Key::E, Modifiers::NONE));
        assert!(app.actions.is_empty());
    }

    #[test]
    fn a_bracket_without_ctrl_and_shift_together_stays_typed() {
        let (_root, mut app, ids) = app_with_chats(3);
        app.open_chat = Some(ids[1].clone());
        let ctx = egui::Context::default();
        let typed = [
            Modifiers::NONE,
            Modifiers::SHIFT,
            Modifiers::COMMAND,
            Modifiers {
                ctrl: true,
                command: true,
                ..Modifiers::NONE
            },
            Modifiers {
                mac_cmd: true,
                command: true,
                ..Modifiers::NONE
            },
            Modifiers::ALT,
            Modifiers::ALT | Modifiers::SHIFT,
        ];
        for modifiers in typed {
            for key in [
                Key::OpenBracket,
                Key::OpenCurlyBracket,
                Key::CloseBracket,
                Key::CloseCurlyBracket,
            ] {
                app.actions.clear();
                let survived = press(&mut app, &ctx, key, modifiers);
                assert!(survived, "{key:?} with {modifiers:?} was consumed");
                assert!(
                    !app.actions
                        .iter()
                        .any(|action| matches!(action, Action::OpenChat(_))),
                    "{key:?} with {modifiers:?} switched chats"
                );
            }
        }
    }

    #[test]
    fn page_keys_scroll_the_open_chat_and_are_consumed() {
        let (_root, mut app, ids) = app_with_chats(1);
        app.open_chat = Some(ids[0].clone());
        let ctx = egui::Context::default();
        for (key, expected) in [
            (Key::PageUp, Action::ScrollPage(Scroll::PageUp)),
            (Key::PageDown, Action::ScrollPage(Scroll::PageDown)),
            (Key::Home, Action::ScrollPage(Scroll::Top)),
            (Key::End, Action::ScrollPage(Scroll::Bottom)),
        ] {
            app.actions.clear();
            let survived = press(&mut app, &ctx, key, Modifiers::NONE);
            assert!(!survived, "{key:?} reached the views");
            assert_eq!(
                app.actions,
                [expected],
                "{key:?} did not push the expected action"
            );
        }
    }

    #[test]
    fn page_keys_leave_shift_and_ctrl_combinations_alone() {
        let (_root, mut app, ids) = app_with_chats(1);
        app.open_chat = Some(ids[0].clone());
        let ctx = egui::Context::default();
        for (key, modifiers) in [
            (Key::Home, Modifiers::SHIFT),
            (Key::PageUp, Modifiers::SHIFT),
            (Key::Home, Modifiers::COMMAND),
        ] {
            app.actions.clear();
            let survived = press(&mut app, &ctx, key, modifiers);
            assert!(survived, "{key:?} with {modifiers:?} was consumed");
            assert!(app.actions.is_empty(), "{key:?} with {modifiers:?} acted");
        }
        // Ctrl+End (Cmd+End on macOS) still jumps to the newest message.
        app.actions.clear();
        let survived = press(&mut app, &ctx, Key::End, Modifiers::COMMAND);
        assert!(!survived);
        assert_eq!(app.actions, [Action::ScrollToBottom]);
    }

    /// The focused message list takes the arrows, Home, End and the page
    /// keys for its cursor; unfocused, the page keys still scroll.
    #[test]
    fn a_focused_message_list_takes_the_navigation_keys() {
        use crate::ui::list_nav::Step;
        let (_root, mut app, ids) = app_with_chats(1);
        app.open_chat = Some(ids[0].clone());
        let ctx = egui::Context::default();
        let list = crate::ui::conversation::messages_list_id(&ids[0]);
        for (key, expected) in [
            (Key::ArrowUp, Step::Previous),
            (Key::ArrowDown, Step::Next),
            (Key::Home, Step::First),
            (Key::End, Step::Last),
            (Key::PageUp, Step::PageUp(8)),
            (Key::PageDown, Step::PageDown(8)),
        ] {
            app.actions.clear();
            ctx.memory_mut(|memory| memory.request_focus(list));
            let survived = press(&mut app, &ctx, key, Modifiers::NONE);
            assert!(!survived, "{key:?} reached the views");
            assert_eq!(app.actions, [Action::MessageCursor(expected)], "{key:?}");
        }
        // Shift+Home and the like are not list keys.
        app.actions.clear();
        ctx.memory_mut(|memory| memory.request_focus(list));
        assert!(press(&mut app, &ctx, Key::Home, Modifiers::SHIFT));
        assert!(app.actions.is_empty(), "{:?}", app.actions);
        // Unfocused, PgUp scrolls as before and the arrows are left alone.
        ctx.memory_mut(|memory| memory.surrender_focus(list));
        app.actions.clear();
        assert!(!press(&mut app, &ctx, Key::PageUp, Modifiers::NONE));
        assert_eq!(app.actions, [Action::ScrollPage(Scroll::PageUp)]);
        app.actions.clear();
        assert!(press(&mut app, &ctx, Key::ArrowDown, Modifiers::NONE));
        assert!(app.actions.is_empty());
    }

    #[test]
    fn arrows_walk_a_messages_items_and_space_opens_a_phone_menu() {
        use crate::model::{Content, Delivery, Message};
        let (_root, mut app, ids) = app_with_chats(1);
        let chat = ids[0].clone();
        app.open_chat = Some(chat.clone());
        app.conversations
            .entry(chat.clone())
            .or_default()
            .messages
            .push(Message {
                id: "m1".into(),
                chat: chat.clone(),
                sender: "alice".into(),
                sender_name: None,
                from_me: false,
                timestamp: 1,
                history_order: None,
                content: Content::Text {
                    text: "see https://a.org or call 0501234567".into(),
                    preview: None,
                },
                status: Delivery::None,
                delivered_at: None,
                read_at: None,
                quoted: None,
                reactions: Vec::new(),
                edited: false,
                mentions: Vec::new(),
                forwarded: false,
                thumbnail: None,
            });
        app.message_cursor.insert(chat.clone(), "m1".into());
        let ctx = egui::Context::default();
        let list = crate::ui::conversation::messages_list_id(&chat);
        // Right walks from the first item on, announcing each.
        for (index, words) in [(1, "Phone number 2 of 2"), (1, "Phone number 2 of 2")] {
            ctx.memory_mut(|memory| memory.request_focus(list));
            app.actions.clear();
            app.accessibility_announcements.clear();
            assert!(!press(&mut app, &ctx, Key::ArrowRight, Modifiers::NONE));
            assert!(
                app.actions.is_empty(),
                "walking says rather than acts: {:?}",
                app.actions
            );
            assert!(
                app.message_item
                    .as_ref()
                    .is_some_and(|(item_chat, row, item, ..)| item_chat == &chat
                        && row == "m1"
                        && *item == index),
                "{:?}",
                app.message_item
            );
            assert!(
                app.accessibility_announcements
                    .iter()
                    .any(|said| said.contains(words)),
                "{:?}",
                app.accessibility_announcements
            );
        }
        // Space on the phone number asks for its menu.
        ctx.memory_mut(|memory| memory.request_focus(list));
        app.actions.clear();
        assert!(!press(&mut app, &ctx, Key::Space, Modifiers::NONE));
        assert!(
            app.actions.contains(&Action::OpenPhoneMenu {
                message: "m1".into(),
                url: "tel:0501234567".into(),
            }),
            "{:?}",
            app.actions
        );
        // Left walks back to the link.
        ctx.memory_mut(|memory| memory.request_focus(list));
        app.actions.clear();
        assert!(!press(&mut app, &ctx, Key::ArrowLeft, Modifiers::NONE));
        assert!(
            app.message_item
                .as_ref()
                .is_some_and(|(_, _, item, ..)| *item == 0)
        );
    }

    #[test]
    fn the_messages_shortcut_focuses_the_list() {
        let (_root, mut app, ids) = app_with_chats(1);
        app.open_chat = Some(ids[0].clone());
        let ctx = egui::Context::default();
        let modifiers = Modifiers::ALT;
        assert!(!press(&mut app, &ctx, Key::Num2, modifiers));
        assert_eq!(app.actions, [Action::FocusMessages]);
        // Under a dialog the shortcut leaves focus where it was.
        app.actions.clear();
        app.dialog = Some(Dialog::Shortcuts);
        press(&mut app, &ctx, Key::Num2, modifiers);
        assert!(app.actions.is_empty(), "{:?}", app.actions);
        // With Shift held it is not the list's key.
        app.dialog = None;
        app.actions.clear();
        press(&mut app, &ctx, Key::Num2, modifiers | Modifiers::SHIFT);
        assert!(
            !app.actions.contains(&Action::FocusMessages),
            "{:?}",
            app.actions
        );
    }

    #[test]
    fn ctrl_end_and_home_end_leave_a_field_with_text_alone() {
        // No field focused, or an empty composer: the keys scroll the chat.
        assert!(jump_keys_reach_chat(false, false, false));
        assert!(jump_keys_reach_chat(true, true, true));
        // A composer with text, or any other field (search, dialogs): the
        // keys move the text cursor (#456).
        assert!(!jump_keys_reach_chat(true, true, false));
        assert!(!jump_keys_reach_chat(true, false, true));
        assert!(!jump_keys_reach_chat(true, false, false));
    }

    #[test]
    fn page_keys_do_nothing_without_an_open_chat_a_dialog_or_off_the_chats_page() {
        let (_root, mut app, ids) = app_with_chats(1);
        let ctx = egui::Context::default();
        let keys = [Key::PageUp, Key::PageDown, Key::Home, Key::End];
        // No chat open.
        for key in keys {
            app.actions.clear();
            assert!(
                press(&mut app, &ctx, key, Modifiers::NONE),
                "{key:?} survived with no chat open"
            );
            assert!(app.actions.is_empty());
        }
        app.open_chat = Some(ids[0].clone());
        // A dialog is open.
        app.dialog = Some(Dialog::Shortcuts);
        for key in keys {
            app.actions.clear();
            assert!(press(&mut app, &ctx, key, Modifiers::NONE));
            assert!(app.actions.is_empty());
        }
        app.dialog = None;
        // Off the Chats page.
        app.page = Page::Settings;
        for key in keys {
            app.actions.clear();
            assert!(press(&mut app, &ctx, key, Modifiers::NONE));
            assert!(app.actions.is_empty());
        }
    }
}
