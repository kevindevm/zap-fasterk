//! Read-only stories delivered during the current connection.
use egui::{Rect, Sense, Vec2, pos2};

use crate::{
    app::App,
    model::{Action, Content, MediaState, Message},
    settings::ScreenPrivacyWhat,
    theme, util,
};

use super::widgets;

/// One author's updates, newest first.
struct AuthorGroup {
    sender: String,
    entries: Vec<Message>,
}

/// Visible updates grouped by sender, each group newest first and groups
/// ordered by their newest update. Expired updates and locked chats stay out.
fn groups(app: &App) -> Vec<AuthorGroup> {
    let now = util::now();
    let mut groups: Vec<AuthorGroup> = Vec::new();
    for row in app.statuses.iter().filter(|row| {
        row.timestamp > now - 86400 && app.chat(&row.sender).is_none_or(|chat| !chat.locked)
    }) {
        if let Some(group) = groups.iter_mut().find(|group| group.sender == row.sender) {
            group.entries.push(row.clone());
        } else {
            groups.push(AuthorGroup {
                sender: row.sender.clone(),
                entries: vec![row.clone()],
            });
        }
    }
    for group in &mut groups {
        group
            .entries
            .sort_by_key(|row| std::cmp::Reverse(row.timestamp));
    }
    groups.sort_by_key(|group| {
        std::cmp::Reverse(group.entries.first().map(|row| row.timestamp).unwrap_or(0))
    });
    groups
}

fn unread(entries: &[Message]) -> usize {
    entries.iter().filter(|row| row.read_at.is_none()).count()
}

fn author_name(app: &App, row: &Message) -> String {
    if app
        .settings
        .screen_privacy
        .hides(ScreenPrivacyWhat::Name, false)
    {
        crate::i18n::gettext(app.locale, "Contact").into_owned()
    } else if row.from_me {
        crate::i18n::gettext(app.locale, "You").into_owned()
    } else {
        app.chat(&row.sender)
            .map(|chat| app.chat_title(chat))
            .unwrap_or_else(|| {
                row.sender_name
                    .clone()
                    .unwrap_or_else(|| row.sender.clone())
            })
    }
}

pub(crate) fn show(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    super::widgets::rich_text(
        ui,
        &crate::i18n::gettext(app.locale, "Status"),
        theme::semibold(18.0),
        palette.text,
    );
    ui.add(egui::Label::new(
        egui::RichText::new(crate::i18n::gettext(
            app.locale,
            "Statuses received while Zap Fasterk is running. They expire after 24 hours. Seen here only; the phone is not told.",
        ))
        .font(theme::regular(12.0))
        .color(palette.secondary),
    ).wrap());
    if let Some(key) = app.status_shown.clone() {
        viewer(app, ui, &key);
    } else {
        list(app, ui);
    }
    let close = crate::i18n::gettext(app.locale, "Close");
    if theme::pill_button(ui, &palette, &close, false).clicked() {
        app.actions.push(Action::CloseDialog);
    }
}

/// The authors' list: one row per contact with their newest update's time
/// and a dot while an update of theirs is not seen yet.
fn list(app: &mut App, ui: &mut egui::Ui) {
    let palette = app.palette;
    let groups = groups(app);
    if groups.is_empty() {
        super::widgets::rich_text(
            ui,
            &crate::i18n::gettext(app.locale, "No recent statuses received"),
            theme::regular(14.0),
            palette.secondary,
        );
        return;
    }
    super::widgets::rich_text(
        ui,
        &crate::i18n::gettext(app.locale, "Recent updates"),
        theme::medium(13.0),
        palette.secondary,
    );
    egui::ScrollArea::vertical()
        .id_salt(("statuses", app.account().id.as_str()))
        .max_height(super::widgets::dialog_scroll_height(ui))
        .show(ui, |ui| {
            for group in &groups {
                author_row(app, ui, group);
            }
        });
}

/// One author's row. The row's own click target goes first, so the texts
/// and the avatar never keep its clicks.
fn author_row(app: &mut App, ui: &mut egui::Ui, group: &AuthorGroup) {
    let palette = app.palette;
    let Some(newest) = group.entries.first() else {
        return;
    };
    let name = author_name(app, newest);
    let picture = app.avatar(&group.sender);
    let stamp = util::chat_stamp(app.locale, newest.timestamp);
    let unseen = unread(&group.entries);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), theme::ROW_HEIGHT),
        Sense::hover(),
    );
    let response = ui
        .interact(
            rect,
            egui::Id::new(("status-author", &group.sender)),
            Sense::click(),
        )
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    theme::reveal_focus(&response);
    if let Some(key) = app.status_open_key(&group.sender)
        && response.clicked()
    {
        app.actions.push(Action::StatusOpen(key));
    }
    if !ui.is_rect_visible(rect) {
        return;
    }
    if response.hovered() || response.has_focus() {
        widgets::row_highlight(ui, &palette, rect, palette.surface_hover);
    }
    let avatar_rect =
        Rect::from_center_size(pos2(rect.left() + 38.0, rect.center().y), Vec2::splat(48.0));
    widgets::paint_avatar(
        ui,
        &palette,
        avatar_rect,
        &name,
        &group.sender,
        picture.as_deref(),
    );
    let left = rect.left() + 76.0;
    let name_line = widgets::line(ui, &name, theme::medium(14.5), palette.text, 260.0, 1);
    name_line.paint(ui, pos2(left, rect.top() + 14.0), palette.text);
    let stamp_line = widgets::line(ui, &stamp, theme::regular(12.5), palette.dim, 200.0, 1);
    stamp_line.paint(ui, pos2(left, rect.top() + 38.0), palette.dim);
    if unseen > 0 {
        ui.painter().circle_filled(
            pos2(rect.right() - 16.0, rect.center().y),
            5.0,
            palette.accent,
        );
    }
}

/// The update the viewer shows, with its author at the top and, when they
/// have more, the two buttons that step through them.
fn viewer(app: &mut App, ui: &mut egui::Ui, key: &str) {
    let palette = app.palette;
    let shown = app.statuses.iter().find(|row| row.id == key).cloned();
    let Some(entry) = shown else {
        // What it showed left the list before a click; nothing to hold on.
        app.status_shown = None;
        return;
    };
    let entries: Vec<Message> = groups(app)
        .into_iter()
        .find(|group| group.sender == entry.sender)
        .map(|group| group.entries)
        .unwrap_or_default();
    ui.horizontal(|ui| {
        if theme::icon_button(
            ui,
            theme::Icon::ArrowLeft,
            20.0,
            palette.secondary,
            palette.text,
            &crate::i18n::gettext(app.locale, "Back"),
        )
        .clicked()
        {
            app.actions.push(Action::StatusClose);
        }
        let name = author_name(app, &entry);
        let picture = app.avatar(&entry.sender);
        widgets::avatar(ui, &palette, &name, &entry.sender, 32.0, picture.as_deref());
        ui.vertical(|ui| {
            widgets::rich_text(ui, &name, theme::medium(14.0), palette.text);
            widgets::rich_text(
                ui,
                &util::clock(entry.timestamp),
                theme::regular(12.0),
                palette.secondary,
            );
        });
    });
    ui.add_space(12.0);
    // Only the update itself scrolls: the author above and the stepping row
    // below stay put, so a tall photo never pushes stepping out of reach.
    egui::ScrollArea::vertical()
        .id_salt(("status-viewer", app.account().id.as_str()))
        .max_height(super::widgets::dialog_scroll_height(ui))
        .show(ui, |ui| {
            status_entry(app, ui, &entry);
        });
    if entries.len() > 1 {
        let at = entries.iter().position(|row| row.id == key);
        let where_they_stand = match at {
            // Numbered oldest first, so stepping forward from the
            // first update meets the second, not the last.
            Some(at) => format!("{}/{}", entries.len() - at, entries.len()),
            None => format!("{}", entries.len()),
        };
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if theme::icon_button(
                ui,
                theme::Icon::ChevronLeft,
                18.0,
                palette.secondary,
                palette.text,
                &crate::i18n::gettext(app.locale, "Previous update"),
            )
            .clicked()
            {
                app.actions.push(Action::StatusStep { back: true });
            }
            widgets::rich_text(
                ui,
                &where_they_stand,
                theme::regular(12.5),
                palette.secondary,
            );
            if theme::icon_button(
                ui,
                theme::Icon::ChevronRight,
                18.0,
                palette.secondary,
                palette.text,
                &crate::i18n::gettext(app.locale, "Next update"),
            )
            .clicked()
            {
                app.actions.push(Action::StatusStep { back: false });
            }
        });
    }
}

/// One update's body: text large, a photo or a video with its download
/// states, and where a kind cannot show here, says so.
fn status_entry(app: &mut App, ui: &mut egui::Ui, row: &Message) {
    let palette = app.palette;
    if app
        .settings
        .screen_privacy
        .hides(ScreenPrivacyWhat::Message, false)
    {
        super::widgets::rich_text(
            ui,
            &crate::i18n::gettext(app.locale, "Content hidden"),
            theme::regular(14.0),
            palette.secondary,
        );
        return;
    }
    match &row.content {
        Content::Text { text, .. } => {
            super::widgets::rich_text(ui, text, theme::regular(16.0), palette.text);
        }
        Content::Image { caption, media, .. } | Content::Video { caption, media, .. } => {
            if let Some(caption) = caption {
                super::widgets::rich_text(ui, caption, theme::regular(14.0), palette.text);
            }
            if app
                .settings
                .screen_privacy
                .hides(ScreenPrivacyWhat::Media, false)
            {
                return;
            }
            if let Some(path) = &media.path {
                if matches!(row.content, Content::Image { .. })
                    && crate::safety::can_preview_image(path)
                {
                    // One photo, clickable like the chat's: the system viewer
                    // opens it. A generic image preview would lose this
                    // sender's expiry/lock scope, so the Status view keeps
                    // its own.
                    let shown = ui.add(
                        egui::Image::new(crate::util::image_uri(path))
                            .max_size(egui::vec2(ui.available_width().min(640.0), 480.0)),
                    );
                    if ui
                        .interact(
                            shown.rect,
                            egui::Id::new(("status-photo", &row.id)),
                            Sense::click(),
                        )
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        app.actions.push(Action::OpenFile(path.clone()));
                    }
                    return;
                }
                if matches!(row.content, Content::Video { .. }) {
                    status_video(app, ui, row, &palette);
                }
                if super::widgets::menu_item(
                    ui,
                    &palette,
                    None,
                    &crate::i18n::gettext(app.locale, "Open status"),
                ) {
                    app.actions.push(Action::OpenFile(path.clone()));
                }
            } else if matches!(media.state, MediaState::Downloading) {
                super::widgets::rich_text(
                    ui,
                    &crate::i18n::gettext(app.locale, "Downloading…"),
                    theme::regular(12.0),
                    palette.secondary,
                );
            } else {
                if matches!(media.state, MediaState::Failed(_)) {
                    super::widgets::rich_text(
                        ui,
                        &crate::i18n::gettext(
                            app.locale,
                            "Could not download status. Click to retry.",
                        ),
                        theme::regular(12.0),
                        palette.secondary,
                    );
                }
                if super::widgets::menu_item(
                    ui,
                    &palette,
                    None,
                    &crate::i18n::gettext(app.locale, "Download status"),
                ) {
                    app.actions.push(Action::DownloadStatus(row.id.clone()));
                    if matches!(row.content, Content::Video { .. }) {
                        // Play it as soon as the download lands.
                        app.status_video_wanted = Some(app.status_video_key(row));
                    }
                }
            }
        }
        _ => {}
    }
}

/// A status video with in-app playback: the thumbnail until it plays, then
/// the decoded frames with a control strip. Videos never open a chat, so
/// they play under a `status/…` key instead of a message id.
fn status_video(app: &mut App, ui: &mut egui::Ui, row: &Message, palette: &theme::Palette) {
    let key = app.status_video_key(row);
    let path = match &row.content {
        Content::Video { media, .. } => media.path.clone(),
        _ => None,
    };
    let Some(path) = path else {
        // Not downloaded yet: tapping fetches it and plays it on arrival.
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width().min(340.0), 200.0),
            Sense::click(),
        );
        if ui.is_rect_visible(rect) {
            ui.painter().rect_filled(rect, 6.0, palette.surface);
            let icon = egui::pos2(rect.center().x - 13.0, rect.center().y - 13.0);
            theme::paint_icon(
                ui,
                theme::Icon::Play,
                Rect::from_min_size(icon, Vec2::splat(26.0)),
                26.0,
                palette.secondary,
            );
        }
        if response
            .on_hover_cursor(egui::CursorIcon::PointingHand)
            .clicked()
        {
            app.actions.push(Action::DownloadStatus(row.id.clone()));
            app.status_video_wanted = Some(key);
        }
        return;
    };
    let size = egui::vec2(ui.available_width().min(480.0), 270.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let status = app.video.status(&key);
    let playing = status
        .as_ref()
        .is_some_and(|status| status.state == crate::video::State::Playing);
    if ui.is_rect_visible(rect) {
        if status.is_some() {
            app.video.saw(&key);
        }
        match status.as_ref().and_then(|status| status.frame.as_ref()) {
            Some(frame) => {
                ui.painter().image(
                    frame.id(),
                    rect,
                    Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                super::gallery::thumbnail(ui, palette, rect, row, None);
                let icon = egui::pos2(rect.center().x - 16.0, rect.center().y - 16.0);
                theme::paint_icon(
                    ui,
                    theme::Icon::Play,
                    Rect::from_min_size(icon, Vec2::splat(32.0)),
                    32.0,
                    egui::Color32::WHITE,
                );
            }
        }
    }
    if response
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        app.actions.push(Action::PlayStatusVideo {
            key: key.clone(),
            path: path.clone(),
        });
    }
    let bar = Rect::from_min_max(pos2(rect.left(), rect.bottom() - 32.0), rect.max);
    ui.painter()
        .rect_filled(bar, 6.0, egui::Color32::from_black_alpha(150));
    let id = ui.id().with(("status-video", &key));
    let toggle = Rect::from_center_size(pos2(bar.left() + 18.0, bar.center().y), Vec2::splat(26.0));
    theme::paint_icon(
        ui,
        if playing {
            theme::Icon::Pause
        } else {
            theme::Icon::Play
        },
        toggle,
        16.0,
        egui::Color32::WHITE,
    );
    if ui
        .interact(toggle, id.with("toggle"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        app.actions.push(Action::PlayStatusVideo {
            key: key.clone(),
            path: path.clone(),
        });
    }
    if let Some(status) = status.as_ref() {
        let fraction = status.fraction();
        let track = Rect::from_min_max(
            pos2(toggle.right() + 8.0, bar.center().y - 3.0),
            pos2(bar.right() - 76.0, bar.center().y + 3.0),
        );
        ui.painter()
            .rect_filled(track, 3.0, egui::Color32::from_white_alpha(60));
        let filled = Rect::from_min_max(
            track.left_top(),
            pos2(track.left() + track.width() * fraction, track.bottom()),
        );
        ui.painter().rect_filled(filled, 3.0, egui::Color32::WHITE);
        let seek = ui.interact(track, id.with("seek"), Sense::click_and_drag());
        if seek.clicked() || seek.dragged() {
            let x = ui
                .input(|input| input.pointer.interact_pos())
                .or_else(|| seek.hover_pos())
                .map(|pos| pos.x)
                .unwrap_or(track.left());
            let at = ((x - track.left()) / track.width().max(1.0)).clamp(0.0, 1.0);
            app.actions.push(Action::SeekVideo {
                message: key.clone(),
                fraction: at,
            });
        }
    }
    let window =
        Rect::from_center_size(pos2(bar.right() - 44.0, bar.center().y), Vec2::splat(26.0));
    theme::paint_icon(
        ui,
        theme::Icon::Maximize,
        window,
        16.0,
        egui::Color32::WHITE,
    );
    if ui
        .interact(window, id.with("window"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(crate::i18n::gettext(app.locale, "Fill the window"))
        .clicked()
    {
        app.actions.push(Action::ExpandVideo {
            message: key.clone(),
            path: path.clone(),
        });
    }
    let sound = Rect::from_center_size(
        pos2(window.left() - 15.0, bar.center().y),
        Vec2::splat(26.0),
    );
    let muted = app.video.muted();
    theme::paint_icon(
        ui,
        if muted {
            theme::Icon::VolumeX
        } else {
            theme::Icon::Volume2
        },
        sound,
        16.0,
        egui::Color32::WHITE,
    );
    if ui
        .interact(sound, id.with("sound"), Sense::click())
        .on_hover_cursor(egui::CursorIcon::PointingHand)
        .clicked()
    {
        app.actions.push(Action::ToggleVideoSound);
    }
}
