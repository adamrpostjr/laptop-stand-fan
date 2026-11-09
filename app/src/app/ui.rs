//! Drawing: theme, layout and the small custom widgets.

use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, Layout, Pos2, RichText, Sense, Shape, Stroke, ViewportCommand, vec2,
};
use fanstand_ipc::{Link, Mode, Request, Snapshot};

use super::{FanApp, HISTORY, Sample, UiEvent, lock, send};
use crate::icon::Tone;

impl eframe::App for FanApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Close button hides to the tray. (While hidden, input is stale, so
        // only look at it while we're actually shown.)
        if self.visible && !self.quitting && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(ViewportCommand::Visible(false));
            self.visible = false;
        }
        while let Ok(ev) = self.events.try_recv() {
            match ev {
                UiEvent::Show => {
                    ctx.send_viewport_cmd(ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(ViewportCommand::Focus);
                    self.visible = true;
                }
                UiEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            }
        }
        self.update_tray();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let (snap, history, cmd, busy) = {
            let st = lock(&self.shared);
            (
                st.snap.clone(),
                st.history.iter().copied().collect::<Vec<_>>(),
                st.cmd.clone(),
                st.busy,
            )
        };

        // Fan glyph animation: advance by real time, scaled from RPM so a
        // faster fan visibly spins faster (true speed would just alias).
        // Only while focused: a background window redraws just once a second
        // (on new data), so it costs next to nothing.
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;
        if let Some(Ok(Snapshot {
            stand: Some(st), ..
        })) = &snap
            && ctx.input(|i| i.focused)
        {
            for i in 0..2 {
                self.spin[i] = (self.spin[i] + dt * st.rpm[i] as f32 / 1500.0 * 9.0)
                    % std::f32::consts::TAU;
            }
            if st.rpm.iter().any(|&r| r > 0) {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
        }

        egui::Panel::top("header")
            .frame(egui::Frame::new().fill(pal::BG).inner_margin(egui::Margin {
                left: 18,
                right: 18,
                top: 14,
                bottom: 6,
            }))
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    fan_glyph(ui, 22.0, 0.0, pal::ACCENT);
                    ui.label(RichText::new("FanStand").size(20.0).strong().color(pal::TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (tone, text) = link_label(snap.as_ref());
                        pill(ui, tone, &text);
                    });
                });
            });

        egui::Panel::bottom("footer")
            .frame(
                egui::Frame::new()
                    .fill(pal::BG)
                    .inner_margin(egui::Margin::symmetric(18, 10)),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(Ok(s)) = &snap {
                        ui.label(
                            RichText::new(format!("service {}", s.service_version))
                                .size(11.0)
                                .color(pal::FAINT),
                        );
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if link_button(ui, "Logs").clicked() {
                            let _ = std::process::Command::new("explorer")
                                .arg(super::log_dir())
                                .spawn();
                        }
                        if matches!(&snap, Some(Ok(_))) && link_button(ui, "Reconnect").clicked()
                        {
                            send(&self.shared, &ctx, Request::Reconnect);
                        }
                    });
                });
            });

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(pal::BG)
                    .inner_margin(egui::Margin::symmetric(18, 8)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match &snap {
                    None => waiting(ui, "Connecting to the service…", None),
                    Some(Err(e)) => service_down(ui, e),
                    Some(Ok(s)) => self.body(ui, &ctx, s, &history, cmd.as_ref(), busy),
                });
            });
    }
}

impl FanApp {
    fn body(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        s: &Snapshot,
        history: &[Sample],
        cmd: Option<&(String, bool, Instant)>,
        busy: bool,
    ) {
        let Some(st) = &s.stand else {
            let msg = match s.link {
                Link::Suspended => "Paused while the computer sleeps",
                Link::Retrying => "Lost the stand, retrying…",
                _ => "Looking for the stand…",
            };
            waiting(ui, msg, s.last_error.as_deref());
            return;
        };

        // ---- Fans
        let max_rpm = history
            .iter()
            .flat_map(|h| h.rpm)
            .chain(st.rpm)
            .max()
            .unwrap_or(0)
            .max(800) as f32
            * 1.15;
        ui.columns(2, |cols| {
            for (i, col) in cols.iter_mut().enumerate() {
                let c = if st.stalled[i] { pal::RED } else { pal::ACCENT };
                card(col, |ui| {
                    ui.horizontal(|ui| {
                        fan_glyph(ui, 18.0, self.spin[i], c);
                        ui.label(caps(&format!("Fan {}", i + 1)));
                        if st.stalled[i] {
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                pill(ui, Tone::Alarm, "stalled");
                            });
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 5.0;
                        ui.label(
                            RichText::new(st.rpm[i].to_string())
                                .size(38.0)
                                .strong()
                                .color(pal::TEXT),
                        );
                        ui.with_layout(Layout::left_to_right(Align::Max), |ui| {
                            ui.label(RichText::new("rpm").size(13.0).color(pal::WEAK));
                        });
                    });
                    let vals: Vec<f32> = history.iter().map(|h| h.rpm[i] as f32).collect();
                    sparkline(ui, &vals, max_rpm, c);
                });
            }
        });

        // ---- Stat tiles
        ui.columns(3, |cols| {
            card(&mut cols[0], |ui| {
                ui.label(caps("CPU"));
                match s.cpu {
                    Some(c) => {
                        ui.label(
                            RichText::new(format!("{:.0}°", c.celsius))
                                .size(26.0)
                                .strong()
                                .color(temp_color(c.celsius)),
                        );
                        ui.label(
                            RichText::new(if c.estimated { "estimated" } else { "sensor" })
                                .size(11.0)
                                .color(pal::WEAK),
                        );
                    }
                    None => {
                        ui.label(RichText::new("—").size(26.0).color(pal::WEAK));
                        ui.label(RichText::new("no reading").size(11.0).color(pal::WEAK));
                    }
                }
            })
            .response
            .on_hover_text(&s.temp_source);

            card(&mut cols[1], |ui| {
                ui.label(caps("Duty"));
                ui.label(
                    RichText::new(format!("{:.0}%", st.duty_pct))
                        .size(26.0)
                        .strong()
                        .color(pal::TEXT),
                );
                bar(ui, st.duty_pct / 100.0, st.target_pct / 100.0);
            })
            .response
            .on_hover_text(format!(
                "Now {:.1}%, heading to {:.1}%",
                st.duty_pct, st.target_pct
            ));

            card(&mut cols[2], |ui| {
                ui.label(caps("Status"));
                let (big, small, tone) = if st.stalled.iter().any(|&x| x) {
                    ("Stall", "fans at 100%", Tone::Alarm)
                } else if st.calibrating {
                    ("Calib.", "in progress", Tone::Attention)
                } else if st.failsafe {
                    ("Failsafe", "no fresh temp", Tone::Attention)
                } else {
                    ("Normal", "all good", Tone::Ok)
                };
                ui.label(RichText::new(big).size(26.0).strong().color(color(tone)));
                ui.label(RichText::new(small).size(11.0).color(pal::WEAK));
            })
            .response
            .on_hover_text(format!("Stand flags: {}", st.flags));
        });

        // ---- Control
        card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(caps("Fan control"));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if busy {
                        ui.add(egui::Spinner::new().size(12.0).color(pal::WEAK));
                    } else if let Some((msg, err, at)) = cmd
                        && at.elapsed() < Duration::from_secs(6)
                    {
                        let c = if *err { pal::RED } else { pal::ACCENT };
                        ui.label(RichText::new(msg).size(11.0).color(c));
                    }
                });
            });
            ui.add_space(6.0);

            let held = self.slider_held_until.is_some_and(|t| t > Instant::now());
            if !held {
                self.slider = st.manual_pct;
            }
            let manual = st.mode == Mode::Manual;
            ui.add_enabled_ui(!busy, |ui| {
                // Segmented Auto | Manual.
                let w = (ui.available_width() - 6.0) / 2.0;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    if segment(ui, "Auto · fan curve", !manual, w).clicked() && manual {
                        send(&self.shared, ctx, Request::SetAuto);
                    }
                    if segment(ui, "Manual", manual, w).clicked() && !manual {
                        send(&self.shared, ctx, Request::SetManual { pct: self.slider });
                    }
                });
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.label(RichText::new("Speed").color(pal::WEAK));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{}%", self.slider))
                                .size(16.0)
                                .strong()
                                .color(if manual { pal::TEXT } else { pal::WEAK }),
                        );
                    });
                });
                ui.spacing_mut().slider_width = ui.available_width();
                let r = ui.add(egui::Slider::new(&mut self.slider, 0..=100).show_value(false));
                if r.dragged() || r.changed() {
                    // Don't let the next poll yank the handle back.
                    self.slider_held_until = Some(Instant::now() + Duration::from_secs(3));
                }
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    send(&self.shared, ctx, Request::SetManual { pct: self.slider });
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for p in [30u8, 50, 75, 100] {
                        if chip(ui, &format!("{p}%"), manual && st.manual_pct == p).clicked() {
                            self.slider = p;
                            self.slider_held_until =
                                Some(Instant::now() + Duration::from_secs(3));
                            send(&self.shared, ctx, Request::SetManual { pct: p });
                        }
                    }
                });
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(if manual {
                    "Manual speed holds until you switch back to Auto."
                } else {
                    "Following the fan curve. Pick a speed to override it."
                })
                .size(11.0)
                .color(pal::FAINT),
            );
        });
    }
}

// ------------------------------------------------------------------ theme

mod pal {
    use eframe::egui::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x10, 0x12, 0x16);
    pub const CARD: Color32 = Color32::from_rgb(0x18, 0x1b, 0x21);
    pub const CARD_STROKE: Color32 = Color32::from_rgb(0x24, 0x28, 0x31);
    pub const RAISED: Color32 = Color32::from_rgb(0x22, 0x26, 0x2e);
    pub const RAISED_HOVER: Color32 = Color32::from_rgb(0x2b, 0x30, 0x3a);
    pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xed);
    pub const WEAK: Color32 = Color32::from_rgb(0x8d, 0x95, 0xa3);
    pub const FAINT: Color32 = Color32::from_rgb(0x5f, 0x66, 0x72);
    pub const ACCENT: Color32 = Color32::from_rgb(0x3d, 0xc4, 0x8a);
    pub const ACCENT_BG: Color32 = Color32::from_rgb(0x17, 0x3a, 0x2c);
    pub const AMBER: Color32 = Color32::from_rgb(0xf2, 0xb1, 0x3a);
    pub const RED: Color32 = Color32::from_rgb(0xf0, 0x54, 0x4c);
}

pub(super) fn apply_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut v = egui::Visuals::dark();
    v.panel_fill = pal::BG;
    v.window_fill = pal::BG;
    v.extreme_bg_color = pal::BG;
    v.faint_bg_color = pal::CARD;
    v.selection.bg_fill = pal::ACCENT_BG;
    v.selection.stroke = Stroke::new(1.0, pal::ACCENT);
    v.slider_trailing_fill = true;
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
        &mut v.widgets.noninteractive,
    ] {
        w.corner_radius = egui::CornerRadius::same(8);
    }
    v.widgets.noninteractive.fg_stroke.color = pal::TEXT;
    v.widgets.inactive.bg_fill = pal::RAISED;
    v.widgets.inactive.weak_bg_fill = pal::RAISED;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = pal::RAISED_HOVER;
    v.widgets.hovered.weak_bg_fill = pal::RAISED_HOVER;
    v.widgets.hovered.bg_stroke = Stroke::NONE;
    v.widgets.active.bg_fill = pal::ACCENT;
    v.widgets.active.weak_bg_fill = pal::RAISED_HOVER;
    ctx.set_visuals_of(egui::Theme::Dark, v);
    ctx.all_styles_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(12.0, 6.0);
        s.spacing.interact_size.y = 26.0;
    });
}

// ------------------------------------------------------------------ widgets

fn card<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(pal::CARD)
        .stroke(Stroke::new(1.0, pal::CARD_STROKE))
        .corner_radius(12)
        .inner_margin(14)
        .outer_margin(egui::Margin::symmetric(0, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
}

/// Small all-caps section label.
fn caps(s: &str) -> RichText {
    RichText::new(s.to_uppercase())
        .size(11.0)
        .strong()
        .color(pal::WEAK)
        .extra_letter_spacing(0.8)
}

fn pill(ui: &mut egui::Ui, tone: Tone, text: &str) {
    let c = color(tone);
    // `horizontal` inherits a right-to-left parent, so add the parts in
    // reverse there to keep the dot on the left.
    let rtl = ui.layout().prefer_right_to_left();
    egui::Frame::new()
        .fill(c.gamma_multiply(0.14))
        .corner_radius(255)
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                let dot = |ui: &mut egui::Ui| {
                    let (rect, _) = ui.allocate_exact_size(vec2(7.0, 7.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.5, c);
                };
                let label = |ui: &mut egui::Ui| {
                    ui.label(RichText::new(text).size(12.0).color(c));
                };
                if rtl {
                    label(ui);
                    dot(ui);
                } else {
                    dot(ui);
                    label(ui);
                }
            });
        });
}

fn segment(ui: &mut egui::Ui, text: &str, on: bool, width: f32) -> egui::Response {
    let (fill, fg) = if on {
        (pal::ACCENT_BG, pal::ACCENT)
    } else {
        (pal::RAISED, pal::WEAK)
    };
    ui.add(
        egui::Button::new(RichText::new(text).strong().color(fg))
            .fill(fill)
            .stroke(if on {
                Stroke::new(1.0, pal::ACCENT)
            } else {
                Stroke::NONE
            })
            .corner_radius(8)
            .min_size(vec2(width, 34.0)),
    )
}

fn chip(ui: &mut egui::Ui, text: &str, on: bool) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(text)
                .size(12.0)
                .color(if on { pal::ACCENT } else { pal::TEXT }),
        )
        .fill(if on { pal::ACCENT_BG } else { pal::RAISED })
        .stroke(if on {
            Stroke::new(1.0, pal::ACCENT)
        } else {
            Stroke::NONE
        })
        .corner_radius(255)
        .min_size(vec2(52.0, 24.0)),
    )
}

fn link_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add(egui::Button::new(RichText::new(text).size(12.0).color(pal::WEAK)).frame(false))
        .on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Duty bar: filled to `now`, with a tick at `target`.
fn bar(ui: &mut egui::Ui, now: f32, target: f32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let track =
        egui::Rect::from_min_size(rect.left_center() - vec2(0.0, 2.5), vec2(rect.width(), 5.0));
    let p = ui.painter();
    p.rect_filled(track, 3, pal::RAISED);
    let mut fill = track;
    fill.set_width(track.width() * now.clamp(0.0, 1.0));
    p.rect_filled(fill, 3, pal::ACCENT);
    if (target - now).abs() > 0.005 {
        let x = track.left() + track.width() * target.clamp(0.0, 1.0);
        p.line_segment(
            [egui::pos2(x, track.top() - 3.0), egui::pos2(x, track.bottom() + 3.0)],
            Stroke::new(2.0, pal::TEXT),
        );
    }
}

fn waiting(ui: &mut egui::Ui, msg: &str, detail: Option<&str>) {
    ui.add_space(30.0);
    card(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(10.0);
            ui.add(egui::Spinner::new().size(22.0).color(pal::ACCENT));
            ui.add_space(10.0);
            ui.label(RichText::new(msg).size(15.0).color(pal::TEXT));
            if let Some(d) = detail {
                ui.add_space(4.0);
                ui.label(RichText::new(d).size(11.0).color(pal::WEAK));
            }
            ui.add_space(10.0);
        });
    });
}

fn service_down(ui: &mut egui::Ui, e: &str) {
    ui.add_space(30.0);
    card(ui, |ui| {
        ui.vertical_centered(|ui| {
            ui.add_space(6.0);
            fan_glyph(ui, 34.0, 0.0, pal::FAINT);
            ui.add_space(6.0);
            ui.label(RichText::new(e).size(15.0).strong().color(pal::TEXT));
            ui.label(
                RichText::new("This app shows what the FanStand service sees.")
                    .size(12.0)
                    .color(pal::WEAK),
            );
        });
        ui.add_space(12.0);
        for (what, cmd) in [
            ("Install it (elevated terminal)", "fanstand service install --key <file>"),
            ("Start it", "fanstand service start"),
            ("Test without the service", "fanstand run --serve"),
        ] {
            ui.label(RichText::new(what).size(11.0).color(pal::WEAK));
            egui::Frame::new()
                .fill(pal::BG)
                .corner_radius(6)
                .inner_margin(egui::Margin::symmetric(10, 6))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(cmd).monospace().size(12.0).color(pal::TEXT));
                });
            ui.add_space(4.0);
        }
    });
}

fn link_label(snap: Option<&Result<Snapshot, String>>) -> (Tone, String) {
    match snap {
        None => (Tone::Idle, "starting".into()),
        Some(Err(_)) => (Tone::Alarm, "service offline".into()),
        Some(Ok(s)) => match (&s.stand, s.link) {
            (Some(st), _) => (Tone::Ok, format!("connected · fw {}", st.firmware)),
            (None, Link::Suspended) => (Tone::Idle, "paused (sleep)".into()),
            (None, Link::Retrying) => (Tone::Attention, "retrying".into()),
            (None, _) => (Tone::Attention, "searching".into()),
        },
    }
}

fn color(t: Tone) -> Color32 {
    let [r, g, b] = t.rgb();
    Color32::from_rgb(r, g, b)
}

fn temp_color(c: f32) -> Color32 {
    if c >= 80.0 {
        pal::RED
    } else if c >= 65.0 {
        pal::AMBER
    } else {
        pal::TEXT
    }
}

/// A fan: ring, hub and four swept blades, rotated by `angle`.
fn fan_glyph(ui: &mut egui::Ui, size: f32, angle: f32, c: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    let p = ui.painter();
    let ctr = rect.center();
    let r = size / 2.0;
    p.circle_stroke(ctr, r - 1.0, Stroke::new((size / 14.0).max(1.2), c));
    let at = |rad: f32, a: f32| ctr + vec2(a.cos(), a.sin()) * rad;
    for b in 0..4 {
        let a = angle + b as f32 * std::f32::consts::FRAC_PI_2;
        let pts = vec![
            at(r * 0.22, a - 0.55),
            at(r * 0.78, a - 0.30),
            at(r * 0.80, a + 0.15),
            at(r * 0.22, a + 0.35),
        ];
        p.add(Shape::convex_polygon(pts, c, Stroke::NONE));
    }
    p.circle_filled(ctr, r * 0.2, c);
}

/// RPM history as a soft area chart.
fn sparkline(ui: &mut egui::Ui, values: &[f32], max: f32, c: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), Sense::hover());
    let p = ui.painter();
    if values.len() < 2 {
        p.line_segment(
            [rect.left_bottom(), rect.right_bottom()],
            Stroke::new(1.0, pal::CARD_STROKE),
        );
        return;
    }
    let step = rect.width() / (HISTORY - 1) as f32;
    let n = values.len();
    let pts: Vec<Pos2> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let x = rect.right() - (n - 1 - i) as f32 * step;
            let y = rect.bottom() - (v / max).clamp(0.0, 1.0) * rect.height();
            Pos2::new(x, y)
        })
        .collect();
    // Area under the line, fading out towards the bottom.
    let mut mesh = egui::Mesh::default();
    let top = c.gamma_multiply(0.22);
    for pt in &pts {
        mesh.colored_vertex(*pt, top);
        mesh.colored_vertex(Pos2::new(pt.x, rect.bottom()), Color32::TRANSPARENT);
    }
    for i in 0..pts.len() as u32 - 1 {
        let (a, b, c2, d) = (2 * i, 2 * i + 1, 2 * i + 2, 2 * i + 3);
        mesh.add_triangle(a, b, c2);
        mesh.add_triangle(c2, b, d);
    }
    p.add(Shape::mesh(mesh));
    let last = *pts.last().unwrap();
    p.add(Shape::line(pts, Stroke::new(1.8, c)));
    p.circle_filled(last, 2.5, c);
}
