//! Deliberate Tab navigation for the main chat view. Message text and chat rows
//! remain pointer/accessibility targets; phone links join the primary cycle.

use egui::{Context, FocusDirection, Id, Response};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stop {
    Composer,
    ViewOnce,
    Send,
    Attach,
    Emoji,
    RecordCancel,
    RecordSend,
    PhoneLink(u32),
    /// The chat header's Search, then the search pane's controls in reading
    /// order. The arrows walk its results from the field.
    ChatSearch,
    ChatSearchClose,
    ChatSearchDate,
    ChatSearchField,
    ChatSearchDay,
    Back,
    Profile,
    /// The open chat's message list: one stop, the arrows walk its messages.
    Messages,
    Sidebar,
    NewChat,
    Settings,
    Search,
    All,
    Unread,
    Private,
    Favorites,
    Groups,
    Channels,
    Statuses,
    Archived,
    Locked,
    /// One chip in the label row, by its position among the labels.
    Label(u8),
    ManageLabels,
    NotificationOpen(u64),
    NotificationDismiss(u64),
}

#[derive(Clone, Copy, Debug)]
struct Move {
    from: Option<Stop>,
    steps: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Section {
    Sidebar,
    Messages,
    Composer,
    Notifications,
}

#[derive(Clone, Copy)]
struct SectionMove {
    from: Option<Section>,
    steps: i32,
}

#[derive(Clone, Default)]
struct Order {
    frame: Option<u64>,
    toolbar_frame: Option<u64>,
    toolbar_target: Option<Id>,
    controls: Vec<(Stop, Id)>,
    movement: Option<Move>,
    sections: Vec<(Section, Id)>,
    section_movement: Option<SectionMove>,
}

pub(crate) fn section(response: &Response, section: Section) {
    if response.enabled() && response.ctx.content_rect().intersects(response.rect) {
        response.ctx.data_mut(|data| {
            let order = data.get_temp_mut_or_default::<Order>(order_id());
            if !order.sections.iter().any(|(known, _)| *known == section) {
                order.sections.push((section, response.id));
            }
        });
    }
}

fn stop_section(stop: Stop) -> Section {
    match stop {
        Stop::NotificationOpen(_) | Stop::NotificationDismiss(_) => Section::Notifications,
        Stop::Composer
        | Stop::ViewOnce
        | Stop::Send
        | Stop::Attach
        | Stop::Emoji
        | Stop::RecordCancel
        | Stop::RecordSend => Section::Composer,
        Stop::PhoneLink(_)
        | Stop::ChatSearch
        | Stop::ChatSearchClose
        | Stop::ChatSearchDate
        | Stop::ChatSearchField
        | Stop::ChatSearchDay
        | Stop::Back
        | Stop::Profile
        | Stop::Messages => Section::Messages,
        _ => Section::Sidebar,
    }
}

fn next_section(order: &Order) -> Option<Id> {
    let movement = order.section_movement?;
    let mut sections = order.sections.clone();
    sections.sort_by_key(|(section, _)| *section);
    if sections.is_empty() {
        return None;
    }
    let index = movement
        .from
        .and_then(|from| sections.iter().position(|(section, _)| *section == from))
        .map_or(if movement.steps < 0 { 0 } else { -1 }, |index| {
            index as i32
        });
    Some(sections[(index + movement.steps).rem_euclid(sections.len() as i32) as usize].1)
}

fn order_id() -> Id {
    Id::new("main-chat-tab-order")
}

/// Attach a semantic position to an enabled main-view control. Using the real
/// response id keeps pointer clicks and keyboard activation on the same widget.
pub trait TabStop: Sized {
    fn tab_stop(self, stop: Stop) -> Self;
}

impl TabStop for Response {
    fn tab_stop(self, stop: Stop) -> Self {
        if matches!(
            stop,
            Stop::Composer | Stop::RecordCancel | Stop::Search | Stop::NotificationOpen(_)
        ) {
            section(&self, stop_section(stop));
        }
        if self.enabled() {
            // `reveal_focus` has queued the scroll while this control's
            // scroll area is open. Re-layout so its ring is visible now.
            if self.gained_focus() && self.interact_rect != self.rect {
                self.ctx.request_discard("reveal primary Tab control");
            }
            self.ctx.data_mut(|data| {
                let order = data.get_temp_mut_or_default::<Order>(order_id());
                order.controls.retain(|(known, _)| *known != stop);
                order.controls.push((stop, self.id));
            });
        }
        self
    }
}

/// Register each phone link in draw order so Tab can reach every link,
/// including multiple wrapped hit regions in a conversation.
pub(crate) fn phone_link(response: Response) -> Response {
    let index = response.ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Order>(order_id())
            .controls
            .iter()
            .filter_map(|(stop, _)| match stop {
                Stop::PhoneLink(index) => Some(*index),
                _ => None,
            })
            .max()
            .map_or(0, |index| index.saturating_add(1))
    });
    response.tab_stop(Stop::PhoneLink(index))
}

/// The widget last registered for `stop`, such as an icon button without
/// painted text, so a scripted demo can find it where it was drawn.
#[cfg(any(test, feature = "demo"))]
pub(crate) fn control(ctx: &Context, stop: Stop) -> Option<Id> {
    ctx.data(|data| data.get_temp::<Order>(order_id()))?
        .controls
        .into_iter()
        .find_map(|(known, id)| (known == stop).then_some(id))
}

/// Intercept Tab before any widgets are registered. Merely consuming the key
/// is insufficient: egui already chose a focus direction during begin_pass.
pub fn begin(ctx: &Context, active: bool) {
    let frame = ctx.cumulative_frame_nr();
    let toolbar_replay = ctx.data(|data| {
        data.get_temp::<Order>(order_id())
            .and_then(|order| (order.toolbar_frame == Some(frame)).then_some(order.toolbar_target))
    });
    if let Some(target) = toolbar_replay {
        if active && let Some(target) = target {
            ctx.memory_mut(|memory| {
                memory.move_focus(FocusDirection::None);
                memory.request_focus(target);
            });
        }
    } else {
        let mut toolbar_target = None;
        if active && !ctx.text_edit_focused() {
            let focused = ctx.memory(|memory| memory.focused());
            let controls = ctx
                .data(|data| {
                    data.get_temp::<Order>(order_id())
                        .map(|order| order.controls)
                })
                .unwrap_or_default();
            if let Some((stop, _)) = controls.iter().find(|(_, id)| Some(*id) == focused)
                && let Some(group) = toolbar_group(*stop)
            {
                let step = ctx.input_mut(|input| {
                    if super::keys::take_plain(input, egui::Key::ArrowRight) {
                        1
                    } else if super::keys::take_plain(input, egui::Key::ArrowLeft) {
                        -1
                    } else {
                        0
                    }
                });
                let group_controls: Vec<_> = controls
                    .iter()
                    .copied()
                    .filter(|(stop, _)| toolbar_group(*stop) == Some(group))
                    .collect();
                if step != 0
                    && let Some(target) = next(
                        &group_controls,
                        Move {
                            from: Some(*stop),
                            steps: step,
                        },
                    )
                {
                    toolbar_target = Some(target);
                    ctx.data_mut(|data| data.insert_temp(crate::theme::keyboard_focus_id(), true));
                    ctx.memory_mut(|memory| {
                        memory.move_focus(FocusDirection::None);
                        memory.request_focus(target);
                    });
                }
            }
        }
        ctx.data_mut(|data| {
            let order = data.get_temp_mut_or_default::<Order>(order_id());
            order.toolbar_frame = Some(frame);
            order.toolbar_target = toolbar_target;
        });
    }
    let mut pressed = false;
    let mut steps = 0;
    let mut section_steps = 0;
    if active {
        ctx.input_mut(|input| {
            input.events.retain(|event| {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                    && matches!(key, egui::Key::Tab | egui::Key::F6)
                    && (!modifiers.any() || modifiers.shift_only())
                {
                    let direction = if modifiers.shift { -1 } else { 1 };
                    if *key == egui::Key::F6 {
                        section_steps += direction;
                    } else {
                        pressed = true;
                        steps += direction;
                    }
                    false
                } else {
                    true
                }
            });
        });
    }
    let focused = ctx.memory(|memory| memory.focused());
    let frame = ctx.cumulative_frame_nr();
    let target = ctx.data_mut(|data| {
        let order = data.get_temp_mut_or_default::<Order>(order_id());
        // Re-layout passes replay the same key. Always navigate from the
        // original source, not from the target chosen in the previous pass.
        if order.frame != Some(frame) {
            order.frame = Some(frame);
            order.movement = (active && pressed && steps != 0).then(|| Move {
                from: order
                    .controls
                    .iter()
                    .find(|(_, id)| Some(*id) == focused)
                    .map(|(stop, _)| *stop),
                steps,
            });
            order.section_movement = (active && section_steps != 0).then(|| SectionMove {
                from: order
                    .controls
                    .iter()
                    .find(|(_, id)| Some(*id) == focused)
                    .map(|(stop, _)| stop_section(*stop))
                    .or_else(|| {
                        order
                            .sections
                            .iter()
                            .find(|(_, id)| Some(*id) == focused)
                            .map(|(section, _)| *section)
                    }),
                steps: section_steps,
            });
        }
        let target = active
            .then(|| {
                next_section(order).or_else(|| {
                    order.movement.and_then(|movement| {
                        next(&tab_controls(&order.controls, movement.from), movement)
                    })
                })
            })
            .flatten();
        order.controls.clear();
        order.sections.clear();
        target
    });
    if active && (pressed || target.is_some()) {
        ctx.memory_mut(|memory| {
            memory.move_focus(FocusDirection::None);
            if let Some(target) = target {
                memory.request_focus(target);
            }
        });
    }
}

/// Reconcile against controls actually drawn this pass (a hidden sidebar or a
/// read-only chat can remove stops). Menus and dialogs keep egui's own order.
pub fn finish(ctx: &Context, active: bool) {
    let target = ctx.data_mut(|data| {
        let order = data.get_temp_mut_or_default::<Order>(order_id());
        order.controls.sort_by_key(|(stop, _)| *stop);
        next_section(order).or_else(|| {
            order
                .movement
                .and_then(|movement| next(&tab_controls(&order.controls, movement.from), movement))
        })
    });
    if active
        && !egui::Popup::is_any_open(ctx)
        && let Some(target) = target
    {
        ctx.memory_mut(|memory| {
            memory.move_focus(FocusDirection::None);
            memory.request_focus(target);
        });
    }
}

fn next(controls: &[(Stop, Id)], movement: Move) -> Option<Id> {
    if controls.is_empty() {
        return None;
    }
    let index = if let Some(index) = controls
        .iter()
        .position(|(stop, _)| Some(*stop) == movement.from)
    {
        index as i32 + movement.steps
    } else if movement.steps < 0 {
        let before = movement.from.map_or(controls.len(), |from| {
            controls.partition_point(|(stop, _)| *stop < from)
        });
        before as i32 + movement.steps
    } else {
        let after = movement.from.map_or(0, |from| {
            controls.partition_point(|(stop, _)| *stop <= from)
        });
        after as i32 + movement.steps.saturating_sub(1)
    };
    Some(controls[index.rem_euclid(controls.len() as i32) as usize].1)
}

fn toolbar_group(stop: Stop) -> Option<u8> {
    match stop {
        Stop::ViewOnce
        | Stop::Send
        | Stop::Attach
        | Stop::Emoji
        | Stop::RecordCancel
        | Stop::RecordSend => Some(0),
        Stop::Back | Stop::Profile | Stop::ChatSearch => Some(1),
        Stop::Sidebar | Stop::NewChat | Stop::Settings => Some(2),
        Stop::All
        | Stop::Unread
        | Stop::Private
        | Stop::Favorites
        | Stop::Groups
        | Stop::Channels
        | Stop::Statuses
        | Stop::Archived
        | Stop::Locked
        | Stop::Label(_)
        | Stop::ManageLabels => Some(3),
        Stop::PhoneLink(_) => Some(4),
        _ => None,
    }
}

fn tab_controls(controls: &[(Stop, Id)], from: Option<Stop>) -> Vec<(Stop, Id)> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &(stop, id) in controls {
        // Phone hit regions keep their own Tab stops, including the last
        // wrapped segment reached backwards from ChatSearch. Arrow keys can
        // still move among them as a group.
        if let Some(group) = toolbar_group(stop)
            && !matches!(stop, Stop::PhoneLink(_))
        {
            if !seen.insert(group) {
                continue;
            }
            let current = from.and_then(|from| {
                controls
                    .iter()
                    .copied()
                    .find(|(stop, _)| *stop == from && toolbar_group(*stop) == Some(group))
            });
            result.push(current.unwrap_or((stop, id)));
        } else {
            result.push((stop, id));
        }
    }
    result
}

#[cfg(test)]
pub fn stops(ctx: &Context) -> Vec<(Stop, Id)> {
    ctx.data(|data| {
        data.get_temp::<Order>(order_id())
            .unwrap_or_default()
            .controls
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapped_phone_links_keep_each_tab_stop_in_both_directions() {
        let controls = [
            Stop::Emoji,
            Stop::PhoneLink(0),
            Stop::PhoneLink(1),
            Stop::ChatSearch,
        ]
        .map(|stop| (stop, Id::new(format!("wrapped-phone-{stop:?}"))));
        for (from, steps, expected) in [
            (Stop::Emoji, 1, controls[1].1),
            (Stop::ChatSearch, -1, controls[2].1),
            (Stop::PhoneLink(0), 1, controls[2].1),
            (Stop::PhoneLink(1), -1, controls[1].1),
        ] {
            assert_eq!(
                next(
                    &tab_controls(&controls, Some(from)),
                    Move {
                        from: Some(from),
                        steps
                    }
                ),
                Some(expected)
            );
        }
    }

    #[test]
    fn toolbars_have_one_tab_stop_and_keep_the_roved_control() {
        let controls = [
            Stop::Composer,
            Stop::Send,
            Stop::Attach,
            Stop::Emoji,
            Stop::Search,
            Stop::All,
            Stop::Unread,
        ]
        .map(|stop| (stop, Id::new(format!("toolbar-{stop:?}"))));
        let tabs = tab_controls(&controls, Some(Stop::Emoji));
        assert_eq!(
            tabs.iter().map(|(stop, _)| *stop).collect::<Vec<_>>(),
            [Stop::Composer, Stop::Emoji, Stop::Search, Stop::All]
        );
        assert_eq!(
            next(
                &tabs,
                Move {
                    from: Some(Stop::Emoji),
                    steps: 1
                }
            ),
            Some(controls[4].1)
        );
    }

    #[test]
    fn f6_cycles_real_sections_in_both_directions_without_advancing_on_relayout() {
        let ctx = Context::default();
        let draw = |ui: &mut egui::Ui| {
            begin(ui.ctx(), true);
            ui.button("Chat search").tab_stop(Stop::Search);
            let message = ui.button("Synthetic message actions");
            section(&message, Section::Messages);
            ui.button("Message composer").tab_stop(Stop::Composer);
            finish(ui.ctx(), true);
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), draw);
        output.textures_delta.clear();
        let ids = ctx.data(|data| data.get_temp::<Order>(order_id()).unwrap().sections);
        let id = |section| ids.iter().find(|(known, _)| *known == section).unwrap().1;
        ctx.memory_mut(|memory| memory.request_focus(id(Section::Sidebar)));
        for (shift, expected) in [
            (false, Section::Messages),
            (false, Section::Composer),
            (false, Section::Sidebar),
            (true, Section::Composer),
        ] {
            let mut passes = 0;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key: egui::Key::F6,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: if shift {
                            egui::Modifiers::SHIFT
                        } else {
                            egui::Modifiers::NONE
                        },
                    }],
                    ..Default::default()
                },
                |ui| {
                    draw(ui);
                    passes += 1;
                    if passes == 1 {
                        ui.ctx().request_discard("F6 section fixture relayout");
                    }
                },
            );
            output.textures_delta.clear();
            assert_eq!(ctx.memory(|memory| memory.focused()), Some(id(expected)));
        }
    }

    #[test]
    fn navigation_wraps_and_skips_missing_controls_in_both_directions() {
        let controls = [Stop::Composer, Stop::Send, Stop::Search]
            .map(|stop| (stop, Id::new(format!("{stop:?}"))));
        for (from, steps, expected) in [
            (Some(Stop::Composer), 1, 1),
            (Some(Stop::Search), 1, 0),
            (Some(Stop::Composer), -1, 2),
            (Some(Stop::Attach), 1, 2),
            (Some(Stop::Attach), -1, 1),
            (Some(Stop::Locked), 1, 0),
            (None, 1, 0),
            (None, -1, 2),
            (Some(Stop::Composer), 5, 2),
            (Some(Stop::Composer), -5, 1),
        ] {
            assert_eq!(
                next(&controls, Move { from, steps }),
                Some(controls[expected].1)
            );
        }
        assert_eq!(
            next(
                &[],
                Move {
                    from: None,
                    steps: 1
                }
            ),
            None
        );
    }

    #[test]
    fn a_relayout_does_not_advance_toolbar_arrows_twice() {
        let ctx = Context::default();
        let draw = |ui: &mut egui::Ui| {
            begin(ui.ctx(), true);
            for stop in [Stop::Send, Stop::Attach, Stop::Emoji] {
                ui.button(format!("{stop:?}")).tab_stop(stop);
            }
            finish(ui.ctx(), true);
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), draw);
        output.textures_delta.clear();
        let controls = stops(&ctx);
        for (key, source, target) in [(egui::Key::ArrowRight, 0, 1), (egui::Key::ArrowLeft, 2, 1)] {
            ctx.memory_mut(|memory| memory.request_focus(controls[source].1));
            let mut passes = 0;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    events: vec![egui::Event::Key {
                        key,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: egui::Modifiers::NONE,
                    }],
                    ..Default::default()
                },
                |ui| {
                    draw(ui);
                    passes += 1;
                    if passes == 1 {
                        ui.ctx().request_discard("toolbar arrow fixture relayout");
                    }
                },
            );
            output.textures_delta.clear();
            assert_eq!(passes, 2);
            assert_eq!(
                ctx.memory(|memory| memory.focused()),
                Some(controls[target].1)
            );
        }
    }

    #[test]
    fn a_relayout_does_not_advance_tab_twice() {
        let ctx = Context::default();
        let draw = |ui: &mut egui::Ui| {
            begin(ui.ctx(), true);
            for stop in [Stop::Composer, Stop::Send, Stop::Attach] {
                ui.button(format!("{stop:?}")).tab_stop(stop);
            }
            finish(ui.ctx(), true);
        };
        let mut output = ctx.run_ui(egui::RawInput::default(), draw);
        output.textures_delta.clear();
        let controls = stops(&ctx);
        ctx.memory_mut(|memory| memory.request_focus(controls[0].1));
        let mut passes = 0;
        let mut output = ctx.run_ui(
            egui::RawInput {
                events: vec![egui::Event::Key {
                    key: egui::Key::Tab,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers::NONE,
                }],
                ..Default::default()
            },
            |ui| {
                draw(ui);
                passes += 1;
                if passes == 1 {
                    ui.ctx().request_discard("test re-layout");
                }
            },
        );
        output.textures_delta.clear();
        assert_eq!(passes, 2);
        assert_eq!(ctx.memory(|memory| memory.focused()), Some(controls[1].1));
    }
}
