//! Panels hidden by Tab or Shift+Tab, shown for a while: resting the pointer on the left or right
//! edge of the dock area shows the docked panels on that side of the canvas
//! (`window.panels.peek`). They hide again once the pointer has been away from them for
//! [`GRACE`] seconds; Tab or Shift+Tab shows them for good. The canvas keeps the size it has with
//! every panel hidden, so the panels cover its edge and the artwork stays where it is. Floating
//! panels, the Tools panel and the Control bar stay hidden.
//!
//! **Predictive peek**: when the pointer moves fast toward an edge (velocity exceeding
//! [`PREDICT_SPEED_MIN`] within [`PREDICT_ZONE`] points of it), the panels appear after just
//! [`PREDICT_DELAY`] seconds instead of the full [`DELAY`]. Conversely, when the panels are shown
//! and the pointer moves decisively back toward the canvas (exceeding [`REVERSE_SPEED_MIN`]), the
//! grace period shortens to [`REVERSE_GRACE`] so the panels vanish almost immediately.

use craft_ui::docking::{Layout, Node};
use craft_ui::layout::SplitAxis;
use serde_json::{Value, json};

use super::CANVAS;
use crate::VectorcraftApp;

/// Width of the band along each edge of the dock area that shows the panels on that side.
pub(crate) const EDGE: f32 = 6.0;
/// How long the pointer rests on an edge band before the panels show, in seconds.
pub(crate) const DELAY: f64 = 0.3;
/// How long the pointer stays away from the panels shown before they hide, in seconds.
pub(crate) const GRACE: f64 = 0.3;
/// Distance from an edge, in points, within which velocity-based prediction activates.
pub(crate) const PREDICT_ZONE: f32 = 120.0;
/// Minimum horizontal speed (points/second) toward an edge to trigger predictive show.
pub(crate) const PREDICT_SPEED_MIN: f32 = 200.0;
/// How long the pointer must head toward an edge before the panels show, in seconds.
/// Shorter than [`DELAY`]: the velocity makes the intent clearer.
pub(crate) const PREDICT_DELAY: f64 = 0.05;
/// Minimum horizontal speed (points/second) away from shown panels to hide them early.
pub(crate) const REVERSE_SPEED_MIN: f32 = 150.0;
/// Grace period when the pointer is moving decisively away from shown panels, in seconds.
pub(crate) const REVERSE_GRACE: f64 = 0.08;

/// A side of the canvas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    Left,
    Right,
}

impl Edge {
    fn id(self) -> &'static str {
        match self {
            Edge::Left => "left",
            Edge::Right => "right",
        }
    }
}

/// The panels shown on one side.
#[derive(Clone, Copy, Debug)]
struct Shown {
    edge: Edge,
    /// When the pointer left them; `None` while it is over them or has not moved since they showed.
    left_at: Option<f64>,
    /// Whether a press, a modal dialog, a popup or a flyout held them open last frame.
    held: bool,
    /// Whether the eased hide transition is underway.
    closing: bool,
}

/// The session's state of the panels shown from the edges.
pub(crate) struct State {
    /// Whether Tab or Shift+Tab hides the panels this frame.
    hidden: bool,
    /// The dock area this frame.
    area: egui::Rect,
    /// The edge band the pointer rests on, and since when.
    resting: Option<(Edge, f64)>,
    /// The edge the pointer is approaching by velocity, and since when.
    approaching: Option<(Edge, f64)>,
    shown: Option<Shown>,
    /// Eased progress of the visible edge panels, from fully hidden to fully shown.
    progress: f32,
    /// How far the canvas's corners lay from the dock area's with every panel hidden.
    canvas: Option<(egui::Vec2, egui::Vec2)>,
    /// The part of the canvas left uncovered while panels show.
    uncovered: Option<egui::Rect>,
}

impl Default for State {
    fn default() -> Self {
        Self { hidden: false, area: egui::Rect::NOTHING, resting: None, approaching: None, shown: None, progress: 0.0, canvas: None, uncovered: None }
    }
}

/// The docked panels on `edge`'s side of the canvas, at any depth of horizontal splits.
pub(crate) fn edge_panels(layout: &Layout<String>, edge: Edge) -> Vec<String> {
    let canvas = CANVAS.to_string();
    let tree = |node: &Node<String>| Layout { root: Some(node.clone()), floating: Vec::new() };
    let mut out = Vec::new();
    let mut node = layout.root.as_ref();
    let mut depth = 0usize;
    while let Some(Node::Split { axis, first, second, .. }) = node {
        depth = depth.saturating_add(1);
        if depth > 2 * craft_ui::docking::MAX_PANELS {
            break;
        }
        let (inner, outer, side) = if tree(first).contains(&canvas) { (first, second, Edge::Right) } else { (second, first, Edge::Left) };
        if matches!(axis, SplitAxis::Horizontal) && side == edge {
            out.extend(tree(outer).panels().into_iter().filter(|p| **p != canvas).cloned());
        }
        node = Some(inner);
    }
    out
}

/// The side whose panels show this frame. `area` is the dock area and `blocked` whether a modal
/// dialog or a popup takes the input.
pub(super) fn update(app: &mut VectorcraftApp, ctx: &egui::Context, area: egui::Rect, layout: &Layout<String>, blocked: bool) -> Option<Edge> {
    let hidden = !app.ui.dock && app.ui.screen_mode < 3;
    let flyout = app.ui.open_panel.is_some();
    // The docked Tools panel's rect, read only while Tools shows (Tab turns `toolbar` off).
    let tools = app.ui.toolbar.then(|| ctx.data(|d| d.get_temp::<egui::Rect>(crate::toolbar::docked_rect_id()))).flatten();
    let st = &mut app.dock_rt.peek;
    st.hidden = hidden;
    st.area = area;
    if !hidden {
        st.resting = None;
        st.approaching = None;
        st.shown = None;
        st.uncovered = None;
        st.progress = animate(ctx, false);
        return None;
    }
    let (now, pointer, down, moved, velocity) = ctx.input(|i| {
        // A release counts as moving away: a press may have held the panels while it left them. A
        // pointer entering the window has no delta on its first frame, so the move event counts too.
        let moved = i.pointer.delta() != egui::Vec2::ZERO
            || i.pointer.any_released()
            || i.events.iter().any(|e| matches!(e, egui::Event::PointerMoved(_) | egui::Event::PointerGone));
        (i.time, i.pointer.hover_pos(), i.pointer.any_down(), moved, i.pointer.velocity())
    });
    // A workspace switched to while panels show may dock none on that side.
    if st.shown.is_some_and(|shown| edge_panels(layout, shown.edge).is_empty()) {
        st.shown = None;
        st.uncovered = None;
        st.progress = animate(ctx, false);
    }
    if let Some(shown) = st.shown.as_mut() {
        // The panels shown sit beside the canvas, level with it.
        let over = pointer.is_some_and(|p| {
            let uncovered = st.uncovered.unwrap_or_else(|| area.shrink2(egui::vec2(EDGE, 0.0)));
            area.contains(p)
                && (uncovered.top()..=uncovered.bottom()).contains(&p.y)
                && match shown.edge {
                    Edge::Left => p.x < uncovered.left(),
                    Edge::Right => p.x > uncovered.right(),
                }
        });
        let held = down || blocked || flyout;
        if over || held {
            shown.left_at = None;
            shown.closing = false;
        } else if moved || shown.held {
            // A hold that ends with the pointer away from them counts as moving away, as closing a
            // popup or flyout with Escape does not move the pointer.
            shown.left_at.get_or_insert(now);
        }
        shown.held = held;
        // Predictive early hide: a decisive move back toward the canvas shortens the grace
        // period so the panels vanish sooner while the pointer is heading into the artwork.
        let retreating = match shown.edge {
            Edge::Left => velocity.x > REVERSE_SPEED_MIN,
            Edge::Right => velocity.x < -REVERSE_SPEED_MIN,
        };
        let grace = if retreating { REVERSE_GRACE } else { GRACE };
        if let Some(left) = shown.left_at {
            if now - left + 1e-4 >= grace {
                shown.closing = true;
                // A flyout opened from the panels shown closes with them.
                app.ui.open_panel = None;
            } else if !shown.closing {
                ctx.request_repaint_after_secs((grace - (now - left)) as f32);
            }
        }
        let edge = shown.edge;
        let closing = shown.closing;
        st.progress = animate(ctx, !closing);
        if closing && st.progress <= f32::EPSILON {
            st.shown = None;
            st.uncovered = None;
            return None;
        }
        return Some(edge);
    }
    // Predictive show: the pointer is within PREDICT_ZONE of an edge and moving toward it
    // faster than PREDICT_SPEED_MIN. After PREDICT_DELAY the panels appear — shorter than
    // the resting delay, since the velocity signals clear intent.
    let predict_edge = pointer
        .filter(|p| area.contains(*p) && !tools.is_some_and(|r| r.contains(*p)))
        .and_then(|p| {
            if p.x < area.left() + PREDICT_ZONE && velocity.x < -PREDICT_SPEED_MIN {
                Some(Edge::Left)
            } else if p.x > area.right() - PREDICT_ZONE && velocity.x > PREDICT_SPEED_MIN {
                Some(Edge::Right)
            } else {
                None
            }
        })
        .filter(|e| !down && !blocked && !edge_panels(layout, *e).is_empty());
    if let Some(edge) = predict_edge {
        let since = match st.approaching {
            Some((approached, since)) if approached == edge => since,
            _ => now,
        };
        if now - since + 1e-4 >= PREDICT_DELAY {
            st.approaching = None;
            st.resting = None;
            st.shown = Some(Shown { edge, left_at: None, held: false, closing: false });
            st.progress = animate(ctx, true);
            return Some(edge);
        }
        st.approaching = Some((edge, since));
        ctx.request_repaint_after_secs((PREDICT_DELAY - (now - since)) as f32);
        // A wider band at lower opacity shows the panels are about to appear.
        let band = match edge {
            Edge::Left => egui::Rect::from_min_max(area.left_top(), egui::pos2(area.left() + EDGE * 3.0, area.bottom())),
            Edge::Right => egui::Rect::from_min_max(egui::pos2(area.right() - EDGE * 3.0, area.top()), area.right_bottom()),
        };
        let t = crate::theme::Tokens::get(ctx);
        let layer = egui::LayerId::new(egui::Order::Foreground, super::area_id().with("peek-strip"));
        ctx.layer_painter(layer).rect_filled(band, 0.0, t.accent.gamma_multiply(0.3));
        st.progress = animate(ctx, false);
        return None;
    }
    st.approaching = None;

    let edge = pointer.filter(|p| area.contains(*p) && !tools.is_some_and(|r| r.contains(*p))).and_then(|p| {
        if p.x <= area.left() + EDGE {
            Some(Edge::Left)
        } else if p.x >= area.right() - EDGE {
            Some(Edge::Right)
        } else {
            None
        }
    });
    let Some(edge) = edge.filter(|e| !down && !blocked && !edge_panels(layout, *e).is_empty()) else {
        st.resting = None;
        st.progress = animate(ctx, false);
        return None;
    };
    let since = match st.resting {
        Some((rested, since)) if rested == edge => since,
        _ => now,
    };
    if now - since >= DELAY {
        st.resting = None;
        st.shown = Some(Shown { edge, left_at: None, held: false, closing: false });
        st.progress = animate(ctx, true);
        return Some(edge);
    }
    st.resting = Some((edge, since));
    ctx.request_repaint_after_secs((DELAY - (now - since)) as f32);
    // The strip the pointer rests on.
    let band = match edge {
        Edge::Left => egui::Rect::from_min_max(area.left_top(), egui::pos2(area.left() + EDGE, area.bottom())),
        Edge::Right => egui::Rect::from_min_max(egui::pos2(area.right() - EDGE, area.top()), area.right_bottom()),
    };
    let t = crate::theme::Tokens::get(ctx);
    let layer = egui::LayerId::new(egui::Order::Foreground, super::area_id().with("peek-strip"));
    ctx.layer_painter(layer).rect_filled(band, 0.0, t.accent.gamma_multiply(0.6));
    st.progress = animate(ctx, false);
    None
}

fn animate(ctx: &egui::Context, shown: bool) -> f32 {
    ctx.animate_bool_responsive(super::area_id().with("peek-content"), shown)
}

/// The side shown, for `ui.docking`.
pub(super) fn shown(app: &VectorcraftApp) -> Option<&'static str> {
    app.dock_rt.peek.shown.map(|s| s.edge.id())
}

#[cfg(test)]
pub(super) fn progress(app: &VectorcraftApp) -> f32 {
    app.dock_rt.peek.progress
}

/// Eases the body of an edge panel into or out of its clipped reveal area.
pub(super) fn animate_panel_body(app: &VectorcraftApp, ui: &mut egui::Ui, layout: &Layout<String>, panel: &str) {
    let Some(shown) = app.dock_rt.peek.shown else { return };
    if !edge_panels(layout, shown.edge).iter().any(|id| id == panel) {
        return;
    }
    let rect = ui.max_rect();
    if !rect.is_positive() {
        return;
    }
    let progress = app.dock_rt.peek.progress.clamp(0.0, 1.0);
    let revealed = match shown.edge {
        Edge::Left => egui::Rect::from_min_max(rect.min, egui::pos2(rect.left() + rect.width() * progress, rect.bottom())),
        Edge::Right => egui::Rect::from_min_max(egui::pos2(rect.right() - rect.width() * progress, rect.top()), rect.max),
    };
    ui.set_clip_rect(ui.clip_rect().intersect(revealed));
    ui.multiply_opacity(progress);
}

/// Draw the canvas panel's body with `draw`. While panels show from an edge, the canvas keeps the
/// rect it has with every panel hidden, drawn under the panels and taking input only where they
/// leave it uncovered.
pub(super) fn canvas(app: &mut VectorcraftApp, ui: &mut egui::Ui, draw: impl FnOnce(&mut VectorcraftApp, &mut egui::Ui)) {
    let uncovered = ui.max_rect();
    let st = &mut app.dock_rt.peek;
    let wide = if !st.hidden {
        st.canvas = None;
        None
    } else if st.shown.is_none() {
        st.canvas = Some((uncovered.min - st.area.min, uncovered.max - st.area.max));
        None
    } else {
        st.uncovered = Some(uncovered);
        // The canvas's insets in the dock area with every panel hidden, kept as the window resizes;
        // none before such a frame was drawn, as it fills the area then.
        let (min, max) = st.canvas.unwrap_or((egui::Vec2::ZERO, egui::Vec2::ZERO));
        Some(egui::Rect::from_min_max(st.area.min + min, st.area.max + max))
    };
    match wide {
        Some(full) if full != uncovered && full.is_positive() => {
            let clip = uncovered.intersect(ui.clip_rect());
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(full));
            child.set_clip_rect(clip);
            draw(app, &mut child);
        }
        _ => draw(app, ui),
    }
}

/// `window.panels.peek {side: left|right|null}`: show the docked panels on that side while Tab or
/// Shift+Tab hides the panels, as resting the pointer on that edge does, or (null) hide them.
pub(super) fn command(app: &mut VectorcraftApp, params: &Value) -> Result<Value, String> {
    let edge = match params.get("side") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == "left" => Some(Edge::Left),
        Some(Value::String(s)) if s == "right" => Some(Edge::Right),
        Some(_) => return Err("`side` must be left, right or null".into()),
    };
    if app.ui.screen_mode >= 3 {
        return Err("panels can't be shown in Presentation Mode".into());
    }
    if app.ui.dock {
        return Err("the panels are not hidden".into());
    }
    match edge {
        Some(edge) => {
            if edge_panels(&super::layout_of(&app.ui), edge).is_empty() {
                return Err(format!("no panels are docked on the {} side", edge.id()));
            }
            let st = &mut app.dock_rt.peek;
            st.resting = None;
            st.shown = Some(Shown { edge, left_at: None, held: false, closing: false });
            Ok(json!(edge.id()))
        }
        None => {
            let st = &mut app.dock_rt.peek;
            if st.shown.take().is_some() {
                st.uncovered = None;
                app.ui.open_panel = None;
            }
            Ok(Value::Null)
        }
    }
}
