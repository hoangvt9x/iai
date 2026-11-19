// Document tab bar — shown between the menu bar and the tool-options bar.
// One tab per open document; clicking switches the active document.
//
// When the tabs do not all fit, the strip is a window on them that slides:
// it shows a run of neighbouring tabs, in order, and remembers where it was.
// When the tab worked on is outside it (a document just opened, one picked
// from the list), the strip slides just far enough to bring it in — the old
// tabs on the left make room for the new ones on the right. Switching between
// tabs already on the strip does not move it. The mouse wheel over the strip
// slides it by hand. The tabs off either end are listed, in order, under the
// ▾ button at the right.

use super::{UiActions, UiData};
use egui;
use egui_phosphor::regular as ph;

const TAB_H: f32 = 26.0;
const TAB_MIN_W: f32 = 80.0;
const TAB_MAX_W: f32 = 180.0;
const CLOSE_W: f32 = 18.0;
const NEW_BTN_W: f32 = 28.0;
const DOC_LIST_BTN_W: f32 = 30.0;
/// Room kept for the ▾ button when some tabs do not fit: its count ("12 ⌄")
/// can be wider than the button's least width.
const DOC_LIST_ROOM: f32 = 44.0;
const DOC_LIST_ROW_H: f32 = 24.0;
const DOC_LIST_MIN_W: f32 = 260.0;
const DOC_LIST_MAX_W: f32 = 360.0;
const PREVIEW_MAX_W: f32 = 160.0;
const PREVIEW_MAX_H: f32 = 120.0;
/// Wheel travel (points) that slides the strip by one tab.
const WHEEL_STEP: f32 = 40.0;

/// Which tabs the strip shows, left to right, and which the ▾ list holds.
#[derive(Debug, PartialEq)]
struct StripPlan {
    shown: Vec<usize>,
    hidden: Vec<usize>,
}

/// What the strip remembers from one frame to the next.
#[derive(Clone, Copy, Default)]
struct StripMemory {
    /// The first tab on the strip.
    first: usize,
    /// The tab worked on and the number of tabs the last time the strip
    /// made sure the tab worked on was shown: only a change of either brings
    /// it back in, so the wheel can slide away from it.
    active: usize,
    count: usize,
    /// Wheel travel not yet turned into a slide.
    wheel: f32,
}

/// The tabs from `first` that fit in `room` (at least one), as an end index.
fn fit_from(widths: &[f32], first: usize, room: f32) -> usize {
    let mut end = first;
    let mut used = 0.0;
    while let Some(&w) = widths.get(end) {
        if end > first && used + w > room + 0.5 {
            break;
        }
        used += w;
        end += 1;
    }
    end
}

/// The strip as a window from `first`: when `reveal` is given, the window
/// slides the least it can to show that tab; a window that ends at the last
/// tab with room to spare slides back to fill it. Returns the plan and the
/// first tab shown.
fn plan_window(
    widths: &[f32],
    first: usize,
    reveal: Option<usize>,
    room: f32,
) -> (StripPlan, usize) {
    let n = widths.len();
    if n == 0 {
        return (
            StripPlan {
                shown: Vec::new(),
                hidden: Vec::new(),
            },
            0,
        );
    }
    let mut first = first.min(n - 1);
    if let Some(t) = reveal.filter(|&t| t < n) {
        if t < first {
            first = t;
        } else if t >= fit_from(widths, first, room) {
            // Slide right until `t` is the last tab that fits.
            first = t;
            let mut used = widths.get(t).copied().unwrap_or(0.0);
            while let Some(&w) = first.checked_sub(1).and_then(|i| widths.get(i)) {
                if used + w > room + 0.5 {
                    break;
                }
                used += w;
                first -= 1;
            }
        }
    }
    // Room left after the last tab: bring earlier tabs back in.
    let mut end = fit_from(widths, first, room);
    if end == n {
        let mut used: f32 = widths.get(first..n).map_or(0.0, |w| w.iter().sum());
        while let Some(&w) = first.checked_sub(1).and_then(|i| widths.get(i)) {
            if used + w > room + 0.5 {
                break;
            }
            used += w;
            first -= 1;
        }
        end = n;
    }
    (
        StripPlan {
            shown: (first..end).collect(),
            hidden: (0..first).chain(end..n).collect(),
        },
        first,
    )
}

/// The strip in `width` points: the tabs, the new-tab button after them, and
/// the ▾ button at the right when some tabs are listed.
fn plan_for_width(
    widths: &[f32],
    first: usize,
    reveal: Option<usize>,
    width: f32,
) -> (StripPlan, usize) {
    let all = plan_window(widths, first, reveal, width - NEW_BTN_W);
    if all.0.hidden.is_empty() {
        return all;
    }
    plan_window(widths, first, reveal, width - NEW_BTN_W - DOC_LIST_ROOM)
}

pub fn build(ctx: &egui::Context, data: &UiData, actions: &mut UiActions) {
    let pal = data.chrome.theme_mode.palette();
    let active = data.doc.active_doc_idx;
    let doc_count = data.doc.doc_count;

    let ctrl = ctx.input(|i| i.modifiers.ctrl);
    if ctrl && ctx.input(|i| i.key_pressed(egui::Key::Tab)) {
        let shift = ctx.input(|i| i.modifiers.shift);
        if doc_count > 1 {
            actions.doc.switch_doc = Some(if shift {
                if active == 0 {
                    doc_count - 1
                } else {
                    active - 1
                }
            } else {
                (active + 1) % doc_count
            });
        }
    }

    #[allow(deprecated)]
    egui::TopBottomPanel::top("tabbar")
        .exact_size(TAB_H)
        .frame(
            egui::Frame::new()
                .fill(pal.app_bg)
                .inner_margin(egui::Margin::symmetric(2, 0)),
        )
        .show(ctx, |ui| {
            // Strict modal lock: document navigation is disabled while a
            // modal operation is open; clicking the strip rings the bell.
            let strip = ui.add_enabled_ui(!data.chrome.is_tool_modal, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.spacing_mut().button_padding = egui::vec2(0.0, 0.0);
                    let widths: Vec<f32> = (0..doc_count).map(|i| tab_width(data, i)).collect();
                    let memory_id = ui.id().with("tab_strip_window");
                    let mut memory: StripMemory =
                        ctx.data(|d| d.get_temp(memory_id)).unwrap_or_default();
                    // The wheel over the strip slides it, one tab a step.
                    if ui.rect_contains_pointer(ui.max_rect()) {
                        let delta = ctx.input(|i| i.smooth_scroll_delta);
                        let d = if delta.x.abs() > delta.y.abs() {
                            delta.x
                        } else {
                            delta.y
                        };
                        memory.wheel += d;
                    } else {
                        memory.wheel = 0.0;
                    }
                    let mut first = memory.first;
                    while memory.wheel >= WHEEL_STEP {
                        memory.wheel -= WHEEL_STEP;
                        first = first.saturating_sub(1);
                    }
                    while memory.wheel <= -WHEEL_STEP {
                        memory.wheel += WHEEL_STEP;
                        first += 1;
                    }
                    let reveal =
                        (memory.active != active || memory.count != doc_count).then_some(active);
                    let (plan, first) =
                        plan_for_width(&widths, first, reveal, ui.available_width());
                    memory.first = first;
                    memory.active = active;
                    memory.count = doc_count;
                    ctx.data_mut(|d| d.insert_temp(memory_id, memory));

                    // The ▾ list sits at the right edge, the tabs from the left.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        build_document_list(ctx, ui, data, actions, &plan.hidden);
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            build_tabs(ui, data, actions, &plan.shown, &widths);
                        });
                    });
                });
            });
            if data.chrome.is_tool_modal
                && ui.input(|i| i.pointer.primary_clicked())
                && ui
                    .input(|i| i.pointer.interact_pos())
                    .is_some_and(|pos| strip.response.rect.contains(pos))
            {
                actions.chrome.modal_denied = true;
            }
        });
}

/// The title tab `i` shows: its name (shortened), `*` when it has unsaved
/// changes.
fn tab_label(data: &UiData, i: usize) -> String {
    let title = data
        .doc
        .doc_titles
        .get(i)
        .map(|s| s.as_str())
        .unwrap_or("Untitled");
    let dirty = data.doc.doc_modified.get(i).copied().unwrap_or(false);
    let display = if dirty {
        format!("{}*", title)
    } else {
        title.to_string()
    };
    truncate_title(&display, 18)
}

/// Whether tab `i` blinks: work ended while it was not in front.
fn tab_wants_attention(data: &UiData, i: usize) -> bool {
    i != data.doc.active_doc_idx && data.doc.doc_attention.get(i).copied().unwrap_or(false)
}

fn badge_width(data: &UiData, i: usize) -> f32 {
    let busy = data.doc.doc_ai_busy.get(i).is_some_and(Option::is_some);
    if busy || tab_wants_attention(data, i) {
        14.0
    } else {
        0.0
    }
}

/// Tab `i`'s width: its label counted in letters (not bytes: a Vietnamese
/// letter with marks takes two or three), its badge and its close button.
fn tab_width(data: &UiData, i: usize) -> f32 {
    let text_w = 7.0_f32 * tab_label(data, i).chars().count() as f32;
    (text_w + CLOSE_W + badge_width(data, i) + 20.0).clamp(TAB_MIN_W, TAB_MAX_W)
}

fn build_tabs(
    ui: &mut egui::Ui,
    data: &UiData,
    actions: &mut UiActions,
    shown: &[usize],
    widths: &[f32],
) {
    let pal = data.chrome.theme_mode.palette();
    for &i in shown {
        let is_active = i == data.doc.active_doc_idx;
        let ai_busy = data.doc.doc_ai_busy.get(i).and_then(Option::as_ref);
        // Work that ended while the tab was not in front (an ID photo made):
        // the tab blinks until it is shown.
        let attention = tab_wants_attention(data, i);
        let lit = attention && ui.ctx().input(|input| (input.time * 2.0).fract() < 0.5);
        if attention {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(250));
        }

        let label_text = tab_label(data, i);
        let tab_fill = if is_active {
            pal.panel_bg
        } else if lit {
            pal.accent_primary.gamma_multiply(0.45)
        } else {
            pal.app_bg
        };
        let text_color = if is_active || attention {
            pal.text
        } else {
            pal.text_dim
        };
        let font_id = egui::FontId::proportional(11.5);
        let busy_w = badge_width(data, i);
        let tab_w = widths.get(i).copied().unwrap_or(TAB_MIN_W);

        let (tab_rect, tab_response) =
            ui.allocate_exact_size(egui::vec2(tab_w, TAB_H), egui::Sense::click());
        if tab_response.hovered() {
            actions.doc.hovered_doc = Some(i);
        }
        let tab_response = with_doc_preview(tab_response, data, i);
        let painter = ui.painter();
        painter.rect_filled(tab_rect, 0.0, tab_fill);

        if is_active {
            painter.hline(
                tab_rect.left()..=tab_rect.right(),
                tab_rect.bottom() - 2.0,
                egui::Stroke::new(2.0_f32, pal.accent_primary),
            );
        }

        if !is_active || i + 1 < data.doc.doc_count {
            painter.vline(
                tab_rect.right(),
                tab_rect.top()..=tab_rect.bottom(),
                egui::Stroke::new(1.0_f32, pal.separator),
            );
        }

        let label_rect = egui::Rect::from_min_size(
            egui::pos2(tab_rect.left() + 8.0, tab_rect.top()),
            egui::vec2(tab_w - CLOSE_W - busy_w - 10.0, TAB_H),
        );
        painter.text(
            label_rect.left_center(),
            egui::Align2::LEFT_CENTER,
            &label_text,
            font_id,
            text_color,
        );

        if attention && ai_busy.is_none() {
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(tab_rect.right() - CLOSE_W - busy_w, tab_rect.top()),
                egui::vec2(busy_w, TAB_H),
            );
            painter.text(
                badge_rect.center(),
                egui::Align2::CENTER_CENTER,
                ph::CHECK_CIRCLE,
                egui::FontId::proportional(12.0),
                pal.accent_primary,
            );
            ui.interact(
                badge_rect,
                egui::Id::new(("tab_done", i)),
                egui::Sense::hover(),
            )
            .on_hover_text("Ảnh thẻ đã xong — bấm vào tab để mở và in");
        }

        if let Some(busy) = ai_busy {
            let badge_rect = egui::Rect::from_min_size(
                egui::pos2(tab_rect.right() - CLOSE_W - busy_w, tab_rect.top()),
                egui::vec2(busy_w, TAB_H),
            );
            let badge_resp = ui.interact(
                badge_rect,
                egui::Id::new(("tab_ai_busy", i)),
                egui::Sense::hover(),
            );
            if let Some(pos) = busy.queue_pos {
                painter.text(
                    badge_rect.center(),
                    egui::Align2::CENTER_CENTER,
                    ph::CLOCK,
                    egui::FontId::proportional(12.0),
                    pal.text_dim,
                );
                badge_resp.on_hover_text(format!("Queued (position {pos})"));
            } else {
                let font = egui::FontId::proportional(12.0);
                let galley =
                    painter.layout_no_wrap(ph::CIRCLE_NOTCH.to_string(), font, pal.accent_primary);
                let pos = badge_rect.center() - galley.size() * 0.5;
                let angle = ui.ctx().input(|input| input.time as f32 * 4.0);
                painter.add(
                    egui::epaint::TextShape::new(pos, galley, pal.accent_primary)
                        .with_angle_and_anchor(angle, egui::Align2::CENTER_CENTER),
                );
                badge_resp.on_hover_text(format!(
                    "Processing ({})… {}s",
                    busy.label,
                    busy.elapsed_secs.unwrap_or(0)
                ));
            }
        }

        let close_rect = egui::Rect::from_min_size(
            egui::pos2(tab_rect.right() - CLOSE_W, tab_rect.top()),
            egui::vec2(CLOSE_W, TAB_H),
        );
        let close_id = egui::Id::new(("tab_close", i));
        let close_resp = ui.interact(close_rect, close_id, egui::Sense::click());
        let close_col = if close_resp.hovered() {
            pal.danger
        } else if is_active {
            pal.text_dim
        } else {
            pal.separator
        };
        painter.text(
            close_rect.center(),
            egui::Align2::CENTER_CENTER,
            ph::X,
            egui::FontId::proportional(13.0),
            close_col,
        );
        if close_resp.clicked() {
            actions.doc.close_doc_tab = Some(i);
        }

        if tab_response.clicked() && !close_resp.clicked() && !is_active {
            actions.doc.switch_doc = Some(i);
        }
    }

    let (new_rect, new_resp) =
        ui.allocate_exact_size(egui::vec2(NEW_BTN_W, TAB_H), egui::Sense::click());
    let new_fill = if new_resp.hovered() {
        pal.button_hover
    } else {
        pal.app_bg
    };
    ui.painter().rect_filled(new_rect, 0.0, new_fill);
    ui.painter().text(
        new_rect.center(),
        egui::Align2::CENTER_CENTER,
        ph::PLUS,
        egui::FontId::proportional(16.0),
        pal.text_dim,
    );
    if new_resp.clicked() {
        actions.doc.new_doc_tab = true;
    }
}

/// Attach tab `i`'s preview as a hover tooltip: only the low-resolution image,
/// upscaled soft. Documents without a pixel preview get no tooltip.
fn with_doc_preview(response: egui::Response, data: &UiData, i: usize) -> egui::Response {
    let Some((w, h)) = data.doc.doc_dims.get(i).copied().flatten() else {
        return response;
    };
    let fit = |size: egui::Vec2| {
        let scale = (PREVIEW_MAX_W / size.x.max(1.0)).min(PREVIEW_MAX_H / size.y.max(1.0));
        size * scale
    };
    let thumb = data.doc.doc_thumbs.get(i).copied().flatten();
    response.on_hover_ui(|ui| {
        let size = fit(thumb.map_or(egui::vec2(w as f32, h as f32), |(_, size)| size));
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        match thumb {
            Some((texture, _)) => {
                ui.painter().image(
                    texture,
                    rect,
                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            }
            None => {
                let pal = data.chrome.theme_mode.palette();
                ui.painter().rect_filled(rect, 2.0, pal.app_bg);
                ui.put(rect, egui::Spinner::new().size(16.0));
            }
        }
    })
}

fn build_document_list(
    ctx: &egui::Context,
    ui: &mut egui::Ui,
    data: &UiData,
    actions: &mut UiActions,
    hidden: &[usize],
) {
    let pal = data.chrome.theme_mode.palette();
    // The tabs off either end of the strip, in order.
    if hidden.is_empty() {
        return;
    }
    // The popup frame has 6 px of vertical margin on each side. Grow with the
    // hidden tabs up to the top edge of the status bar, then scroll.
    let available_height =
        (ctx.content_rect().bottom() - ui.max_rect().bottom() - super::statusbar::HEIGHT - 12.0)
            .max(DOC_LIST_ROW_H);
    let button = egui::Button::new(
        egui::RichText::new(format!("{} {}", hidden.len(), ph::CARET_DOWN))
            .size(12.0)
            .color(pal.text_dim),
    )
    .min_size(egui::vec2(DOC_LIST_BTN_W, TAB_H))
    .fill(pal.app_bg)
    .corner_radius(0.0);

    let (response, _) = egui::containers::menu::MenuButton::from_button(button).ui(ui, |ui| {
        ui.set_min_width(DOC_LIST_MIN_W);
        ui.set_max_width(DOC_LIST_MAX_W);

        egui::ScrollArea::vertical()
            .id_salt("document_list_scroll")
            .max_height(available_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_min_width(DOC_LIST_MIN_W);

                for &i in hidden {
                    let title = data
                        .doc
                        .doc_titles
                        .get(i)
                        .map(|s| s.as_str())
                        .unwrap_or("Untitled");
                    let dirty = data.doc.doc_modified.get(i).copied().unwrap_or(false);
                    let display = if dirty {
                        format!("{} *", title)
                    } else {
                        title.to_string()
                    };

                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let close = ui
                                .add_sized(
                                    [DOC_LIST_ROW_H, DOC_LIST_ROW_H],
                                    egui::Button::new(ph::X).frame(false),
                                )
                                .on_hover_text(format!("Close {}", title));

                            let row_width = ui.available_width();
                            let label = egui::RichText::new(truncate_title(&display, 42));
                            let select = ui.add_sized(
                                [row_width, DOC_LIST_ROW_H],
                                egui::Button::new(())
                                    .left_text(label)
                                    .frame_when_inactive(false),
                            );
                            if select.hovered() {
                                actions.doc.hovered_doc = Some(i);
                            }
                            let select = with_doc_preview(select, data, i);

                            if close.clicked() {
                                actions.doc.close_doc_tab = Some(i);
                                ui.close();
                            } else if select.clicked() {
                                actions.doc.switch_doc = Some(i);
                                ui.close();
                            }
                        });
                    });
                }
            });
    });

    response.on_hover_text(format!("{} more open documents", hidden.len()));
}

fn truncate_title(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else {
        let truncated: String = chars[..max_chars - 1].iter().collect();
        format!("{}…", truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::{StripPlan, plan_window};

    fn plan(shown: &[usize], hidden: &[usize]) -> StripPlan {
        StripPlan {
            shown: shown.to_vec(),
            hidden: hidden.to_vec(),
        }
    }

    #[test]
    fn everything_that_fits_is_shown() {
        let widths = [100.0; 6];
        assert_eq!(
            plan_window(&widths, 0, Some(2), 600.0).0,
            plan(&[0, 1, 2, 3, 4, 5], &[])
        );
        // Three fit, the strip at the start: the rest are listed.
        assert_eq!(
            plan_window(&widths, 0, Some(1), 320.0).0,
            plan(&[0, 1, 2], &[3, 4, 5])
        );
    }

    /// The strip drawn to a picture, to look at (not a check):
    /// `IAI_UI_SNAPSHOT=<folder> cargo test --lib probe_tab_strip -- --ignored`.
    #[test]
    #[ignore = "picture probe"]
    fn probe_tab_strip() {
        use crate::ui::{UiActions, UiData};
        let Ok(dir) = std::env::var("IAI_UI_SNAPSHOT") else {
            return;
        };
        for (name, active) in [("cuoi", 29usize), ("giua", 14), ("dau", 2)] {
            let mut data = UiData::default();
            let n = 30;
            data.doc.doc_count = n;
            data.doc.active_doc_idx = active;
            data.doc.doc_titles =
                std::sync::Arc::new((1..=n).map(|i| format!("Ảnh khách {i:02}")).collect());
            data.doc.doc_modified = std::sync::Arc::new(vec![false; n]);
            data.doc.doc_ai_busy = std::sync::Arc::new(vec![None; n]);
            data.doc.doc_attention = std::sync::Arc::new(vec![false; n]);
            data.doc.doc_thumbs = std::sync::Arc::new(vec![None; n]);
            data.doc.doc_dims = std::sync::Arc::new(vec![None; n]);
            data.doc.doc_ids =
                std::sync::Arc::new(vec![crate::core::document::DocumentId::NONE; n]);
            let image = crate::ui::snapshot::render(1100.0, 60.0, 1.0, None, |ctx| {
                let mut actions = UiActions::default();
                super::build(ctx, &data, &mut actions);
            });
            image
                .save(std::path::Path::new(&dir).join(format!("tab_{name}.png")))
                .unwrap();
        }
    }

    #[test]
    fn a_tab_off_the_strip_slides_it_the_least_and_old_tabs_make_room() {
        let widths = [100.0; 6];
        // The last opened: the strip slides right, the first tabs go.
        assert_eq!(
            plan_window(&widths, 0, Some(5), 320.0),
            (plan(&[3, 4, 5], &[0, 1, 2]), 3)
        );
        // Back to a tab on the left: it becomes the first.
        assert_eq!(
            plan_window(&widths, 3, Some(1), 320.0),
            (plan(&[1, 2, 3], &[0, 4, 5]), 1)
        );
        // No room even for it: it is still shown, alone.
        assert_eq!(plan_window(&widths, 0, Some(4), 50.0).0.shown, vec![4]);
    }

    #[test]
    fn switching_between_tabs_on_the_strip_does_not_move_it() {
        let widths = [100.0; 6];
        for t in 3..6 {
            assert_eq!(plan_window(&widths, 3, Some(t), 320.0).1, 3);
        }
    }

    #[test]
    fn a_strip_with_room_left_at_the_end_slides_back() {
        // Two tabs closed: the window at 4 would leave room, so it fills.
        let widths = [100.0; 5];
        assert_eq!(
            plan_window(&widths, 4, None, 320.0),
            (plan(&[2, 3, 4], &[0, 1]), 2)
        );
        // The wheel past the end stops at the last full window.
        assert_eq!(plan_window(&widths, 9, None, 320.0).1, 2);
    }
}
