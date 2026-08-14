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

use crate::build;
use crate::viewer::Ui;
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
    Soon(&'static str),
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
        match self.screen {
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
            Screen::Soon(_) => vec![Choice::new("Back", "")],
        }
    }

    fn title(&self) -> &str {
        match self.screen {
            Screen::Menu => "Save Timelapse",
            Screen::Watch => "Watch a timelapse",
            Screen::Soon(what) => what,
        }
    }

    /// What a click on `index` does. Split from the event loop so the loop
    /// reads as "find what was clicked, then act on it".
    fn choose(&mut self, index: usize) {
        match self.screen {
            Screen::Menu => match index {
                0 => {
                    self.timelapses = list_timelapses();
                    self.screen = Screen::Watch;
                }
                1 => self.screen = Screen::Soon("Build one from a recording"),
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
            Screen::Soon(_) => self.screen = Screen::Menu,
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

        if let Screen::Watch = self.screen {
            if self.timelapses.is_empty() {
                let message = "Nothing built yet.";
                let width = self.ui.width(message, NOTE_SIZE);
                self.ui.text(message, (screen_width() - width) / 2.0, column.top - 24.0, NOTE_SIZE, TEXT_DIM);
            }
        }
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

        app.draw(&column, &choices, hovered);
        next_frame().await;
    }
}
