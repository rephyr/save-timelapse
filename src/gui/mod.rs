//! The window, and the screens in it.
//!
//! Hand drawn rather than built on a widget toolkit, for the same reason
//! `viewer::chrome` is: this program already owns a renderer and a font, the
//! screens are a handful of lists and buttons, and a toolkit would bring its
//! own look to argue with the one the viewer already has.
//!
//! Everything about where things go lives in [`layout`], which draws nothing
//! and can be tested. This module is the part that needs a window: reading the
//! mouse, painting, and deciding which screen is up.

pub mod layout;

use macroquad::prelude::*;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::Arc;

use crate::viewer::Ui;
use crate::{build, describe, replay};
use layout::Column;

/// Dark, and a shade off neutral so it does not read as an unpainted window.
const BACKGROUND: Color = Color::new(0.09, 0.09, 0.11, 1.0);
const ROW: Color = Color::new(1.0, 1.0, 1.0, 0.06);
const ROW_HOVER: Color = Color::new(1.0, 1.0, 1.0, 0.13);
const ROW_EDGE: Color = Color::new(1.0, 1.0, 1.0, 0.10);
const TEXT: Color = Color::new(0.94, 0.94, 0.96, 1.0);
const TEXT_DIM: Color = Color::new(0.94, 0.94, 0.96, 0.55);
const ACCENT: Color = Color::new(0.45, 0.75, 1.0, 1.0);
/// The way out of a screen. Warm rather than the blue everything else uses, so
/// Back and Quit are found by colour instead of by reading every row.
const LEAVE: Color = Color::new(0.98, 0.72, 0.42, 1.0);
const LEAVE_ROW: Color = Color::new(0.98, 0.72, 0.42, 0.07);
const LEAVE_ROW_HOVER: Color = Color::new(0.98, 0.72, 0.42, 0.18);

const ROW_HEIGHT: f32 = 46.0;
const LABEL_SIZE: f32 = 20.0;
const NOTE_SIZE: f32 = 16.0;
const TITLE_SIZE: f32 = 34.0;
/// The name, bigger than a screen's own title: it is the one piece of the
/// window that is the same on every screen, so it carries the identity.
const NAME_SIZE: f32 = 44.0;
/// How small the name may shrink before it stops. Past this it is unreadable
/// anyway, and a window that narrow has bigger problems.
const NAME_MIN_SIZE: f32 = 16.0;

/// A cool light pink for the name, with a faint wider one behind it for the
/// glow. Two shades of one colour, so it reads as lit rather than decorated.
const NAME: Color = Color::new(1.0, 0.72, 0.86, 1.0);
const NAME_GLOW: Color = Color::new(1.0, 0.60, 0.82, 0.10);

/// One choice in a column: what it says, and the quieter thing it says on the
/// right. The note is where a count goes, so a row can carry "3 ready" without
/// a second column to align.
struct Choice {
    label: String,
    note: String,
    /// The way out: Back, Quit, and keeping something rather than deleting it.
    ///
    /// Marked rather than recognised from the label, so a row that says
    /// something else can still be the exit and a row that happens to say
    /// "Back" cannot become one by accident.
    leave: bool,
}

impl Choice {
    fn new(label: &str, note: impl Into<String>) -> Choice {
        Choice { label: label.to_string(), note: note.into(), leave: false }
    }

    fn leaving(label: &str) -> Choice {
        Choice { leave: true, ..Choice::new(label, "") }
    }
}

/// Which screen is up. Deliberately small: every screen here is a list of
/// choices, and the ones that are not yet built say so rather than being
/// missing from the menu.
enum Screen {
    Menu,
    Watch,
    /// Which recording to build. Loaded when the screen opens, since it reads
    /// the disk and the answer changes as somebody plays.
    Recordings(Vec<Recording>),
    /// Reading a baseline, which on a megabase is tens of seconds. Its own
    /// phase rather than the first part of the build, because the places to
    /// choose between are not known until it finishes, and doing it here means
    /// the build does not read the same file twice.
    Opening(Opening),
    /// Which places to include. Multi select: rows toggle, and the row under
    /// them starts the build with whatever is ticked.
    Places(Choosing),
    /// How often to take a picture, as presets rather than a number to type.
    Interval(Choosing),
    /// Whether to read the grass, water and trees under the factory.
    CaptureGround(Choosing),
    /// Which save to read it from. Ground only exists where a save had already
    /// been, so a later save can only ever cover more of the factory.
    GroundSave(Choosing),
    /// A build in flight, or the sentence it ended with. The window keeps
    /// drawing throughout, which is the whole reason the work is on a thread.
    Building(Running),
    Done(String),
    /// Which saves to build from, newest first. Multi select, like places.
    Saves(SavePick),
    /// Whether to read the ground, which costs one more Factorio run.
    Ground(SavePick),
    /// Which timelapse to render, then how. Each step is a list, so the
    /// answers accumulate in one place rather than in a screen each.
    Render(Render),
    /// The three things this program leaves on disk. Split by how recoverable
    /// each is, the same split the console menu makes.
    Manage,
    Listing(Listing),
    /// Nothing is deleted without this. A list where a click removes something
    /// is a list where a mis-click does.
    Confirm(Confirm),
    Soon(&'static str),
}

/// A video being set up. `step` is which question is on screen.
struct Render {
    built: Vec<build::BuiltTimelapse>,
    step: RenderStep,
    chosen: Option<usize>,
    /// The places this timelapse holds, empty for one unnamed world.
    surfaces: Vec<String>,
    /// One place, `"all"` for a file each, or `None` for the busiest.
    surface: Option<String>,
    size: (u32, u32),
    fps: u32,
    video: bool,
    clock: bool,
    players: bool,
    /// Whether this timelapse knows where anybody was. Only a live capture
    /// does, so offering the marker otherwise is offering nothing.
    has_players: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum RenderStep {
    Which,
    /// Skipped for a timelapse with one world in it, there being no choice.
    Place,
    Size,
    Rate,
    Kind,
    /// Skipped for an image sequence: overlays are burnt into a video, and a
    /// folder of frames is for somebody else's editor to label.
    Overlays,
}

/// The sizes offered, largest last so going up reads as a direction.
const SIZES: [(u32, u32, &str); 4] =
    [(1920, 1080, "1080p"), (1280, 720, "720p, smallest and fastest"), (2560, 1440, "1440p"), (3840, 2160, "4K")];

/// Frame rates, as what they are for rather than as numbers alone.
const RATES: [(u32, &str); 3] = [(30, "30 per second"), (60, "60 per second, smoothest"), (24, "24 per second, filmic")];

/// The saves offered and which are ticked.
struct SavePick {
    saves: Vec<std::path::PathBuf>,
    labels: Vec<String>,
    notes: Vec<String>,
    picked: Vec<bool>,
    ground: bool,
}

impl SavePick {
    fn chosen(&self) -> Vec<std::path::PathBuf> {
        self.saves.iter().zip(&self.picked).filter(|(_, p)| **p).map(|(save, _)| save.clone()).collect()
    }
}

/// Which of the three is being looked through, and what it holds.
struct Listing {
    kind: Kind,
    rows: Vec<Deletable>,
}

/// What a row in a listing is, which decides what deleting it costs.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// A playthrough's recorded history, and the only copy of it.
    LogData,
    Timelapse,
    Video,
}

impl Kind {
    fn title(self) -> &'static str {
        match self {
            Kind::LogData => "Log data",
            Kind::Timelapse => "Built timelapses",
            Kind::Video => "Videos",
        }
    }

    /// What losing one costs, said before anything is deleted rather than
    /// after. Only the first cannot be undone by rebuilding.
    fn warning(self) -> &'static str {
        match self {
            Kind::LogData => {
                "The only copy of that playthrough's history. It cannot be recovered from your saves, \
                 because Factorio keeps no record of when anything was built."
            }
            Kind::Timelapse => "You can build it again from the log data.",
            Kind::Video => "You can save it again from the timelapse.",
        }
    }
}

/// One thing a listing offers to delete.
struct Deletable {
    label: String,
    note: String,
    path: std::path::PathBuf,
}

/// A delete waiting to be agreed to.
struct Confirm {
    kind: Kind,
    label: String,
    path: std::path::PathBuf,
}

/// A recording the build screen can offer.
#[derive(Clone)]
struct Recording {
    label: String,
    note: String,
    /// Which playthrough this is, which is what ties ground to it: a scan of a
    /// save from another game lands under a different id and is refused.
    session_id: u32,
    session_dir: std::path::PathBuf,
    baseline_path: std::path::PathBuf,
    name: String,
}

/// A baseline being read on its own thread.
struct Opening {
    loaded: Receiver<Result<Box<Loaded>, String>>,
    what: String,
    /// Carried through the wait, because the screens after it need the name
    /// and the folder and this is the only thing that outlives the choice.
    recording: Recording,
}

/// A recording read and ready to build, with the answers gathered so far.
struct Loaded {
    replay: replay::Replay,
    surfaces: Vec<String>,
}

/// A loaded recording part way through being configured.
struct Choosing {
    loaded: Box<Loaded>,
    recording: Recording,
    /// One per surface, in the same order.
    picked: Vec<bool>,
    seconds: u64,
    /// Where the ground comes from, or `None` for a build without it.
    ground: Option<GroundFrom>,
}

/// How the ground for this build is going to be got.
#[derive(Clone)]
enum GroundFrom {
    /// Already read for this playthrough, so it costs a file copy.
    Cache(Vec<std::path::PathBuf>),
    Save(std::path::PathBuf),
}

impl Choosing {
    fn chosen_surfaces(&self) -> Vec<String> {
        chosen_of(&self.loaded.surfaces, &self.picked)
    }
}

/// Space kept clear at each end of a row, and between a label and its note.
const ROW_PAD: f32 = 18.0;
const ROW_GAP: f32 = 16.0;

/// The most of a row a note may take, leaving the rest to the label.
///
/// The label is what a row is; the note is what it is like. So when they will
/// not both fit the note gives way first, but it is never dropped outright,
/// because "13 hours in, last played just now" cut short still says more than
/// nothing does.
const NOTE_SHARE: f32 = 0.5;

/// `text` cut to fit `max`, ending in dots when something was taken off.
///
/// Takes its measuring as an argument rather than reaching for the font, which
/// is what lets it be tested: the real measure needs a window, and the rule
/// being tested is about widths rather than about glyphs.
///
/// Trimmed by character rather than by byte, so a cut lands between letters
/// instead of inside one.
fn elide(text: &str, max: f32, width_of: &dyn Fn(&str) -> f32) -> String {
    if width_of(text) <= max {
        return text.to_string();
    }
    let mut end = text.len();
    while end > 0 {
        // Back up to a character boundary, then try that much plus the dots.
        end -= 1;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        let candidate = format!("{}...", &text[..end]);
        if width_of(&candidate) <= max {
            return candidate;
        }
    }
    String::new()
}

/// How wide a row's label and note may each be drawn.
///
/// Split rather than clipped: two strings drawn from opposite edges of a box
/// have nothing stopping them meeting in the middle, and on a narrow window
/// they did.
fn share_row(width: f32, note_width: f32) -> (f32, f32) {
    let available = (width - ROW_PAD * 2.0).max(0.0);
    if note_width <= 0.0 {
        return (available, 0.0);
    }
    let for_note = note_width.min(available * NOTE_SHARE);
    ((available - for_note - ROW_GAP).max(0.0), for_note)
}

/// The ticked names, in the order they were offered.
///
/// Order matters rather than being incidental: these go to the writer, which
/// treats exactly one place differently from several. A free function because
/// the answer is a property of two lists and nothing else, which is also what
/// lets it be tested without a loaded recording behind it.
fn chosen_of(surfaces: &[String], picked: &[bool]) -> Vec<String> {
    surfaces.iter().zip(picked).filter(|(_, picked)| **picked).map(|(name, _)| name.clone()).collect()
}

/// The intervals offered, in seconds of game time per frame.
///
/// Presets rather than a number to type, for the same reason every list here
/// is numbered: this is a trade between smoothness and file size that four
/// points cover, and a free number invites answers nobody wants to sit through.
const INTERVALS: [(u64, &str); 4] =
    [(10, "Every 10 seconds"), (30, "Every 30 seconds"), (60, "Every minute"), (300, "Every 5 minutes")];

/// What a build says to the window while it runs.
enum Update {
    Frames(usize),
    /// The sentence to show when it is over, whether it worked or not.
    Ended(String),
}

/// A build on its own thread, and the two things the window needs to reach it.
struct Running {
    updates: Receiver<Update>,
    cancel: Arc<AtomicBool>,
    what: String,
    frames: Option<usize>,
    /// Whether this job counts anything. A render does not: the window it
    /// opens shows its own progress, and a second count here would be a
    /// number nobody can check.
    counts: bool,
    /// What to say while it has nothing to count.
    waiting: String,
}

impl Running {
    /// Takes in everything said since the last frame, and hands back the
    /// closing sentence once there is one.
    ///
    /// Drained rather than read once, because a build reports far more often
    /// than the window redraws and a queue losing one entry a frame would fall
    /// behind and never catch up.
    fn drain(&mut self) -> Option<String> {
        loop {
            match self.updates.try_recv() {
                Ok(Update::Frames(count)) => self.frames = Some(count),
                Ok(Update::Ended(message)) => return Some(message),
                // Disconnected without a word means the thread died. Saying so
                // beats a count that never moves again and no way to leave.
                Err(TryRecvError::Disconnected) => return Some("The build stopped unexpectedly.".to_string()),
                Err(TryRecvError::Empty) => return None,
            }
        }
    }

    /// Asks the build to stop. It notices within a frame and reports what it
    /// managed, which is why nothing here changes screen.
    fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct App {
    ui: Ui,
    screen: Screen,
    /// Rebuilt when a screen opens rather than every frame: it reads the disk.
    timelapses: Vec<crate::gui::Built>,
    /// How far the current screen is scrolled. Reset whenever the screen
    /// changes, so opening a short list does not land part way down it.
    scroll: f32,
    quit: bool,
}

/// A built timelapse, as a screen needs it. Kept here rather than borrowed
/// from the menu code so the window owns everything it draws.
pub struct Built {
    pub name: String,
    pub path: std::path::PathBuf,
    pub note: String,
}

impl Default for App {
    fn default() -> App {
        App { ui: Ui::new(), screen: Screen::Menu, timelapses: Vec::new(), scroll: 0.0, quit: false }
    }
}

impl App {
    fn choices(&self) -> Vec<Choice> {
        match &self.screen {
            Screen::Menu => {
                let ready = match self.timelapses.len() {
                    0 => String::new(),
                    n => format!("{n} ready"),
                };
                vec![
                    Choice::new("Watch a timelapse", ready),
                    Choice::new("Build one from a recording", ""),
                    Choice::new("Build one from save files", ""),
                    Choice::new("Save one as a video", ""),
                    Choice::new("Manage", ""),
                    Choice::leaving("Quit"),
                ]
            }
            Screen::Watch => {
                let mut rows: Vec<Choice> = self.timelapses.iter().map(|t| Choice::new(&t.name, t.note.clone())).collect();
                rows.push(Choice::leaving("Back"));
                rows
            }
            Screen::Recordings(found) => {
                let mut rows: Vec<Choice> = found.iter().map(|r| Choice::new(&r.label, r.note.clone())).collect();
                rows.push(Choice::leaving("Back"));
                rows
            }
            // Nothing to choose while a file is being read, but the row is
            // there so the screen has the same shape as every other one.
            Screen::Opening(_) => vec![Choice::leaving("Cancel")],
            Screen::Places(choosing) => {
                let mut rows: Vec<Choice> = choosing
                    .loaded
                    .surfaces
                    .iter()
                    .zip(&choosing.picked)
                    .map(|(name, picked)| Choice::new(&describe::pretty_place(name), if *picked { "included" } else { "" }))
                    .collect();
                let picked = choosing.picked.iter().filter(|p| **p).count();
                rows.push(Choice::new("Continue", format!("{picked} of {}", choosing.picked.len())));
                rows.push(Choice::leaving("Back"));
                rows
            }
            Screen::Interval(_) => {
                let mut rows: Vec<Choice> = INTERVALS.iter().map(|(_, label)| Choice::new(label, "")).collect();
                rows.push(Choice::leaving("Back"));
                rows
            }
            Screen::CaptureGround(choosing) => {
                let cached = !cached_for(&choosing.recording).is_empty();
                vec![
                    Choice::new(
                        "Yes, add the ground",
                        // Said on the row, because it changes the answer: a
                        // rescan is one more Factorio run and a reuse is not.
                        if cached { "already read, instant" } else { "one more Factorio run" },
                    ),
                    Choice::new("No, just the factory", ""),
                    Choice::leaving("Back"),
                ]
            }
            Screen::GroundSave(_) => {
                let mut rows: Vec<Choice> =
                    saves_newest_first().into_iter().map(|(label, note, _)| Choice::new(&label, note)).collect();
                rows.push(Choice::leaving("Back"));
                rows
            }
            // One row, and it stops the build rather than leaving the screen:
            // walking away from work that is still running is how somebody
            // ends up with a half written timelapse and no idea why.
            Screen::Building(_) => vec![Choice::leaving("Stop")],
            Screen::Saves(pick) => {
                let mut rows: Vec<Choice> = pick
                    .labels
                    .iter()
                    .zip(&pick.notes)
                    .zip(&pick.picked)
                    .map(|((label, note), picked)| {
                        Choice::new(label, if *picked { "included".to_string() } else { note.clone() })
                    })
                    .collect();
                let picked = pick.picked.iter().filter(|p| **p).count();
                rows.push(Choice::new("Continue", format!("{picked} chosen")));
                rows.push(Choice::leaving("Back"));
                rows
            }
            Screen::Ground(pick) => vec![
                Choice::new("Yes, read the ground", if pick.ground { "chosen" } else { "" }),
                Choice::new("No, just the factory", ""),
                Choice::leaving("Back"),
            ],
            Screen::Render(render) => match render.step {
                RenderStep::Which => {
                    let mut rows: Vec<Choice> =
                        render.built.iter().map(|t| Choice::new(&t.name, format!("{} frames", t.frames))).collect();
                    rows.push(Choice::leaving("Back"));
                    rows
                }
                RenderStep::Size => {
                    let mut rows: Vec<Choice> = SIZES.iter().map(|(_, _, label)| Choice::new(label, "")).collect();
                    rows.push(Choice::leaving("Back"));
                    rows
                }
                RenderStep::Rate => {
                    let mut rows: Vec<Choice> = RATES.iter().map(|(_, label)| Choice::new(label, "")).collect();
                    rows.push(Choice::leaving("Back"));
                    rows
                }
                RenderStep::Kind => vec![
                    Choice::new("One video file", ""),
                    Choice::new("A picture per frame", "for editing"),
                    Choice::leaving("Back"),
                ],
                RenderStep::Place => {
                    let mut rows: Vec<Choice> =
                        render.surfaces.iter().map(|name| Choice::new(&describe::pretty_place(name), "")).collect();
                    rows.push(Choice::new("One video for each", ""));
                    rows.push(Choice::leaving("Back"));
                    rows
                }
                RenderStep::Overlays => {
                    let on = |yes: bool| if yes { "on" } else { "off" };
                    let mut rows = vec![Choice::new("In-game clock", on(render.clock))];
                    if render.has_players {
                        rows.push(Choice::new("Where you were", on(render.players)));
                    }
                    rows.push(Choice::new("Render", ""));
                    rows.push(Choice::leaving("Back"));
                    rows
                }
            },
            Screen::Manage => vec![
                Choice::new(Kind::LogData.title(), summary(Kind::LogData)),
                Choice::new(Kind::Timelapse.title(), summary(Kind::Timelapse)),
                Choice::new(Kind::Video.title(), summary(Kind::Video)),
                Choice::leaving("Back"),
            ],
            Screen::Listing(listing) => {
                let mut rows: Vec<Choice> = listing.rows.iter().map(|row| Choice::new(&row.label, row.note.clone())).collect();
                rows.push(Choice::leaving("Back"));
                rows
            }
            // Delete second, so the row under the pointer when this screen
            // opens is the harmless one.
            Screen::Confirm(_) => vec![Choice::leaving("Keep it"), Choice::new("Delete", "")],
            Screen::Done(_) | Screen::Soon(_) => vec![Choice::leaving("Back")],
        }
    }

    fn title(&self) -> &str {
        match &self.screen {
            Screen::Menu => "Save Timelapse",
            Screen::Watch => "Watch a timelapse",
            Screen::Recordings(_) => "Build from a recording",
            Screen::Opening(opening) => &opening.what,
            Screen::Places(_) => "Which places?",
            Screen::Interval(_) => "How often a picture?",
            Screen::CaptureGround(_) => "Add the ground?",
            Screen::GroundSave(_) => "Read the ground from which save?",
            Screen::Building(running) => &running.what,
            Screen::Done(_) => "Done",
            Screen::Render(render) => match render.step {
                RenderStep::Which => "Which timelapse?",
                RenderStep::Size => "How big?",
                RenderStep::Rate => "How smooth?",
                RenderStep::Place => "Which place?",
                RenderStep::Kind => "A video, or the frames?",
                RenderStep::Overlays => "Anything on top?",
            },
            Screen::Saves(_) => "Which saves?",
            Screen::Ground(_) => "Include the ground?",
            Screen::Manage => "Manage",
            Screen::Listing(listing) => listing.kind.title(),
            Screen::Confirm(confirm) => &confirm.label,
            Screen::Soon(what) => what,
        }
    }

    /// What a click on `index` does. Split from the event loop so the loop
    /// reads as "find what was clicked, then act on it".
    fn choose(&mut self, index: usize) {
        match &mut self.screen {
            Screen::Menu => match index {
                0 => {
                    self.timelapses = list_timelapses();
                    self.screen = Screen::Watch;
                }
                1 => {
                    self.screen = match recordings() {
                        found if found.is_empty() => Screen::Soon("No recordings found"),
                        found => Screen::Recordings(found),
                    }
                }
                2 => {
                    self.screen = match save_pick() {
                        Some(pick) => Screen::Saves(pick),
                        None => Screen::Done("No Factorio saves found.".to_string()),
                    }
                }
                3 => {
                    self.screen = match build::list_timelapses() {
                        built if built.is_empty() => Screen::Done("Build a timelapse first.".to_string()),
                        built => Screen::Render(Render {
                            built,
                            step: RenderStep::Which,
                            chosen: None,
                            surfaces: Vec::new(),
                            surface: None,
                            size: (1920, 1080),
                            fps: 30,
                            video: true,
                            // The clock is what most timelapses want, so it is
                            // on and the marker is not.
                            clock: true,
                            players: false,
                            has_players: false,
                        }),
                    }
                }
                4 => self.screen = Screen::Manage,
                _ => self.quit = true,
            },
            Screen::Watch => match self.timelapses.get(index) {
                // Its own process, because macroquad allows one window each
                // and this one is still needed. The same re-execution the menu
                // has always done, so a timelapse opens exactly as before.
                Some(chosen) => {
                    if let Ok(mut command) = build::viewer_command() {
                        let _ = command.arg(&chosen.path).stdout(std::process::Stdio::null()).spawn();
                    }
                }
                None => self.screen = Screen::Menu,
            },
            Screen::Recordings(found) => match found.get(index) {
                Some(chosen) => self.screen = Screen::Opening(start_opening(chosen)),
                None => self.screen = Screen::Menu,
            },
            // Nothing to cancel cleanly part way through a file read, so this
            // drops the thread's answer rather than pretending to stop it.
            Screen::Opening(_) => self.screen = Screen::Menu,
            Screen::Places(choosing) => {
                let places = choosing.picked.len();
                match index {
                    // A place toggles. Nothing confirms until the row below,
                    // so a mistaken tap costs one more tap rather than a build.
                    at if at < places => choosing.picked[at] = !choosing.picked[at],
                    // Continue, unless nothing is ticked: a build of nowhere
                    // would spend minutes producing an empty folder.
                    at if at == places && choosing.picked.iter().any(|p| *p) => {
                        let Screen::Places(choosing) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                            unreachable!("just matched")
                        };
                        self.screen = Screen::Interval(choosing);
                    }
                    at if at == places => {}
                    _ => self.screen = Screen::Menu,
                }
            }
            Screen::Interval(_) => {
                let Screen::Interval(mut choosing) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                    unreachable!("just matched")
                };
                self.screen = match INTERVALS.get(index) {
                    Some(&(seconds, _)) => {
                        choosing.seconds = seconds;
                        Screen::CaptureGround(choosing)
                    }
                    None => Screen::Menu,
                };
            }
            Screen::CaptureGround(_) => {
                let Screen::CaptureGround(mut choosing) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                    unreachable!("just matched")
                };
                self.screen = match index {
                    // Already read means there is nothing to ask: the save it
                    // came from stopped mattering once the ground was kept.
                    0 => match cached_for(&choosing.recording) {
                        cached if cached.is_empty() => Screen::GroundSave(choosing),
                        cached => {
                            choosing.ground = Some(GroundFrom::Cache(cached));
                            Screen::Building(start_build(choosing))
                        }
                    },
                    1 => Screen::Building(start_build(choosing)),
                    _ => Screen::Menu,
                };
            }
            Screen::GroundSave(_) => {
                let Screen::GroundSave(mut choosing) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                    unreachable!("just matched")
                };
                let saves = saves_newest_first();
                self.screen = match saves.get(index) {
                    Some((_, _, path)) => {
                        choosing.ground = Some(GroundFrom::Save(path.clone()));
                        Screen::Building(start_build(choosing))
                    }
                    None => Screen::Menu,
                };
            }
            // The flag, not the screen: the thread notices within a frame and
            // reports what it managed, and the window waits for that rather
            // than pretending it already stopped.
            Screen::Building(running) => running.stop(),
            Screen::Render(render) => match render.step {
                RenderStep::Which if index < render.built.len() => {
                    render.chosen = Some(index);
                    let chosen = &render.built[index];
                    render.surfaces = build::surfaces_in(&chosen.path);
                    // Only a live capture records where anybody was, and the
                    // marker means nothing without it.
                    render.has_players = chosen.path.join("players.jsonl").is_file();
                    // One world has no choice to offer, and the viewer picking
                    // the busiest of one is picking that one.
                    render.step = match render.surfaces.len() > 1 {
                        true => RenderStep::Place,
                        false => RenderStep::Size,
                    };
                }
                RenderStep::Place if index <= render.surfaces.len() => {
                    render.surface = match render.surfaces.get(index) {
                        Some(name) => Some(name.clone()),
                        // The row past the places is "one video for each",
                        // which the renderer already understands by name.
                        None => Some("all".to_string()),
                    };
                    render.step = RenderStep::Size;
                }
                RenderStep::Size if index < SIZES.len() => {
                    let (width, height, _) = SIZES[index];
                    render.size = (width, height);
                    render.step = RenderStep::Rate;
                }
                RenderStep::Rate if index < RATES.len() => {
                    render.fps = RATES[index].0;
                    render.step = RenderStep::Kind;
                }
                RenderStep::Kind if index < 2 => {
                    render.video = index == 0;
                    match render.video {
                        true => render.step = RenderStep::Overlays,
                        false => {
                            let Screen::Render(render) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                                unreachable!("just matched")
                            };
                            self.screen = Screen::Building(start_render(render));
                        }
                    }
                }
                RenderStep::Overlays => {
                    // The rows shift when there is no player log, so they are
                    // counted rather than numbered: clock, then the marker if
                    // it is offered, then render, then back.
                    let marker = render.has_players.then_some(1);
                    let render_row = 1 + marker.map_or(0, |_| 1);
                    match index {
                        0 => render.clock = !render.clock,
                        at if Some(at) == marker => render.players = !render.players,
                        at if at == render_row => {
                            let Screen::Render(render) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                                unreachable!("just matched")
                            };
                            self.screen = Screen::Building(start_render(render));
                        }
                        _ => render.step = RenderStep::Kind,
                    }
                }
                // Back, from wherever: one step at a time rather than out to
                // the menu, so changing one answer does not lose the others.
                RenderStep::Which => self.screen = Screen::Menu,
                RenderStep::Place => render.step = RenderStep::Which,
                RenderStep::Size => {
                    render.step = match render.surfaces.len() > 1 {
                        true => RenderStep::Place,
                        false => RenderStep::Which,
                    }
                }
                RenderStep::Rate => render.step = RenderStep::Size,
                RenderStep::Kind => render.step = RenderStep::Rate,
            },
            Screen::Saves(pick) => {
                let count = pick.saves.len();
                match index {
                    at if at < count => pick.picked[at] = !pick.picked[at],
                    at if at == count && pick.picked.iter().any(|p| *p) => {
                        let Screen::Saves(pick) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                            unreachable!("just matched")
                        };
                        self.screen = Screen::Ground(pick);
                    }
                    at if at == count => {}
                    _ => self.screen = Screen::Menu,
                }
            }
            Screen::Ground(_) => {
                let Screen::Ground(mut pick) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                    unreachable!("just matched")
                };
                self.screen = match index {
                    0 | 1 => {
                        pick.ground = index == 0;
                        Screen::Building(start_from_saves(pick))
                    }
                    _ => Screen::Menu,
                };
            }
            Screen::Manage => {
                self.screen = match index {
                    0 => open_listing(Kind::LogData),
                    1 => open_listing(Kind::Timelapse),
                    2 => open_listing(Kind::Video),
                    _ => Screen::Menu,
                }
            }
            Screen::Listing(listing) => match listing.rows.get(index) {
                Some(row) => {
                    self.screen = Screen::Confirm(Confirm {
                        kind: listing.kind,
                        label: format!("Delete {}?", row.label),
                        path: row.path.clone(),
                    })
                }
                None => self.screen = Screen::Manage,
            },
            Screen::Confirm(confirm) => {
                let kind = confirm.kind;
                self.screen = match index {
                    1 => match build::delete_path(&confirm.path) {
                        Ok(()) => open_listing(kind),
                        Err(e) => Screen::Done(format!("Could not delete it: {e}")),
                    },
                    _ => open_listing(kind),
                };
            }
            Screen::Done(_) | Screen::Soon(_) => self.screen = Screen::Menu,
        }
    }

    /// Moves on if the build has finished. Everything else about it is the
    /// job's own business.
    fn poll(&mut self) {
        if let Screen::Building(running) = &mut self.screen {
            if let Some(message) = running.drain() {
                self.screen = Screen::Done(message);
            }
            return;
        }

        let Screen::Opening(opening) = &mut self.screen else { return };
        let answer = match opening.loaded.try_recv() {
            Ok(answer) => answer,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("Reading that recording stopped unexpectedly.".to_string()),
        };
        let Screen::Opening(opening) = std::mem::replace(&mut self.screen, Screen::Menu) else { unreachable!("just matched") };
        self.screen = match answer {
            // Everywhere ticked to begin with: the common answer is all of
            // them, and starting from nothing means every build needs work
            // before it can start.
            Ok(loaded) => Screen::Places(Choosing {
                picked: vec![true; loaded.surfaces.len()],
                loaded,
                recording: opening.recording,
                seconds: 60,
                ground: None,
            }),
            Err(message) => Screen::Done(message),
        };
    }

    /// The program's name, drawn as the one thing here that is not a list.
    ///
    /// A faint wider copy of the glyphs behind the face, in eight directions
    /// at two radii. Offset copies are not a blur, so this is a halo rather
    /// than a real glow: eight directions rather than four, because the axes
    /// alone come out as a cross rather than as light around the letters.
    ///
    /// The radii scale with the size actually drawn, so a name shrunk to fit a
    /// narrow window keeps the same halo in proportion instead of one three
    /// times too wide for it. That, and its origin being rounded to a whole
    /// pixel, is what keeps the copies overlapping: where they stop
    /// overlapping the seams read as the text drawn twice.
    ///
    /// No outline and no sideways smear. Both were attempts at weight, and
    /// both are the same trick as the halo without its excuse of being faint.
    fn draw_name(&self, center_x: f32, baseline: f32, room: f32) {
        let name = "Save Timelapse";
        let natural = self.ui.width(name, NAME_SIZE);
        let size = match natural > room && room > 0.0 {
            true => (NAME_SIZE * room / natural).max(NAME_MIN_SIZE),
            false => NAME_SIZE,
        };

        let width = self.ui.width(name, size);
        let left = (center_x - width / 2.0).round();
        let baseline = baseline.round();

        let scale = size / NAME_SIZE;
        for ring in [3.0f32, 6.0] {
            let spread = ring * scale;
            let diagonal = spread * std::f32::consts::FRAC_1_SQRT_2;
            for (dx, dy) in [
                (-spread, 0.0),
                (spread, 0.0),
                (0.0, -spread),
                (0.0, spread),
                (-diagonal, -diagonal),
                (diagonal, -diagonal),
                (-diagonal, diagonal),
                (diagonal, diagonal),
            ] {
                self.ui.text(name, left + dx, baseline + dy, size, NAME_GLOW);
            }
        }
        self.ui.text(name, left, baseline, size, NAME);
    }

    fn draw(&self, column: &Column, choices: &[Choice], hovered: Option<usize>) {
        clear_background(BACKGROUND);

        let (center_x, center_y) = column.header_center(screen_width());
        // The logo goes here. Until there is one, the name carries the header
        // on its own rather than a placeholder box standing in for artwork
        // nobody has drawn yet.
        match self.screen {
            Screen::Menu => self.draw_name(center_x, center_y + NAME_SIZE / 2.0, screen_width() - 32.0),
            _ => {
                let title = self.title();
                let width = self.ui.width(title, TITLE_SIZE);
                self.ui.text(title, center_x - width / 2.0, center_y + TITLE_SIZE / 2.0, TITLE_SIZE, TEXT);
            }
        }

        for (index, choice) in choices.iter().enumerate() {
            if !column.visible(index) {
                continue;
            }
            let row = column.row(index);
            let lit = hovered == Some(index);
            let fill = match (choice.leave, lit) {
                (true, true) => LEAVE_ROW_HOVER,
                (true, false) => LEAVE_ROW,
                (false, true) => ROW_HOVER,
                (false, false) => ROW,
            };
            draw_rectangle(row.x, row.y, row.width, row.height, fill);
            draw_rectangle_lines(row.x, row.y, row.width, row.height, 1.0, ROW_EDGE);

            let baseline = row.text_baseline(LABEL_SIZE);
            let colour = match (choice.leave, lit) {
                (true, _) => LEAVE,
                (false, true) => ACCENT,
                (false, false) => TEXT,
            };

            let measure_note = |text: &str| self.ui.width(text, NOTE_SIZE);
            let (label_room, note_room) = share_row(row.width, measure_note(&choice.note));
            let label = elide(&choice.label, label_room, &|text| self.ui.width(text, LABEL_SIZE));
            self.ui.text(&label, row.x + ROW_PAD, baseline, LABEL_SIZE, colour);

            if note_room > 0.0 {
                let note = elide(&choice.note, note_room, &measure_note);
                let width = measure_note(&note);
                self.ui.text(&note, row.x + row.width - ROW_PAD - width, baseline, NOTE_SIZE, TEXT_DIM);
            }

            // A ticked place gets an edge as well as a word, so which rows are
            // in is readable at a glance rather than by reading every note.
            if choice.note == "included" {
                draw_rectangle(row.x, row.y, 3.0, row.height, ACCENT);
            }
        }

        // A list that scrolls says so, otherwise the rows below the fold are
        // as unreachable as they were before scrolling existed.
        if column.max_scroll > 0.0 {
            let track_x = column.x + column.width + 8.0;
            let track_top = column.view_top;
            let track_height = column.view_bottom - column.view_top;
            draw_rectangle(track_x, track_top, 4.0, track_height, ROW);

            let shown = track_height / (track_height + column.max_scroll);
            let thumb = (track_height * shown).max(24.0);
            let at = track_top + (track_height - thumb) * (column.scroll / column.max_scroll);
            draw_rectangle(track_x, at, 4.0, thumb, ROW_HOVER);
        }

        if let Screen::Confirm(confirm) = &self.screen {
            let warning = confirm.kind.warning();
            let width = self.ui.width(warning, NOTE_SIZE);
            // Wrapped by hand only when it has to be: one line reads better,
            // and the long warning is the one that never fits.
            match width < column.width * 1.6 {
                true => self.ui.text(warning, (screen_width() - width) / 2.0, column.top - 30.0, NOTE_SIZE, TEXT_DIM),
                false => {
                    for (line, part) in wrapped(warning, 64).iter().enumerate() {
                        let width = self.ui.width(part, NOTE_SIZE);
                        let y = column.top - 52.0 + line as f32 * 20.0;
                        self.ui.text(part, (screen_width() - width) / 2.0, y, NOTE_SIZE, TEXT_DIM);
                    }
                }
            }
        }

        if let Screen::Opening(_) = &self.screen {
            let line = "Reading it. This takes a while on a big factory.";
            let width = self.ui.width(line, NOTE_SIZE);
            self.ui.text(line, (screen_width() - width) / 2.0, column.top - 30.0, NOTE_SIZE, TEXT_DIM);
        }

        if let Screen::Building(running) = &self.screen {
            let line = match (running.counts, running.frames) {
                (false, _) => running.waiting.clone(),
                (true, None) => "Starting...".to_string(),
                (true, Some(count)) => format!("{} frames", crate::with_thousands(count as u64)),
            };
            let width = self.ui.width(&line, LABEL_SIZE);
            self.ui.text(&line, (screen_width() - width) / 2.0, column.top - 54.0, LABEL_SIZE, ACCENT);

            // A bar with nothing to measure against, because none of these
            // know their total until they are over: a recording's frame count
            // depends on how long it was played, and a render's window shows
            // its own. It says "still going" rather than "this far along",
            // which is the honest amount to claim.
            let bar = layout::Rect { x: column.x, y: column.top - 36.0, width: column.width, height: 6.0 };
            draw_rectangle(bar.x, bar.y, bar.width, bar.height, ROW);
            let sweep = (get_time() as f32 * 0.6).fract();
            let lit = bar.width * 0.25;
            // Wrapped rather than bounced, so it reads as movement in one
            // direction rather than as something stuck.
            let at = bar.x + (bar.width + lit) * sweep - lit;
            let from = at.max(bar.x);
            let to = (at + lit).min(bar.x + bar.width);
            if to > from {
                draw_rectangle(from, bar.y, to - from, bar.height, ACCENT);
            }
        }

        if let Screen::Done(message) = &self.screen {
            let width = self.ui.width(message, NOTE_SIZE);
            self.ui.text(message, (screen_width() - width) / 2.0, column.top - 30.0, NOTE_SIZE, TEXT_DIM);
        }

        if let Screen::Watch = self.screen {
            if self.timelapses.is_empty() {
                let message = "Nothing built yet.";
                let width = self.ui.width(message, NOTE_SIZE);
                self.ui.text(message, (screen_width() - width) / 2.0, column.top - 24.0, NOTE_SIZE, TEXT_DIM);
            }
        }
    }
}

/// Ground already read for this playthrough, if any.
fn cached_for(recording: &Recording) -> Vec<std::path::PathBuf> {
    let Some(user_dir) = crate::locate::factorio_user_dir() else { return Vec::new() };
    build::cached_ground(&user_dir, recording.session_id)
}

/// The saves this machine has, newest first, as label, age and path.
///
/// Newest first because ground only exists where a save had already been, so a
/// later save can only ever cover more of the factory.
fn saves_newest_first() -> Vec<(String, String, std::path::PathBuf)> {
    let Some(user_dir) = crate::locate::factorio_user_dir() else { return Vec::new() };
    let now = std::time::SystemTime::now();

    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(user_dir.join("saves"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("zip"))
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    found
        .into_iter()
        .map(|(modified, path)| {
            let label = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
            let age = describe::describe_age(now.duration_since(modified).unwrap_or_default());
            (label, age, path)
        })
        .collect()
}

/// Puts the ground beside the frames, however it was chosen.
fn add_ground(from: GroundFrom, out: &std::path::Path, session_id: u32) -> String {
    let cached = match from {
        GroundFrom::Cache(cached) => {
            return match build::reuse_ground(&cached, out) {
                0 => "The ground already read for this playthrough could not be copied.".to_string(),
                _ => "Reused the ground already read for this playthrough.".to_string(),
            }
        }
        GroundFrom::Save(save) => save,
    };

    let Some(factorio) = crate::locate::locate_factorio() else {
        return "Could not find factorio.exe, so the ground was not read.".to_string();
    };
    let Some(user_dir) = crate::locate::factorio_user_dir() else {
        return "Could not find your Factorio folder, so the ground was not read.".to_string();
    };
    let Ok(mod_source) = build::mod_source_dir() else {
        return "Could not find the mod folder, so the ground was not read.".to_string();
    };

    let config = crate::export::ExportConfig {
        factorio,
        user_mods: user_dir.join("mods"),
        mod_source,
        include_resources: false,
        capture_terrain: true,
        terrain_scan: true,
    };
    match build::scan_ground(&cached, out, &config, Some(session_id)) {
        Ok(surfaces) => format!("Ground added for {surfaces} place(s)."),
        Err(message) => message,
    }
}

/// Renders a video on its own thread.
///
/// Reports nothing while it runs, because the renderer opens its own window
/// and shows the frames as it writes them. What the thread buys is a menu that
/// still answers while that happens.
fn start_render(render: Render) -> Running {
    let (send, updates) = channel();
    let cancel = Arc::new(AtomicBool::new(false));

    let chosen = &render.built[render.chosen.expect("a timelapse was chosen to get here")];
    let what = format!("Rendering {}", chosen.name);
    let request = build::VideoRequest {
        timelapse: chosen.path.clone(),
        target: build::videos_root().join(build::as_folder_name(&chosen.name)),
        width: render.size.0,
        height: render.size.1,
        surface: render.surface.clone(),
        video: render.video,
        fps: render.fps,
        // Only when FFmpeg is already installed: an MP4 is smaller and is what
        // sharing sites accept, and nothing here asks anybody to go and get it.
        mp4: render.video && crate::ffmpeg_available(),
        overlay_players: render.video && render.players,
        overlay_clock: render.video && render.clock,
    };

    std::thread::spawn(move || {
        let ended = match std::fs::create_dir_all(request.target.parent().unwrap_or(&request.target))
            .and_then(|()| build::video(&request))
        {
            Ok(()) => format!("Saved to {}", request.target.display()),
            Err(e) => format!("The render failed: {e}"),
        };
        let _ = send.send(Update::Ended(ended));
    });

    // A render counts nothing here: the window it opens shows its own frames,
    // and a second count would be a number nobody can check against it.
    Running { updates, cancel, what, frames: None, counts: false, waiting: "Rendering in its own window...".to_string() }
}

/// The saves this machine has, newest first, with nothing ticked.
///
/// Nothing rather than everything, unlike the places picker: every save is a
/// full Factorio run, so a build of all forty is an hour somebody did not ask
/// for. Places cost nothing extra to include.
fn save_pick() -> Option<SavePick> {
    let user_dir = crate::locate::factorio_user_dir()?;
    let now = std::time::SystemTime::now();

    let mut found: Vec<(std::time::SystemTime, std::path::PathBuf)> = std::fs::read_dir(user_dir.join("saves"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("zip"))
        .filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path)))
        .collect();
    if found.is_empty() {
        return None;
    }
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));

    let labels = found.iter().map(|(_, path)| path.file_stem().unwrap_or_default().to_string_lossy().into_owned()).collect();
    let notes =
        found.iter().map(|(modified, _)| describe::describe_age(now.duration_since(*modified).unwrap_or_default())).collect();
    let picked = vec![false; found.len()];
    let saves = found.into_iter().map(|(_, path)| path).collect();

    Some(SavePick { saves, labels, notes, picked, ground: false })
}

/// Builds from the chosen saves on its own thread.
///
/// The order matters and is not the order they were ticked: a timelapse runs
/// forwards, so the saves go in oldest first however the list showed them.
fn start_from_saves(pick: SavePick) -> Running {
    let (send, updates) = channel();
    let cancel = Arc::new(AtomicBool::new(false));

    let mut saves = pick.chosen();
    saves.reverse();
    let ground = pick.ground;
    let flag = Arc::clone(&cancel);
    let what = format!("Building from {} saves", saves.len());

    std::thread::spawn(move || {
        let ended = from_saves_on_thread(&saves, ground, &flag, &send);
        let _ = send.send(Update::Ended(ended));
    });

    Running { updates, cancel, what, frames: None, counts: true, waiting: "Starting Factorio...".to_string() }
}

/// The from-saves build, as one function returning the sentence to show.
fn from_saves_on_thread(
    saves: &[std::path::PathBuf],
    ground: bool,
    cancel: &AtomicBool,
    send: &std::sync::mpsc::Sender<Update>,
) -> String {
    let Some(factorio) = crate::locate::locate_factorio() else {
        return "Could not find factorio.exe. Build from a recording instead, or start the console menu once to point it at your install.".to_string();
    };
    let Some(user_dir) = crate::locate::factorio_user_dir() else {
        return "Could not find your Factorio folder.".to_string();
    };
    let Ok(mod_source) = build::mod_source_dir() else {
        return "Could not find the mod folder that has to sit beside this program.".to_string();
    };

    let config = crate::export::ExportConfig {
        factorio,
        user_mods: user_dir.join("mods"),
        mod_source,
        include_resources: false,
        capture_terrain: ground,
        terrain_scan: false,
    };

    let name = saves.last().and_then(|s| s.file_stem()).map(|s| s.to_string_lossy().into_owned());
    let out = build::timelapses_root().join(build::as_folder_name(&name.unwrap_or_else(|| "timelapse".to_string())));
    let _ = std::fs::remove_dir_all(&out);
    if let Err(e) = std::fs::create_dir_all(&out) {
        return format!("Could not make a folder for it: {e}");
    }
    let workspace = std::env::temp_dir().join(format!("save-timelapse-gui-{}", std::process::id()));

    // Counted rather than named: the row shows a running total the same way a
    // build from a recording does, and the saves have names too long for it.
    let mut done = 0usize;
    let mut on_save = |step: build::SaveStep| {
        if let build::SaveStep::Exported { .. } = step {
            done += 1;
            let _ = send.send(Update::Frames(done));
        }
    };
    let exported = match build::from_saves(saves, &out, &workspace, &config, &mut build::Watch { on: &mut on_save, cancel }) {
        Ok(exported) => exported,
        Err(e) => return format!("The build failed: {e}"),
    };
    let _ = std::fs::remove_dir_all(&workspace);

    if exported.frames.is_empty() {
        return "None of those saves could be exported.".to_string();
    }
    // Best effort from here: a timelapse that exists beats one abandoned for a
    // step that only makes it smaller or prettier.
    let _ = build::write_as_delta_chain(&exported.frames);
    let milestones = crate::milestone::from_saves(exported.milestones);
    if !milestones.is_empty() {
        let _ = crate::milestone::write_jsonl(&out.join("milestones.jsonl"), &milestones);
    }

    let stopped = if exported.cancelled { "Stopped. " } else { "" };
    format!("{stopped}Built {} frames in {}", exported.frames.len(), out.display())
}

/// How much of one kind there is, for the row that opens it.
fn summary(kind: Kind) -> String {
    let rows = deletables(kind);
    let bytes: u64 = rows.iter().map(|row| build::size_on_disk(&row.path)).sum();
    format!("{}, {}", rows.len(), describe::describe_size(bytes))
}

fn open_listing(kind: Kind) -> Screen {
    Screen::Listing(Listing { kind, rows: deletables(kind) })
}

/// What one kind holds, newest first.
///
/// All three read the disk each time rather than being cached: deleting one
/// changes the answer, and so does playing the game in the background.
fn deletables(kind: Kind) -> Vec<Deletable> {
    match kind {
        Kind::LogData => sessions()
            .into_iter()
            .map(|session| {
                let places = describe::describe_places(&session.baseline.surfaces);
                Deletable {
                    label: session.label().unwrap_or_else(|| places.clone()),
                    note: describe::describe_size(session.size_on_disk()),
                    path: session.session_dir,
                }
            })
            .collect(),
        Kind::Timelapse => build::list_timelapses()
            .into_iter()
            .map(|built| Deletable {
                label: built.name,
                note: format!("{} frames, {}", built.frames, describe::describe_size(built.bytes)),
                path: built.path,
            })
            .collect(),
        Kind::Video => build::list_videos()
            .into_iter()
            .map(|video| {
                let age = video.modified.elapsed().map(describe::describe_age).unwrap_or_else(|_| "unknown".to_string());
                Deletable {
                    label: video.name,
                    note: format!("{}, {age}", describe::describe_size(video.bytes)),
                    path: video.path,
                }
            })
            .collect(),
    }
}

/// Every recording this machine has, or none if Factorio cannot be found.
fn sessions() -> Vec<replay::Session> {
    let Some(user_dir) = crate::locate::factorio_user_dir() else { return Vec::new() };
    replay::discover_sessions(&user_dir.join("script-output").join("save-timelapse")).unwrap_or_default()
}

/// Every recording, newest first, described the way a row wants it.
fn recordings() -> Vec<Recording> {
    let now = std::time::SystemTime::now();
    sessions()
        .into_iter()
        .map(|session| {
            let places = describe::describe_places(&session.baseline.surfaces);
            let label = session.label().unwrap_or_else(|| places.clone());
            let age = describe::describe_age(now.duration_since(session.last_modified).unwrap_or_default());
            Recording {
                session_id: session.session_id,
                note: format!("{}, {age}", describe::describe_play_time(session.played_tick())),
                name: session.label().unwrap_or_else(|| format!("{places} ({:08x})", session.session_id)),
                label,
                session_dir: session.session_dir,
                baseline_path: session.baseline_path,
            }
        })
        .collect()
}

/// Reads a recording's baseline on its own thread.
///
/// Its own phase because it is slow enough to need a screen of its own, and
/// because what it produces is what the next two screens ask about: the places
/// to choose between are the ones this finds.
fn start_opening(chosen: &Recording) -> Opening {
    let (send, loaded) = channel();
    let baseline_path = chosen.baseline_path.clone();
    let session_dir = chosen.session_dir.clone();

    std::thread::spawn(move || {
        let answer = match replay::load_baseline(&baseline_path) {
            Ok(replay) => match replay::discover_surfaces(&session_dir, &replay) {
                Ok(surfaces) => Ok(Box::new(Loaded { replay, surfaces })),
                Err(e) => Err(format!("Could not tell where this recording has been: {e}")),
            },
            Err(e) => Err(format!("Could not read that recording: {e}")),
        };
        let _ = send.send(answer);
    });

    Opening { loaded, what: format!("Reading {}", chosen.label), recording: chosen.clone() }
}

/// Starts a build on its own thread and hands back the two ends the window
/// needs: what it says, and how to stop it.
///
/// Takes the recording it was given rather than reading it again: the baseline
/// is already in hand from the screen before, and on a megabase reading it
/// twice is the slowest thing here done for nothing.
fn start_build(choosing: Choosing) -> Running {
    let (send, updates) = channel();
    let cancel = Arc::new(AtomicBool::new(false));

    let surfaces = choosing.chosen_surfaces();
    let out = build::timelapses_root().join(build::as_folder_name(&choosing.recording.name));
    let session_dir = choosing.recording.session_dir.clone();
    let what = format!("Building {}", choosing.recording.label);
    let seconds = choosing.seconds;
    let ground = choosing.ground.clone();
    let session_id = choosing.recording.session_id;
    let mut replay = choosing.loaded.replay;
    let flag = Arc::clone(&cancel);

    std::thread::spawn(move || {
        let built = build_on_thread(&mut replay, &session_dir, &out, surfaces, seconds, &flag, &send);
        // Ground last, and only if the frames worked: it is the slow half, and
        // there is nothing to lay it under otherwise.
        let ended = match ground {
            Some(from) if !built.starts_with("The build failed") && !built.starts_with("Could not") => {
                format!(
                    "{built}
{}",
                    add_ground(from, &out, session_id)
                )
            }
            _ => built,
        };
        let _ = send.send(Update::Ended(ended));
    });

    Running { updates, cancel, what, frames: None, counts: true, waiting: "Reading the recording...".to_string() }
}

/// The build itself, as one function returning the sentence to show. Split out
/// so the thread body is two lines and every failure has somewhere to go.
#[allow(clippy::too_many_arguments)]
fn build_on_thread(
    replay: &mut replay::Replay,
    session_dir: &std::path::Path,
    out: &std::path::Path,
    surfaces: Vec<String>,
    seconds: u64,
    cancel: &AtomicBool,
    send: &std::sync::mpsc::Sender<Update>,
) -> String {
    let _ = std::fs::remove_dir_all(out);
    if let Err(e) = std::fs::create_dir_all(out) {
        return format!("Could not make a folder for it: {e}");
    }

    let plan = build::Plan {
        surfaces,
        options: replay::Options { interval: seconds * crate::viewer::TICKS_PER_SECOND, max_frames: 20_000 },
    };
    let mut on_frame = |written: usize| {
        let _ = send.send(Update::Frames(written));
    };
    match build::timelapse(replay, session_dir, out, &plan, &mut build::Watch { on: &mut on_frame, cancel }) {
        Ok(built) if built.cancelled => format!("Stopped after {} frames. What was written is usable.", built.written),
        Ok(built) => format!("Built {} frames in {}", built.written, out.display()),
        Err(e) => format!("The build failed: {e}"),
    }
}

/// Every built timelapse, newest first, described the way a row wants it.
///
/// The walking and the counting live in `build`, shared with the console menu:
/// two front ends listing the same folder separately is how they come to
/// disagree about what exists.
fn list_timelapses() -> Vec<Built> {
    build::list_timelapses()
        .into_iter()
        .map(|found| Built { name: found.name, path: found.path, note: format!("{} frames", found.frames) })
        .collect()
}

/// Splits `text` into lines of at most `columns` characters, breaking at
/// spaces. Crude on purpose: the only long text here is one warning, and a
/// real wrapper would need the font to measure against.
fn wrapped(text: &str, columns: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    for word in text.split_whitespace() {
        let line = lines.last_mut().expect("seeded with one");
        if !line.is_empty() && line.len() + 1 + word.len() > columns {
            lines.push(word.to_string());
        } else {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
    }
    lines
}

pub fn window_conf() -> macroquad::conf::Conf {
    macroquad::conf::Conf {
        miniquad_conf: miniquad::conf::Conf {
            window_title: "Save Timelapse".to_string(),
            window_width: 960,
            window_height: 720,
            high_dpi: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

pub async fn run() {
    let mut app = App { timelapses: list_timelapses(), ..Default::default() };

    while !app.quit {
        let choices = app.choices();
        let column = Column::centered(screen_width(), screen_height(), choices.len(), ROW_HEIGHT, app.scroll);
        // Clamped back from what the column worked out, so a wheel spun past
        // the end does not keep counting up invisibly and then need spinning
        // all the way back.
        app.scroll = column.scroll;

        let (_, wheel) = mouse_wheel();
        if wheel != 0.0 && column.max_scroll > 0.0 {
            app.scroll = (app.scroll - wheel * ROW_HEIGHT).clamp(0.0, column.max_scroll);
        }

        let (mouse_x, mouse_y) = mouse_position();
        let hovered = column.hit(mouse_x, mouse_y);

        if is_mouse_button_pressed(MouseButton::Left) {
            if let Some(index) = hovered {
                let was = std::mem::discriminant(&app.screen);
                app.choose(index);
                if was != std::mem::discriminant(&app.screen) {
                    app.scroll = 0.0;
                }
            }
        }
        // Escape goes back one level, and out of the menu entirely, so the
        // keyboard reaches every exit the mouse does.
        if is_key_pressed(KeyCode::Escape) {
            match app.screen {
                Screen::Menu => app.quit = true,
                _ => app.screen = Screen::Menu,
            }
            app.scroll = 0.0;
        }

        app.poll();
        app.draw(&column, &choices, hovered);
        next_frame().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::Sender;

    /// A running build and the end a thread would send on. Deliberately not an
    /// `App`: building one loads a font through macroquad, which needs a
    /// window, and none of this needs either.
    fn running() -> (Running, Sender<Update>) {
        let (send, updates) = channel();
        let running = Running {
            updates,
            cancel: Arc::new(AtomicBool::new(false)),
            what: "Building nauvis".to_string(),
            frames: None,
            counts: true,
            waiting: "Reading the recording...".to_string(),
        };
        (running, send)
    }

    /// A build reports far more often than the window redraws. Reading one
    /// update a frame would fall behind and never catch up, so a frame takes
    /// everything waiting and shows the last of it.
    #[test]
    fn a_frame_drains_every_update_waiting_rather_than_one() {
        let (mut job, send) = running();
        for count in 1..=500 {
            send.send(Update::Frames(count)).unwrap();
        }
        assert_eq!(job.drain(), None, "not over yet");
        assert_eq!(job.frames, Some(500));
    }

    /// A build that has not written anything yet shows no count at all. Zero
    /// would claim work that has not started, and the screen says "Starting"
    /// instead.
    #[test]
    fn nothing_is_counted_until_the_first_frame_lands() {
        let (mut job, send) = running();
        assert_eq!(job.drain(), None);
        assert_eq!(job.frames, None);

        send.send(Update::Frames(1)).unwrap();
        job.drain();
        assert_eq!(job.frames, Some(1));
    }

    #[test]
    fn the_closing_sentence_comes_back_once() {
        let (mut job, send) = running();
        send.send(Update::Frames(9)).unwrap();
        send.send(Update::Ended("Built 9 frames".to_string())).unwrap();
        assert_eq!(job.drain().as_deref(), Some("Built 9 frames"));
        assert_eq!(job.frames, Some(9), "what it reported before finishing still stands");
    }

    /// A thread that died without a word would otherwise leave a figure that
    /// never changes again and no way to tell it is over.
    #[test]
    fn a_thread_that_vanishes_still_ends() {
        let (mut job, send) = running();
        drop(send);
        let ended = job.drain().expect("a disconnected build must not sit there forever");
        assert!(ended.contains("unexpectedly"), "{ended}");
    }

    fn places(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The common answer is all of them, so a build needs no work before it
    /// can start.
    #[test]
    fn every_place_starts_ticked() {
        let surfaces = places(&["nauvis", "vulcanus"]);
        assert_eq!(chosen_of(&surfaces, &vec![true; surfaces.len()]), ["nauvis", "vulcanus"]);
    }

    /// Order matters: the chosen names go to the writer, which treats exactly
    /// one place differently from several.
    #[test]
    fn unticking_leaves_the_rest_in_their_original_order() {
        let surfaces = places(&["nauvis", "vulcanus", "gleba"]);
        assert_eq!(chosen_of(&surfaces, &[true, false, true]), ["nauvis", "gleba"]);
        assert_eq!(chosen_of(&surfaces, &[false, false, true]), ["gleba"]);
    }

    #[test]
    fn unticking_everything_chooses_nothing() {
        assert!(chosen_of(&places(&["nauvis", "vulcanus"]), &[false, false]).is_empty());
    }

    /// Four points cover the trade between smoothness and file size, and each
    /// has to say what it means in game time rather than in ticks.
    #[test]
    fn the_intervals_offered_are_ordered_and_distinct() {
        let seconds: Vec<u64> = INTERVALS.iter().map(|(s, _)| *s).collect();
        assert!(seconds.windows(2).all(|w| w[0] < w[1]), "{seconds:?}");
        assert!(INTERVALS.iter().all(|(_, label)| !label.is_empty()));
    }

    fn save_pick_of(saves: &[&str], picked: &[bool]) -> SavePick {
        SavePick {
            saves: saves.iter().map(std::path::PathBuf::from).collect(),
            labels: saves.iter().map(|s| s.to_string()).collect(),
            notes: vec![String::new(); saves.len()],
            picked: picked.to_vec(),
            ground: false,
        }
    }

    /// Nothing ticked to begin with, unlike places: every save is a full
    /// Factorio run, so a build of all forty is an hour nobody asked for.
    #[test]
    fn saves_start_unticked_and_keep_their_order() {
        let pick = save_pick_of(&["c.zip", "b.zip", "a.zip"], &[false, true, true]);
        assert_eq!(pick.chosen(), [std::path::PathBuf::from("b.zip"), std::path::PathBuf::from("a.zip")]);
        assert!(save_pick_of(&["a.zip"], &[false]).chosen().is_empty());
    }

    /// The list shows newest first, but a timelapse runs forwards, so what
    /// reaches the exporter has to be reversed. Getting this backwards builds
    /// a factory that shrinks.
    #[test]
    fn the_chosen_saves_are_reversed_into_playing_order() {
        let mut order = save_pick_of(&["newest.zip", "middle.zip", "oldest.zip"], &[true, true, true]).chosen();
        order.reverse();
        let names: Vec<String> = order.iter().map(|p| p.display().to_string()).collect();
        assert_eq!(names, ["oldest.zip", "middle.zip", "newest.zip"]);
    }

    /// Every step has to be reachable and every option say what it means.
    #[test]
    fn the_render_steps_offer_something_at_each_one() {
        assert!(SIZES.iter().all(|(w, h, label)| *w > 0 && *h > 0 && !label.is_empty()));
        assert!(RATES.iter().all(|(fps, label)| *fps > 0 && !label.is_empty()));
        assert_eq!(SIZES[0].0, 1920, "the recommended size leads");
        assert_eq!(RATES[0].0, 30, "and the recommended rate");
    }

    /// Ground read once for a playthrough is reused, so a rebuild does not
    /// launch Factorio to be told the same thing. The screen has to say which
    /// it will be, because one is instant and the other is a game load.
    #[test]
    fn cached_ground_is_only_reused_when_it_holds_scenery() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("script-output").join("save-timelapse").join("0000002a");
        std::fs::create_dir_all(&session).unwrap();

        let bare = crate::frame::FrameOut { tick: 0, surface: "nauvis", ..Default::default() };
        std::fs::write(session.join("terrain_nauvis.stfr"), crate::frame::write_binary(&bare)).unwrap();
        assert!(
            build::cached_ground(dir.path(), 42).is_empty(),
            "ground read before the scan collected scenery must not be reused forever"
        );

        let trees = vec![crate::frame::Entity { n: "tree-01".into(), x: 0.5, y: 0.5, d: 0, w: 1, h: 1 }];
        let scanned = crate::frame::FrameOut { tick: 0, surface: "nauvis", entities: &trees, ..Default::default() };
        std::fs::write(session.join("terrain_nauvis.stfr"), crate::frame::write_binary(&scanned)).unwrap();
        assert_eq!(build::cached_ground(dir.path(), 42).len(), 1);
    }

    fn a_render(video: bool, has_players: bool) -> Render {
        Render {
            built: Vec::new(),
            step: RenderStep::Overlays,
            chosen: Some(0),
            surfaces: Vec::new(),
            surface: None,
            size: (1920, 1080),
            fps: 30,
            video,
            clock: true,
            players: false,
            has_players,
        }
    }

    /// The overlay rows shift when there is no player log, so which row does
    /// what is counted rather than numbered. Getting it wrong renders with the
    /// opposite of what was asked, several minutes later.
    #[test]
    fn the_render_row_moves_when_the_marker_is_not_offered() {
        // Clock, marker, render, back.
        let with_marker = a_render(true, true);
        assert_eq!(with_marker.has_players.then_some(1), Some(1));

        // Clock, render, back: the marker's row is gone, not merely disabled.
        let without = a_render(true, false);
        assert_eq!(without.has_players.then_some(1), None);
    }

    /// Most timelapses want the clock and do not want the marker, so that is
    /// where the toggles start.
    #[test]
    fn the_clock_starts_on_and_the_marker_off() {
        let render = a_render(true, true);
        assert!(render.clock);
        assert!(!render.players);
    }

    /// Overlays are burnt into a video. A folder of frames is for somebody
    /// else's editor to label, so neither flag may reach one.
    #[test]
    fn an_image_sequence_carries_neither_overlay() {
        let mut frames = a_render(false, true);
        frames.clock = true;
        frames.players = true;
        assert!(!(frames.video && frames.clock));
        assert!(!(frames.video && frames.players));
    }

    /// Ten pixels a character, so a width in this test is a character count.
    fn ten_per_char(text: &str) -> f32 {
        text.chars().count() as f32 * 10.0
    }

    #[test]
    fn text_that_fits_is_left_alone() {
        assert_eq!(elide("nauvis", 200.0, &ten_per_char), "nauvis");
        assert_eq!(elide("nauvis", 60.0, &ten_per_char), "nauvis", "exactly filling is still fitting");
    }

    /// The point of the dots: a cut that says nothing was cut reads as a
    /// shorter name rather than as a longer one that did not fit.
    #[test]
    fn text_too_long_is_cut_and_says_so() {
        let cut = elide("a very long timelapse name", 100.0, &ten_per_char);
        assert!(cut.ends_with("..."), "{cut}");
        assert!(ten_per_char(&cut) <= 100.0, "{cut} is {} wide", ten_per_char(&cut));
        assert!(cut.starts_with("a very"), "what is left has to be the start: {cut}");
    }

    /// A box too small even for the dots gets nothing rather than something
    /// wider than the box it is in.
    #[test]
    fn a_box_too_small_for_even_the_dots_draws_nothing() {
        assert_eq!(elide("nauvis", 10.0, &ten_per_char), "");
        assert_eq!(elide("nauvis", 0.0, &ten_per_char), "");
    }

    /// Cut between letters, never inside one: trimming by byte would split a
    /// multi-byte character and produce something unprintable.
    #[test]
    fn a_cut_lands_between_characters() {
        let cut = elide("vegetation-turquoise-grass", 90.0, &|text| text.chars().count() as f32 * 10.0);
        assert!(cut.is_char_boundary(cut.len()));
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    /// The bug this exists for: a label and a note drawn from opposite edges
    /// have nothing stopping them meeting, and on a narrow window they did.
    #[test]
    fn a_label_and_its_note_never_overlap() {
        for width in [120.0f32, 200.0, 320.0, 620.0] {
            for note in [10.0f32, 80.0, 400.0] {
                let (label_room, note_room) = share_row(width, note);
                let used = label_room + note_room + ROW_PAD * 2.0 + if note_room > 0.0 { ROW_GAP } else { 0.0 };
                assert!(used <= width + 0.01, "row {width} overflows at note {note}: {label_room} + {note_room}");
            }
        }
    }

    /// A note gives way before the label does: the label is what a row is.
    #[test]
    fn the_note_gives_up_room_before_the_label() {
        let (label_room, note_room) = share_row(400.0, 900.0);
        assert!(note_room <= (400.0 - ROW_PAD * 2.0) * NOTE_SHARE + 0.01, "{note_room}");
        assert!(label_room > 0.0, "the label must keep something: {label_room}");
    }

    /// No note means the label gets everything, which is the common row.
    #[test]
    fn a_row_with_no_note_gives_the_label_the_whole_width() {
        let (label_room, note_room) = share_row(400.0, 0.0);
        assert_eq!(note_room, 0.0);
        assert_eq!(label_room, 400.0 - ROW_PAD * 2.0);
    }

    /// Every screen has a way out and it has to be the one that stands out,
    /// since that is the whole point of colouring it differently.
    #[test]
    fn the_way_out_is_marked_rather_than_recognised_from_its_label() {
        assert!(Choice::leaving("Back").leave);
        assert!(!Choice::new("Back to the future", "").leave, "a label is not what makes a row an exit");
        assert!(!Choice::new("Continue", "3 of 4").leave);
    }

    /// Deleting log data is the one thing here that cannot be undone, so it
    /// says so rather than sharing a sentence with the two that can.
    #[test]
    fn only_log_data_warns_that_it_cannot_be_recovered() {
        assert!(Kind::LogData.warning().contains("cannot be recovered"));
        assert!(Kind::Timelapse.warning().contains("build it again"));
        assert!(Kind::Video.warning().contains("save it again"));
    }

    #[test]
    fn every_kind_is_named_for_what_it_holds() {
        for kind in [Kind::LogData, Kind::Timelapse, Kind::Video] {
            assert!(!kind.title().is_empty());
            assert!(!kind.warning().is_empty());
        }
    }

    #[test]
    fn wrapping_breaks_at_spaces_and_keeps_every_word() {
        let text = "the only copy of that playthrough's history and nothing else";
        let lines = wrapped(text, 20);
        assert!(lines.iter().all(|line| line.len() <= 20), "{lines:?}");
        assert_eq!(lines.join(" "), text, "no word may be lost or split");
    }

    /// A word longer than the limit goes on its own line rather than looping
    /// forever looking for a break that is not there.
    #[test]
    fn a_word_longer_than_the_line_still_terminates() {
        let lines = wrapped("short verylongwordthatcannotfit end", 10);
        assert_eq!(lines, ["short", "verylongwordthatcannotfit", "end"]);
    }

    /// Stopping raises the flag and nothing else: the thread notices within a
    /// frame and reports what it managed, and moving on would lose that.
    #[test]
    fn stopping_raises_the_flag_and_leaves_the_job_running() {
        let (job, _send) = running();
        assert!(!job.cancel.load(Ordering::Relaxed));
        job.stop();
        assert!(job.cancel.load(Ordering::Relaxed));
        assert_eq!(job.frames, None, "stopping reports nothing of its own");
    }
}
