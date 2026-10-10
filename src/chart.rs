// SPDX-License-Identifier: GPL-2.0-or-later

//! The usage chart: a sunburst ring chart and a treemap, drawn with cairo.
//!
//! The current directory sits in the middle, its children form the first ring,
//! and the biggest grandchildren form a second ring. Children are coloured in
//! a fixed eight-hue order (validated for colour-vision deficiency); everything
//! past the eighth slot is neutral, and deeper rings reuse the parent's hue at
//! a lighter tint - no colour is invented.

use crate::model::*;
use gtk4 as gtk;
use gtk::cairo::Context;
use gtk::prelude::*;
use gtk::pango;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

pub const PALETTE_DARK: [(f64, f64, f64); 8] = [
    (0x39 as f64 / 255.0, 0x87 as f64 / 255.0, 0xe5 as f64 / 255.0),
    (0xd9 as f64 / 255.0, 0x59 as f64 / 255.0, 0x26 as f64 / 255.0),
    (0x19 as f64 / 255.0, 0x9e as f64 / 255.0, 0x70 as f64 / 255.0),
    (0xc9 as f64 / 255.0, 0x85 as f64 / 255.0, 0x00 as f64 / 255.0),
    (0xd5 as f64 / 255.0, 0x51 as f64 / 255.0, 0x81 as f64 / 255.0),
    (0x00 as f64 / 255.0, 0x83 as f64 / 255.0, 0x00 as f64 / 255.0),
    (0x90 as f64 / 255.0, 0x85 as f64 / 255.0, 0xe9 as f64 / 255.0),
    (0xe6 as f64 / 255.0, 0x67 as f64 / 255.0, 0x67 as f64 / 255.0),
];

pub const PALETTE_LIGHT: [(f64, f64, f64); 8] = [
    (0x2a as f64 / 255.0, 0x78 as f64 / 255.0, 0xd6 as f64 / 255.0),
    (0xeb as f64 / 255.0, 0x68 as f64 / 255.0, 0x34 as f64 / 255.0),
    (0x1b as f64 / 255.0, 0xaf as f64 / 255.0, 0x7a as f64 / 255.0),
    (0xed as f64 / 255.0, 0xa1 as f64 / 255.0, 0x00 as f64 / 255.0),
    (0xe8 as f64 / 255.0, 0x7b as f64 / 255.0, 0xa4 as f64 / 255.0),
    (0x00 as f64 / 255.0, 0x83 as f64 / 255.0, 0x00 as f64 / 255.0),
    (0x4a as f64 / 255.0, 0x3a as f64 / 255.0, 0xa7 as f64 / 255.0),
    (0xe3 as f64 / 255.0, 0x49 as f64 / 255.0, 0x48 as f64 / 255.0),
];

const MAX_COLOR: usize = 8;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Ring,
    Treemap,
}

#[derive(Clone)]
pub struct Hit {
    pub id: u32,
    pub ring: Option<(f64, f64, f64, f64)>, // a0, a1, r0, r1
    pub rect: Option<(f64, f64, f64, f64)>, // x, y, w, h
}

pub struct ChartState {
    pub tree: Option<Arc<Tree>>,
    pub node: u32,
    pub metric: Metric,
    pub view: View,
    pub dark: bool,
    pub hover: Option<u32>,
    pub selected: Option<u32>,
    pub hits: Vec<Hit>,
    pub colors: HashMap<u32, (f64, f64, f64)>,
    /// centre of the ring chart, stored by the draw pass for hit testing
    pub center: (f64, f64),
    pub on_click: Option<Rc<dyn Fn(u32)>>,
    pub on_hover: Option<Rc<dyn Fn(Option<u32>)>>,
}

pub struct Chart {
    pub area: gtk::DrawingArea,
    pub state: Rc<RefCell<ChartState>>,
}

impl Chart {
    pub fn new() -> Chart {
        let state = Rc::new(RefCell::new(ChartState {
            tree: None,
            node: 0,
            metric: Metric::Owned,
            view: View::Ring,
            dark: true,
            hover: None,
            selected: None,
            hits: Vec::new(),
            colors: HashMap::new(),
            center: (0.0, 0.0),
            on_click: None,
            on_hover: None,
        }));
        let area = gtk::DrawingArea::builder()
            .hexpand(true)
            .vexpand(true)
            .build();

        {
            let state = state.clone();
            area.set_draw_func(move |_, cr, w, h| {
                let mut st = state.borrow_mut();
                st.hits.clear();
                match st.view {
                    View::Ring => draw_ring(cr, w as f64, h as f64, &mut st),
                    View::Treemap => draw_treemap(cr, w as f64, h as f64, &mut st),
                }
            });
        }
        {
            let motion = gtk::EventControllerMotion::new();
            let st_motion = state.clone();
            motion.connect_motion(move |_, x, y| {
                let hit = hit_test(&st_motion.borrow(), x, y);
                let changed = {
                    let mut st = st_motion.borrow_mut();
                    if st.hover == hit {
                        false
                    } else {
                        st.hover = hit;
                        true
                    }
                };
                if changed {
                    let cb = st_motion.borrow().on_hover.clone();
                    if let Some(cb) = cb {
                        cb(hit);
                    }
                }
            });
            let st_leave = state.clone();
            motion.connect_leave(move |_| {
                let was = {
                    let mut st = st_leave.borrow_mut();
                    st.hover.take().is_some()
                };
                if was {
                    let cb = st_leave.borrow().on_hover.clone();
                    if let Some(cb) = cb {
                        cb(None);
                    }
                }
            });
            area.add_controller(motion);
        }
        {
            let st_click = state.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, x, y| {
                if let Some(id) = hit_test(&st_click.borrow(), x, y) {
                    let cb = st_click.borrow().on_click.clone();
                    if let Some(cb) = cb {
                        cb(id);
                    }
                }
            });
            area.add_controller(click);
        }
        Chart { area, state }
    }

    /// Show a directory: recolour its children and redraw.
    pub fn show(&self, tree: Arc<Tree>, node: u32) {
        let mut st = self.state.borrow_mut();
        st.tree = Some(tree.clone());
        st.node = node;
        st.hover = None;
        st.colors.clear();
        let mut kids: Vec<u32> = tree
            .child_ids(node)
            .filter(|&c| st.metric.value(&tree, c) > 0)
            .collect();
        let pal = if st.dark { PALETTE_DARK } else { PALETTE_LIGHT };
        kids.sort_by_key(|&c| std::cmp::Reverse(st.metric.value(&tree, c)));
        for (i, c) in kids.iter().enumerate() {
            let col = if i < MAX_COLOR {
                pal[i % MAX_COLOR]
            } else {
                neutral(st.dark)
            };
            st.colors.insert(*c, col);
        }
        drop(st);
        self.area.queue_draw();
    }

    pub fn set_metric(&self, m: Metric) {
        self.state.borrow_mut().metric = m;
        let tree = self.state.borrow().tree.clone();
        if let Some(tree) = tree {
            let node = self.state.borrow().node;
            self.show(tree, node); // recolours for the new ranking
        }
    }

    pub fn set_view(&self, v: View) {
        self.state.borrow_mut().view = v;
        self.area.queue_draw();
    }

    pub fn set_dark(&self, dark: bool) {
        self.state.borrow_mut().dark = dark;
        let tree = self.state.borrow().tree.clone();
        match tree {
            Some(tree) => {
                let node = self.state.borrow().node;
                self.show(tree, node);
            }
            None => self.area.queue_draw(),
        }
    }

}

fn neutral(dark: bool) -> (f64, f64, f64) {
    if dark {
        (0.33, 0.33, 0.31)
    } else {
        (0.72, 0.72, 0.70)
    }
}

fn surface(dark: bool) -> (f64, f64, f64) {
    if dark {
        (0.102, 0.102, 0.098)
    } else {
        (0.988, 0.988, 0.984)
    }
}

fn text_color(dark: bool) -> (f64, f64, f64) {
    if dark {
        (0.95, 0.95, 0.93)
    } else {
        (0.05, 0.05, 0.05)
    }
}

fn muted(dark: bool) -> (f64, f64, f64) {
    if dark {
        (0.60, 0.60, 0.56)
    } else {
        (0.42, 0.42, 0.40)
    }
}

/// Deeper rings keep the parent's hue, mixed toward the surface.
fn mix(c: (f64, f64, f64), t: f64, dark: bool) -> (f64, f64, f64) {
    let s = surface(dark);
    (
        c.0 + (s.0 - c.0) * t,
        c.1 + (s.1 - c.1) * t,
        c.2 + (s.2 - c.2) * t,
    )
}

fn set_color(cr: &Context, c: (f64, f64, f64)) {
    cr.set_source_rgb(c.0, c.1, c.2);
}

fn label_layout(cr: &Context, size: f64) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    let mut fd = pango::FontDescription::new();
    fd.set_absolute_size(size * f64::from(pango::SCALE));
    layout.set_font_description(Some(&fd));
    layout
}

fn draw_text(cr: &Context, layout: &pango::Layout, text: &str, x: f64, y: f64, c: (f64, f64, f64)) {
    layout.set_text(text);
    set_color(cr, c);
    cr.move_to(x, y);
    pangocairo::functions::show_layout(cr, layout);
}

fn draw_ring(cr: &Context, w: f64, h: f64, st: &mut ChartState) {
    let Some(tree) = st.tree.clone() else { return };
    let dark = st.dark;
    let cx = w / 2.0;
    let cy = h / 2.0;
    st.center = (cx, cy);
    let r_max = w.min(h) * 0.44;
    let r0 = r_max * 0.34;
    let thickness = (r_max - r0) / 2.1;
    let gap = 0.012;

    let mut kids: Vec<u32> = tree
        .child_ids(st.node)
        .filter(|&c| st.metric.value(&tree, c) > 0)
        .collect();
    kids.sort_by_key(|&c| std::cmp::Reverse(st.metric.value(&tree, c)));
    let sum_kids: u64 = kids.iter().map(|&c| st.metric.value(&tree, c)).sum();
    let own = st.metric.value(&tree, st.node);
    let denom = sum_kids.max(own);
    if denom == 0 {
        let layout = label_layout(cr, 13.0);
        let text = "nothing to show here";
        layout.set_text(text);
        let (tw, th) = layout.pixel_size();
        draw_text(
            cr,
            &layout,
            text,
            cx - tw as f64 / 2.0,
            cy - th as f64 / 2.0,
            muted(dark),
        );
        return;
    }
    let leftover = denom.saturating_sub(sum_kids);

    // centre disc: this directory and its own value for the metric
    cr.arc(cx, cy, r0, 0.0, std::f64::consts::TAU);
    let disc = if dark {
        (0.14, 0.14, 0.135)
    } else {
        (0.925, 0.925, 0.915)
    };
    set_color(cr, disc);
    cr.fill_preserve().unwrap();
    set_color(cr, mix(text_color(dark), 0.75, dark));
    cr.set_line_width(1.0);
    cr.stroke().unwrap();

    let layout = label_layout(cr, 12.0);
    let name = if st.node == tree.root {
        "/".to_string()
    } else {
        String::from_utf8_lossy(tree.name(st.node)).into_owned()
    };
    let short = if name.chars().count() > 18 {
        name.chars().take(17).collect::<String>() + "…"
    } else {
        name
    };
    let bold = label_layout(cr, 12.5);
    let (tw, th) = {
        bold.set_text(&short);
        bold.pixel_size()
    };
    draw_text(cr, &bold, &short, cx - tw as f64 / 2.0, cy - th as f64 - 1.0, text_color(dark));
    let val = human(own);
    layout.set_text(&val);
    let (vw, _) = layout.pixel_size();
    draw_text(cr, &layout, &val, cx - vw as f64 / 2.0, cy + 2.0, muted(dark));

    // first ring: the children
    let mut a = -std::f64::consts::FRAC_PI_2;
    let mut label_candidates: Vec<(u32, f64, f64, (f64, f64, f64), f64)> = Vec::new();
    for k in &kids {
        let id = *k;
        let v = st.metric.value(&tree, id);
        let span = v as f64 / denom as f64 * std::f64::consts::TAU;
        let a0 = a + gap / 2.0;
        let a1 = a + span - gap / 2.0;
        a += span;
        if a1 <= a0 {
            continue;
        }
        let color = *st.colors.get(&id).unwrap_or(&neutral(dark));
        let hovered = st.hover == Some(id) || st.selected == Some(id);
        ring_band(cr, cx, cy, r0, r0 + thickness, a0, a1, color, dark, hovered);
        st.hits.push(Hit {
            id,
            ring: Some((a0, a1, r0, r0 + thickness)),
            rect: None,
        });
        if span > 0.20 {
            label_candidates.push((id, a0, a1, color, span));
        }

        // second ring: the biggest grandchildren
        let mut sub: Vec<u32> = tree
            .child_ids(id)
            .filter(|&c| st.metric.value(&tree, c) > 0)
            .collect();
        if !sub.is_empty() {
            sub.sort_by_key(|&c| std::cmp::Reverse(st.metric.value(&tree, c)));
            let shown: Vec<u32> = sub.iter().take(12).cloned().collect();
            let shown_total: u64 = shown.iter().map(|&c| st.metric.value(&tree, c)).sum();
            if shown_total > 0 {
                let scale = (a1 - a0) / shown_total as f64;
                let mut b = a0;
                for c in shown {
                    let v2 = st.metric.value(&tree, c);
                    let s2 = v2 as f64 * scale;
                    let b0 = b + gap / 2.0;
                    let b1 = b + s2 - gap / 2.0;
                    b += s2;
                    if b1 <= b0 {
                        continue;
                    }
                    let c2 = mix(color, 0.34, dark);
                    let hovered = st.hover == Some(c) || st.selected == Some(c);
                    ring_band(cr, cx, cy, r0 + thickness, r0 + thickness * 2.0, b0, b1, c2, dark, hovered);
                    st.hits.push(Hit {
                        id: c,
                        ring: Some((b0, b1, r0 + thickness, r0 + thickness * 2.0)),
                        rect: None,
                    });
                }
            }
        }
    }

    // bytes no single child can account for (non-additive metrics)
    if leftover > 0 && leftover * 200 > denom {
        let span = leftover as f64 / denom as f64 * std::f64::consts::TAU;
        let color = neutral(dark);
        ring_band(cr, cx, cy, r0, r0 + thickness, a + gap / 2.0, a + span - gap / 2.0, color, dark, false);
        st.hits.push(Hit {
            id: u32::MAX,
            ring: Some((a, a + span, r0, r0 + thickness)),
            rect: None,
        });
    }

    // direct labels for the biggest wedges, with leader lines
    let mut placed: Vec<f64> = Vec::new();
    for (id, a0, a1, color, _) in label_candidates.iter().take(10) {
        let mid = (a0 + a1) / 2.0;
        let rr = r0 + thickness * 2.0 + 6.0;
        let x = cx + mid.cos() * rr;
        let y = cy + mid.sin() * rr;
        let right = mid.cos() >= 0.0;
        let lx = x + if right { 10.0 } else { -10.0 };
        let ly = y + if right { 8.0 } else { -8.0 };
        // skip labels that would collide
        if placed
            .iter()
            .any(|py| (py - y).abs() < 14.0 && (py > &0.0) == (y > 0.0))
        {
            continue;
        }
        placed.push(y);
        cr.set_line_width(1.0);
        set_color(cr, *color);
        cr.move_to(cx + mid.cos() * (r0 + thickness * 2.0), cy + mid.sin() * (r0 + thickness * 2.0));
        cr.line_to(x, y);
        cr.stroke().unwrap();
        let name = String::from_utf8_lossy(tree.name(*id)).into_owned();
        let short = if name.chars().count() > 20 {
            name.chars().take(19).collect::<String>() + "…"
        } else {
            name
        };
        let text = format!("{}  {}", short, human(st.metric.value(&tree, *id)));
        let layout = label_layout(cr, 11.0);
        layout.set_text(&text);
        let (tw, _) = layout.pixel_size();
        let tx = if right { lx } else { lx - tw as f64 };
        draw_text(cr, &layout, &text, tx, ly - 8.0, text_color(dark));
    }
}

#[allow(clippy::too_many_arguments)]
fn ring_band(
    cr: &Context,
    cx: f64,
    cy: f64,
    r0: f64,
    r1: f64,
    a0: f64,
    a1: f64,
    color: (f64, f64, f64),
    dark: bool,
    hovered: bool,
) {
    if a1 <= a0 {
        return;
    }
    cr.new_path();
    cr.arc(cx, cy, r1, a0, a1);
    cr.arc_negative(cx, cy, r0, a1, a0);
    cr.close_path();
    let c = if hovered { lighten(color) } else { color };
    set_color(cr, c);
    cr.fill_preserve().unwrap();
    set_color(cr, surface(dark));
    cr.set_line_width(1.5);
    cr.stroke().unwrap();
}

fn lighten(c: (f64, f64, f64)) -> (f64, f64, f64) {
    (
        (c.0 * 0.75 + 0.25).min(1.0),
        (c.1 * 0.75 + 0.25).min(1.0),
        (c.2 * 0.75 + 0.25).min(1.0),
    )
}

fn draw_treemap(cr: &Context, w: f64, h: f64, st: &mut ChartState) {
    let Some(tree) = st.tree.clone() else { return };
    let dark = st.dark;
    let mut kids: Vec<u32> = tree
        .child_ids(st.node)
        .filter(|&c| st.metric.value(&tree, c) > 0)
        .collect();
    kids.sort_by_key(|&c| std::cmp::Reverse(st.metric.value(&tree, c)));
    let total: u64 = kids.iter().map(|&c| st.metric.value(&tree, c)).sum();
    if total == 0 {
        return;
    }
    let rects = squarify(
        &kids
            .iter()
            .map(|&c| (c, st.metric.value(&tree, c) as f64))
            .collect::<Vec<_>>(),
        0.0,
        0.0,
        w,
        h,
    );
    let layout = label_layout(cr, 11.0);
    for (id, x, y, rw, rh) in rects {
        let color = *st.colors.get(&id).unwrap_or(&neutral(dark));
        let hovered = st.hover == Some(id) || st.selected == Some(id);
        set_color(cr, if hovered { lighten(color) } else { color });
        cr.rectangle(x, y, (rw - 2.0).max(0.0), (rh - 2.0).max(0.0));
        cr.fill().unwrap();
        st.hits.push(Hit {
            id,
            ring: None,
            rect: Some((x, y, rw, rh)),
        });
        if rw > 70.0 && rh > 26.0 {
            let name = String::from_utf8_lossy(tree.name(id)).into_owned();
            let short: String = name.chars().take((rw / 7.0) as usize).collect();
            draw_text(cr, &layout, &short, x + 5.0, y + 4.0, (1.0, 1.0, 1.0));
            if rh > 42.0 {
                let v = human(st.metric.value(&tree, id));
                draw_text(cr, &layout, &v, x + 5.0, y + 19.0, (1.0, 1.0, 1.0));
            }
        }
    }
}

/// Squarified treemap layout; returns (node, x, y, w, h).
fn squarify(items: &[(u32, f64)], x: f64, y: f64, w: f64, h: f64) -> Vec<(u32, f64, f64, f64, f64)> {
    let mut out = Vec::new();
    let mut rest: Vec<(u32, f64)> = items.to_vec();
    let total: f64 = rest.iter().map(|i| i.1).sum();
    if total <= 0.0 || w <= 1.0 || h <= 1.0 {
        return out;
    }
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    let mut scale = (w * h) / total;
    let mut row: Vec<(u32, f64)> = Vec::new();
    while !rest.is_empty() {
        let side = w.min(h);
        let cand: Vec<(u32, f64)> = row.iter().cloned().chain(rest.first().cloned()).collect();
        if row.is_empty() || worst(&cand, side, scale) <= worst(&row, side, scale) {
            row.push(rest.remove(0));
            continue;
        }
        let s: f64 = row.iter().map(|i| i.1 * scale).sum();
        if w >= h {
            let rw = s / h;
            let mut yy = y;
            for (id, v) in row.drain(..) {
                let rh = (v * scale) / rw.max(1e-6);
                out.push((id, x, yy, rw, rh));
                yy += rh;
            }
            x += rw;
            w -= rw;
        } else {
            let rh = s / w;
            let mut xx = x;
            for (id, v) in row.drain(..) {
                let rw = (v * scale) / rh.max(1e-6);
                out.push((id, xx, y, rw, rh));
                xx += rw;
            }
            y += rh;
            h -= rh;
        }
        let left: f64 = rest.iter().map(|i| i.1).sum();
        scale = (w * h) / left.max(1.0);
        if w <= 1.0 || h <= 1.0 {
            break;
        }
    }
    if !row.is_empty() && w > 1.0 && h > 1.0 {
        let s: f64 = row.iter().map(|i| i.1 * scale).sum();
        if w >= h {
            let rw = (s / h).min(w);
            let mut yy = y;
            for (id, v) in row.drain(..) {
                let rh = (v * scale) / rw.max(1e-6);
                out.push((id, x, yy, rw, rh));
                yy += rh;
            }
        } else {
            let rh = (s / w).min(h);
            let mut xx = x;
            for (id, v) in row.drain(..) {
                let rw = (v * scale) / rh.max(1e-6);
                out.push((id, xx, y, rw, rh));
                xx += rw;
            }
        }
    }
    out
}

fn worst(row: &[(u32, f64)], side: f64, scale: f64) -> f64 {
    let s: f64 = row.iter().map(|i| i.1 * scale).sum();
    if s <= 0.0 {
        return f64::INFINITY;
    }
    let mut mx: f64 = 0.0;
    let mut mn = f64::INFINITY;
    for i in row {
        let l = i.1 * scale;
        mx = mx.max(l);
        mn = mn.min(l);
    }
    (side * side * mx / (s * s)).max(s * s / (side * side * mn))
}

fn hit_test(st: &ChartState, x: f64, y: f64) -> Option<u32> {
    // wedges first: the ring chart is the dense target
    let (cx, cy) = st.center;
    let dx = x - cx;
    let dy = y - cy;
    let r = (dx * dx + dy * dy).sqrt();
    let mut ang = dy.atan2(dx);
    if ang < -std::f64::consts::FRAC_PI_2 {
        ang += std::f64::consts::TAU;
    }
    for h in &st.hits {
        if let Some((a0, a1, r0, r1)) = h.ring {
            if r >= r0 && r <= r1 && ang >= a0 && ang <= a1 {
                return if h.id == u32::MAX { None } else { Some(h.id) };
            }
        }
    }
    for h in &st.hits {
        if let Some((rx, ry, rw, rh)) = h.rect {
            if x >= rx && x <= rx + rw && y >= ry && y <= ry + rh {
                return Some(h.id);
            }
        }
    }
    None
}
