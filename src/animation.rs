//! The transcribing animation: a 90s demoscene take on the Omacon look.
//!
//! A synthwave grid scrolls towards you under a setting, striped neon sun, with
//! a decrypting, glitching title, a segmented progress bar, the last lines of
//! transcript typing themselves out, and CRT scanlines plus stepped film grain
//! on top.
//!
//! The colours come from omacom/omacon-site.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::cairo::{self, Context, Filter, Format, ImageSurface, Operator, SurfacePattern};
use gtk::prelude::*;

type Rgb = (f64, f64, f64);

const fn rgb(r: u8, g: u8, b: u8) -> Rgb {
    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

// The Omacon colours, used when there is no Omarchy theme. With a theme the
// scene takes its background, text, accent and cyan from it.
const OMACON_DARK: Rgb = rgb(13, 8, 38);
const OMACON_LIGHT: Rgb = rgb(248, 245, 242);
const OMACON_PINK: Rgb = rgb(255, 138, 255);
const OMACON_CYAN: Rgb = rgb(80, 220, 255);

fn dark() -> Rgb {
    crate::theme::color("darker_background", OMACON_DARK)
}

fn light() -> Rgb {
    crate::theme::color(
        "bright_foreground",
        crate::theme::color("foreground", OMACON_LIGHT),
    )
}

fn neon_color() -> Rgb {
    crate::theme::color("accent", OMACON_PINK)
}

fn neon_dark() -> Rgb {
    crate::theme::mix(neon_color(), dark(), 0.25)
}

fn neon_light() -> Rgb {
    crate::theme::mix(neon_color(), light(), 0.45)
}

fn glitch() -> Rgb {
    crate::theme::color("cyan", OMACON_CYAN)
}

const TITLE: &str = "TRANSCRIBING";
const SCRAMBLE: &[u8] = b"#%&*+=<>/\\|01$@?!";
const FONT: &str = "JetBrains Mono";
const SEGMENTS: usize = 28;
/// Transcript lines visible under the bar, and their height in font sizes.
/// At most this many transcript lines; fewer when the window is low.
const ROWS: usize = 6;
const ROW: f64 = 1.55;
/// The transcript lines are set smaller than the stage line, so more of them fit.
const TEXT_SCALE: f64 = 0.8;

/// The same cubic in-out the Omacon header uses.
fn ease(t: f64) -> f64 {
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

/// Cheap deterministic noise: the same input gives the same value every frame.
fn hash(n: u64) -> f64 {
    let mut x = n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 31;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    (x >> 11) as f64 / (1u64 << 53) as f64
}

#[derive(Default)]
struct Model {
    progress: f64,
    shown_progress: f64,
    stage: String,
    lines: Vec<String>,
    line_started: f64,
    running: bool,
    start_time: Option<i64>,
    now: f64,
    tick: Option<gtk::TickCallbackId>,
    /// When the last frame was drawn, in frame-clock microseconds.
    last_frame: i64,
    grain: Vec<ImageSurface>,
    /// The vignette never moves: drawn once per size, then copied.
    vignette: Option<ImageSurface>,
}

#[derive(Clone)]
pub struct TranscribeAnimation {
    area: gtk::DrawingArea,
    model: Rc<RefCell<Model>>,
}

impl Default for TranscribeAnimation {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscribeAnimation {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::builder()
            .content_height(220)
            .hexpand(true)
            .vexpand(true)
            .build();
        let model: Rc<RefCell<Model>> = Rc::default();
        model.borrow_mut().grain = (0..3).map(|i| grain(i as u64)).collect();

        let drawing = model.clone();
        area.set_draw_func(move |_, cr, width, height| {
            let mut m = drawing.borrow_mut();
            draw(cr, f64::from(width), f64::from(height), &mut m);
        });
        TranscribeAnimation { area, model }
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Overall progress from 0.0 to 1.0.
    pub fn set_progress(&self, progress: f64) {
        self.model.borrow_mut().progress = progress.clamp(0.0, 1.0);
    }

    /// A short label for what is happening, e.g. "Loading model" or "Transcribing you".
    pub fn set_stage(&self, stage: &str) {
        self.model.borrow_mut().stage = stage.to_owned();
    }

    /// A freshly transcribed line; it types itself out.
    pub fn push_text(&self, line: &str) {
        let mut m = self.model.borrow_mut();
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        m.lines.push(line.to_owned());
        m.line_started = m.now;
    }

    /// Clears the lines and the progress, for a new run.
    pub fn reset(&self) {
        let mut m = self.model.borrow_mut();
        m.lines.clear();
        m.progress = 0.0;
        m.shown_progress = 0.0;
        m.stage.clear();
    }

    /// Starts or stops the frame clock; stopped, it costs nothing.
    pub fn set_running(&self, running: bool) {
        let mut m = self.model.borrow_mut();
        if running == m.running {
            return;
        }
        m.running = running;
        if running {
            m.start_time = None;
            let model = self.model.clone();
            let id = self.area.add_tick_callback(move |area, clock| {
                let mut m = model.borrow_mut();
                let frame = clock.frame_time();
                let start = *m.start_time.get_or_insert(frame);
                m.now = (frame - start) as f64 / 1_000_000.0;
                // Ease the bar towards the real value instead of jumping.
                m.shown_progress += (m.progress - m.shown_progress) * 0.08;
                if frame - m.last_frame < FRAME_INTERVAL_US {
                    return glib::ControlFlow::Continue;
                }
                m.last_frame = frame;
                drop(m);
                area.queue_draw();
                glib::ControlFlow::Continue
            });
            m.tick = Some(id);
        } else if let Some(id) = m.tick.take() {
            id.remove();
        }
    }
}

use gtk::glib;

/// A tile of film grain; three of them are cycled like the site's `steps(3)`.
fn grain(seed: u64) -> ImageSurface {
    let (w, h) = (192, 120);
    let mut surface = ImageSurface::create(Format::ARgb32, w, h).expect("grain surface");
    let stride = surface.stride() as usize;
    {
        let mut data = surface.data().expect("grain data");
        for y in 0..h as usize {
            for x in 0..w as usize {
                let n = hash(seed * 1_000_003 + (y * w as usize + x) as u64);
                let v = (n * 255.0) as u8;
                let a = (hash(seed * 7_919 + (x * 31 + y * 17) as u64) * 34.0) as u8;
                // Premultiplied BGRA.
                let p = (u16::from(v) * u16::from(a) / 255) as u8;
                let i = y * stride + x * 4;
                data[i..i + 4].copy_from_slice(&[p, p, p, a]);
            }
        }
    }
    surface
}

/// 30 frames a second: the scene moves slowly, and every frame is drawn on the CPU.
const FRAME_INTERVAL_US: i64 = 1_000_000 / 30;

fn set(cr: &Context, c: Rgb, alpha: f64) {
    cr.set_source_rgba(c.0, c.1, c.2, alpha);
}

fn draw(cr: &Context, w: f64, h: f64, m: &mut Model) {
    let t = m.now;
    let horizon = (h * 0.6).round();

    set(cr, dark(), 1.0);
    let _ = cr.paint();

    // Everything neon is drawn small and blown up for the bloom, a tight pass
    // and a wide one, and then once more crisp on top.
    bloom(cr, w, h, |g| neon(g, w, h, horizon, t));
    neon(cr, w, h, horizon, t);

    title(cr, w, h, t);
    hud(cr, w, h, horizon, m);
    crt(cr, w, h, t, m);
}

/// Darkens the edges; drawn once per window size.
fn vignette(w: f64, h: f64) -> Option<ImageSurface> {
    let surface = ImageSurface::create(Format::ARgb32, w.ceil() as i32, h.ceil() as i32).ok()?;
    let cr = Context::new(&surface).ok()?;
    let vignette =
        cairo::RadialGradient::new(w / 2.0, h / 2.0, h * 0.3, w / 2.0, h / 2.0, w.max(h) * 0.75);
    vignette.add_color_stop_rgba(0.0, 0.0, 0.0, 0.0, 0.0);
    vignette.add_color_stop_rgba(1.0, 0.0, 0.0, 0.0, 0.55);
    cr.set_source(&vignette).ok();
    let _ = cr.paint();
    drop(cr);
    Some(surface)
}

/// Renders `paint` at 1/`scale` into a new surface.
fn shrunk(w: f64, h: f64, scale: f64, paint: &impl Fn(&Context)) -> Option<ImageSurface> {
    let small = ImageSurface::create(
        Format::ARgb32,
        (w / scale).ceil() as i32,
        (h / scale).ceil() as i32,
    )
    .ok()?;
    let g = Context::new(&small).ok()?;
    g.scale(1.0 / scale, 1.0 / scale);
    paint(&g);
    drop(g);
    Some(small)
}

/// Adds `small`, scaled up by `scale`, onto `cr`.
fn add_scaled(cr: &Context, small: &ImageSurface, scale: f64, alpha: f64, filter: Filter) {
    let _ = cr.save();
    cr.scale(scale, scale);
    let pattern = SurfacePattern::create(small);
    pattern.set_filter(filter);
    cr.set_source(&pattern).ok();
    cr.set_operator(Operator::Add);
    let _ = cr.paint_with_alpha(alpha);
    let _ = cr.restore();
}

/// The glow: `paint` rendered at 1/6 and at 1/16 and blown back up, which is a
/// cheap blur. Smooth scaling costs per pixel it writes, so the two passes are
/// combined while small, smoothed up to half the window and only then doubled,
/// without smoothing, to the full size: the glow is soft enough not to show it.
fn bloom(cr: &Context, w: f64, h: f64, paint: impl Fn(&Context)) {
    let (Some(tight), Some(wide)) = (shrunk(w, h, 6.0, &paint), shrunk(w, h, 16.0, &paint)) else {
        return;
    };
    let Ok(glow) = ImageSurface::create(Format::ARgb32, tight.width(), tight.height()) else {
        return;
    };
    let Ok(g) = Context::new(&glow) else { return };
    g.set_source_surface(&tight, 0.0, 0.0).ok();
    let _ = g.paint_with_alpha(0.7);
    add_scaled(&g, &wide, 16.0 / 6.0, 0.9, Filter::Good);
    drop(g);
    let Ok(half) = ImageSurface::create(
        Format::ARgb32,
        (w / 2.0).ceil() as i32,
        (h / 2.0).ceil() as i32,
    ) else {
        return;
    };
    let Ok(g) = Context::new(&half) else { return };
    add_scaled(&g, &glow, 3.0, 1.0, Filter::Good);
    drop(g);
    add_scaled(cr, &half, 2.0, 1.0, Filter::Nearest);
}

/// Sky glow, the sun of shapes and the grid: the parts that bloom.
fn neon(cr: &Context, w: f64, h: f64, horizon: f64, t: f64) {
    // A haze sitting on the horizon.
    let haze = cairo::LinearGradient::new(0.0, horizon - h * 0.35, 0.0, horizon);
    haze.add_color_stop_rgba(0.0, neon_dark().0, neon_dark().1, neon_dark().2, 0.0);
    haze.add_color_stop_rgba(1.0, neon_dark().0, neon_dark().1, neon_dark().2, 0.22);
    cr.set_source(&haze).ok();
    cr.rectangle(0.0, horizon - h * 0.35, w, h * 0.35);
    let _ = cr.fill();

    sun(cr, w, horizon, t);
    grid(cr, w, h, horizon, t);
}

fn sun(cr: &Context, w: f64, horizon: f64, t: f64) {
    // A big sun, half set, breathing a little.
    let r = (w * 0.3).min(horizon * 0.62) * (1.0 + 0.012 * (t * 2.4).sin());
    let (cx, cy) = (w / 2.0, horizon - r * 0.3);

    let _ = cr.save();
    cr.rectangle(0.0, 0.0, w, horizon);
    cr.clip();
    cr.push_group();

    let fill = cairo::LinearGradient::new(0.0, cy - r, 0.0, cy + r);
    fill.add_color_stop_rgb(0.0, neon_light().0, neon_light().1, neon_light().2);
    fill.add_color_stop_rgb(0.55, neon_color().0, neon_color().1, neon_color().2);
    fill.add_color_stop_rgb(1.0, neon_dark().0, neon_dark().1, neon_dark().2);
    cr.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
    cr.set_source(&fill).ok();
    let _ = cr.fill();

    // Sunset stripes: bands cut out of the lower half, sliding down and widening.
    cr.set_operator(Operator::Clear);
    let top = cy - r * 0.15;
    let span = horizon - top;
    for i in 0..8 {
        let f = ((i as f64 + (t * 0.35) % 1.0) / 8.0).min(1.0);
        let y = top + span * f;
        let band = 1.0 + r * 0.075 * f;
        cr.rectangle(0.0, y, w, band);
    }
    let _ = cr.fill();

    let _ = cr.pop_group_to_source();
    let _ = cr.paint();
    let _ = cr.restore();
}

fn grid(cr: &Context, w: f64, h: f64, horizon: f64, t: f64) {
    let depth = h - horizon;
    let floor = cairo::LinearGradient::new(0.0, horizon, 0.0, h);
    floor.add_color_stop_rgba(
        0.0,
        neon_dark().0 * 0.35,
        neon_dark().1 * 0.2,
        neon_dark().2 * 0.45,
        1.0,
    );
    floor.add_color_stop_rgba(1.0, dark().0, dark().1, dark().2, 1.0);
    cr.set_source(&floor).ok();
    cr.rectangle(0.0, horizon, w, depth);
    let _ = cr.fill();

    cr.set_line_width(1.2);
    // Rows come towards you; 1/d spacing gives the perspective.
    let scroll = (t * 0.9) % 1.0;
    for i in 0..18 {
        let d = i as f64 + 1.0 - scroll;
        if d < 0.35 {
            continue;
        }
        let y = horizon + depth / d.max(1.0) * d.min(1.0);
        let alpha = (1.0 / d).clamp(0.08, 0.9);
        set(cr, neon_color(), alpha);
        cr.move_to(0.0, y.round() + 0.5);
        cr.line_to(w, y.round() + 0.5);
        let _ = cr.stroke();
    }
    // Columns fan out from the vanishing point.
    let vx = w / 2.0;
    for i in -14..=14 {
        let x = vx + i as f64 * w * 0.11;
        set(cr, neon_color(), 0.45);
        cr.move_to(vx + i as f64 * w * 0.012, horizon);
        cr.line_to(x, h);
        let _ = cr.stroke();
    }
    // The horizon itself burns brightest.
    set(cr, neon_light(), 0.95);
    cr.set_line_width(2.0);
    cr.move_to(0.0, horizon);
    cr.line_to(w, horizon);
    let _ = cr.stroke();
}

fn font(cr: &Context, size: f64, bold: bool) {
    cr.select_font_face(
        FONT,
        cairo::FontSlant::Normal,
        if bold {
            cairo::FontWeight::Bold
        } else {
            cairo::FontWeight::Normal
        },
    );
    cr.set_font_size(size);
}

/// "TRANSCRIBING" decrypts from noise, then glitches now and then.
fn title(cr: &Context, w: f64, h: f64, t: f64) {
    // Fit the width too: the title is 12 wide letters plus spacing.
    let size = (h * 0.12).min(w / 11.5).clamp(12.0, 64.0);
    font(cr, size, true);
    let cycle = t % 8.0;
    let tick = (t * 24.0) as u64;
    let text: String = TITLE
        .chars()
        .enumerate()
        .map(|(i, c)| {
            let reveal = 0.25 + i as f64 * 0.07;
            if cycle >= reveal {
                c
            } else {
                SCRAMBLE[(hash(tick * 131 + i as u64) * SCRAMBLE.len() as f64) as usize] as char
            }
        })
        .collect();

    // Letter spacing by hand: the toy text API has none.
    let spacing = size * 0.18;
    let advance = cr
        .text_extents("M")
        .map(|e| e.x_advance())
        .unwrap_or(size * 0.6)
        + spacing;
    let total = advance * TITLE.len() as f64 - spacing;
    let x0 = (w - total) / 2.0;
    let y = h * 0.2 + size * 0.35;

    // A glitch burst: a few frames where the colour channels split and a slice jumps.
    let burst = (t * 3.0) as u64;
    let glitching = hash(burst * 977) > 0.82 && (t * 3.0) % 1.0 < 0.35;
    let split = if glitching { size * 0.08 } else { size * 0.025 };
    let slice_y = y - size * hash(tick) * 0.7;
    let slice_dx = if glitching {
        (hash(tick + 5) - 0.5) * size * 0.8
    } else {
        0.0
    };

    let paint = |cr: &Context, dx: f64, color: Rgb, alpha: f64| {
        set(cr, color, alpha);
        for (i, c) in text.chars().enumerate() {
            cr.move_to(x0 + i as f64 * advance + dx, y);
            let _ = cr.show_text(&c.to_string());
        }
    };

    let _ = cr.save();
    cr.set_operator(Operator::Add);
    paint(cr, -split, glitch(), 0.55);
    paint(cr, split, neon_dark(), 0.75);
    let _ = cr.restore();
    // Soft glow, then the crisp letters.
    cr.set_line_join(cairo::LineJoin::Round);
    cr.set_line_cap(cairo::LineCap::Round);
    for (width, alpha) in [(size * 0.28, 0.07), (size * 0.14, 0.12)] {
        cr.set_line_width(width);
        set(cr, neon_color(), alpha);
        for (i, c) in text.chars().enumerate() {
            cr.move_to(x0 + i as f64 * advance, y);
            cr.text_path(&c.to_string());
        }
        let _ = cr.stroke();
    }
    paint(cr, 0.0, light(), 1.0);

    if glitching {
        let _ = cr.save();
        cr.rectangle(0.0, slice_y, w, size * 0.18);
        cr.clip();
        set(cr, dark(), 1.0);
        let _ = cr.paint();
        paint(cr, slice_dx, neon_light(), 1.0);
        let _ = cr.restore();
    }
}

/// Stage, percentage, the segmented bar and the line being typed.
fn hud(cr: &Context, w: f64, h: f64, horizon: f64, m: &Model) {
    let t = m.now;
    let pad = (w * 0.06).max(12.0);
    let size = (h * 0.052).clamp(9.0, 18.0);
    // As many transcript lines as fit below the horizon, at least two.
    let room = h - horizon - 8.0 - pad * 0.5 - size * 3.8;
    let rows = ((room / (size * ROW * TEXT_SCALE)).floor() as usize).clamp(2, ROWS);
    let panel_h = size * (3.8 + rows as f64 * ROW * TEXT_SCALE);
    let panel_top = (h - panel_h - pad * 0.5).max(horizon + 8.0);

    // A dark glass panel so the text stays readable over the grid.
    set(cr, dark(), 0.72);
    rounded(cr, pad * 0.6, panel_top, w - pad * 1.2, panel_h, 6.0);
    let _ = cr.fill_preserve();
    set(cr, neon_color(), 0.35);
    cr.set_line_width(1.0);
    let _ = cr.stroke();

    font(cr, size, true);
    let line1 = panel_top + size * 1.5;
    let stage = if m.stage.is_empty() {
        "Warming up"
    } else {
        m.stage.as_str()
    };
    set(cr, neon_light(), 1.0);
    cr.move_to(pad, line1);
    let _ = cr.show_text(&format!("> {}", stage.to_uppercase()));
    let pct = format!("{:>3.0}%", m.shown_progress * 100.0);
    let pct_w = cr.text_extents(&pct).map(|e| e.x_advance()).unwrap_or(0.0);
    cr.move_to(w - pad - pct_w, line1);
    let _ = cr.show_text(&pct);

    // Segmented bar, the leading segment blinks.
    let bar_y = line1 + size * 0.7;
    let bar_h = size * 0.9;
    let gap = 3.0;
    let seg_w = ((w - pad * 2.0) - gap * (SEGMENTS as f64 - 1.0)) / SEGMENTS as f64;
    let filled = m.shown_progress * SEGMENTS as f64;
    for i in 0..SEGMENTS {
        let x = pad + i as f64 * (seg_w + gap);
        let amount = (filled - i as f64).clamp(0.0, 1.0);
        cr.rectangle(x, bar_y, seg_w, bar_h);
        if amount >= 1.0 {
            set(cr, neon_color(), 0.95);
        } else if amount > 0.0 || i == filled as usize {
            let blink = if (t * 3.0) % 1.0 < 0.5 { 0.85 } else { 0.25 };
            set(cr, neon_light(), blink);
        } else {
            set(cr, neon_dark(), 0.18);
        }
        let _ = cr.fill();
    }

    // The last few lines of transcript: older ones fade and slide up when a new
    // one arrives, the newest types itself out with a block cursor.
    let size = size * TEXT_SCALE;
    font(cr, size, false);
    let row_h = size * ROW;
    let area_top = bar_y + bar_h + size * 0.6;
    let base = area_top + row_h * rows as f64 - size * 0.45;
    let age = t - m.line_started;
    let slide = row_h * (1.0 - ease((age / 0.35).clamp(0.0, 1.0)));
    let max_w = w - pad * 2.0 - size;

    let _ = cr.save();
    cr.rectangle(0.0, area_top, w, row_h * rows as f64 + size * 0.2);
    cr.clip();
    let count = m.lines.len();
    for back in (0..rows.min(count)).rev() {
        let line = &m.lines[count - 1 - back];
        let y = base - back as f64 * row_h + slide;
        if back == 0 {
            let typed: String = line.chars().take((age * 45.0).max(0.0) as usize).collect();
            let shown = fit_tail(cr, &format!("\u{201c}{typed}"), max_w);
            set(cr, light(), 0.95);
            cr.move_to(pad, y);
            let _ = cr.show_text(&shown);
            let end_x = pad
                + cr.text_extents(&shown)
                    .map(|e| e.x_advance())
                    .unwrap_or(0.0);
            if (t * 2.0) % 1.0 < 0.55 {
                set(cr, neon_color(), 1.0);
                cr.rectangle(end_x + 2.0, y - size * 0.8, size * 0.55, size);
                let _ = cr.fill();
            }
        } else {
            let shown = fit_head(cr, &format!("\u{201c}{line}\u{201d}"), max_w);
            set(cr, neon_light(), 0.72 - back as f64 * 0.11);
            cr.move_to(pad, y);
            let _ = cr.show_text(&shown);
        }
    }
    let _ = cr.restore();
}

/// Keeps the start of `text` that fits in `max_w`, with an ellipsis at the end.
fn fit_head(cr: &Context, text: &str, max_w: f64) -> String {
    let width = |s: &str| cr.text_extents(s).map(|e| e.x_advance()).unwrap_or(0.0);
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for end in (1..chars.len()).rev() {
        let candidate: String = chars[..end]
            .iter()
            .copied()
            .chain(std::iter::once('\u{2026}'))
            .collect();
        if width(&candidate) <= max_w {
            return candidate;
        }
    }
    String::new()
}

/// Keeps the end of `text` that fits in `max_w`, with an ellipsis in front.
fn fit_tail(cr: &Context, text: &str, max_w: f64) -> String {
    let width = |s: &str| cr.text_extents(s).map(|e| e.x_advance()).unwrap_or(0.0);
    if width(text) <= max_w {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for start in 1..chars.len() {
        let candidate: String = std::iter::once('\u{2026}')
            .chain(chars[start..].iter().copied())
            .collect();
        if width(&candidate) <= max_w {
            return candidate;
        }
    }
    String::new()
}

fn rounded(cr: &Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::{FRAC_PI_2, PI};
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -FRAC_PI_2, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, FRAC_PI_2);
    cr.arc(x + r, y + h - r, r, FRAC_PI_2, PI);
    cr.arc(x + r, y + r, r, PI, PI + FRAC_PI_2);
    cr.close_path();
}

/// Scanlines, stepped grain, a rolling bar, vignette and a faint flicker.
fn crt(cr: &Context, w: f64, h: f64, t: f64, m: &mut Model) {
    set(cr, (0.0, 0.0, 0.0), 0.16);
    let mut y = 0.0;
    while y < h {
        cr.rectangle(0.0, y, w, 1.0);
        y += 3.0;
    }
    let _ = cr.fill();

    // Like the site's `animation: noise 0.5s steps(3)`: three frames, jumping around.
    let step = (t * 6.0) as usize;
    if let Some(tile) = m.grain.get(step % m.grain.len().max(1)) {
        let pattern = SurfacePattern::create(tile);
        pattern.set_extend(cairo::Extend::Repeat);
        pattern.set_filter(Filter::Nearest);
        let mut matrix = cairo::Matrix::identity();
        matrix.scale(0.5, 0.5);
        matrix.translate(-hash(step as u64) * 192.0, -hash(step as u64 + 9) * 120.0);
        pattern.set_matrix(matrix);
        cr.set_source(&pattern).ok();
        let _ = cr.paint();
    }

    // A slow bright band rolling down, like a badly synced CRT.
    let roll = (t * 0.25) % 1.4 - 0.2;
    let band = cairo::LinearGradient::new(0.0, (roll - 0.08) * h, 0.0, (roll + 0.08) * h);
    band.add_color_stop_rgba(0.0, 1.0, 1.0, 1.0, 0.0);
    band.add_color_stop_rgba(0.5, 1.0, 1.0, 1.0, 0.035);
    band.add_color_stop_rgba(1.0, 1.0, 1.0, 1.0, 0.0);
    cr.set_source(&band).ok();
    let _ = cr.paint();

    let size = (w.ceil() as i32, h.ceil() as i32);
    if m.vignette.as_ref().map(|v| (v.width(), v.height())) != Some(size) {
        m.vignette = vignette(w, h);
    }
    if let Some(vignette) = &m.vignette {
        cr.set_source_surface(vignette, 0.0, 0.0).ok();
        let _ = cr.paint();
    }

    let flicker = 0.02 * hash((t * 30.0) as u64);
    set(cr, dark(), flicker);
    let _ = cr.paint();
}
