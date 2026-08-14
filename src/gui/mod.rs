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

const ROW_HEIGHT: f32 = 46.0;
const LABEL_SIZE: f32 = 20.0;
const NOTE_SIZE: f32 = 16.0;
const TITLE_SIZE: f32 = 34.0;

/// One choice in a column: what it says, and the quieter thing it says on the
/// right. The note is where a count goes, so a row can carry "3 ready" without
/// a second column to align.
struct Choice {
    label: String,
    note: String,
}

impl Choice {
    fn new(label: &str, note: impl Into<String>) -> Choice {
        Choice { label: label.to_string(), note: note.into() }
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
    size: (u32, u32),
    fps: u32,
    video: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum RenderStep {
    Which,
    Size,
    Rate,
    Kind,
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
}

impl Choosing {
    fn chosen_surfaces(&self) -> Vec<String> {
        chosen_of(&self.loaded.surfaces, &self.picked)
    }
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
        App { ui: Ui::new(), screen: Screen::Menu, timelapses: Vec::new(), quit: false }
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
                    Choice::new("Quit", ""),
                ]
            }
            Screen::Watch => {
                let mut rows: Vec<Choice> = self.timelapses.iter().map(|t| Choice::new(&t.name, t.note.clone())).collect();
                rows.push(Choice::new("Back", ""));
                rows
            }
            Screen::Recordings(found) => {
                let mut rows: Vec<Choice> = found.iter().map(|r| Choice::new(&r.label, r.note.clone())).collect();
                rows.push(Choice::new("Back", ""));
                rows
            }
            // Nothing to choose while a file is being read, but the row is
            // there so the screen has the same shape as every other one.
            Screen::Opening(_) => vec![Choice::new("Cancel", "")],
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
                rows.push(Choice::new("Back", ""));
                rows
            }
            Screen::Interval(_) => {
                let mut rows: Vec<Choice> = INTERVALS.iter().map(|(_, label)| Choice::new(label, "")).collect();
                rows.push(Choice::new("Back", ""));
                rows
            }
            // One row, and it stops the build rather than leaving the screen:
            // walking away from work that is still running is how somebody
            // ends up with a half written timelapse and no idea why.
            Screen::Building(_) => vec![Choice::new("Stop", "")],
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
                rows.push(Choice::new("Back", ""));
                rows
            }
            Screen::Ground(pick) => vec![
                Choice::new("Yes, read the ground", if pick.ground { "chosen" } else { "" }),
                Choice::new("No, just the factory", ""),
                Choice::new("Back", ""),
            ],
            Screen::Render(render) => match render.step {
                RenderStep::Which => {
                    let mut rows: Vec<Choice> =
                        render.built.iter().map(|t| Choice::new(&t.name, format!("{} frames", t.frames))).collect();
                    rows.push(Choice::new("Back", ""));
                    rows
                }
                RenderStep::Size => {
                    let mut rows: Vec<Choice> = SIZES.iter().map(|(_, _, label)| Choice::new(label, "")).collect();
                    rows.push(Choice::new("Back", ""));
                    rows
                }
                RenderStep::Rate => {
                    let mut rows: Vec<Choice> = RATES.iter().map(|(_, label)| Choice::new(label, "")).collect();
                    rows.push(Choice::new("Back", ""));
                    rows
                }
                RenderStep::Kind => vec![
                    Choice::new("One video file", ""),
                    Choice::new("A picture per frame", "for editing"),
                    Choice::new("Back", ""),
                ],
            },
            Screen::Manage => vec![
                Choice::new(Kind::LogData.title(), summary(Kind::LogData)),
                Choice::new(Kind::Timelapse.title(), summary(Kind::Timelapse)),
                Choice::new(Kind::Video.title(), summary(Kind::Video)),
                Choice::new("Back", ""),
            ],
            Screen::Listing(listing) => {
                let mut rows: Vec<Choice> = listing.rows.iter().map(|row| Choice::new(&row.label, row.note.clone())).collect();
                rows.push(Choice::new("Back", ""));
                rows
            }
            // Delete second, so the row under the pointer when this screen
            // opens is the harmless one.
            Screen::Confirm(_) => vec![Choice::new("Keep it", ""), Choice::new("Delete", "")],
            Screen::Done(_) | Screen::Soon(_) => vec![Choice::new("Back", "")],
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
            Screen::Building(running) => &running.what,
            Screen::Done(_) => "Done",
            Screen::Render(render) => match render.step {
                RenderStep::Which => "Which timelapse?",
                RenderStep::Size => "How big?",
                RenderStep::Rate => "How smooth?",
                RenderStep::Kind => "A video, or the frames?",
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
                            size: (1920, 1080),
                            fps: 30,
                            video: true,
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
            Screen::Interval(_) => match INTERVALS.get(index) {
                Some(&(seconds, _)) => {
                    let Screen::Interval(choosing) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                        unreachable!("just matched")
                    };
                    self.screen = Screen::Building(start_build(choosing, seconds));
                }
                None => self.screen = Screen::Menu,
            },
            // The flag, not the screen: the thread notices within a frame and
            // reports what it managed, and the window waits for that rather
            // than pretending it already stopped.
            Screen::Building(running) => running.stop(),
            Screen::Render(render) => match render.step {
                RenderStep::Which if index < render.built.len() => {
                    render.chosen = Some(index);
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
                    let Screen::Render(render) = std::mem::replace(&mut self.screen, Screen::Menu) else {
                        unreachable!("just matched")
                    };
                    self.screen = Screen::Building(start_render(render));
                }
                // Back, from wherever: one step at a time rather than out to
                // the menu, so changing one answer does not lose the others.
                RenderStep::Which => self.screen = Screen::Menu,
                RenderStep::Size => render.step = RenderStep::Which,
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
            Ok(loaded) => {
                Screen::Places(Choosing { picked: vec![true; loaded.surfaces.len()], loaded, recording: opening.recording })
            }
            Err(message) => Screen::Done(message),
        };
    }

    fn draw(&self, column: &Column, choices: &[Choice], hovered: Option<usize>) {
        clear_background(BACKGROUND);

        let (center_x, center_y) = column.header_center(screen_width());
        // The logo goes here. Until there is one, the name carries the header
        // on its own rather than a placeholder box standing in for artwork
        // nobody has drawn yet.
        let title = self.title();
        let width = self.ui.width(title, TITLE_SIZE);
        self.ui.text(title, center_x - width / 2.0, center_y + TITLE_SIZE / 2.0, TITLE_SIZE, TEXT);

        for (index, choice) in choices.iter().enumerate() {
            let row = column.row(index);
            let lit = hovered == Some(index);
            draw_rectangle(row.x, row.y, row.width, row.height, if lit { ROW_HOVER } else { ROW });
            draw_rectangle_lines(row.x, row.y, row.width, row.height, 1.0, ROW_EDGE);

            let baseline = row.text_baseline(LABEL_SIZE);
            self.ui.text(&choice.label, row.x + 18.0, baseline, LABEL_SIZE, if lit { ACCENT } else { TEXT });

            // A ticked place gets an edge as well as a word, so which rows are
            // in is readable at a glance rather than by reading every note.
            if choice.note == "included" {
                draw_rectangle(row.x, row.y, 3.0, row.height, ACCENT);
            }

            if !choice.note.is_empty() {
                let note_width = self.ui.width(&choice.note, NOTE_SIZE);
                self.ui.text(&choice.note, row.x + row.width - 18.0 - note_width, baseline, NOTE_SIZE, TEXT_DIM);
            }
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
            let line = match running.frames {
                None => "Reading the recording...".to_string(),
                Some(0) => "Starting...".to_string(),
                Some(count) => format!("{} frames", crate::with_thousands(count as u64)),
            };
            let width = self.ui.width(&line, LABEL_SIZE);
            self.ui.text(&line, (screen_width() - width) / 2.0, column.top - 30.0, LABEL_SIZE, ACCENT);
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
        // The busiest place. Choosing between them is one more step, and the
        // answer for a single-world timelapse is the only one there is.
        surface: None,
        video: render.video,
        fps: render.fps,
        // Only when FFmpeg is already installed: an MP4 is smaller and is what
        // sharing sites accept, and nothing here asks anybody to go and get it.
        mp4: render.video && crate::ffmpeg_available(),
        overlay_players: false,
        overlay_clock: render.video,
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

    Running { updates, cancel, what, frames: None }
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

    Running { updates, cancel, what, frames: None }
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
                note: format!("{}, {age}", describe::describe_play_time(session.baseline.tick)),
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
fn start_build(choosing: Choosing, seconds: u64) -> Running {
    let (send, updates) = channel();
    let cancel = Arc::new(AtomicBool::new(false));

    let surfaces = choosing.chosen_surfaces();
    let out = build::timelapses_root().join(build::as_folder_name(&choosing.recording.name));
    let session_dir = choosing.recording.session_dir.clone();
    let what = format!("Building {}", choosing.recording.label);
    let mut replay = choosing.loaded.replay;
    let flag = Arc::clone(&cancel);

    std::thread::spawn(move || {
        let ended = build_on_thread(&mut replay, &session_dir, &out, surfaces, seconds, &flag, &send);
        let _ = send.send(Update::Ended(ended));
    });

    Running { updates, cancel, what, frames: None }
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
        let column = Column::centered(screen_width(), screen_height(), choices.len(), ROW_HEIGHT);
        let (mouse_x, mouse_y) = mouse_position();
        let hovered = column.hit(mouse_x, mouse_y);

        if is_mouse_button_pressed(MouseButton::Left) {
            if let Some(index) = hovered {
                app.choose(index);
            }
        }
        // Escape goes back one level, and out of the menu entirely, so the
        // keyboard reaches every exit the mouse does.
        if is_key_pressed(KeyCode::Escape) {
            match app.screen {
                Screen::Menu => app.quit = true,
                _ => app.screen = Screen::Menu,
            }
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
        let running =
            Running { updates, cancel: Arc::new(AtomicBool::new(false)), what: "Building nauvis".to_string(), frames: None };
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
