//! save-timelapse: one interactive tool, no flags to learn. Asks what you want
//! to do, asks whatever it could not auto-detect, then opens the viewer on the
//! result.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime};

use save_timelapse::export;
use save_timelapse::frame;
use save_timelapse::locate::{factorio_user_dir, locate_factorio};
use save_timelapse::milestone;
use save_timelapse::replay::{self, Options};
use save_timelapse::settings::Settings;
use save_timelapse::with_thousands;

/// Default game time per frame during live-capture replay, asked about
/// interactively so a longer playthrough can trade a larger export for
/// smoother, more finely-spaced playback.
const DEFAULT_FRAME_SECONDS: u64 = 60;

/// Factorio's normal game speed: one real second is 60 ticks.
const TICKS_PER_SECOND: u64 = 60;

const MAX_FRAMES: usize = 100_000;

/// Offered when nothing has been exported yet, then remembered. 1080p rather
/// than the largest: a 4K export of a long playthrough is several gigabytes
/// and many minutes, which is a bad default to pick for somebody.
const DEFAULT_EXPORT_SIZE: (u32, u32) = (1920, 1080);

/// Smooth without being wasteful. Frames are spaced by game time, not by
/// this, so it only sets how fast the finished file plays back.
const DEFAULT_EXPORT_FPS: u32 = 30;

/// How many saves to list when asking which one the ground comes from. A
/// saves folder holds every autosave; the ones worth choosing between are the
/// most recent few.
const TERRAIN_SAVE_CHOICES: usize = 15;

enum Mode {
    OpenExisting,
    LiveCapture,
    FromSaves,
    ExportVideo,
    ManageCaptures,
    Quit,
}

/// Prints `question`, reads one line, trims it. EOF comes back as an error
/// rather than an empty `Ok`, which would spin the retry loops forever.
fn prompt(question: &str) -> io::Result<String> {
    print!("{question} ");
    io::stdout().flush()?;
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "no more input"));
    }
    Ok(line.trim().to_string())
}

/// Whether an error means there is nobody left to ask. A failed action returns
/// to the menu, so without this a closed stdin would fail the action, fail the
/// menu's prompt, and spin forever.
fn input_closed(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof
}

/// Reports a failed action and waits, then hands back to the menu. A
/// double-clicked console has no scrollback, so a message not paused on is a
/// message nobody read. The only place a failure is printed.
fn show_problem(e: &io::Error) {
    println!("\nThat didn't work.\n\n{e}\n");
    print!("Press Enter to go back to the menu...");
    io::stdout().flush().ok();
    let mut discard = String::new();
    io::stdin().read_line(&mut discard).ok();
    println!();
}

/// Announces a step and leaves the line open, so what the step finds finishes
/// the sentence. Loading a megabase baseline is tens of seconds of blank
/// screen otherwise. Flushed explicitly, stdout being line buffered.
fn step(what: &str) {
    // Callers prefix a blank line to separate one stage of a flow from the
    // last. That has to stay in front of the indent, or the indent lands on
    // the line being left behind rather than the line being written.
    let body = what.trim_start_matches('\n');
    let breaks = &what[..what.len() - body.len()];
    print!("{breaks}  {body}... ");
    io::stdout().flush().ok();
}

/// `already_built` changes what option 1 says, never the numbering: a menu
/// whose option 2 means different things on different days is worse than a line
/// that sometimes reads "none yet". Quit is numbered rather than a bare Enter,
/// a menu that cannot be left the same way it is used being a trap.
fn ask_mode(already_built: usize) -> io::Result<Mode> {
    let ready = match already_built {
        0 => String::new(),
        1 => "         1 ready".to_string(),
        n => format!("         {n} ready"),
    };
    loop {
        let input = prompt(&format!(
            "  What would you like to do?\n\n\
             \x20   1  Watch a timelapse{ready}\n\
             \x20   2  Build one from what you recorded while playing\n\
             \x20   3  Build one from your save files\n\
             \x20   4  Save one as a video file\n\
             \x20   5  Manage your recordings\n\
             \x20   6  Quit\n\n\
             \x20 Type a number:"
        ))?;
        match input.as_str() {
            "1" => return Ok(Mode::OpenExisting),
            "2" => return Ok(Mode::LiveCapture),
            "3" => return Ok(Mode::FromSaves),
            "4" => return Ok(Mode::ExportVideo),
            "5" => return Ok(Mode::ManageCaptures),
            // Bare Enter still leaves, both because it is what long-time users
            // already press and because it is what leaves every screen below
            // this one, so it should keep meaning "back" one level up.
            "6" | "q" | "Q" | "" => return Ok(Mode::Quit),
            _ => println!("\n  Please type a number from 1 to 6.\n"),
        }
    }
}

/// Factorio's data folder: remembered answer first, then auto-detection, then
/// asking. Validated by checking for a `mods` subfolder, including the
/// remembered path, so a folder that has since moved falls through to detection
/// rather than failing confusingly later. Whatever resolves is written back.
fn locate_factorio_user_dir_interactive(settings: &mut Settings) -> io::Result<PathBuf> {
    step("Finding your Factorio folder");

    // Says which of the three it was. "Remembered" versus "found" is the only
    // clue anyone gets that a saved setting is in play.
    if let Some(dir) = settings.valid_factorio_dir() {
        println!("found");
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = factorio_user_dir() {
        if dir.join("mods").is_dir() {
            println!("found");
            settings.factorio_dir = Some(dir.clone());
            return Ok(dir);
        }
    }
    println!("not found\n");
    loop {
        let input = prompt(
            "  Where is your Factorio data folder? It is the one with \"mods\" and\n  \
             \"saves\" inside it, usually %APPDATA%\\Factorio.\n\n  \
             Paste the path here:",
        )?;
        let path = PathBuf::from(&input);
        if path.join("mods").is_dir() {
            settings.factorio_dir = Some(path.clone());
            return Ok(path);
        }
        println!("\n  That folder has no \"mods\" inside it, so it is not the right one.\n");
    }
}

/// Same idea for the actual game executable, only needed by the from-saves
/// flow, which launches it headless.
fn locate_factorio_exe_interactive(settings: &mut Settings) -> io::Result<PathBuf> {
    step("Finding your Factorio install");

    if let Some(exe) = settings.valid_factorio_exe() {
        println!("found");
        return Ok(exe.to_path_buf());
    }
    if let Some(exe) = locate_factorio() {
        println!("found");
        settings.factorio_exe = Some(exe.clone());
        return Ok(exe);
    }
    println!("not found\n");
    loop {
        let input = prompt(
            "  Where is factorio.exe? In a Steam install it is usually under\n  \
             Factorio\\bin\\x64\\factorio.exe.\n\n  \
             Paste the path here:",
        )?;
        let path = PathBuf::from(&input);
        if path.is_file() {
            settings.factorio_exe = Some(path.clone());
            return Ok(path);
        }
        println!("\n  There is no file there. Check the path and try again.\n");
    }
}

/// Empty input is the caller's job to treat as "every surface"; this only
/// ever gets asked about a genuine name, matched case-insensitively so
/// "Nauvis" and "nauvis" aren't different answers.
fn find_surface<'a>(input: &str, surfaces: &'a [String]) -> Option<&'a str> {
    surfaces.iter().find(|s| s.eq_ignore_ascii_case(input)).map(String::as_str)
}

/// `None` for anything that is not a recognized yes/no word, including empty
/// input: whether blank means yes is the caller's business.
fn parse_yes_no(input: &str) -> Option<bool> {
    match input.trim().to_lowercase().as_str() {
        "y" | "yes" => Some(true),
        "n" | "no" => Some(false),
        _ => None,
    }
}

fn ask_yes_no(question: &str, default: bool) -> io::Result<bool> {
    let hint = if default { "Y/n" } else { "y/N" };
    loop {
        let input = prompt(&format!("{question} [{hint}]:"))?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_yes_no(&input) {
            Some(answer) => return Ok(answer),
            None => println!("Please answer y or n.\n"),
        }
    }
}

/// `None` for anything that isn't a positive whole number, including empty
/// input, matching `parse_yes_no`'s convention of leaving the "blank means
/// default" decision to the caller.
fn parse_frame_seconds(input: &str) -> Option<u64> {
    let seconds: u64 = input.trim().parse().ok()?;
    (seconds > 0).then_some(seconds)
}

fn ask_frame_seconds(default: u64) -> io::Result<u64> {
    loop {
        let input = prompt(&format!(
            "  How often should the timelapse take a picture?\n  \
             A smaller number is smoother to watch but makes a bigger file.\n\n  \
             Press Enter for every {default} seconds of game time, or type a number:"
        ))?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_frame_seconds(&input) {
            Some(seconds) => return Ok(seconds),
            None => println!("\n  Please type a whole number bigger than 0.\n"),
        }
    }
}

fn ask_surface_choice(surfaces: &[String]) -> io::Result<Option<String>> {
    loop {
        let input = prompt(&format!(
            "  Include everywhere you have been, or just one place?\n  \
             You have been to: {}\n\n  \
             Press Enter for all of them, or type one name:",
            surfaces.iter().map(|s| pretty_place(s)).collect::<Vec<_>>().join(", ")
        ))?;
        if input.is_empty() {
            return Ok(None);
        }
        if let Some(found) = find_surface(&input, surfaces) {
            return Ok(Some(found.to_string()));
        }
        println!("\n  There is no \"{input}\" in that list. Try again.\n");
    }
}

/// A coarse "how long ago" label, good enough to recognise your own
/// playthrough in a list. Factorio gives mods no way to read a save name.
fn describe_age(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        return "just now".to_string();
    }
    if secs < 3600 {
        let minutes = secs / 60;
        return format!("{minutes} minute{} ago", if minutes == 1 { "" } else { "s" });
    }
    if secs < 86400 {
        let hours = secs / 3600;
        return format!("{hours} hour{} ago", if hours == 1 { "" } else { "s" });
    }
    let days = secs / 86400;
    format!("{days} day{} ago", if days == 1 { "" } else { "s" })
}

/// 1-based `input` as an index into a list of `count` items. `None` for
/// anything out of range or not a number, so the caller reprompts on a miss
/// rather than silently guessing which playthrough was meant.
fn parse_session_index(input: &str, count: usize) -> Option<usize> {
    let one_based: usize = input.trim().parse().ok()?;
    let index = one_based.checked_sub(1)?;
    (index < count).then_some(index)
}

/// Only reached when more than one playthrough has capture data waiting.
/// Always picks exactly one rather than offering "all": combining them would
/// jump between different bases in one timelapse.
fn ask_session_choice(sessions: &[replay::Session]) -> io::Result<usize> {
    println!("\n  You have {} recordings:\n", sessions.len());
    let now = SystemTime::now();
    for (i, session) in sessions.iter().enumerate() {
        println!("  {}  {}\n", i + 1, describe_session(session, now));
    }
    loop {
        let input = prompt("  Which one? Type a number:")?;
        if let Some(index) = parse_session_index(&input, sessions.len()) {
            return Ok(index);
        }
        println!("\n  Please type a number from 1 to {}.\n", sessions.len());
    }
}

/// Saves are usually numbered, so order by that number rather than
/// lexicographically, which would place "base10" before "base2".
fn ordering_key(path: &Path) -> (u64, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let digits: String = stem.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(0), stem.to_lowercase())
}

/// `all` selects everything. Otherwise, if every comma-separated part parses as
/// a number, those are 1-based indices, out-of-range ones dropped so a typo does
/// not throw away a valid selection; otherwise a case-insensitive substring
/// filter. Blank selects nothing, so the caller can reprompt.
fn parse_save_selection(input: &str, saves: &[PathBuf]) -> Vec<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    if trimmed.eq_ignore_ascii_case("all") {
        return saves.to_vec();
    }

    let parts: Vec<&str> = trimmed.split(',').map(str::trim).collect();
    if parts.iter().all(|part| part.parse::<usize>().is_ok()) {
        return parts
            .iter()
            .filter_map(|part| part.parse::<usize>().ok())
            .filter_map(|one_based| one_based.checked_sub(1))
            .filter_map(|index| saves.get(index).cloned())
            .collect();
    }

    let needle = trimmed.to_lowercase();
    saves
        .iter()
        .filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(|name| name.to_lowercase().contains(&needle)))
        .cloned()
        .collect()
}

/// The Lua mod's source. Tried next to this program first (how it travels
/// once distributed), then the compile-time project root (running straight out
/// of target/release), then the current folder.
fn mod_source_dir() -> io::Result<PathBuf> {
    let exe_sibling = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join("mod")));
    if let Some(dir) = &exe_sibling {
        if dir.is_dir() {
            return Ok(dir.clone());
        }
    }
    let manifest_candidate = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mod");
    if manifest_candidate.is_dir() {
        return Ok(manifest_candidate);
    }
    let cwd_candidate = PathBuf::from("mod");
    if cwd_candidate.is_dir() {
        return Ok(cwd_candidate);
    }
    Err(io::Error::other(
        "Could not find the mod/ folder needed to export from saves. It should sit next to \
         this program (or in the current folder, if running from source).",
    ))
}

/// Where a mode writes its output: beside the running exe, so the result is
/// easy to find wherever Factorio's user data lives. Falls back to the current
/// directory only if the exe's path cannot be determined.
fn output_dir_next_to_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(".")).join(name)
}

/// Where built timelapses are kept, one subfolder each. A single fixed folder
/// that every run rebuilt meant reopening yesterday's timelapse was
/// impossible: the only way to see one was to make it again.
fn timelapses_root() -> PathBuf {
    output_dir_next_to_exe("timelapses")
}

/// Turns a playthrough name or save name into something safe to use as a
/// folder name, since both come from the user and can hold anything.
fn as_folder_name(raw: &str) -> String {
    // Dots are kept, a name like "v1.2 run" being ordinary, and trimmed from
    // the ends, since "." and ".." name directories.
    let cleaned: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.') { c } else { '_' }).collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "timelapse".to_string()
    } else {
        trimmed.to_string()
    }
}

/// One timelapse already built and sitting on disk.
struct BuiltTimelapse {
    name: String,
    path: PathBuf,
    frames: usize,
    bytes: u64,
    modified: SystemTime,
}

/// Every built timelapse, newest first. Counts frames and weighs the folder,
/// since which of two somebody wants is usually answered by how big and how
/// recent it is, neither visible from a name.
fn list_timelapses() -> Vec<BuiltTimelapse> {
    list_timelapses_in(&timelapses_root())
}

/// Split from [`list_timelapses`] only so it can be tested: the real root is
/// derived from the running executable's own location, which no test can
/// arrange.
fn list_timelapses_in(root: &Path) -> Vec<BuiltTimelapse> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };

    let mut found: Vec<BuiltTimelapse> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|entry| {
            let path = entry.path();
            let files: Vec<_> = std::fs::read_dir(&path).ok()?.filter_map(Result::ok).collect();
            let frames = files.iter().filter(|f| f.path().extension().and_then(|e| e.to_str()) == Some("stfr")).count();
            // A folder with no frames in it is a half-finished or interrupted
            // build, not something worth offering to open.
            if frames == 0 {
                return None;
            }
            let bytes = files.iter().filter_map(|f| f.metadata().ok()).map(|m| m.len()).sum();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some(BuiltTimelapse { name: entry.file_name().to_string_lossy().into_owned(), path, frames, bytes, modified })
        })
        .collect();

    found.sort_by_key(|t| std::cmp::Reverse(t.modified));
    found
}

fn ask_timelapse_choice(built: &[BuiltTimelapse], question: &str) -> io::Result<Option<usize>> {
    println!("\n  Your timelapses:\n");
    for (i, t) in built.iter().enumerate() {
        let age = t.modified.elapsed().map(describe_age).unwrap_or_else(|_| "unknown".to_string());
        println!("  {}  {}", i + 1, t.name);
        println!("     {} frames, {}, built {age}\n", with_thousands(t.frames as u64), describe_size(t.bytes));
    }
    loop {
        let input = prompt(&format!("  {question} Type a number, or press Enter to go back:"))?;
        if input.is_empty() {
            return Ok(None);
        }
        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= built.len() => return Ok(Some(n - 1)),
            _ => println!("\n  Please type a number from 1 to {}.\n", built.len()),
        }
    }
}

/// `viewer` is a sibling binary rather than a library this crate can call: it
/// depends on this one, and its `main` is a macroquad event loop. So launching
/// it means finding the executable cargo built next to this one.
fn viewer_path() -> io::Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().ok_or_else(|| io::Error::other("could not determine this program's own folder"))?;
    let candidate = dir.join(format!("viewer{}", std::env::consts::EXE_SUFFIX));
    if !candidate.exists() {
        return Err(io::Error::other(format!(
            "Could not find {} next to this program. Reinstall save-timelapse so both files \
             end up in the same folder.",
            candidate.display()
        )));
    }
    Ok(candidate)
}

/// The surfaces a built timelapse holds, read off its filenames. A
/// single-surface build writes `frame_0000.stfr` with no name in it, so an
/// empty result means "one surface, unnamed" rather than "none". Split at the
/// first underscore after the index, so a name containing one survives.
fn surfaces_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("stfr"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            // Terrain files are named per surface too, so counting both would
            // list a name twice. Terrain alone is not a surface anyone can
            // export.
            let rest = stem.strip_prefix("frame_")?;
            let (_index, surface) = rest.split_once('_')?;
            (!surface.is_empty()).then(|| surface.to_string())
        })
        .collect();
    names.sort();
    names.dedup();
    names
}

/// `None` means "let the viewer pick", which is its busiest surface, and is
/// what Enter gives. `Some("all")` is passed straight through, since the
/// viewer already understands it as every surface.
fn ask_export_surface(surfaces: &[String]) -> io::Result<Option<String>> {
    loop {
        let input = prompt(&format!(
            "\nThis timelapse has more than one world in it.\n\
             Surfaces: {}\n\
             Enter a name, \"all\" for one file each, or press Enter for the busiest one:",
            surfaces.join(", ")
        ))?;
        if input.is_empty() {
            return Ok(None);
        }
        if input.eq_ignore_ascii_case("all") {
            return Ok(Some("all".to_string()));
        }
        if let Some(found) = find_surface(&input, surfaces) {
            return Ok(Some(found.to_string()));
        }
        println!("\"{input}\" doesn't match any surface listed above. Try again.");
    }
}

/// The four presets, in the order they are offered. 1080p leads because it
/// is the default; the rest run smallest to largest so the cost of going up
/// is visible as a direction rather than something to work out.
const RESOLUTION_PRESETS: [(u32, u32, &str); 4] = [
    (1920, 1080, "1080p, recommended"),
    (1280, 720, "720p, smallest and fastest"),
    (2560, 1440, "1440p"),
    (3840, 2160, "4K, large and slow"),
];

/// A preset number, or a literal `WIDTHxHEIGHT`. `None` for anything else,
/// including a size with a zero in it. Accepts `x` in either case.
fn parse_resolution(input: &str) -> Option<(u32, u32)> {
    let trimmed = input.trim();
    if let Ok(n) = trimmed.parse::<usize>() {
        let (w, h, _) = *RESOLUTION_PRESETS.get(n.checked_sub(1)?)?;
        return Some((w, h));
    }
    let lowered = trimmed.to_lowercase();
    let (w, h) = lowered.split_once('x')?;
    let (w, h) = (w.trim().parse::<u32>().ok()?, h.trim().parse::<u32>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn ask_resolution(default: (u32, u32)) -> io::Result<(u32, u32)> {
    println!("\nHow big should the video be?");
    for (i, (w, h, label)) in RESOLUTION_PRESETS.iter().enumerate() {
        println!("  {}) {w}x{h} ({label})", i + 1);
    }
    loop {
        let input = prompt(&format!("Enter a number, or a size like 1920x1080 [default {}x{}]:", default.0, default.1))?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_resolution(&input) {
            Some(size) => return Ok(size),
            None => println!("Please enter one of the numbers above, or a size like 1920x1080."),
        }
    }
}

/// Rejects zero the same way `parse_frame_seconds` does, and caps at the
/// viewer's own limit rather than letting a typo like 300 through to be
/// silently clamped somewhere the user cannot see it happen.
fn parse_fps(input: &str) -> Option<u32> {
    let fps: u32 = input.trim().parse().ok()?;
    (1..=240).contains(&fps).then_some(fps)
}

fn ask_fps(default: u32) -> io::Result<u32> {
    loop {
        let input = prompt(&format!(
            "\nHow fast should it play? Frames are spaced by game time, so this only sets \
             playback speed: 30 is smooth, 10 is slow enough to read. Enter frames per second \
             [default {default}]:"
        ))?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_fps(&input) {
            Some(fps) => return Ok(fps),
            None => println!("Please enter a whole number between 1 and 240."),
        }
    }
}

/// Where finished videos and image sequences go: one folder next to the
/// executable, like `timelapses/`, so "where did it go" has one answer.
fn videos_root() -> PathBuf {
    output_dir_next_to_exe("videos")
}

/// Renders a built timelapse to a video file or an image sequence, by running
/// the viewer rather than reimplementing it: rendering needs a GPU context and
/// the whole sprite pipeline.
fn run_export(settings: &mut Settings) -> io::Result<()> {
    let built = list_timelapses();
    if built.is_empty() {
        println!("\n  You need a timelapse before you can save one as a video.\n  Try option 2 or 3 first.\n");
        return Ok(());
    }

    let Some(index) = ask_timelapse_choice(&built, "Which would you like to save as a video?")? else {
        return Ok(());
    };
    let chosen = &built[index];

    let surfaces = surfaces_in(&chosen.path);
    let surface = if surfaces.len() > 1 { ask_export_surface(&surfaces)? } else { None };

    // Asked as a question about the output rather than as a format choice:
    // "AVI or PNG" means nothing to somebody who just wants to post their
    // base, and the reason to want the frames instead is editing them.
    let video = ask_yes_no(
        "\nWrite one video file? Answering no writes a numbered PNG per frame instead, \
         which is what you want if you are editing rather than watching",
        true,
    )?;

    let size = ask_resolution(settings.export_size().unwrap_or(DEFAULT_EXPORT_SIZE))?;
    let fps = if video { ask_fps(settings.export_fps.unwrap_or(DEFAULT_EXPORT_FPS))? } else { DEFAULT_EXPORT_FPS };

    // Saved before the render, which is the slow part and the part most
    // likely to be interrupted. Retyping preferences because something else
    // went wrong is exactly the friction this exists to remove.
    settings.export_width = Some(size.0);
    settings.export_height = Some(size.1);
    settings.export_fps = Some(fps);
    remember(settings);

    let root = videos_root();
    std::fs::create_dir_all(&root)?;
    // No extension here: the viewer appends `.avi` for a video and treats
    // the path as a folder for an image sequence, so the same argument
    // serves both.
    let target = root.join(as_folder_name(&chosen.name));

    let viewer = viewer_path()?;
    let mut command = Command::new(&viewer);
    command
        .arg(&chosen.path)
        .arg("--export")
        .arg(&target)
        .arg("--width")
        .arg(size.0.to_string())
        .arg("--height")
        .arg(size.1.to_string());
    if video {
        command.arg("--video").arg("--fps").arg(fps.to_string());
    }
    if let Some(name) = &surface {
        command.arg("--surface").arg(name);
    }

    println!("\nRendering. A window opens while this runs; leave it alone until it closes.\n");
    // `status` rather than `spawn`: every other mode hands off to the viewer
    // and exits, but an export finishes, and returning to the menu before it
    // does would leave its progress printing over a fresh prompt.
    let status = command.status()?;
    if !status.success() {
        return Err(io::Error::other(format!("the viewer exited with {status} without finishing the export")));
    }

    // Naming the folder, not the file: with "all" there is one file per
    // surface, and with an image sequence there is a folder of them, so the
    // one thing that is always true is where to go looking.
    println!("\nDone. Find it in {}\n", root.display());
    Ok(())
}

fn run_live_capture(settings: &mut Settings) -> io::Result<PathBuf> {
    let user_dir = locate_factorio_user_dir_interactive(settings)?;

    let capture = user_dir.join("script-output").join("save-timelapse");
    // A missing capture folder and one naming no finished baseline are the
    // same "not started yet" state, so a read_dir failure folds into the
    // empty case rather than surfacing as an IO error.
    step("Looking for recordings");
    let sessions = replay::discover_sessions(&capture).unwrap_or_default();
    println!("{} found", sessions.len());
    if sessions.is_empty() {
        return Err(io::Error::other(
            "You have no recordings yet.\n\n  \
             In Factorio, open Settings > Mod Settings > Runtime and turn on \"Live capture\"\n  \
             for Save Timelapse. Play for a while, then come back here.",
        ));
    }

    // Playthroughs are tagged separately precisely so they are never
    // combined, so only ask when there is more than one.
    let chosen = if sessions.len() == 1 {
        sessions.into_iter().next().expect("checked non-empty above")
    } else {
        let index = ask_session_choice(&sessions)?;
        sessions.into_iter().nth(index).expect("ask_session_choice returned a valid index")
    };

    // The slowest silent step in the whole tool: a megabase baseline is tens
    // of megabytes and takes real time to read, with nothing on screen until
    // it finished.
    step("\nReading your recording");
    let mut replay_state = replay::load_baseline(&chosen.baseline_path)?;
    // Only what somebody built: a capture also keeps trees, ore and nests for
    // context, and counting those reports the map rather than the factory.
    let described = save_timelapse::prototypes::read(&chosen.session_dir);
    let buildings = replay_state.world.count_entities(|name| described.as_ref().is_none_or(|p| p.is_built(name)));
    println!("{} buildings", with_thousands(buildings as u64));

    step("Finding places");
    let surfaces = replay::discover_surfaces(&chosen.session_dir, &replay_state)?;
    println!("{}\n", describe_places(&surfaces));

    let chosen_surface = ask_surface_choice(&surfaces)?;
    let frame_seconds = ask_frame_seconds(settings.frame_seconds.unwrap_or(DEFAULT_FRAME_SECONDS))?;

    // Saved before the export, so an answer survives a build that fails.
    // Surface choice is deliberately not remembered: which surfaces a capture
    // has changes as a playthrough reaches new planets.
    settings.frame_seconds = Some(frame_seconds);
    remember(settings);

    // Named after the playthrough, so rebuilding one leaves the others alone.
    // Clearing just this one first is still right: a shorter capture must not
    // leave stale higher-numbered frames behind. Falls back to where the
    // playthrough happened rather than its session id, which tells the user
    // nothing; the id stays as the last resort against a collision.
    let name = chosen.label().unwrap_or_else(|| match chosen.baseline.surfaces.as_slice() {
        [] => format!("playthrough-{:08x}", chosen.session_id),
        surfaces => format!("{} ({:08x})", describe_places(surfaces), chosen.session_id),
    });
    let out = timelapses_root().join(as_folder_name(&name));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    // Terrain is fixed the instant the baseline loads, so it is written once
    // here rather than per emitted frame. Only finds anything for a capture
    // recorded before ground moved into its own scan. Kept first because
    // `offer_terrain_for_capture` overwrites it afterwards, which is the right
    // precedence, a scan covering the factory's final extent.
    match &chosen_surface {
        None => replay::write_all_terrain(&replay_state.world, replay_state.baseline.tick, &out)?,
        Some(name) => replay::write_terrain(&replay_state.world, name, replay_state.baseline.tick, &out)?,
    }

    copy_session_sidecars(&chosen.session_dir, &out)?;

    let options = Options { interval: frame_seconds * TICKS_PER_SECOND, max_frames: MAX_FRAMES };
    let mut written = 0usize;
    let mut error: Option<io::Error> = None;
    // Last revision written per surface, carried across the run so
    // `write_all_surfaces` can skip a surface nothing has touched.
    let mut surface_revisions: std::collections::HashMap<String, u64> = Default::default();

    let emitted = match &chosen_surface {
        None => {
            println!("\n  Building your timelapse. On a big factory this takes a while.\n");
            replay::run(&mut replay_state, &chosen.session_dir, &options, |world, tick| {
                if error.is_some() {
                    return;
                }
                if let Err(e) = replay::write_all_surfaces(world, tick, &out, written, &mut surface_revisions) {
                    error = Some(e);
                    return;
                }
                written += 1;
                if written.is_multiple_of(25) {
                    print!("\r  {written} frames");
                    io::stdout().flush().ok();
                }
            })?
        }
        Some(name) => {
            println!("\n  Building your timelapse of {}. On a big factory this takes a while.\n", pretty_place(name));
            replay::run(&mut replay_state, &chosen.session_dir, &options, |world, tick| {
                if error.is_some() {
                    return;
                }
                let frame = world.to_frame(name, tick);
                let path = out.join(format!("frame_{written:04}.stfr"));
                if let Err(e) = std::fs::write(&path, frame::write_binary(&frame.as_out())) {
                    error = Some(e);
                    return;
                }
                written += 1;
                if written.is_multiple_of(25) {
                    print!("\r  {written} frames");
                    io::stdout().flush().ok();
                }
            })?
        }
    };
    if let Some(e) = error {
        return Err(e);
    }
    println!(
        "\r  Built {} frames covering {}.",
        with_thousands(emitted as u64),
        describe_span(replay_state.baseline.tick, replay_state.world.tick)
    );
    println!("  Saved in {}", out.display());

    // Only what changes what somebody should do next, said as what to do
    // rather than what was counted. The replay handling something correctly is
    // not news to the person who wanted a timelapse.
    let mut acted = false;

    // Deliberately not phrased as corruption: a skipped extension record only
    // means the mod that wrote this recording is newer than this build. The
    // timelapse is correct as far as it goes.
    if replay_state.unknown_extensions > 0 {
        println!("\n  Part of this recording came from a newer version of the mod than this tool.");
        println!("  Update Save Timelapse and build again to include it.");
        acted = true;
    }
    // Distinct from a skipped segment: the file opened and parsed, and the
    // records inside it named things the file never defined, so they could
    // only be thrown away.
    if replay_state.undefined_references > 0 {
        println!(
            "
  {} recorded changes could not be read and were lost.",
            with_thousands(replay_state.undefined_references as u64)
        );
        println!("  This happens to recordings made before v0.7.1 if the capture was reset");
        println!("  mid-playthrough. Starting a fresh recording in game fixes it.");
        acted = true;
    }
    if replay_state.skipped_segments > 0 || replay_state.out_of_order_batches > 0 {
        println!("\n  Some of this recording could not be read, so parts of the history may be missing.");
        acted = true;
    }
    let total = replay_state.applied_events + replay_state.no_op_events;
    if total >= 20 && replay_state.no_op_events * 2 > total {
        println!("\n  Most of what was recorded did not match the starting snapshot, so this");
        println!("  timelapse may be wrong. Starting a fresh recording in game usually fixes it.");
        acted = true;
    }
    // Paused on only when something was actually said, and never for a clean
    // build: a message nobody needs is not worth a keypress.
    if acted {
        wait_for_enter();
    }

    offer_terrain_for_capture(settings, &user_dir, &out, chosen.session_id, replay_state.world.tick)?;

    Ok(out)
}

/// Offer to read the ground for a live capture, from a save of the same
/// playthrough.
///
/// Live capture records what changes and ground does not, so it is read
/// afterwards from any single save. That keeps the most expensive part out of
/// the game, at the cost of the one thing this flow otherwise never needs,
/// which is Factorio itself, so it is asked rather than assumed.
/// Which save to read the ground from.
///
/// Offered rather than assumed: somebody may have started a different game
/// since, or keep a "before I wrecked it" save. Picking wrong is caught by the
/// scan. `saves` must be newest first, so Enter takes the common case.
fn ask_terrain_save(saves: &[PathBuf]) -> io::Result<&Path> {
    let now = SystemTime::now();
    // The caveat lives here rather than beside the yes/no above, because this
    // is the moment it can be acted on: somebody reading it while picking a
    // save can still go and make a newer one.
    println!("\n  Which save should the ground come from?");
    println!("  Ground only exists where that save had already been, so pick a recent one.\n");
    for (i, save) in saves.iter().enumerate().take(TERRAIN_SAVE_CHOICES) {
        let age = save.metadata().and_then(|m| m.modified()).ok().and_then(|t| now.duration_since(t).ok());
        let when = age.map(describe_age).unwrap_or_else(|| "unknown".to_string());
        let name = save.file_name().unwrap_or_default().to_string_lossy();
        println!("  {:>2}  {name}  ({when})", i + 1);
    }
    if saves.len() > TERRAIN_SAVE_CHOICES {
        println!("      and {} older ones", saves.len() - TERRAIN_SAVE_CHOICES);
    }

    let shown = saves.len().min(TERRAIN_SAVE_CHOICES);
    loop {
        let input = prompt("\n  Press Enter for the newest, or type a number:")?;
        if input.trim().is_empty() {
            return Ok(&saves[0]);
        }
        if let Some(index) = parse_session_index(&input, shown) {
            return Ok(&saves[index]);
        }
        println!("\n  Please type a number from 1 to {shown}, or press Enter.");
    }
}

fn offer_terrain_for_capture(
    settings: &mut Settings,
    user_dir: &Path,
    out: &Path,
    session_id: u32,
    capture_tick: u64,
) -> io::Result<()> {
    let mut saves: Vec<PathBuf> = std::fs::read_dir(user_dir.join("saves"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();
    if saves.is_empty() {
        return Ok(());
    }
    // Newest first, because that is both the default and the one most likely
    // to be wanted: ground only exists where a save had already been, so a
    // later save can only ever cover more of the factory.
    saves.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)));

    println!();
    let wanted = ask_yes_no(
        "  Add the grass, water and trees under your factory?\n  \
         It looks much better, and takes about a minute.",
        settings.capture_terrain.unwrap_or(false),
    )?;
    settings.capture_terrain = Some(wanted);
    remember(settings);
    if !wanted {
        return Ok(());
    }

    let chosen = ask_terrain_save(&saves)?;

    let factorio = locate_factorio_exe_interactive(settings)?;
    let config = export::ExportConfig {
        factorio,
        user_mods: user_dir.join("mods"),
        mod_source: mod_source_dir()?,
        include_resources: false,
        capture_terrain: true,
        terrain_scan: true,
    };
    add_terrain(chosen, out, &config, Some(session_id), Some(capture_tick));
    Ok(())
}

/// Scan one save for natural ground and put it beside the frames.
///
/// Ground does not change, so one save describes it for the whole timelapse:
/// doing it per frame would repeat the same answer, and during live capture
/// would charge somebody's game for it.
///
/// `expect_session` is the playthrough the caller believes the save belongs to.
/// The mod writes into a folder named for the map seed, so a save from a
/// different game is refused rather than laying an unrelated landscape under
/// the factory. Best effort throughout.
fn add_terrain(
    save: &Path,
    out: &Path,
    config: &export::ExportConfig,
    expect_session: Option<u32>,
    capture_tick: Option<u64>,
) -> bool {
    let label = save.file_name().unwrap_or_default().to_string_lossy().into_owned();
    step(&format!("Reading the ground from {label}"));

    let staged = std::env::temp_dir().join(format!("save-timelapse-terrain-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staged);

    let result = export::scan_terrain(save, &staged, config);
    let copied = match result {
        Ok(scan) => match expect_session {
            Some(want) if want != scan.session_id => {
                eprintln!(
                    "warning: {label} is from a different playthrough ({:08x}, not {want:08x}), \
                     so its ground would not match this timelapse. Skipped.",
                    scan.session_id
                );
                0
            }
            _ => {
                let mut copied = 0usize;
                for file in &scan.files {
                    let Some(name) = file.file_name() else { continue };
                    match std::fs::copy(file, out.join(name)) {
                        Ok(_) => copied += 1,
                        Err(e) => eprintln!("warning: could not copy {}: {e}", name.to_string_lossy()),
                    }
                }
                println!("{copied} surface(s) of ground in {:.1}s", scan.seconds);

                // The one failure this cannot prevent and can always see:
                // Factorio generates chunks as somebody goes, so playing past
                // the last save grows the factory into territory that save
                // never heard of, leaving buildings on nothing.
                if let Some(end) = capture_tick {
                    if scan.tick + TICKS_PER_SECOND * 60 < end {
                        let minutes = (end - scan.tick) / (TICKS_PER_SECOND * 60);
                        println!(
                            "\nnote: that save is {minutes} minute(s) of game time behind the end of \
                             your capture. Anything you built after saving has no ground under it, \
                             because Factorio had not generated that part of the map yet. Save in \
                             game and build this timelapse again to fill it in."
                        );
                    }
                }
                copied
            }
        },
        Err(e) => {
            eprintln!("warning: could not read the ground: {e}");
            0
        }
    };

    let _ = std::fs::remove_dir_all(&staged);
    copied > 0
}

fn run_from_saves(settings: &mut Settings) -> io::Result<PathBuf> {
    let factorio = locate_factorio_exe_interactive(settings)?;
    let user_dir = locate_factorio_user_dir_interactive(settings)?;
    let user_mods = user_dir.join("mods");
    let saves_dir = user_dir.join("saves");

    step("Scanning your saves folder");
    let mut saves: Vec<PathBuf> = std::fs::read_dir(&saves_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();
    saves.sort_by_key(|p| ordering_key(p));
    println!("{} save(s) found", saves.len());

    if saves.is_empty() {
        return Err(io::Error::other(format!("No .zip saves found in {}.", saves_dir.display())));
    }

    println!("\nIn {}:", saves_dir.display());
    for (i, save) in saves.iter().enumerate() {
        println!("  {}) {}", i + 1, save.file_name().unwrap_or_default().to_string_lossy());
    }

    let chosen = loop {
        let input = prompt(
            "\nMultiple saves can be from different playthroughs; combining unrelated ones \
             jumps between different bases in one timelapse.\n\
             Which belong to ONE playthrough? Enter numbers (e.g. 1,3,4), a text filter \
             matching part of the filename, or \"all\":",
        )?;
        let selected = parse_save_selection(&input, &saves);
        if selected.is_empty() {
            println!("That didn't match any save. Try again.");
            continue;
        }
        break selected;
    };

    println!(
        "\nUsing {} save(s): {}\n",
        chosen.len(),
        chosen.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy()).collect::<Vec<_>>().join(", ")
    );

    let capture_terrain = ask_yes_no(
        "Include natural terrain (grass, water, trees, cliffs) around the base? This can \
         significantly increase export size and time (roughly 5x in testing)",
        // Remembered, but still asked. It is a real cost decision rather than
        // a preference, so it keeps being put in front of the user; what is
        // remembered is only which way Enter goes.
        settings.capture_terrain.unwrap_or(false),
    )?;
    settings.capture_terrain = Some(capture_terrain);
    remember(settings);

    // Named after the first save chosen, the earliest one, which usually names
    // the playthrough rather than the moment. Renameable by renaming the
    // folder.
    let name = chosen
        .first()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "from-saves".to_string());
    let out = timelapses_root().join(as_folder_name(&name));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    let workspace = std::env::temp_dir().join(format!("save-timelapse-{}", std::process::id()));
    let config = export::ExportConfig {
        factorio,
        user_mods,
        mod_source: mod_source_dir()?,
        include_resources: false,
        capture_terrain,
        terrain_scan: false,
    };

    let mut done = 0usize;
    let mut milestone_states: Vec<milestone::State> = Vec::new();
    for (index, save) in chosen.iter().enumerate() {
        let label = save.file_name().unwrap_or_default().to_string_lossy().into_owned();
        print!("[{:>3}/{}] {label} ... ", index + 1, chosen.len());
        io::stdout().flush().ok();

        let staged = workspace.join(format!("stage_{index}"));
        match export::export_save(save, &staged, &config) {
            Ok(outcome) => {
                let target = out.join(format!("frame_{index:04}.stfr"));
                let primary = &outcome.frames[0];
                std::fs::rename(primary, &target).or_else(|_| std::fs::copy(primary, &target).map(drop))?;
                // Appended, not overwritten: each save contributes its own
                // one-shot sample at its own real tick.
                if let Some(log) = &outcome.players_log {
                    let mut combined = std::fs::OpenOptions::new().create(true).append(true).open(out.join("players.jsonl"))?;
                    combined.write_all(&std::fs::read(log)?)?;
                }
                if let Some(state) = outcome.milestones {
                    milestone_states.push(state);
                }
                let kib = target.metadata().map(|m| m.len()).unwrap_or(0) / 1024;
                println!("ok, {kib} KiB in {:.1}s", outcome.seconds);
                done += 1;
            }
            Err(err) => println!("failed: {err}"),
        }
        let _ = std::fs::remove_dir_all(&staged);
    }
    let _ = std::fs::remove_dir_all(&workspace);
    println!("\n{done} of {} exported to {}", chosen.len(), out.display());

    if done == 0 {
        return Err(io::Error::other("none of the selected saves exported successfully"));
    }

    // The last save chosen: ground is scanned once for the whole timelapse,
    // and the latest save's map is generated furthest out.
    if capture_terrain {
        if let Some(last) = chosen.last() {
            add_terrain(last, &out, &config, None, None);
        }
    }

    // Milestones cannot be derived from a single save, which reports only
    // totals. When something first became true needs consecutive ones.
    let milestones = milestone::from_saves(milestone_states);
    if !milestones.is_empty() {
        match milestone::write_jsonl(&out.join("milestones.jsonl"), &milestones) {
            Ok(()) => println!(
                "{} milestone(s) recovered by comparing saves; each is marked at the first save \
                 that shows it, so they are as precise as your save cadence",
                milestones.len()
            ),
            // Markers are an annotation on a timelapse that is already built
            // and already usable, so failing to write them is worth saying
            // and not worth failing over.
            Err(e) => eprintln!("warning: could not write milestones: {e}"),
        }
    }

    Ok(out)
}

/// Writes the settings, reporting a failure without treating it as one. Not
/// remembering an answer costs one prompt next time, which is no reason to
/// refuse to build somebody's timelapse.
fn remember(settings: &Settings) {
    if let Err(e) = settings.save() {
        eprintln!("warning: could not save your preferences ({e}); they will be asked again next time");
    }
}

/// Opens the viewer without waiting. `spawn`, not `status`: the viewer is a
/// window somebody keeps open, so blocking would freeze this program behind it,
/// and detaching is what lets the menu come straight back.
fn open_viewer(path: &Path) -> io::Result<()> {
    let viewer = viewer_path()?;
    println!("\n  Opening the viewer.\n");
    // The viewer narrates its loading on stdout and inherits this console, so
    // it would land on top of the menu. Discarding stdout and keeping stderr
    // silences the narration without silencing a viewer that failed.
    Command::new(&viewer).arg(path).stdout(Stdio::null()).spawn()?;
    Ok(())
}

/// The menu. Returns only when the user asks to leave, or when there is nobody
/// left to ask. Every action returns here, including ones that open the viewer
/// and ones that fail.
fn run() -> io::Result<()> {
    // Said once, on the run that has nothing saved yet, so the questions that
    // follow read as setup rather than as something that happens forever.
    // Checked before loading, since loading is what would create the file.
    let first_run = Settings::is_first_run();
    let mut settings = Settings::load();

    println!("\n  Save Timelapse\n");
    if first_run {
        println!("  Your answers are remembered, so you are only asked once.\n");
    }

    loop {
        let built = list_timelapses();
        let outcome = match ask_mode(built.len())? {
            // Straight to the viewer, building nothing. The whole point: a
            // timelapse that already exists should not have to be made again
            // to be looked at.
            Mode::OpenExisting => {
                if built.is_empty() {
                    println!("\nYou haven't built one yet. Try option 2 or 3 first.\n");
                    continue;
                }
                match ask_timelapse_choice(&built, "Which one?")? {
                    Some(index) => open_viewer(&built[index].path),
                    // Back to the menu rather than out of the program, the
                    // same as leaving the management screen.
                    None => {
                        println!();
                        continue;
                    }
                }
            }
            Mode::LiveCapture => run_live_capture(&mut settings).and_then(|out| open_viewer(&out)),
            Mode::FromSaves => run_from_saves(&mut settings).and_then(|out| open_viewer(&out)),
            // No viewer afterwards: the export has already produced a video
            // file, and opening a timelapse on top of it would be answering a
            // question nobody asked.
            Mode::ExportVideo => run_export(&mut settings),
            Mode::ManageCaptures => locate_factorio_user_dir_interactive(&mut settings).and_then(|dir| {
                // Locating the folder may well have been the thing that needed
                // asking, and it is worth keeping even though nothing was
                // built.
                remember(&settings);
                manage_captures(&dir.join("script-output").join("save-timelapse"))
            }),
            Mode::Quit => return Ok(()),
        };

        match outcome {
            Ok(()) => println!(),
            // The one failure that cannot be recovered from by trying
            // something else, because trying something else also needs input.
            Err(e) if input_closed(&e) => return Err(e),
            Err(e) => show_problem(&e),
        }
    }
}

/// Bytes as something a person can compare at a glance. Captures range from
/// a few hundred KiB to several GiB, so a single unit would either be
/// unreadably long or lose the distinction between the small ones.
fn describe_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
    for (unit, scale) in UNITS {
        if bytes >= scale {
            return format!("{:.1} {unit}", bytes as f64 / scale as f64);
        }
    }
    format!("{bytes} B")
}

/// A raw surface name as a player would say it. Factorio's own names are
/// lowercase (`nauvis`, `platform-1`), which reads like a database key next to
/// prose.
fn pretty_place(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// The places in a recording, named rather than listed exhaustively. A Space
/// Age playthrough reaches five planets and any number of platforms, which ran
/// to nine names on a real capture. Two and a count says the same thing.
fn describe_places(surfaces: &[String]) -> String {
    match surfaces {
        [] => "nothing yet".to_string(),
        [one] => pretty_place(one),
        [one, two] => format!("{} and {}", pretty_place(one), pretty_place(two)),
        [one, two, rest @ ..] => {
            format!("{}, {} and {} more", pretty_place(one), pretty_place(two), rest.len())
        }
    }
}

/// How much play time a built timelapse covers, from the snapshot it starts
/// at to the last thing replayed. Minutes are dropped once there are hours:
/// at that scale they are noise, and a round number is easier to hold on to.
fn describe_span(from_tick: u64, to_tick: u64) -> String {
    let minutes = to_tick.saturating_sub(from_tick) / TICKS_PER_SECOND / 60;
    match (minutes / 60, minutes) {
        (0, 0) => "less than a minute of play".to_string(),
        (0, 1) => "1 minute of play".to_string(),
        (0, m) => format!("{m} minutes of play"),
        (1, _) => "1 hour of play".to_string(),
        (h, _) => format!("{h} hours of play"),
    }
}

/// How far into a playthrough a tick is, in hours of play.
fn describe_play_time(tick: u64) -> String {
    let hours = tick / TICKS_PER_SECOND / 3600;
    match hours {
        0 => "under an hour in".to_string(),
        1 => "1 hour in".to_string(),
        n => format!("{n} hours in"),
    }
}

/// One line describing a recording, for the picker before a rebuild. Leads with
/// the name when there is one, the places when there is not: a hex session id
/// identifies a recording perfectly and tells the person choosing between two
/// of them nothing. Size belongs in the management screen, where the question
/// is what to delete.
fn describe_session(session: &replay::Session, now: SystemTime) -> String {
    let age = describe_age(now.duration_since(session.last_modified).unwrap_or_default());
    let places = describe_places(&session.baseline.surfaces);
    // Older captures wrote only a total, which counts the trees and ore a
    // capture keeps for context alongside what somebody built.
    let buildings = session.baseline.buildings.unwrap_or(session.baseline.entities);
    let scale = format!("{} buildings, {}", with_thousands(buildings as u64), describe_play_time(session.baseline.tick));
    match session.label() {
        Some(name) => format!("{name}  ({places})\n     {scale}, last played {age}"),
        None => format!("{places}\n     {scale}, last played {age}"),
    }
}

/// The same recording, plus what it costs on disk, for the management screen.
fn describe_session_with_size(session: &replay::Session, now: SystemTime) -> String {
    format!("{}, {}", describe_session(session, now), describe_size(session.size_on_disk()))
}

/// The capture management screen: name a playthrough, see what each costs on
/// disk, delete ones finished with. Deleting is offered here because the
/// in-game reset only removes the playthrough currently loaded.
fn manage_captures(capture_dir: &Path) -> io::Result<()> {
    loop {
        let mut sessions = replay::discover_sessions(capture_dir).unwrap_or_default();
        if sessions.is_empty() {
            println!(
                "
No captures found in {}.",
                capture_dir.display()
            );
            return Ok(());
        }

        let now = SystemTime::now();
        let total: u64 = sessions.iter().map(replay::Session::size_on_disk).sum();
        println!("\n  {} recordings, {} in total:\n", sessions.len(), describe_size(total));
        for (i, session) in sessions.iter().enumerate() {
            println!("  {}  {}\n", i + 1, describe_session_with_size(session, now));
        }

        let action = prompt(
            "  Type a number to rename one, or \"d\" and a number to delete one.\n\
             \x20 Press Enter to go back:",
        )?;
        let action = action.trim().to_string();
        if action.is_empty() {
            return Ok(());
        }

        if let Some(rest) = action.strip_prefix('d') {
            let Some(index) = parse_session_index(rest, sessions.len()) else {
                println!("Please enter \"d\" followed by a number between 1 and {}.", sessions.len());
                continue;
            };
            // Named in the question rather than just numbered: a number is
            // easy to mistype, and this cannot be undone.
            let session = sessions.remove(index);
            let described = describe_session(&session, now);

            // Spelling out what is lost. "Delete this capture" sounds like
            // discarding a rendered video; it is the only copy of that
            // playthrough's history, a save storing no construction record.
            // The freeze is worth naming too.
            println!("\nThis permanently deletes {}", session.session_dir.display());
            println!(
                "That is the entire capture: the baseline snapshot and every construction event \
                 recorded since. It cannot be recovered from your saves, because Factorio keeps no \
                 history of when things were built. Capturing this playthrough again starts from a \
                 fresh baseline, which freezes the game while it scans the base (tens of seconds on \
                 a megabase)."
            );
            if ask_yes_no(&format!("Delete \"{described}\"?"), false)? {
                match session.delete() {
                    Ok(()) => println!("Deleted."),
                    Err(e) => println!("Could not delete it: {e}"),
                }
            } else {
                println!("Left alone.");
            }
            continue;
        }

        let Some(index) = parse_session_index(&action, sessions.len()) else {
            println!("Please enter a number between 1 and {}, or \"d <number>\" to delete.", sessions.len());
            continue;
        };
        let current = sessions[index].label().unwrap_or_default();
        let hint = if current.is_empty() {
            "Enter a name for this capture (or press Enter to leave it unnamed):".to_string()
        } else {
            format!("Enter a new name (currently \"{current}\", press Enter to clear it):")
        };
        let name = prompt(&hint)?;
        sessions[index].set_label(&name)?;
    }
}

/// Copies the plain-JSON sidecar logs a live capture writes into the rendered
/// output. A straight copy rather than a re-parse, the mod's logs and what the
/// viewer reads being the same shape by design. Each being absent is normal.
fn copy_session_sidecars(session_dir: &Path, out: &Path) -> io::Result<Vec<&'static str>> {
    let mut copied = Vec::new();
    for name in ["players.jsonl", "milestones.jsonl", "prototypes.json"] {
        let source = session_dir.join(name);
        if source.exists() {
            std::fs::copy(&source, out.join(name))?;
            copied.push(name);
        }
    }
    Ok(copied)
}

/// A double-clicked console closes the instant the process exits, so an error
/// message is a flash and nothing else. Waiting for Enter gives somebody who
/// never typed a command a chance to read it.
fn wait_for_enter() {
    print!("\nPress Enter to close this window...");
    io::stdout().flush().ok();
    let mut discard = String::new();
    io::stdin().read_line(&mut discard).ok();
}

fn main() {
    // A panic would otherwise skip the pause below: Rust unwinds straight past
    // the `Err` handling and Windows closes the console either way. The hook
    // makes a bug fail the same way a handled error does.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("\nsave-timelapse hit an unexpected error: {info}");
        wait_for_enter();
    }));

    match std::panic::catch_unwind(run) {
        Ok(Ok(())) => {}
        // Only reachable now when stdin closed, since every other failure is
        // handled inside the menu loop and returns to it. Nothing to pause
        // on: a closed stdin means the pause could not wait either.
        Ok(Err(e)) => {
            eprintln!("\n{e}");
            std::process::exit(1);
        }
        // The panic hook above already printed the message and waited.
        Err(_) => std::process::exit(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both names come from the user, a playthrough label or a save
    /// filename, so both can hold anything a filesystem will not.
    #[test]
    fn folder_names_survive_whatever_the_user_called_it() {
        assert_eq!(as_folder_name("My Megabase"), "My Megabase");
        assert_eq!(as_folder_name("Nauvis/Run:2"), "Nauvis_Run_2");
        assert_eq!(as_folder_name("  spaced  "), "spaced");
        // Empty or punctuation-only would otherwise produce a folder named
        // "" or ".", one of which cannot be created and the other of which
        // is the parent directory.
        assert_eq!(as_folder_name(""), "timelapse");
        assert_eq!(as_folder_name("..."), "timelapse");
        assert_eq!(as_folder_name("///"), "___");
    }

    fn built(root: &Path, name: &str, frames: usize) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..frames {
            std::fs::write(dir.join(format!("frame_{i:04}.stfr")), b"pretend frame").unwrap();
        }
    }

    #[test]
    fn built_timelapses_are_listed_with_their_frame_counts() {
        let root = tempfile::tempdir().unwrap();
        built(root.path(), "alpha", 3);
        built(root.path(), "beta", 7);

        let found = list_timelapses_in(root.path());
        assert_eq!(found.len(), 2);
        let alpha = found.iter().find(|t| t.name == "alpha").expect("alpha listed");
        assert_eq!(alpha.frames, 3);
        assert!(alpha.bytes > 0);
    }

    /// A folder with no frames is an interrupted or failed build, and
    /// offering to open it would only ever lead to the viewer reporting it
    /// found nothing.
    #[test]
    fn a_folder_with_no_frames_is_not_offered() {
        let root = tempfile::tempdir().unwrap();
        built(root.path(), "empty", 0);
        std::fs::write(root.path().join("empty").join("players.jsonl"), b"{}").unwrap();
        built(root.path(), "real", 1);

        let found = list_timelapses_in(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "real");
    }

    /// Nothing built yet is an ordinary state on a first run, not an error.
    #[test]
    fn a_missing_root_lists_nothing_rather_than_failing() {
        let root = tempfile::tempdir().unwrap();
        assert!(list_timelapses_in(&root.path().join("never-created")).is_empty());
    }

    /// Both sidecars a live capture writes have to land next to the frames,
    /// or the viewer never sees them: it looks for them beside the frames it
    /// was pointed at, not in the capture folder they came from.
    #[test]
    fn both_session_sidecars_are_copied_next_to_the_frames() {
        let dir = tempfile::tempdir().unwrap();
        let (session, out) = (dir.path().join("session"), dir.path().join("out"));
        std::fs::create_dir_all(&session).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(
            session.join("players.jsonl"),
            "{\"tick\":1,\"players\":[]}
",
        )
        .unwrap();
        std::fs::write(
            session.join("milestones.jsonl"),
            "{\"tick\":9,\"kind\":\"rocket\",\"id\":\"rocket-launched\"}
",
        )
        .unwrap();

        let copied = copy_session_sidecars(&session, &out).unwrap();
        assert_eq!(copied, ["players.jsonl", "milestones.jsonl"]);

        // Copied verbatim, not re-encoded: the mod's shape and the reader's
        // are the same by design, so the reader must accept it unchanged.
        let milestones = save_timelapse::milestone::read(&out.join("milestones.jsonl")).unwrap();
        assert_eq!(milestones.len(), 1);
        assert_eq!(milestones[0].label(), "First rocket launched");
    }

    /// Every combination of the two being absent is ordinary, not an error:
    /// nobody connected, or nothing notable happened yet, or the capture
    /// predates either file existing.
    #[test]
    fn missing_sidecars_are_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (session, out) = (dir.path().join("session"), dir.path().join("out"));
        std::fs::create_dir_all(&session).unwrap();
        std::fs::create_dir_all(&out).unwrap();

        assert!(copy_session_sidecars(&session, &out).unwrap().is_empty());

        std::fs::write(session.join("milestones.jsonl"), "").unwrap();
        assert_eq!(copy_session_sidecars(&session, &out).unwrap(), ["milestones.jsonl"]);
    }

    /// The export menu asks which world to render, and the only record of
    /// which worlds a built timelapse holds is its own filenames.
    #[test]
    fn surfaces_are_read_off_the_frame_filenames() {
        let root = tempfile::tempdir().unwrap();
        for name in ["frame_0000_nauvis.stfr", "frame_0001_nauvis.stfr", "frame_0000_vulcanus.stfr", "terrain_nauvis.stfr"] {
            std::fs::write(root.path().join(name), b"x").unwrap();
        }
        assert_eq!(surfaces_in(root.path()), vec!["nauvis", "vulcanus"]);
    }

    /// A single-surface build writes untagged filenames, so there is no name
    /// to find. Reporting none is what makes the caller skip the question
    /// entirely, which is right: there is nothing to choose between.
    #[test]
    fn an_untagged_single_surface_build_lists_no_surfaces() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("frame_0000.stfr"), b"x").unwrap();
        assert!(surfaces_in(root.path()).is_empty());
    }

    /// Space platforms are named by the player, so a surface name can hold
    /// an underscore. Splitting at the last one would truncate it.
    #[test]
    fn a_surface_name_containing_an_underscore_survives() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("frame_0003_my_platform.stfr"), b"x").unwrap();
        assert_eq!(surfaces_in(root.path()), vec!["my_platform"]);
    }

    #[test]
    fn parse_resolution_accepts_a_preset_number() {
        assert_eq!(parse_resolution("1"), Some((1920, 1080)));
        assert_eq!(parse_resolution(" 2 "), Some((1280, 720)));
        assert_eq!(parse_resolution("4"), Some((3840, 2160)));
    }

    #[test]
    fn parse_resolution_accepts_an_explicit_size_in_either_case() {
        assert_eq!(parse_resolution("1600x900"), Some((1600, 900)));
        assert_eq!(parse_resolution("1600X900"), Some((1600, 900)));
    }

    /// A zero in either half would make a render target no GPU will accept,
    /// and an out-of-range preset number is a typo, not a size.
    #[test]
    fn parse_resolution_rejects_zero_out_of_range_presets_and_junk() {
        assert_eq!(parse_resolution("0"), None);
        assert_eq!(parse_resolution("9"), None);
        assert_eq!(parse_resolution("0x1080"), None);
        assert_eq!(parse_resolution("1920x0"), None);
        assert_eq!(parse_resolution("big"), None);
        assert_eq!(parse_resolution(""), None);
    }

    #[test]
    fn parse_fps_accepts_a_playable_rate_and_rejects_the_rest() {
        assert_eq!(parse_fps("30"), Some(30));
        assert_eq!(parse_fps(" 1 "), Some(1));
        assert_eq!(parse_fps("240"), Some(240));
        assert_eq!(parse_fps("0"), None);
        assert_eq!(parse_fps("241"), None);
        assert_eq!(parse_fps("fast"), None);
    }

    /// Half an answer is no answer: a width with no height cannot be
    /// completed without inventing an aspect ratio the user never chose.
    #[test]
    fn a_half_remembered_export_size_is_not_used() {
        let mut settings = Settings::default();
        assert_eq!(settings.export_size(), None);
        settings.export_width = Some(1920);
        assert_eq!(settings.export_size(), None);
        settings.export_height = Some(1080);
        assert_eq!(settings.export_size(), Some((1920, 1080)));
    }

    #[test]
    fn parse_yes_no_accepts_short_and_long_forms_case_insensitively() {
        assert_eq!(parse_yes_no("y"), Some(true));
        assert_eq!(parse_yes_no("Yes"), Some(true));
        assert_eq!(parse_yes_no("n"), Some(false));
        assert_eq!(parse_yes_no("NO"), Some(false));
    }

    #[test]
    fn parse_yes_no_rejects_anything_else_including_empty() {
        assert_eq!(parse_yes_no(""), None);
        assert_eq!(parse_yes_no("sure"), None);
        assert_eq!(parse_yes_no("maybe"), None);
    }

    #[test]
    fn parse_frame_seconds_accepts_positive_whole_numbers() {
        assert_eq!(parse_frame_seconds("60"), Some(60));
        assert_eq!(parse_frame_seconds(" 15 "), Some(15));
    }

    #[test]
    fn parse_frame_seconds_rejects_zero_negative_and_non_numeric_input() {
        assert_eq!(parse_frame_seconds(""), None);
        assert_eq!(parse_frame_seconds("0"), None);
        assert_eq!(parse_frame_seconds("-5"), None);
        assert_eq!(parse_frame_seconds("soon"), None);
    }

    #[test]
    fn describe_size_picks_a_unit_a_person_can_compare() {
        assert_eq!(describe_size(0), "0 B");
        assert_eq!(describe_size(512), "512 B");
        assert_eq!(describe_size(1024), "1.0 KiB");
        assert_eq!(describe_size(38 * (1 << 20)), "38.0 MiB");
        assert_eq!(describe_size(3 * (1 << 30) / 2), "1.5 GiB");
    }

    /// A recording with no name has to stay identifiable. It used to be its
    /// session id, which is unique and completely uninformative.
    #[test]
    fn an_unnamed_recording_is_described_by_where_it_happened() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("0000002a");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("baseline.json"), r#"{"tick":100,"entities":7,"tiles":3,"surfaces":["nauvis"]}"#)
            .unwrap();

        let sessions = replay::discover_sessions(dir.path()).unwrap();
        let line = describe_session(&sessions[0], SystemTime::now());
        assert!(line.starts_with("Nauvis"), "leads with the place: {line}");
        assert!(line.contains("7 buildings"), "got: {line}");
        // The things it deliberately stopped saying, none of which help
        // anybody choose between two recordings.
        assert!(!line.contains("0000002a"), "no session id: {line}");
        assert!(!line.contains("tick"), "no raw tick: {line}");

        sessions[0].set_label("Vulcanus run").unwrap();
        let named = describe_session(&replay::discover_sessions(dir.path()).unwrap()[0], SystemTime::now());
        assert!(named.starts_with("Vulcanus run"), "a named recording leads with its name: {named}");
    }

    /// A Space Age playthrough reaches five planets plus any number of space
    /// platforms, and the full comma-separated list ran to nine names on a
    /// real capture. Two and a count is what fits on a line.
    #[test]
    fn places_are_named_up_to_two_then_counted() {
        let of = |names: &[&str]| describe_places(&names.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(of(&[]), "nothing yet");
        assert_eq!(of(&["nauvis"]), "Nauvis");
        assert_eq!(of(&["nauvis", "vulcanus"]), "Nauvis and Vulcanus");
        assert_eq!(of(&["nauvis", "vulcanus", "fulgora"]), "Nauvis, Vulcanus and 1 more");
        assert_eq!(of(&["nauvis", "platform-1", "a", "b", "c"]), "Nauvis, Platform-1 and 3 more");
    }

    #[test]
    fn counts_are_grouped_so_they_read_as_quantities() {
        assert_eq!(with_thousands(0), "0");
        assert_eq!(with_thousands(999), "999");
        assert_eq!(with_thousands(1000), "1,000");
        assert_eq!(with_thousands(945480), "945,480");
    }

    /// Minutes vanish once there are hours: at that scale they are noise, and
    /// the singular cases are the ones that read wrong if left unhandled.
    #[test]
    fn a_built_span_is_rounded_to_something_sayable() {
        let hour = TICKS_PER_SECOND * 3600;
        assert_eq!(describe_span(0, 0), "less than a minute of play");
        assert_eq!(describe_span(0, TICKS_PER_SECOND * 60), "1 minute of play");
        assert_eq!(describe_span(0, TICKS_PER_SECOND * 60 * 19), "19 minutes of play");
        assert_eq!(describe_span(0, hour), "1 hour of play");
        assert_eq!(describe_span(hour, hour * 4), "3 hours of play");
    }

    #[test]
    fn describe_age_just_now_for_under_a_minute() {
        assert_eq!(describe_age(Duration::from_secs(30)), "just now");
    }

    #[test]
    fn describe_age_minutes() {
        assert_eq!(describe_age(Duration::from_secs(60)), "1 minute ago");
        assert_eq!(describe_age(Duration::from_secs(60 * 5)), "5 minutes ago");
    }

    #[test]
    fn describe_age_hours() {
        assert_eq!(describe_age(Duration::from_secs(3600)), "1 hour ago");
        assert_eq!(describe_age(Duration::from_secs(3600 * 3)), "3 hours ago");
    }

    #[test]
    fn describe_age_days() {
        assert_eq!(describe_age(Duration::from_secs(86400)), "1 day ago");
        assert_eq!(describe_age(Duration::from_secs(86400 * 2)), "2 days ago");
    }

    #[test]
    fn parse_session_index_accepts_a_one_based_number_in_range() {
        assert_eq!(parse_session_index("1", 3), Some(0));
        assert_eq!(parse_session_index("3", 3), Some(2));
    }

    #[test]
    fn parse_session_index_rejects_zero_out_of_range_and_non_numeric() {
        assert_eq!(parse_session_index("0", 3), None);
        assert_eq!(parse_session_index("4", 3), None);
        assert_eq!(parse_session_index("nope", 3), None);
        assert_eq!(parse_session_index("", 3), None);
    }

    #[test]
    fn ordering_key_sorts_numerically_not_lexicographically() {
        let mut saves = vec![PathBuf::from("base2.zip"), PathBuf::from("base10.zip"), PathBuf::from("base1.zip")];
        saves.sort_by_key(|p| ordering_key(p));
        assert_eq!(saves, vec![PathBuf::from("base1.zip"), PathBuf::from("base2.zip"), PathBuf::from("base10.zip")]);
    }

    #[test]
    fn ordering_key_falls_back_to_name_when_no_digits_are_present() {
        let mut saves = vec![PathBuf::from("zzz.zip"), PathBuf::from("aaa.zip")];
        saves.sort_by_key(|p| ordering_key(p));
        assert_eq!(saves, vec![PathBuf::from("aaa.zip"), PathBuf::from("zzz.zip")]);
    }

    #[test]
    fn find_surface_matches_case_insensitively() {
        let surfaces = vec!["nauvis".to_string(), "vulcanus".to_string()];
        assert_eq!(find_surface("Vulcanus", &surfaces), Some("vulcanus"));
    }

    #[test]
    fn find_surface_returns_none_for_no_match() {
        let surfaces = vec!["nauvis".to_string()];
        assert_eq!(find_surface("fulgora", &surfaces), None);
    }

    fn saves(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn parse_save_selection_all_is_case_insensitive() {
        let s = saves(&["a.zip", "b.zip"]);
        assert_eq!(parse_save_selection("ALL", &s), s);
        assert_eq!(parse_save_selection("all", &s), s);
    }

    #[test]
    fn parse_save_selection_by_index() {
        let s = saves(&["a.zip", "b.zip", "c.zip"]);
        assert_eq!(parse_save_selection("1,3", &s), vec![s[0].clone(), s[2].clone()]);
    }

    #[test]
    fn parse_save_selection_by_index_tolerates_spaces_and_ignores_out_of_range() {
        let s = saves(&["a.zip", "b.zip"]);
        assert_eq!(parse_save_selection("1, 5", &s), vec![s[0].clone()]);
    }

    #[test]
    fn parse_save_selection_by_name_filter() {
        let s = saves(&["base1.zip", "base2.zip", "other.zip"]);
        assert_eq!(parse_save_selection("base", &s), vec![s[0].clone(), s[1].clone()]);
    }

    #[test]
    fn parse_save_selection_empty_input_selects_nothing() {
        let s = saves(&["a.zip"]);
        assert!(parse_save_selection("", &s).is_empty());
        assert!(parse_save_selection("   ", &s).is_empty());
    }

    #[test]
    fn parse_save_selection_filter_with_no_matches_is_empty() {
        let s = saves(&["a.zip"]);
        assert!(parse_save_selection("zzz", &s).is_empty());
    }
}
