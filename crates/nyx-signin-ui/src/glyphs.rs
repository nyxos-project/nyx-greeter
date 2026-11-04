//! The indicator's glyphs, drawn rather than taken from an icon font that may not be
//! installed: a face, a fingerprint, a key, a padlock, a check, a cross and an hourglass.

use crate::{Kind, Phase};
use gtk::{cairo, gdk};
use std::f64::consts::PI;

/// Draw the glyph for `kind` in `phase`, centred at (`cx`, `cy`), `r` in radius,
/// at `alpha`.
#[allow(clippy::too_many_arguments)]
pub fn draw(cr: &cairo::Context, kind: Kind, phase: Phase, cx: f64, cy: f64, r: f64, c: &gdk::RGBA, alpha: f64) {
    cr.save().ok();
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64 * alpha);
    cr.set_line_width((r * 0.13).max(1.6));
    cr.set_line_cap(cairo::LineCap::Round);
    cr.set_line_join(cairo::LineJoin::Round);
    match phase {
        Phase::Success => {
            cr.move_to(cx - r * 0.6, cy + r * 0.05);
            cr.line_to(cx - r * 0.15, cy + r * 0.5);
            cr.line_to(cx + r * 0.65, cy - r * 0.45);
            let _ = cr.stroke();
        }
        Phase::Failure => {
            let d = r * 0.5;
            cr.move_to(cx - d, cy - d);
            cr.line_to(cx + d, cy + d);
            cr.move_to(cx + d, cy - d);
            cr.line_to(cx - d, cy + d);
            let _ = cr.stroke();
        }
        Phase::Timeout => {
            let (w, h) = (r * 0.55, r * 0.8);
            cr.move_to(cx - w, cy - h);
            cr.line_to(cx + w, cy - h);
            cr.line_to(cx - w, cy + h);
            cr.line_to(cx + w, cy + h);
            cr.close_path();
            let _ = cr.stroke();
        }
        _ => match kind {
            Kind::Face => face(cr, cx, cy, r),
            Kind::Fingerprint => fingerprint(cr, cx, cy, r),
            Kind::Key => key(cr, cx, cy, r),
            Kind::Password => padlock(cr, cx, cy, r),
        },
    }
    cr.restore().ok();
}

fn face(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    // Scan corners around a face: two eyes and a smile.
    let k = r * 0.95;
    let l = r * 0.35;
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let (x, y) = (cx + sx * k, cy + sy * k);
        cr.move_to(x, y - sy * l);
        cr.line_to(x, y);
        cr.line_to(x - sx * l, y);
    }
    let _ = cr.stroke();
    for sx in [-1.0, 1.0] {
        cr.arc(cx + sx * r * 0.3, cy - r * 0.2, r * 0.07, 0.0, 2.0 * PI);
        let _ = cr.fill();
    }
    cr.arc(cx, cy + r * 0.05, r * 0.35, 0.2 * PI, 0.8 * PI);
    let _ = cr.stroke();
}

fn fingerprint(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    // Nested arcs, open at the bottom, as ridges.
    for i in 0..4 {
        let rr = r * (0.25 + 0.25 * i as f64);
        let gap = 0.18 + 0.05 * i as f64;
        cr.new_sub_path();
        cr.arc(cx, cy + r * 0.1, rr, PI * (0.5 + gap), PI * (2.5 - gap));
        let _ = cr.stroke();
    }
}

fn key(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    let hx = cx - r * 0.35;
    cr.arc(hx, cy, r * 0.38, 0.0, 2.0 * PI);
    let _ = cr.stroke();
    cr.move_to(hx + r * 0.38, cy);
    cr.line_to(cx + r * 0.85, cy);
    cr.move_to(cx + r * 0.55, cy);
    cr.line_to(cx + r * 0.55, cy + r * 0.3);
    cr.move_to(cx + r * 0.8, cy);
    cr.line_to(cx + r * 0.8, cy + r * 0.25);
    let _ = cr.stroke();
}

fn padlock(cr: &cairo::Context, cx: f64, cy: f64, r: f64) {
    let (w, h) = (r * 1.2, r * 0.9);
    let (x, y) = (cx - w / 2.0, cy - r * 0.05);
    cr.new_sub_path();
    cr.rectangle(x, y, w, h);
    let _ = cr.stroke();
    cr.arc(cx, y, r * 0.35, PI, 2.0 * PI);
    let _ = cr.stroke();
    cr.arc(cx, y + h * 0.45, r * 0.08, 0.0, 2.0 * PI);
    let _ = cr.fill();
}
