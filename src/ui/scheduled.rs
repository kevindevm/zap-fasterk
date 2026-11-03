//! Scheduling the composer's text for a later send, client-side: the
//! message only goes out while this client runs linked.

use egui::{Align, Layout};

use crate::app::{App, ScheduleDueError};
use crate::model::Action;
use crate::theme::{self, Icon};

fn field(ui: &mut egui::Ui, id: &str, value: &mut String, width: f32, limit: usize) {
    ui.add(
        egui::TextEdit::singleline(value)
            .id_salt(id)
            .char_limit(limit)
            .font(theme::regular(14.0))
            .desired_width(width),
    );
}

pub fn show(app: &mut App, ui: &mut egui::Ui, chat: &str) {
    let palette = app.palette;
    let locale = app.locale;
    ui.horizontal(|ui| {
        theme::icon(ui, Icon::Clock, 20.0, palette.accent);
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Schedule message"),
            theme::bold(18.0),
            palette.text,
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if theme::icon_button(ui, Icon::X, 16.0, palette.secondary, palette.text, "Close")
                .clicked()
            {
                app.actions.push(Action::CloseDialog);
            }
        });
    });
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if theme::soft_button(
            ui,
            &palette,
            None,
            crate::i18n::gettext(locale, "Tonight 9 PM").as_ref(),
            false,
        )
        .clicked()
        {
            app.actions.push(Action::SchedulePreset {
                chat: chat.to_owned(),
                tonight: true,
            });
        }
        if theme::soft_button(
            ui,
            &palette,
            None,
            crate::i18n::gettext(locale, "Tomorrow 8 AM").as_ref(),
            false,
        )
        .clicked()
        {
            app.actions.push(Action::SchedulePreset {
                chat: chat.to_owned(),
                tonight: false,
            });
        }
    });
    ui.horizontal(|ui| {
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Day"),
            theme::regular(13.0),
            palette.dim,
        );
        field(ui, "schedule-day", &mut app.schedule_day, 36.0, 2);
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Month"),
            theme::regular(13.0),
            palette.dim,
        );
        field(ui, "schedule-month", &mut app.schedule_month, 36.0, 2);
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Year"),
            theme::regular(13.0),
            palette.dim,
        );
        field(ui, "schedule-year", &mut app.schedule_year, 52.0, 4);
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Hour"),
            theme::regular(13.0),
            palette.dim,
        );
        field(ui, "schedule-hour", &mut app.schedule_hour, 36.0, 2);
        theme::text(ui, ":", theme::regular(13.0), palette.dim);
        field(ui, "schedule-minute", &mut app.schedule_minute, 36.0, 2);
    });
    let due = App::parse_schedule_due(
        &app.schedule_day,
        &app.schedule_month,
        &app.schedule_year,
        &app.schedule_hour,
        &app.schedule_minute,
    );
    match due {
        Ok(due) => {
            let stamp = crate::util::moment_stamp(locale, due);
            theme::text(
                ui,
                crate::i18n::gettext(locale, "Sends {stamp}").replace("{stamp}", &stamp),
                theme::regular(12.5),
                palette.secondary,
            );
        }
        Err(ScheduleDueError::NotADate) => {
            theme::text(
                ui,
                crate::i18n::gettext(locale, "Enter a day, month, year, hour and minute."),
                theme::regular(12.5),
                palette.dim,
            );
        }
        Err(ScheduleDueError::NotFuture) => {
            theme::text(
                ui,
                crate::i18n::gettext(locale, "Pick a time in the future."),
                theme::regular(12.5),
                palette.dim,
            );
        }
        Err(ScheduleDueError::TooFar) => {
            theme::text(
                ui,
                crate::i18n::gettext(locale, "No more than a year out."),
                theme::regular(12.5),
                palette.dim,
            );
        }
    }
    if app.reply_to.is_some() {
        theme::text(
            ui,
            crate::i18n::gettext(locale, "The open reply goes along."),
            theme::regular(12.5),
            palette.secondary,
        );
    }
    ui.horizontal(|ui| {
        let cancel = crate::i18n::gettext(locale, "Cancel");
        if theme::soft_button(ui, &palette, None, cancel.as_ref(), false).clicked() {
            app.actions.push(Action::CloseDialog);
        }
        ui.add_enabled_ui(due.is_ok(), |ui| {
            let schedule = crate::i18n::gettext(locale, "Schedule");
            if theme::pill_button(ui, &palette, schedule.as_ref(), true).clicked()
                && let Ok(due) = due
            {
                app.actions.push(Action::SendScheduled {
                    chat: chat.to_owned(),
                    due_at: due,
                });
                app.actions.push(Action::CloseDialog);
            }
        });
    });
    let pending = app.scheduled.get(chat).cloned().unwrap_or_default();
    if !pending.is_empty() {
        ui.add_space(4.0);
        theme::text(
            ui,
            crate::i18n::gettext(locale, "Scheduled messages"),
            theme::bold(14.0),
            palette.text,
        );
        egui::ScrollArea::vertical()
            .id_salt("scheduled-list")
            .max_height(220.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for item in pending {
                    ui.horizontal(|ui| {
                        let snippet: String = item
                            .text
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .chars()
                            .take(48)
                            .collect();
                        theme::text(ui, snippet, theme::regular(13.0), palette.text);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if theme::icon_button(
                                ui,
                                Icon::X,
                                14.0,
                                palette.secondary,
                                palette.text,
                                &crate::i18n::gettext(locale, "Cancel"),
                            )
                            .clicked()
                            {
                                app.actions.push(Action::CancelScheduled { id: item.id });
                            }
                            let stamp = crate::util::moment_stamp(locale, item.due_at);
                            theme::text(ui, stamp, theme::regular(12.0), palette.dim);
                        });
                    });
                }
            });
    }
}
