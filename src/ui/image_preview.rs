//! Native previews for downloaded images and stickers.

use egui::{Align, CornerRadius, Frame, Layout, Margin, Rect, Stroke, Vec2, vec2};

use crate::app::App;
use crate::model::{Action, Content, MediaState};
use crate::theme::{self, Icon};

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(preview) = app.image_preview.clone() else {
        return;
    };
    let palette = app.palette;
    let selected = preview
        .chat()
        .and_then(|chat| app.conversations.get(chat))
        .and_then(|c| preview.message().and_then(|id| c.message(id)))
        .cloned();
    let is_video = selected
        .as_ref()
        .is_some_and(|row| matches!(row.content, Content::Video { .. }));
    // The motion photo this picture belongs to, when it is one.
    let motion = app.open_chat.as_ref().and_then(|chat| {
        let messages = &app.conversations.get(chat)?.messages;
        messages.iter().find_map(|message| match &message.content {
            Content::Image {
                media,
                motion: Some(motion),
                ..
            } if media.path.as_deref() == Some(preview.path()) => {
                Some((chat.clone(), message.id.clone(), motion.clone()))
            }
            _ => None,
        })
    });
    let frame = Frame::new()
        .fill(palette.overlay)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::same(14))
        .shadow(palette.modal_shadow());
    let viewport = ctx.content_rect().size();
    let response = egui::Modal::new(egui::Id::new("image-preview"))
        .frame(frame)
        .backdrop_color(palette.shadow)
        .show(ctx, |ui| {
            ui.set_width((viewport.x - 56.0).max(180.0));
            ui.set_height((viewport.y - 56.0).max(180.0));
            ui.horizontal(|ui| {
                let name = preview
                    .path()
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Image");
                // The buttons are laid out first, from the right, and the name
                // takes what room they leave, scrolling if it needs more.
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if theme::icon_button(
                        ui,
                        Icon::X,
                        18.0,
                        palette.secondary,
                        palette.text,
                        &crate::i18n::gettext(app.locale, "Close preview (Esc)"),
                    )
                    .clicked()
                    {
                        app.actions.push(Action::CloseImagePreview);
                    }
                    if theme::icon_button(
                        ui,
                        Icon::ExternalLink,
                        18.0,
                        palette.secondary,
                        palette.text,
                        &crate::i18n::gettext(app.locale, "Open in another app"),
                    )
                    .clicked()
                    {
                        app.actions
                            .push(Action::OpenFile(preview.path().to_owned()));
                    }
                    if !is_video {
                        let copy_hint = format!(
                            "{} ({})",
                            crate::i18n::gettext(app.locale, "Copy image"),
                            super::keys::label("Ctrl+C"),
                        );
                        if theme::icon_button(
                            ui,
                            Icon::Copy,
                            18.0,
                            palette.secondary,
                            palette.text,
                            &copy_hint,
                        )
                        .clicked()
                            && !is_video
                            && preview.path().is_file()
                        {
                            app.actions
                                .push(Action::CopyImage(preview.path().to_owned()));
                        }
                        if let Some((chat, message, motion)) = &motion {
                            // A failed download says why, and a click tries again.
                            let (icon, hint) = match &motion.state {
                                MediaState::Failed(error) => (Icon::CircleAlert, error.as_str()),
                                _ => (Icon::Play, "Play motion photo"),
                            };
                            if matches!(
                                motion.state,
                                MediaState::Downloading | MediaState::Transferring { .. }
                            ) {
                                if theme::icon_button(
                                    ui,
                                    Icon::X,
                                    18.0,
                                    palette.secondary,
                                    palette.text,
                                    "Cancel clip download",
                                )
                                .clicked()
                                {
                                    app.actions.push(Action::CancelDownload {
                                        chat: chat.clone(),
                                        message: message.clone(),
                                        card: None,
                                    });
                                }
                            } else if theme::icon_button(
                                ui,
                                icon,
                                18.0,
                                palette.secondary,
                                palette.text,
                                hint,
                            )
                            .clicked()
                            {
                                match &motion.path {
                                    Some(path) => {
                                        app.actions.push(Action::CloseImagePreview);
                                        app.actions.push(Action::ExpandVideo {
                                            message: message.clone(),
                                            path: path.clone(),
                                        });
                                    }
                                    None => app.actions.push(Action::DownloadMotion {
                                        chat: chat.clone(),
                                        message: message.clone(),
                                    }),
                                }
                            }
                        }
                        ui.add_space(8.0);
                        // Right to left: zoom in, the current scale, zoom out.
                        if theme::icon_button(
                            ui,
                            Icon::Plus,
                            18.0,
                            palette.secondary,
                            palette.text,
                            "Zoom in",
                        )
                        .clicked()
                        {
                            app.actions.push(Action::ZoomImageIn);
                        }
                        // One control shows the scale and switches between fitting
                        // the window and the original size.
                        let (label, hint, action) = if preview.is_fit() {
                            (
                                "Fit".to_owned(),
                                "Show at original size",
                                Action::ImageActualSize,
                            )
                        } else {
                            (
                                format!("{:.0}%", preview.zoom() * 100.0),
                                "Fit to the window (0)",
                                Action::FitImage,
                            )
                        };
                        if theme::soft_button(ui, &palette, None, &label, false)
                            .on_hover_text(hint)
                            .clicked()
                        {
                            app.actions.push(action);
                        }
                        if theme::icon_button(
                            ui,
                            Icon::Minus,
                            18.0,
                            palette.secondary,
                            palette.text,
                            "Zoom out",
                        )
                        .clicked()
                        {
                            app.actions.push(Action::ZoomImageOut);
                        }
                        ui.add_space(8.0);
                    }
                    ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                        crate::ui::widgets::scrolling_text(
                            ui,
                            name,
                            theme::semibold(14.0),
                            palette.text,
                            palette.overlay,
                        );
                    });
                });
            });
            ui.horizontal(|ui| {
                ui.add_enabled_ui(preview.can_navigate(), |ui| {
                    if theme::icon_button(
                        ui,
                        Icon::ChevronLeft,
                        20.0,
                        palette.secondary,
                        palette.text,
                        "Previous image (Left arrow)",
                    )
                    .clicked()
                    {
                        app.actions.push(Action::PreviousImage);
                    }
                });
                ui.add_enabled_ui(preview.can_navigate(), |ui| {
                    if theme::icon_button(
                        ui,
                        Icon::ChevronRight,
                        20.0,
                        palette.secondary,
                        palette.text,
                        "Next image (Right arrow)",
                    )
                    .clicked()
                    {
                        app.actions.push(Action::NextImage);
                    }
                });
            });
            if let Some(row) = &selected {
                ui.horizontal_wrapped(|ui| {
                    if row.allows_reaction() {
                        if ui
                            .button(crate::i18n::gettext(app.locale, "Reply"))
                            .clicked()
                        {
                            app.actions.push(Action::CloseImagePreview);
                            app.actions.push(Action::Reply(row.id.clone()));
                        }
                        if ui
                            .button(crate::i18n::gettext(app.locale, "React"))
                            .clicked()
                        {
                            app.actions.push(Action::CloseImagePreview);
                            app.actions.push(Action::OpenReactionPicker {
                                chat: row.chat.clone(),
                                message: row.id.clone(),
                                beside_menu: false,
                            });
                        }
                    }
                    if crate::app::can_select(&row.content)
                        && !(row.from_me && row.status.is_local())
                        && ui
                            .button(crate::i18n::gettext(app.locale, "Forward"))
                            .clicked()
                    {
                        app.actions.push(Action::CloseImagePreview);
                        app.actions
                            .push(Action::ShowDialog(crate::model::Dialog::Forward {
                                chat: row.chat.clone(),
                                messages: vec![row.id.clone()],
                            }));
                    }
                    if preview.path().is_file()
                        && ui
                            .button(crate::i18n::gettext(app.locale, "Save as…"))
                            .clicked()
                    {
                        app.actions.push(Action::SaveAttachmentAs {
                            path: preview.path().into(),
                            name: preview
                                .path()
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("attachment")
                                .into(),
                        });
                    }
                    if ui
                        .button(crate::i18n::gettext(app.locale, "Show in the chat"))
                        .clicked()
                    {
                        app.actions.push(Action::CloseImagePreview);
                        app.actions.push(Action::OpenMessage {
                            chat: row.chat.clone(),
                            message: row.id.clone(),
                        });
                    }
                });
            }
            strip(app, ui, &preview);
            ui.separator();

            // The scroll area below takes this rect as its viewport.
            let area = ui.available_rect_before_wrap();
            let canvas = area.size().max(Vec2::ZERO);
            // Registered with the image cache like every other draw site, so a
            // sweep never releases the picture while it is on screen.
            if !preview.path().is_file() {
                missing(app, ui, &selected, canvas);
            } else if is_video {
                clip(app, ui, &selected, preview.path(), area);
            } else if app.settings.screen_privacy.hides(
                crate::settings::ScreenPrivacyWhat::Media,
                ui.rect_contains_pointer(area),
            ) {
                ui.allocate_rect(area, egui::Sense::hover());
                super::widgets::privacy_cover(ui, area, app.palette.surface_hover, 6.0);
            } else {
                let image = crate::ui::widgets::file_image(ui, preview.path());
                match image.load_for_size(ctx, canvas) {
                    Ok(egui::load::TexturePoll::Ready { texture }) => {
                        let size =
                            display_size(texture.size, canvas, preview.is_fit(), preview.zoom());
                        if preview.is_fit()
                            && texture.size.x > 0.0
                            && let Some(state) = &mut app.image_preview
                        {
                            state.set_fit_scale(size.x / texture.size.x);
                        }
                        let scroll_id = ui.make_persistent_id(egui::IdSalt::new((
                            "image-preview-scroll",
                            preview.path(),
                        )));
                        let trackpad = app.scrolling.from_trackpad();
                        // Read before the scroll area, which would otherwise take the
                        // wheel. The zoom itself is applied by `App` after the frame.
                        let zoom = zoom_input(ui, area, trackpad).and_then(|(factor, pointer)| {
                            let mut next = app.image_preview.clone()?;
                            next.zoom_by(factor);
                            let zoomed =
                                display_size(texture.size, canvas, next.is_fit(), next.zoom());
                            Some((factor, pointer, zoomed))
                        });
                        let output = egui::ScrollArea::both()
                            .id_salt(("image-preview-scroll", preview.path()))
                            .auto_shrink([false, false])
                            // egui drags only on touch screens by default.
                            .scroll_source(egui::scroll_area::ScrollSource {
                                drag: egui::scroll_area::DragScroll::Always,
                                ..Default::default()
                            })
                            .on_hover_cursor(egui::CursorIcon::Grab)
                            .on_drag_cursor(egui::CursorIcon::Grabbing)
                            .show(ui, |ui| {
                                ui.allocate_ui_with_layout(
                                    canvas.max(size),
                                    Layout::centered_and_justified(egui::Direction::TopDown),
                                    |ui| {
                                        let image_response = ui.add(
                                            image
                                                .fit_to_exact_size(size)
                                                .sense(egui::Sense::click()),
                                        );
                                        let copy_label =
                                            crate::i18n::gettext(app.locale, "Copy image");
                                        let save_label =
                                            crate::i18n::gettext(app.locale, "Save as…");
                                        let open_label =
                                            crate::i18n::gettext(app.locale, "Open in another app");
                                        let menu_width = crate::ui::widgets::menu_width(
                                            ui,
                                            &[&copy_label, &save_label, &open_label],
                                            true,
                                        )
                                        .max(180.0);
                                        egui::Popup::context_menu(&image_response)
                                            .width(menu_width)
                                            .frame(crate::ui::widgets::menu_frame(&palette))
                                            .show(|ui| {
                                                if crate::ui::widgets::menu_item(
                                                    ui,
                                                    &palette,
                                                    Some(Icon::Copy),
                                                    &copy_label,
                                                ) {
                                                    app.actions.push(Action::CopyImage(
                                                        preview.path().to_owned(),
                                                    ));
                                                }
                                                if crate::ui::widgets::menu_item(
                                                    ui,
                                                    &palette,
                                                    Some(Icon::Download),
                                                    &save_label,
                                                ) {
                                                    let name = preview
                                                        .path()
                                                        .file_name()
                                                        .and_then(|name| name.to_str())
                                                        .unwrap_or("image.png")
                                                        .to_owned();
                                                    app.actions.push(Action::SaveAttachmentAs {
                                                        path: preview.path().to_owned(),
                                                        name,
                                                    });
                                                }
                                                if crate::ui::widgets::menu_item(
                                                    ui,
                                                    &palette,
                                                    Some(Icon::ExternalLink),
                                                    &open_label,
                                                ) {
                                                    app.actions.push(Action::OpenFile(
                                                        preview.path().to_owned(),
                                                    ));
                                                }
                                            });
                                        image_response
                                            .interact_pointer_pos()
                                            .filter(|_| image_response.double_clicked())
                                    },
                                )
                                .inner
                            });
                        // Stored after the scroll area, which clamps its offset to
                        // this frame's size; the next frame lays out the zoomed size
                        // with the pointed-at pixel still under the pointer.
                        if let Some((factor, pointer, zoomed)) = zoom {
                            let mut scroll = output.state;
                            scroll.offset = crate::image_preview::anchored_offset(
                                canvas,
                                size,
                                zoomed,
                                output.state.offset,
                                pointer,
                                pointer,
                            );
                            scroll.store(ctx, scroll_id);
                            app.actions.push(Action::ZoomImageBy(factor));
                        }
                        // The header's Fit/% toggle. The original size opens with the
                        // double-clicked point in the middle: the offset is stored for
                        // the next frame, which lays out the new size.
                        if let Some(pos) = output.inner {
                            if app
                                .image_preview
                                .as_ref()
                                .is_some_and(crate::image_preview::PreviewState::is_fit)
                            {
                                let mut scroll = output.state;
                                scroll.offset = crate::image_preview::anchored_offset(
                                    canvas,
                                    size,
                                    texture.size,
                                    output.state.offset,
                                    pos - area.min,
                                    canvas / 2.0,
                                );
                                scroll.store(ctx, scroll_id);
                                app.actions.push(Action::ImageActualSize);
                            } else {
                                app.actions.push(Action::FitImage);
                            }
                        }
                    }
                    Ok(egui::load::TexturePoll::Pending { .. }) => {
                        let (rect, _) = ui.allocate_exact_size(canvas, egui::Sense::hover());
                        theme::paint_spinner(ui, rect, 28.0, palette.accent);
                    }
                    Err(_) => {
                        ui.allocate_ui_with_layout(
                            canvas,
                            Layout::centered_and_justified(egui::Direction::TopDown),
                            |ui| {
                                ui.label("This image could not be displayed in Zap Fasterk.");
                                if ui.button("Open externally").clicked() {
                                    app.actions
                                        .push(Action::OpenFile(preview.path().to_owned()));
                                }
                            },
                        );
                    }
                }
            }
        });
    if response.should_close() {
        app.actions.push(Action::CloseImagePreview);
    }
}

/// Thumbnails use only archive previews, never full attachment decoding.
fn strip(app: &mut App, ui: &mut egui::Ui, preview: &crate::image_preview::PreviewState) {
    let listing = app
        .gallery
        .listing
        .take()
        .filter(|listing| Some(listing.chat.as_str()) == preview.chat());
    let Some(listing) = listing else {
        return;
    };
    egui::ScrollArea::horizontal()
        .id_salt("viewer-strip")
        .max_height(76.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                for row in listing.media.iter().rev() {
                    let (rect, response) =
                        ui.allocate_exact_size(Vec2::splat(64.0), egui::Sense::click());
                    let hidden = app.settings.screen_privacy.hides(
                        crate::settings::ScreenPrivacyWhat::Media,
                        response.hovered(),
                    );
                    if ui.is_rect_visible(rect) && !hidden {
                        let uri = super::gallery::uri(app, row);
                        if !super::gallery::thumbnail(ui, &app.palette, rect, row, Some(&uri)) {
                            theme::paint_icon(
                                ui,
                                if matches!(row.content, Content::Video { .. }) {
                                    Icon::Video
                                } else {
                                    Icon::Image
                                },
                                rect,
                                24.0,
                                app.palette.secondary,
                            );
                        }
                        if Some(row.id.as_str()) == preview.message() {
                            ui.painter().rect_stroke(
                                rect,
                                6.0,
                                Stroke::new(2.0, app.palette.accent),
                                egui::StrokeKind::Inside,
                            );
                        }
                    } else if hidden {
                        super::widgets::privacy_cover(ui, rect, app.palette.surface_hover, 6.0);
                    }
                    let label = if matches!(row.content, Content::Video { .. }) {
                        crate::i18n::gettext(app.locale, "Video")
                    } else {
                        crate::i18n::gettext(app.locale, "Photo")
                    };
                    response.widget_info(|| {
                        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label.as_ref())
                    });
                    if response.clicked() {
                        app.actions.push(Action::PreviewMedia {
                            chat: row.chat.clone(),
                            message: row.id.clone(),
                        });
                    }
                }
            });
        });
    app.gallery.listing = Some(listing);
}

fn missing(
    app: &mut App,
    ui: &mut egui::Ui,
    selected: &Option<crate::model::Message>,
    canvas: Vec2,
) {
    ui.allocate_ui_with_layout(
        canvas,
        Layout::centered_and_justified(egui::Direction::TopDown),
        |ui| {
            let Some(row) = selected else {
                return;
            };
            let state = row.content.media().map(|media| &media.state);
            match state {
                Some(MediaState::Downloading) => {
                    theme::spinner(ui, 24.0, app.palette.accent);
                }
                Some(MediaState::Transferring { bytes, total }) => {
                    let fraction = total
                        .filter(|total| *total > 0)
                        .map_or(0.0, |total| *bytes as f32 / total as f32);
                    ui.add(
                        egui::ProgressBar::new(fraction.min(1.0)).text(crate::util::bytes(*bytes)),
                    );
                    if ui
                        .button(crate::i18n::gettext(app.locale, "Cancel"))
                        .clicked()
                    {
                        app.actions.push(Action::CancelDownload {
                            chat: row.chat.clone(),
                            message: row.id.clone(),
                            card: None,
                        });
                    }
                }
                _ => {
                    if let Some(MediaState::Failed(reason)) = state {
                        ui.label(reason);
                    }
                    if ui
                        .button(crate::i18n::gettext(app.locale, "Download"))
                        .clicked()
                    {
                        app.actions.push(Action::Download {
                            chat: row.chat.clone(),
                            message: row.id.clone(),
                            card: None,
                        });
                    }
                }
            }
        },
    );
}

fn clip(
    app: &mut App,
    ui: &mut egui::Ui,
    selected: &Option<crate::model::Message>,
    path: &std::path::Path,
    area: Rect,
) {
    let Some(row) = selected else {
        return;
    };
    if let Some(status) = app.video.status(&row.id) {
        let shape = status
            .frame
            .as_ref()
            .map_or(vec2(16.0, 9.0), |frame| frame.size_vec2());
        let rect = super::video_preview::fitted(shape, area);
        let response = ui.interact(rect, ui.id().with("gallery-video"), egui::Sense::click());
        let hidden = app.settings.screen_privacy.hides(
            crate::settings::ScreenPrivacyWhat::Media,
            response.hovered(),
        );
        if hidden {
            super::widgets::privacy_cover(ui, rect, app.palette.surface_hover, 6.0);
        } else if let Some(frame) = &status.frame {
            ui.painter().image(
                frame.id(),
                rect,
                Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        } else {
            theme::paint_spinner(ui, rect, 28.0, app.palette.accent);
        }
        let mut actions = Vec::new();
        super::conversation::video_controls(
            ui,
            &super::conversation::VideoControls {
                player: &app.video,
                locale: app.locale,
                accent: app.palette.accent,
                expanded: false,
            },
            &row.id,
            path,
            rect,
            &status,
            &mut actions,
        );
        if response.clicked() {
            actions.push(Action::PlayVideo {
                message: row.id.clone(),
                path: path.into(),
            });
        }
        app.actions.extend(actions);
        ui.allocate_rect(area, egui::Sense::hover());
    } else {
        ui.allocate_ui_with_layout(
            area.size(),
            Layout::centered_and_justified(egui::Direction::TopDown),
            |ui| {
                if ui
                    .button(crate::i18n::gettext(app.locale, "Play video"))
                    .clicked()
                {
                    app.actions.push(Action::PlayVideo {
                        message: row.id.clone(),
                        path: path.into(),
                    });
                }
            },
        );
    }
}

/// Height kept below a sticker for its favorite button and the card margins.
const STICKER_BUTTON_ROOM: f32 = 110.0;
/// A sticker's side in the viewer, as in WhatsApp's: a card, not the window.
/// Animated frames are decoded at most 320 pixels wide, so this stays sharp.
const STICKER_VIEW_SIDE: f32 = 300.0;

/// Shows a sticker on a square card over the chat. A click on the sticker,
/// the backdrop, or Escape closes it.
pub fn show_sticker(app: &mut App, ctx: &egui::Context) {
    let Some(preview) = app.sticker_view.clone() else {
        return;
    };
    if preview.chat().is_some_and(|id| {
        app.chat(id)
            .is_none_or(|chat| chat.locked && !app.locked_folder_open())
    }) {
        app.actions.push(Action::CloseStickerPreview);
        return;
    }

    let palette = app.palette;
    let favorite = preview.favorite(&app.stickers_saved).cloned();
    let side = sticker_side(ctx.content_rect().size());
    let card = Frame::new()
        .fill(palette.overlay)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(theme::RADIUS + 4))
        .inner_margin(Margin::same(20));
    let response = egui::Modal::new(egui::Id::new("sticker-preview"))
        .frame(card)
        .backdrop_color(palette.shadow)
        .show(ctx, |ui| {
            ui.set_width(side);
            ui.vertical_centered(|ui| {
                let (rect, response) =
                    ui.allocate_exact_size(Vec2::splat(side), egui::Sense::click());
                paint_sticker(ui, &preview, rect, &palette);
                if response
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    app.actions.push(Action::CloseStickerPreview);
                }
                ui.add_space(16.0);
                let (icon, label, action) = match favorite {
                    Some(saved) => (
                        Icon::StarOff,
                        "Remove from favorites",
                        Action::ForgetSticker(saved),
                    ),
                    None => (
                        Icon::Star,
                        "Add to favorites",
                        Action::SaveSticker(preview.path().to_owned()),
                    ),
                };
                let label = crate::i18n::gettext(app.locale, label);
                let button = theme::soft_button(ui, &palette, Some(icon), &label, false);
                ui.ctx()
                    .data_mut(|data| data.insert_temp(sticker_favorite_id(), button.rect));
                if button.clicked() {
                    app.actions.push(action);
                }
            });
        });
    if response.should_close() {
        app.actions.push(Action::CloseStickerPreview);
    }
}

/// Where the sticker preview's favorite button was drawn, for tests.
pub fn sticker_favorite_id() -> egui::Id {
    egui::Id::new("sticker-preview-favorite")
}

/// Side of the square a sticker is shown in, shrunk only when the window
/// cannot fit the card.
fn sticker_side(viewport: Vec2) -> f32 {
    ((viewport.y - STICKER_BUTTON_ROOM).min(viewport.x - 48.0)).clamp(64.0, STICKER_VIEW_SIDE)
}

fn paint_sticker(
    ui: &mut egui::Ui,
    preview: &crate::image_preview::StickerPreview,
    rect: Rect,
    palette: &crate::theme::Palette,
) {
    let full = Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
    let loaded = if preview.animated() {
        match crate::animation::frame(ui, preview.path(), rect, true) {
            crate::animation::Frame::Ready(texture) => {
                ui.painter().image(
                    texture.id(),
                    fit_within(texture.size_vec2(), rect),
                    full,
                    egui::Color32::WHITE,
                );
                Some(true)
            }
            crate::animation::Frame::Pending => None,
            crate::animation::Frame::Unavailable => Some(false),
        }
    } else {
        let image = crate::ui::widgets::file_image(ui, preview.path());
        match image.load_for_size(ui.ctx(), rect.size()) {
            Ok(egui::load::TexturePoll::Ready { texture }) => {
                image.paint_at(ui, fit_within(texture.size, rect));
                Some(true)
            }
            Ok(egui::load::TexturePoll::Pending { .. }) => None,
            Err(_) => Some(false),
        }
    };
    match loaded {
        Some(true) => {}
        Some(false) => theme::paint_icon(ui, Icon::Sticker, rect, 64.0, palette.secondary),
        None => theme::paint_spinner(ui, rect, 28.0, palette.accent),
    }
}

/// The largest rect of `size`'s shape centred in `rect`. Stickers are small,
/// so unlike photos they are enlarged to fill it.
fn fit_within(size: Vec2, rect: Rect) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return rect;
    }
    let scale = (rect.width() / size.x).min(rect.height() / size.y);
    Rect::from_center_size(rect.center(), size * scale)
}

/// Zoom factor the wheel or a pinch asks for over the preview area, with the
/// anchor point relative to the picture area. Each wheel notch is one header
/// zoom step. A plain mouse wheel zooms instead of scrolling, so its delta is
/// taken from the scroll area. Windows and X11 report touchpads as wheel
/// lines, so two fingers zoom there; only scrolling reported in points
/// (macOS, Wayland) keeps moving the picture.
fn zoom_input(ui: &mut egui::Ui, area: Rect, trackpad: bool) -> Option<(f32, Vec2)> {
    let (touch, hover) = ui.input(|input| {
        (
            input.multi_touch().map(|touch| touch.center_pos),
            input.pointer.hover_pos(),
        )
    });
    let anchor = touch.or(hover)?;
    if !area.contains(anchor) || touch.is_none() && !ui.rect_contains_pointer(area) {
        return None;
    }
    let (zoom_speed, line_speed) = ui.ctx().options(|options| {
        let input = &options.input_options;
        (input.scroll_zoom_speed, input.line_scroll_speed)
    });
    let step = crate::image_preview::PreviewState::ZOOM_STEP;
    let factor = ui.input_mut(|input| {
        let pinch = input.multi_touch().is_some()
            || input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Zoom(_)));
        let mut factor = input.zoom_delta();
        if !pinch {
            // Without a pinch the zoom came from Ctrl/Cmd+wheel (Windows
            // touchpad pinches included), to which egui applies its own curve,
            // exp(scroll_zoom_speed * points). It keeps the modifiers from the
            // start of the gesture, so this checks the source, not the keys.
            factor = factor.powf(step.ln() / (zoom_speed * line_speed));
        }
        // Shift and Alt turn the wheel sideways; Ctrl pressed during a plain
        // notch must not hand the rest of it to the scroll area.
        if !trackpad
            && !input.modifiers.shift
            && !input.modifiers.alt
            && input.smooth_scroll_delta.y != 0.0
        {
            factor *= step.powf(input.smooth_scroll_delta.y / line_speed);
            input.smooth_scroll_delta.y = 0.0;
        }
        factor
    });
    (factor != 1.0).then_some((factor, anchor - area.min))
}

/// Size the image is drawn at from the texture's intrinsic pixel dimensions:
/// fitted into the canvas, or scaled by the preview's zoom factor. Zoom is
/// applied here only. The size hint passed when loading does not change the
/// texture: egui decodes raster formats (all the preview accepts) once at full
/// resolution and reports the source size, whatever size is asked for.
fn display_size(original: Vec2, canvas: Vec2, fit: bool, zoom: f32) -> Vec2 {
    let (width, height) = if fit {
        crate::image_preview::fit_size(original.x, original.y, canvas.x, canvas.y)
    } else {
        crate::image_preview::zoomed_size(original.x, original.y, zoom)
    };
    vec2(width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stickers_show_on_a_card_that_fits_small_windows() {
        assert_eq!(sticker_side(vec2(1180.0, 780.0)), STICKER_VIEW_SIDE);
        assert_eq!(
            sticker_side(vec2(1180.0, 300.0)),
            300.0 - STICKER_BUTTON_ROOM
        );
        assert_eq!(sticker_side(vec2(10.0, 10.0)), 64.0);
        let square = Rect::from_min_size(egui::Pos2::ZERO, Vec2::splat(600.0));
        assert_eq!(fit_within(vec2(512.0, 512.0), square), square);
        assert_eq!(
            fit_within(vec2(512.0, 256.0), square),
            Rect::from_min_max(egui::pos2(0.0, 150.0), egui::pos2(600.0, 450.0))
        );
        assert_eq!(fit_within(Vec2::ZERO, square), square);
    }

    #[test]
    fn fitted_images_keep_aspect_ratio_inside_the_canvas() {
        assert_eq!(
            display_size(vec2(1600.0, 1200.0), vec2(800.0, 700.0), true, 1.0),
            vec2(800.0, 600.0)
        );
        assert_eq!(
            display_size(vec2(320.0, 240.0), vec2(800.0, 700.0), true, 1.0),
            vec2(320.0, 240.0)
        );
        assert_eq!(
            display_size(vec2(320.0, 240.0), vec2(800.0, 700.0), false, 2.0),
            vec2(640.0, 480.0)
        );
    }
}
