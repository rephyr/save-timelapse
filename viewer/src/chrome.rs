//! The viewer's on-screen chrome: the surface switcher, the clock, the
//! playback controls, and the keyboard panel behind `?`.
//!
//! Split out of main.rs because every control here needs its geometry twice,
//! once to draw it and once to hit-test a click on it. Keeping both in one
//! place is what stops a button drifting away from the region that activates
//! it. `Chrome::layout` computes every rect once per frame, and `draw` and
//! `hit` only ever read what it produced.
//!
//! The guiding rule for what belongs on screen at all: an element earns its
//! place by answering where am I, when am I, or what can I do. Anything that
//! answers "how is the renderer doing" lives behind `F3` instead.

use macroquad::prelude::*;

use crate::camera::Timeline;

/// Chrome text meant to be read.
const INK: Color = Color::new(1.0, 1.0, 1.0, 1.0);
/// Chrome text that is present without asking for attention. Used where
/// there is a fill behind it, or a quiet corner around it.
const INK_DIM: Color = Color::new(1.0, 1.0, 1.0, 0.55);
/// The same idea for an inactive surface chip, which has neither.
///
/// Brighter than `INK_DIM` because it is the one dim thing painted straight
/// onto the factory: over open ground 55% reads fine, and over a screen full
/// of machine icons it starts to disappear. This is as far as it can go while
/// still losing clearly to the active chip beside it.
const INK_CHIP: Color = Color::new(1.0, 1.0, 1.0, 0.72);
/// An inactive control under the cursor, part way to `INK`.
const INK_HOVER: Color = Color::new(1.0, 1.0, 1.0, 0.85);

/// Fill behind the active surface chip, the key panel, and any hovered
/// control.
///
/// Nearly opaque on purpose. Chrome is painted straight onto the rendered
/// world, which is dark grass in one place and a white space platform in the
/// next. A fill that lets much of that through stops being a background and
/// becomes a tint, which is exactly the failure that makes brightness alone
/// useless as the "this one is active" signal.
const SURFACE: Color = Color::new(0.09, 0.10, 0.13, 0.92);
/// The same surface at the weight used for a hover, which should read as a
/// control waking up rather than as a second active chip.
const SURFACE_HOVER: Color = Color::new(0.09, 0.10, 0.13, 0.55);
/// Hairline around filled chrome, so a dark pill still has an edge when it
/// happens to sit on dark ground.
const EDGE: Color = Color::new(1.0, 1.0, 1.0, 0.14);

/// Window margin for everything anchored to a corner.
const MARGIN: f32 = 16.0;

const CHIP_TEXT: f32 = 18.0;
const CHIP_PAD_X: f32 = 13.0;
const CHIP_HEIGHT: f32 = 32.0;
const CHIP_GAP: f32 = 6.0;

const CLOCK_TEXT: f32 = 21.0;
const COUNT_TEXT: f32 = 14.0;

/// Diameter of a round control.
const BUTTON: f32 = 34.0;
/// The primary action is visibly larger than the ones either side of it, so
/// the cluster has an obvious middle to aim at.
const PLAY_BUTTON: f32 = 42.0;
const BUTTON_GAP: f32 = 8.0;
/// Clearance between the outermost control and the scrub bar it sits beside.
const GUTTER_PAD: f32 = 14.0;

const PANEL_TEXT: f32 = 17.0;
const PANEL_HEADING: f32 = 14.0;
const PANEL_PAD: f32 = 26.0;
const PANEL_ROW: f32 = 26.0;
const PANEL_COLUMN_GAP: f32 = 34.0;

/// Fonts to try, best first.
///
/// A file beside the executable wins, so a release can ship one exact face
/// without this code changing. Failing that the platform's own UI font, which
/// is already on every machine and carries no redistribution obligation.
/// Failing that `None`, which leaves macroquad's built-in ProggyClean: a
/// bitmap face from the early 2000s that works and looks it.
fn font_candidates() -> Vec<std::path::PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            paths.push(dir.join("ui-font.ttf"));
        }
    }
    paths.push(std::path::PathBuf::from("assets/ui-font.ttf"));
    if cfg!(windows) {
        paths.push(std::path::PathBuf::from(r"C:\Windows\Fonts\segoeui.ttf"));
    } else {
        for path in [
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/System/Library/Fonts/Helvetica.ttc",
        ] {
            paths.push(std::path::PathBuf::from(path));
        }
    }
    paths
}

/// The font and the two panels that are toggled rather than always drawn.
///
/// Held for the whole run because a `Font` is a GPU glyph atlas, not a value
/// worth rebuilding per frame.
pub struct Ui {
    font: Option<Font>,
    /// The `?` panel. Opens itself once on a first ever run and is otherwise
    /// entirely on request.
    pub show_keys: bool,
    /// The `F3` readouts. Off unless somebody goes looking for them.
    pub show_debug: bool,
}

impl Ui {
    pub fn new() -> Self {
        let font = font_candidates().iter().find_map(|path| {
            let bytes = std::fs::read(path).ok()?;
            load_ttf_font_from_bytes(&bytes).ok()
        });
        Ui { font, show_keys: false, show_debug: false }
    }

    /// Whether a real font was found, so `main` can say so once at startup
    /// rather than leaving a mysteriously dated looking UI unexplained.
    pub fn has_font(&self) -> bool {
        self.font.is_some()
    }

    pub fn width(&self, text: &str, size: f32) -> f32 {
        measure_text(text, self.font.as_ref(), size as u16, 1.0).width
    }

    pub fn text(&self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let params = TextParams {
            font: self.font.as_ref(),
            font_size: size as u16,
            font_scale: 1.0,
            font_scale_aspect: 1.0,
            rotation: 0.0,
            color,
        };
        draw_text_ex(text, x, y, params);
    }

    /// Text with a dark backing, for anything painted onto the world with no
    /// fill of its own. Same reasoning as main.rs's `draw_text_legible`: a
    /// thin light glyph has no edge against a bright background, and raising
    /// alpha does not give it one.
    pub fn text_legible(&self, text: &str, x: f32, y: f32, size: f32, color: Color) {
        let shadow = Color::new(0.0, 0.0, 0.0, 0.8);
        self.text(text, x + 1.0, y + 1.0, size, shadow);
        self.text(text, x - 1.0, y + 1.0, size, shadow);
        self.text(text, x, y, size, color);
    }

    /// Draws `text` centered in `rect`, both axes.
    ///
    /// Vertical centering goes through `offset_y` (the ascent) rather than
    /// halving the font size, because the two disagree by several pixels on a
    /// real face and the error is obvious once a glyph sits inside a pill.
    fn text_centered(&self, text: &str, rect: Rect, size: f32, color: Color) {
        let dims = measure_text(text, self.font.as_ref(), size as u16, 1.0);
        let x = rect.x + (rect.w - dims.width) / 2.0;
        let y = rect.y + rect.h / 2.0 + dims.offset_y - dims.height / 2.0;
        self.text(text, x, y, size, color);
    }
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

/// A control the user clicked. Returned rather than acted on, since every one
/// of these changes state owned by the draw loop.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Click {
    /// Switch to this surface, by index into the world list.
    Surface(usize),
    /// The `+N more` chip, which reveals the surfaces that did not fit.
    MoreSurfaces,
    StepBack,
    PlayPause,
    StepForward,
    /// Cycle the playback speed, same order the `+` and `-` keys walk.
    Speed,
    /// Reframe onto the factory.
    Fit,
    /// Open the key panel.
    Help,
}

/// Everything the chrome needs to know about the frame it is describing.
/// A struct rather than eight positional arguments, since several of them are
/// the same type and would be silently swappable.
pub struct ChromeState<'a> {
    pub surfaces: &'a [String],
    pub active: usize,
    pub playing: bool,
    pub play_speed: f32,
    /// Elapsed game time at the current frame, already formatted.
    pub clock: &'a str,
    /// Entities in the current frame. The one number from the old readout
    /// worth keeping, because "22,971 buildings" is something people
    /// screenshot and `zoom 1.42x` is not.
    pub buildings: usize,
    /// Set while the surface list is expanded by `+N more`.
    pub surfaces_expanded: bool,
}

struct Chip {
    /// Index into the world list, which is what a click has to report.
    index: usize,
    rect: Rect,
    label: String,
    active: bool,
}

struct Button {
    click: Click,
    rect: Rect,
}

/// Every clickable region for one frame, positioned once and then shared by
/// the draw pass and the input pass.
pub struct Chrome {
    chips: Vec<Chip>,
    /// The overflow chip and how many surfaces it stands for.
    more: Option<(Rect, usize)>,
    /// The expanded surface list, present only while `+N more` is open.
    expanded: Vec<Chip>,
    buttons: Vec<Button>,
}

impl Chrome {
    pub fn layout(ui: &Ui, timeline: &Timeline, state: &ChromeState) -> Chrome {
        let mut chrome =
            Chrome { chips: Vec::new(), more: None, expanded: Vec::new(), buttons: Vec::new() };
        chrome.layout_chips(ui, state);
        chrome.layout_transport(ui, timeline, state);
        chrome
    }

    /// The surface switcher, top left.
    ///
    /// Chips keep their natural order rather than floating the active one to
    /// the front: reordering on every switch would make the row rearrange
    /// itself under the cursor, which is worse than an occasional overflow.
    /// The one exception is an active surface that did not fit, which is
    /// swapped in for the last visible chip, because a switcher that cannot
    /// show where you currently are has failed at its only job.
    fn layout_chips(&mut self, ui: &Ui, state: &ChromeState) {
        if state.surfaces.len() <= 1 {
            return;
        }

        let width_of = |label: &str| ui.width(label, CHIP_TEXT) + CHIP_PAD_X * 2.0;
        // Half the window, so a long surface name can never crowd the clock.
        let budget = screen_width() * 0.5;

        let total: f32 =
            state.surfaces.iter().map(|s| width_of(s) + CHIP_GAP).sum::<f32>() - CHIP_GAP;
        let more_width = ui.width("+00 more", CHIP_TEXT) + CHIP_PAD_X * 2.0;
        let available = if total <= budget { budget } else { budget - more_width - CHIP_GAP };

        let mut shown: Vec<usize> = Vec::new();
        let mut used = 0.0;
        for (index, name) in state.surfaces.iter().enumerate() {
            let width = width_of(name);
            let step = if shown.is_empty() { width } else { width + CHIP_GAP };
            if used + step > available && !shown.is_empty() {
                break;
            }
            used += step;
            shown.push(index);
        }
        if !shown.contains(&state.active) {
            if let Some(last) = shown.last_mut() {
                *last = state.active;
            }
        }

        let mut x = MARGIN;
        for &index in &shown {
            let label = state.surfaces[index].clone();
            let rect = Rect::new(x, MARGIN, width_of(&label), CHIP_HEIGHT);
            x += rect.w + CHIP_GAP;
            self.chips.push(Chip { index, rect, label, active: index == state.active });
        }

        let hidden = state.surfaces.len() - shown.len();
        if hidden > 0 {
            let rect = Rect::new(x, MARGIN, more_width, CHIP_HEIGHT);
            self.more = Some((rect, hidden));
            if state.surfaces_expanded {
                self.layout_expanded(ui, state, &shown);
            }
        }
    }

    /// The dropdown behind `+N more`: everything the row could not fit,
    /// stacked under it at the same chip height so the two read as one
    /// control rather than as a list that appeared from somewhere else.
    fn layout_expanded(&mut self, ui: &Ui, state: &ChromeState, shown: &[usize]) {
        let hidden: Vec<usize> =
            (0..state.surfaces.len()).filter(|i| !shown.contains(i)).collect();
        let width = hidden
            .iter()
            .map(|&i| ui.width(&state.surfaces[i], CHIP_TEXT) + CHIP_PAD_X * 2.0)
            .fold(0.0f32, f32::max);
        let mut y = MARGIN + CHIP_HEIGHT + CHIP_GAP;
        for index in hidden {
            let rect = Rect::new(MARGIN, y, width, CHIP_HEIGHT);
            y += CHIP_HEIGHT + 2.0;
            let label = state.surfaces[index].clone();
            self.expanded.push(Chip { index, rect, label, active: index == state.active });
        }
    }

    /// Playback controls in the empty gutter left of the scrub bar, and the
    /// view controls in the one right of it.
    ///
    /// The bar is 60% of the window and centered (see `Timeline::for_screen`),
    /// so both gutters are already dead space, and putting the controls there
    /// costs no vertical room and cannot collide with the labels stacked
    /// above and below the bar. A row centered over the bar instead would
    /// have to clear the activity graph, the playhead label, and the hover
    /// tooltip, all of which live in that column.
    fn layout_transport(&mut self, ui: &Ui, timeline: &Timeline, state: &ChromeState) {
        let y = timeline.y;
        let speed_label = format!("{}x", state.play_speed);
        let speed_width = ui.width(&speed_label, CHIP_TEXT).max(ui.width("0.25x", CHIP_TEXT)) + 20.0;

        // Widest first, then progressively less, so a narrow window loses the
        // least important control rather than overflowing. Everything dropped
        // here still has a key, and the key panel still lists it.
        let left_room = (timeline.left - GUTTER_PAD - MARGIN).max(0.0);
        let full = BUTTON * 2.0 + PLAY_BUTTON + BUTTON_GAP * 3.0 + speed_width;
        let no_speed = BUTTON * 2.0 + PLAY_BUTTON + BUTTON_GAP * 2.0;
        let (with_steps, with_speed) = if left_room >= full {
            (true, true)
        } else if left_room >= no_speed {
            (true, false)
        } else {
            (false, false)
        };

        let cluster = if with_speed {
            full
        } else if with_steps {
            no_speed
        } else {
            PLAY_BUTTON
        };
        // Right aligned against the bar, so the play button stays a fixed
        // distance from the thing it drives as the window resizes.
        let mut x = timeline.left - GUTTER_PAD - cluster;

        if with_steps {
            self.buttons.push(Button {
                click: Click::StepBack,
                rect: Rect::new(x, y - BUTTON / 2.0, BUTTON, BUTTON),
            });
            x += BUTTON + BUTTON_GAP;
        }
        self.buttons.push(Button {
            click: Click::PlayPause,
            rect: Rect::new(x, y - PLAY_BUTTON / 2.0, PLAY_BUTTON, PLAY_BUTTON),
        });
        x += PLAY_BUTTON + BUTTON_GAP;
        if with_steps {
            self.buttons.push(Button {
                click: Click::StepForward,
                rect: Rect::new(x, y - BUTTON / 2.0, BUTTON, BUTTON),
            });
            x += BUTTON + BUTTON_GAP;
        }
        if with_speed {
            self.buttons.push(Button {
                click: Click::Speed,
                rect: Rect::new(x, y - CHIP_HEIGHT / 2.0, speed_width, CHIP_HEIGHT),
            });
        }

        // Right gutter. Two controls only, so it fits anywhere the window is
        // wide enough to have a bar at all.
        let right_edge = screen_width() - MARGIN;
        let right_start = (timeline.left + timeline.width + GUTTER_PAD).min(right_edge - BUTTON * 2.0 - BUTTON_GAP);
        self.buttons.push(Button {
            click: Click::Fit,
            rect: Rect::new(right_start, y - BUTTON / 2.0, BUTTON, BUTTON),
        });
        self.buttons.push(Button {
            click: Click::Help,
            rect: Rect::new(right_start + BUTTON + BUTTON_GAP, y - BUTTON / 2.0, BUTTON, BUTTON),
        });
    }

    /// What a click at `point` hits, or `None` for a click that should fall
    /// through to the world behind it.
    ///
    /// The expanded surface list is tested before everything else: while it is
    /// open it overlaps whatever is underneath, and the thing on top wins.
    pub fn hit(&self, point: Vec2) -> Option<Click> {
        for chip in &self.expanded {
            if chip.rect.contains(point) {
                return Some(Click::Surface(chip.index));
            }
        }
        for chip in &self.chips {
            if chip.rect.contains(point) {
                return Some(Click::Surface(chip.index));
            }
        }
        if let Some((rect, _)) = self.more {
            if rect.contains(point) {
                return Some(Click::MoreSurfaces);
            }
        }
        self.buttons.iter().find(|b| b.rect.contains(point)).map(|b| b.click)
    }

    /// Whether `point` is over any chrome at all, so the draw loop can keep a
    /// click on a button from also panning the camera underneath it.
    pub fn blocks_world(&self, point: Vec2) -> bool {
        self.hit(point).is_some()
    }

    pub fn draw(&self, ui: &Ui, state: &ChromeState) {
        let mouse: Vec2 = mouse_position().into();

        for chip in self.chips.iter().chain(&self.expanded) {
            draw_chip(ui, &chip.label, chip.rect, chip.active, chip.rect.contains(mouse));
        }
        if let Some((rect, hidden)) = self.more {
            let label = format!("+{hidden} more");
            draw_chip(ui, &label, rect, state.surfaces_expanded, rect.contains(mouse));
        }

        self.draw_clock(ui, state);

        for button in &self.buttons {
            let hovered = button.rect.contains(mouse);
            draw_button_fill(button.rect, hovered);
            let ink = if hovered { INK } else { INK_HOVER };
            match button.click {
                Click::StepBack => draw_step(button.rect, ink, false),
                Click::StepForward => draw_step(button.rect, ink, true),
                Click::PlayPause if state.playing => draw_pause(button.rect, ink),
                Click::PlayPause => draw_play(button.rect, ink),
                Click::Speed => {
                    ui.text_centered(&format!("{}x", state.play_speed), button.rect, CHIP_TEXT, ink)
                }
                Click::Fit => draw_fit(button.rect, ink),
                Click::Help => ui.text_centered("?", button.rect, 22.0, ink),
                _ => {}
            }
        }
    }

    /// Elapsed game time top right, with the building count under it.
    ///
    /// Right aligned rather than left, so both grow away from the window edge
    /// instead of drifting across it as the numbers get longer.
    fn draw_clock(&self, ui: &Ui, state: &ChromeState) {
        let right = screen_width() - MARGIN;

        let clock_width = ui.width(state.clock, CLOCK_TEXT);
        ui.text_legible(state.clock, right - clock_width, MARGIN + CLOCK_TEXT, CLOCK_TEXT, INK);

        let buildings = format!("{} buildings", with_thousands(state.buildings));
        let count_width = ui.width(&buildings, COUNT_TEXT);
        ui.text_legible(
            &buildings,
            right - count_width,
            MARGIN + CLOCK_TEXT + COUNT_TEXT + 7.0,
            COUNT_TEXT,
            INK_DIM,
        );
    }
}

/// One surface chip. The active one gets a filled pill and the rest get
/// nothing, because the chips sit on the rendered world and a fill is the
/// only signal that survives both dark ground and a white space platform.
/// Brightness carries the hover state, where being wrong for one frame on an
/// unlucky background costs nothing.
fn draw_chip(ui: &Ui, label: &str, rect: Rect, active: bool, hovered: bool) {
    if active {
        draw_pill(rect, rect.h / 2.0, SURFACE, Some(EDGE));
    } else if hovered {
        draw_pill(rect, rect.h / 2.0, SURFACE_HOVER, None);
    }
    let ink = match (active, hovered) {
        (true, _) => INK,
        (false, true) => INK_HOVER,
        (false, false) => INK_CHIP,
    };
    if active {
        ui.text_centered(label, rect, CHIP_TEXT, ink);
    } else {
        // No fill behind it, so it needs the shadow that filled chrome does
        // not. Centered by hand rather than through `text_centered`, which
        // draws once and cannot carry a shadow.
        let width = ui.width(label, CHIP_TEXT);
        let x = rect.x + (rect.w - width) / 2.0;
        let y = rect.y + rect.h / 2.0 + CHIP_TEXT * 0.35;
        ui.text_legible(label, x, y, CHIP_TEXT, ink);
    }
}

/// Buttons carry a fill only under the cursor. At rest they are the glyph
/// alone, which is what keeps a row of controls from reading as a toolbar
/// bolted over the factory.
fn draw_button_fill(rect: Rect, hovered: bool) {
    if hovered {
        draw_pill(rect, rect.h / 2.0, SURFACE, Some(EDGE));
    }
}

fn draw_play(rect: Rect, color: Color) {
    let c = rect.center();
    let size = rect.h * 0.30;
    // Nudged right by a fraction of the width: a triangle centered on its
    // bounding box reads as sitting left of centre, which is why every media
    // player offsets it.
    let x = c.x - size * 0.45 + size * 0.15;
    draw_triangle(
        Vec2::new(x, c.y - size),
        Vec2::new(x, c.y + size),
        Vec2::new(x + size * 1.6, c.y),
        color,
    );
}

fn draw_pause(rect: Rect, color: Color) {
    let c = rect.center();
    let h = rect.h * 0.30;
    let w = rect.h * 0.10;
    draw_rectangle(c.x - w * 2.2, c.y - h, w, h * 2.0, color);
    draw_rectangle(c.x + w * 1.2, c.y - h, w, h * 2.0, color);
}

/// One frame forward or back: a triangle against a bar, the shape every
/// transport uses for "next", so it does not have to be explained.
fn draw_step(rect: Rect, color: Color, forward: bool) {
    let c = rect.center();
    let size = rect.h * 0.24;
    let bar = rect.h * 0.07;
    let dir = if forward { 1.0 } else { -1.0 };
    let tip = c.x + dir * size * 0.55;
    let base = c.x - dir * size * 0.75;
    draw_triangle(
        Vec2::new(base, c.y - size),
        Vec2::new(base, c.y + size),
        Vec2::new(tip, c.y),
        color,
    );
    draw_rectangle(tip + if forward { 0.0 } else { -bar }, c.y - size, bar, size * 2.0, color);
}

/// Reframe onto the factory. Four corner brackets rather than a house glyph:
/// this fits the view to the base, and a house would suggest going back to
/// somewhere instead.
fn draw_fit(rect: Rect, color: Color) {
    let c = rect.center();
    let r = rect.h * 0.26;
    let arm = r * 0.62;
    let t = 2.0;
    for (sx, sy) in [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)] {
        let x = c.x + sx * r;
        let y = c.y + sy * r;
        draw_rectangle(x.min(x - sx * arm), y - t / 2.0, arm, t, color);
        draw_rectangle(x - t / 2.0, y.min(y - sy * arm), t, arm, color);
    }
}

/// macroquad has no rounded rectangle, so one is composed from three rects
/// and four circles. Cheap enough at chrome quantities, and it keeps the
/// pill shape that makes a chip read as pressable.
fn draw_rounded_rect(rect: Rect, radius: f32, color: Color) {
    let r = radius.min(rect.w / 2.0).min(rect.h / 2.0);
    draw_rectangle(rect.x + r, rect.y, rect.w - r * 2.0, rect.h, color);
    draw_rectangle(rect.x, rect.y + r, r, rect.h - r * 2.0, color);
    draw_rectangle(rect.x + rect.w - r, rect.y + r, r, rect.h - r * 2.0, color);
    for (cx, cy) in [
        (rect.x + r, rect.y + r),
        (rect.x + rect.w - r, rect.y + r),
        (rect.x + r, rect.y + rect.h - r),
        (rect.x + rect.w - r, rect.y + rect.h - r),
    ] {
        draw_circle(cx, cy, r, color);
    }
}

/// A filled rounded shape with an optional hairline edge.
///
/// The edge is a second rounded rect one pixel larger drawn underneath,
/// rather than a stroked outline: macroquad has no rounded stroke either, and
/// stroking this shape by hand would mean four lines and four arcs that have
/// to meet exactly. Drawing the same shape twice cannot fail to line up.
fn draw_pill(rect: Rect, radius: f32, fill: Color, edge: Option<Color>) {
    if let Some(edge) = edge {
        let outer = Rect::new(rect.x - 1.0, rect.y - 1.0, rect.w + 2.0, rect.h + 2.0);
        draw_rounded_rect(outer, radius + 1.0, edge);
    }
    draw_rounded_rect(rect, radius, fill);
}

/// `22971` reads as a serial number and `22,971` reads as a quantity, which
/// is the entire reason this count survived the move off the default view.
fn with_thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// True the first time the viewer is ever opened on this machine, false
/// forever after.
///
/// A marker beside the tool's own settings file rather than a field inside
/// it: `Settings` is documented as holding only answers a user would
/// otherwise retype, and "has seen the controls once" is not one of those.
/// A sibling file also means deleting it is an obvious way to get the panel
/// back, which a JSON field would not be.
///
/// Failing to write the marker leaves this returning true again next launch,
/// which is the right way round: a panel that reappears is a nuisance, and
/// one that never appears leaves a first-time viewer with nothing but a `?`
/// in the corner and no reason to suspect it matters.
pub fn first_run() -> bool {
    let Some(marker) = save_timelapse::settings::settings_path().map(|p| p.with_file_name("seen-controls")) else {
        return false;
    };
    if marker.exists() {
        return false;
    }
    if let Some(dir) = marker.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(&marker, "The viewer shows its controls once. Delete this file to see them again.\n");
    true
}

/// One row of the key panel: the key itself and what it does.
type Binding = (&'static str, &'static str);

/// Everything the viewer responds to, grouped so the panel scans instead of
/// reading as a wall.
///
/// `m`, `c` and `b` are shipped features that the old always-visible hint
/// line never mentioned, so for most of their life they have been undiscover-
/// able. This table is the fix. `s` and `l` are deliberately absent: they are
/// renderer A/B toggles that make the factory look broken, and they live with
/// the rest of the diagnostics behind `F3`.
const KEY_GROUPS: &[(&str, &[Binding])] = &[
    (
        "Playback",
        &[
            ("space", "play / pause"),
            ("\u{2190} \u{2192}", "step one frame"),
            ("home", "jump to the start"),
            ("end", "jump to the latest"),
            ("+  -", "playback speed"),
        ],
    ),
    (
        "Navigate",
        &[
            ("drag", "pan the map"),
            ("scroll", "zoom"),
            ("m", "next milestone"),
            ("c", "next busy stretch"),
            ("b", "bookmark this moment"),
            ("tab", "next surface"),
        ],
    ),
    (
        "View",
        &[
            ("f", "follow the growing base"),
            ("h", "construction heatmap"),
            ("shift", "with m or c, go back"),
            ("F3", "renderer diagnostics"),
            ("?", "this panel"),
        ],
    ),
];

/// The `?` panel: everything the viewer does, on request and never otherwise.
///
/// Drawn over a full-window scrim rather than as a floating box alone, which
/// both makes the text legible over any factory and signals that the view
/// underneath is paused for attention rather than gone.
pub fn draw_key_panel(ui: &Ui) {
    let mut column_widths = Vec::new();
    let mut rows = 0usize;
    for (_, bindings) in KEY_GROUPS {
        let keys = bindings.iter().map(|(k, _)| ui.width(k, PANEL_TEXT)).fold(0.0f32, f32::max);
        let text = bindings.iter().map(|(_, d)| ui.width(d, PANEL_TEXT)).fold(0.0f32, f32::max);
        column_widths.push((keys, keys + 14.0 + text));
        rows = rows.max(bindings.len());
    }

    let body: f32 = column_widths.iter().map(|(_, w)| w).sum::<f32>()
        + PANEL_COLUMN_GAP * (KEY_GROUPS.len() as f32 - 1.0);
    let title = "Controls";
    let width = body.max(ui.width(title, 24.0)) + PANEL_PAD * 2.0;
    let height = PANEL_PAD * 2.0 + 24.0 + 18.0 + PANEL_HEADING + 10.0 + rows as f32 * PANEL_ROW;

    draw_rectangle(0.0, 0.0, screen_width(), screen_height(), Color::new(0.0, 0.0, 0.0, 0.55));

    let panel = Rect::new(
        (screen_width() - width) / 2.0,
        (screen_height() - height) / 2.0,
        width,
        height,
    );
    draw_pill(panel, 14.0, Color::new(0.07, 0.08, 0.10, 0.97), Some(EDGE));

    ui.text(title, panel.x + PANEL_PAD, panel.y + PANEL_PAD + 20.0, 24.0, INK);

    let mut x = panel.x + PANEL_PAD;
    let top = panel.y + PANEL_PAD + 24.0 + 18.0;
    for ((heading, bindings), (key_width, column_width)) in KEY_GROUPS.iter().zip(&column_widths) {
        ui.text(heading, x, top, PANEL_HEADING, INK_DIM);
        let mut y = top + PANEL_HEADING + 12.0;
        for (key, description) in bindings.iter() {
            ui.text(key, x, y, PANEL_TEXT, INK);
            ui.text(description, x + key_width + 14.0, y, PANEL_TEXT, INK_DIM);
            y += PANEL_ROW;
        }
        x += column_width + PANEL_COLUMN_GAP;
    }

    let dismiss = "press ? or esc to close";
    let width = ui.width(dismiss, COUNT_TEXT);
    ui.text(
        dismiss,
        panel.x + panel.w - PANEL_PAD - width,
        panel.y + PANEL_PAD + 20.0,
        COUNT_TEXT,
        INK_DIM,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_separators_only_appear_above_a_thousand() {
        assert_eq!(with_thousands(0), "0");
        assert_eq!(with_thousands(999), "999");
        assert_eq!(with_thousands(1000), "1,000");
        assert_eq!(with_thousands(22971), "22,971");
        assert_eq!(with_thousands(1234567), "1,234,567");
    }
}
