//! Read-only stories delivered during the current connection.
use crate::{
    app::App,
    model::{Action, Content, MediaState},
    settings::ScreenPrivacyWhat,
    theme,
};

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
            "Statuses received while Zap Fasterk is running. They expire after 24 hours. Viewing does not send a read receipt.",
        ))
        .font(theme::regular(12.0))
        .color(palette.secondary),
    ).wrap());
    let now = crate::util::now();
    let rows: Vec<_> = app
        .statuses
        .iter()
        .filter(|row| {
            row.timestamp > now - 86400 && app.chat(&row.sender).is_none_or(|chat| !chat.locked)
        })
        .cloned()
        .collect();
    if rows.is_empty() {
        super::widgets::rich_text(
            ui,
            &crate::i18n::gettext(app.locale, "No recent statuses received"),
            theme::regular(14.0),
            palette.secondary,
        );
    }
    egui::ScrollArea::vertical()
        .id_salt(("statuses", app.account().id.as_str()))
        .max_height(super::widgets::dialog_scroll_height(ui))
        .show(ui, |ui| {
            for row in rows.iter().rev() {
                ui.push_id(&row.id, |ui| {
                    ui.separator();
                    let name = if app
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
                    };
                    super::widgets::rich_text(ui, &name, theme::medium(14.0), palette.text);
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
                        Content::Image { caption, media, .. }
                        | Content::Video { caption, media, .. } => {
                            if let Some(caption) = caption {
                                super::widgets::rich_text(
                                    ui,
                                    caption,
                                    theme::regular(14.0),
                                    palette.text,
                                );
                            }
                            if app
                                .settings
                                .screen_privacy
                                .hides(ScreenPrivacyWhat::Media, false)
                            {
                                return;
                            }
                            use std::hash::{Hash, Hasher};
                            let mut key = std::collections::hash_map::DefaultHasher::new();
                            (app.account().id.as_str(), &row.id).hash(&mut key);
                            let uri = format!("bytes://status/{:016x}", key.finish());
                            let (rect, _) = ui.allocate_exact_size(
                                egui::vec2(ui.available_width().min(340.0), 200.0),
                                egui::Sense::hover(),
                            );
                            super::gallery::thumbnail(ui, &palette, rect, row, Some(&uri));
                            if let Some(path) = &media.path {
                                if matches!(row.content, Content::Image { .. })
                                    && crate::safety::can_preview_image(path)
                                {
                                    // Stay in this account's filtered Status view. A generic
                                    // image preview would lose this sender's expiry/lock scope.
                                    ui.add(
                                        egui::Image::new(crate::util::image_uri(path)).max_size(
                                            egui::vec2(ui.available_width().min(640.0), 480.0),
                                        ),
                                    );
                                } else if super::widgets::menu_item(
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
                                }
                            }
                        }
                        _ => {}
                    }
                });
            }
        });
    let close = crate::i18n::gettext(app.locale, "Close");
    if theme::pill_button(ui, &palette, &close, false).clicked() {
        app.actions.push(Action::CloseDialog);
    }
}
