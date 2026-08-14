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
    /// A build in flight, or the sentence it ended with. The window keeps
    /// drawing throughout, which is the whole reason the work is on a thread.
    Building(Running),
    Done(String),
    Soon(&'static str),
}

/// A recording the build screen can offer.
struct Recording {
    label: String,
    note: String,
    session_dir: std::path::PathBuf,
    baseline_path: std::path::PathBuf,
    name: String,
}

/// What a build says to the window while it runs.
enum Update {
    /// Reading the baseline, which on a megabase is tens of seconds with
    /// nothing else to report.
    Loading,
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
                Ok(Update::Loading) => self.frames = None,
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
            // One row, and it stops the build rather than leaving the screen:
            // walking away from work that is still running is how somebody
            // ends up with a half written timelapse and no idea why.
            Screen::Building(_) => vec![Choice::new("Stop", "")],
            Screen::Done(_) | Screen::Soon(_) => vec![Choice::new("Back", "")],
        }
    }

    fn title(&self) -> &str {
        match &self.screen {
            Screen::Menu => "Save Timelapse",
            Screen::Watch => "Watch a timelapse",
            Screen::Recordings(_) => "Build from a recording",
            Screen::Building(running) => &running.what,
            Screen::Done(_) => "Done",
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
                2 => self.screen = Screen::Soon("Build one from save files"),
                3 => self.screen = Screen::Soon("Save one as a video"),
                4 => self.screen = Screen::Soon("Manage"),
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
                Some(chosen) => self.screen = Screen::Building(start_build(chosen)),
                None => self.screen = Screen::Menu,
            },
            // The flag, not the screen: the thread notices within a frame and
            // reports what it managed, and the window waits for that rather
            // than pretending it already stopped.
            Screen::Building(running) => running.stop(),
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
        }
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

            if !choice.note.is_empty() {
                let note_width = self.ui.width(&choice.note, NOTE_SIZE);
                self.ui.text(&choice.note, row.x + row.width - 18.0 - note_width, baseline, NOTE_SIZE, TEXT_DIM);
            }
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

/// Every recording, newest first, described the way a row wants it.
fn recordings() -> Vec<Recording> {
    let Some(user_dir) = crate::locate::factorio_user_dir() else { return Vec::new() };
    let capture = user_dir.join("script-output").join("save-timelapse");
    let now = std::time::SystemTime::now();

    replay::discover_sessions(&capture)
        .unwrap_or_default()
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

/// Starts a build on its own thread and hands back the two ends the window
/// needs: what it says, and how to stop it.
///
/// Every place, at the default interval. Choosing those is two more screens,
/// and a build that runs with sensible answers is worth more than one that
/// cannot start until they exist.
fn start_build(chosen: &Recording) -> Running {
    let (send, updates) = channel();
    let cancel = Arc::new(AtomicBool::new(false));

    let out = build::timelapses_root().join(build::as_folder_name(&chosen.name));
    let baseline_path = chosen.baseline_path.clone();
    let session_dir = chosen.session_dir.clone();
    let flag = Arc::clone(&cancel);

    std::thread::spawn(move || {
        let _ = send.send(Update::Loading);
        let ended = build_on_thread(&baseline_path, &session_dir, &out, &flag, &send);
        let _ = send.send(Update::Ended(ended));
    });

    Running { updates, cancel, what: format!("Building {}", chosen.label), frames: None }
}

/// The build itself, as one function returning the sentence to show. Split out
/// so the thread body is three lines and every failure has somewhere to go.
fn build_on_thread(
    baseline_path: &std::path::Path,
    session_dir: &std::path::Path,
    out: &std::path::Path,
    cancel: &AtomicBool,
    send: &std::sync::mpsc::Sender<Update>,
) -> String {
    let mut replay = match replay::load_baseline(baseline_path) {
        Ok(loaded) => loaded,
        Err(e) => return format!("Could not read that recording: {e}"),
    };
    let _ = std::fs::remove_dir_all(out);
    if let Err(e) = std::fs::create_dir_all(out) {
        return format!("Could not make a folder for it: {e}");
    }

    let plan = build::Plan {
        surfaces: Vec::new(),
        options: replay::Options { interval: 60 * crate::viewer::TICKS_PER_SECOND, max_frames: 20_000 },
    };
    let mut on_frame = |written: usize| {
        let _ = send.send(Update::Frames(written));
    };
    match build::timelapse(&mut replay, session_dir, out, &plan, &mut build::Watch { on: &mut on_frame, cancel }) {
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

    #[test]
    fn loading_shows_no_count_until_the_first_frame_lands() {
        let (mut job, send) = running();
        send.send(Update::Loading).unwrap();
        job.drain();
        assert_eq!(job.frames, None, "a count of zero would claim work that has not started");

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
