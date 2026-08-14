//! save-timelapse: one interactive tool, no flags to learn. Asks what you want
//! to do, asks whatever it could not auto-detect, then opens the viewer on the
//! result.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use save_timelapse::build;
use save_timelapse::export;
use save_timelapse::frame;
use save_timelapse::locate::{factorio_user_dir, locate_factorio};
use save_timelapse::milestone;
use save_timelapse::replay::{self, Options};
use save_timelapse::settings::Settings;
use save_timelapse::with_thousands;
use save_timelapse::world;

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
             \x20   5  Manage log data, timelapses and videos\n\
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

/// Which places to build, as their real surface names. Never empty: Enter
/// means all of them.
///
/// Numbered rather than typed, like every other list here. Names are the one
/// thing a player has no reason to know: the places they recognise are called
/// `nauvis` and `platform-5` underneath, and a capture across four planets and
/// three platforms is a lot to spell correctly.
fn ask_surface_choice(surfaces: &[String]) -> io::Result<Vec<String>> {
    println!("\n  Which places should the timelapse include?\n");
    for (i, surface) in surfaces.iter().enumerate() {
        println!("  {}  {}", i + 1, pretty_place(surface));
    }
    loop {
        let input = prompt("\n  Type numbers separated by spaces, or press Enter for all of them:")?;
        if input.trim().is_empty() {
            return Ok(surfaces.to_vec());
        }
        match parse_index_list(&input, surfaces.len()) {
            Some(chosen) => return Ok(chosen.into_iter().map(|i| surfaces[i].clone()).collect()),
            None => println!("\n  Please type numbers from 1 to {}, separated by spaces.\n", surfaces.len()),
        }
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

/// Every index in a list like "1 3 4", "1,3,4" or "1, 3 4".
///
/// All or nothing: one bad entry rejects the whole line rather than quietly
/// selecting the part that parsed, because a typo in a list of places to build
/// would otherwise be discovered only after the build. Repeats collapse and
/// order is kept as typed.
fn parse_index_list(input: &str, count: usize) -> Option<Vec<usize>> {
    let mut chosen: Vec<usize> = Vec::new();
    for token in input.split(|c: char| c == ',' || c.is_whitespace()).filter(|token| !token.is_empty()) {
        let index = parse_session_index(token, count)?;
        if !chosen.contains(&index) {
            chosen.push(index);
        }
    }
    (!chosen.is_empty()).then_some(chosen)
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

/// Rewrites a folder of full snapshots as a delta chain, in place: the first
/// frame stays the whole picture and every one after it becomes only what
/// changed since the one before. Returns bytes before and after.
///
/// Every save reports the entire factory, having no idea another save exists,
/// so writing them through as exported restates everything standing once per
/// save. On a megabase that is 30 MB a frame of which almost all is identical
/// to the frame before, and the live capture path already refuses to pay it.
///
/// Ordered by the tick inside each frame rather than by filename, because a
/// chain has to be built in the order it will be replayed and the viewer
/// replays in tick order. Filenames cannot carry that: Factorio's autosaves
/// rotate, so `_autosave1` is as likely to be the newest as the oldest, and
/// `ordering_key` can only guess from the digits in a name.
///
/// Two saves of one moment are dropped to one here rather than at load. The
/// viewer deduplicates by tick too, but a delta it dropped would take every
/// frame after it along with it.
fn write_as_delta_chain(frames: &[PathBuf]) -> io::Result<(u64, u64)> {
    let size = |path: &PathBuf| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let before: u64 = frames.iter().map(size).sum();

    let mut ordered: Vec<(u64, &PathBuf)> = Vec::new();
    for path in frames {
        match frame::read_header(path) {
            Ok((tick, _)) => ordered.push((tick, path)),
            // Unreadable here means unreadable to the viewer as well, so it is
            // removed rather than left to reset the sequence at load.
            Err(_) => drop(std::fs::remove_file(path)),
        }
    }
    ordered.sort_by_key(|&(tick, _)| tick);

    let mut chain: Vec<&PathBuf> = Vec::new();
    let mut last_tick: Option<u64> = None;
    for (tick, path) in ordered {
        match last_tick == Some(tick) {
            true => drop(std::fs::remove_file(path)),
            false => {
                chain.push(path);
                last_tick = Some(tick);
            }
        }
    }

    let mut previous: Option<frame::Frame> = None;
    for path in &chain {
        let current = frame::read_binary(&std::fs::read(path)?)?;
        // Read before it is written, so rewriting in place is safe: the folder
        // shrinks as it goes rather than needing room for both forms at once.
        if let Some(prev) = &previous {
            std::fs::write(path, frame::write_binary(&world::delta_between(prev, &current).as_out()))?;
        }
        previous = Some(current);
    }

    Ok((before, chain.iter().copied().map(size).sum()))
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

fn ask_timelapse_choice(built: &[build::BuiltTimelapse], question: &str) -> io::Result<Option<usize>> {
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
    println!("\n  This timelapse has more than one world in it.\n");
    for (i, surface) in surfaces.iter().enumerate() {
        println!("  {}  {}", i + 1, pretty_place(surface));
    }
    // Last and numbered like the rest, rather than a word to type: it is one
    // more thing this list can produce, not a different kind of answer.
    let every = surfaces.len() + 1;
    println!("  {every}  One video for each of them");

    loop {
        let input = prompt("\n  Type a number, or press Enter for the busiest one:")?;
        if input.trim().is_empty() {
            return Ok(None);
        }
        match parse_session_index(&input, every) {
            Some(index) if index + 1 == every => return Ok(Some("all".to_string())),
            Some(index) => return Ok(Some(surfaces[index].clone())),
            None => println!("\n  Please type a number from 1 to {every}.\n"),
        }
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
    build::output_dir_next_to_exe("videos")
}

/// Renders a built timelapse to a video file or an image sequence, by running
/// the viewer rather than reimplementing it: rendering needs a GPU context and
/// the whole sprite pipeline.
fn run_export(settings: &mut Settings) -> io::Result<()> {
    let built = build::list_timelapses();
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

    // Only offered when FFmpeg is already installed. The built-in AVI writer is
    // what keeps the tool dependency free, so MP4 is a bonus for people who
    // happen to have FFmpeg rather than something the tool asks anyone to go
    // and install. Asked in terms of what it is for: MJPEG in an AVI is large,
    // and X will not accept one at all.
    let mp4 = video
        && save_timelapse::ffmpeg_available()
        && ask_yes_no(
            "
Make an MP4? It is far smaller and is what sharing sites accept.              Answering no writes an AVI, which needs nothing installed",
            true,
        )?;
    if video && !mp4 && !save_timelapse::ffmpeg_available() {
        println!(
            "
  Writing an AVI. Install FFmpeg and put it on your PATH to get much"
        );
        println!("  smaller MP4s that you can post directly.");
    }

    // Overlays are burned into the pixels, so they are asked before the render
    // rather than toggled afterwards like the viewer's own.
    //
    // The clock defaults on and the marker off, which is not inconsistency:
    // elapsed time is the context almost every timelapse wants and the one
    // thing the footage cannot convey by itself, while where the player stood
    // is a personal touch most videos are better without.
    let has_players = chosen.path.join("players.jsonl").exists();
    let overlay_players = video
        && has_players
        && ask_yes_no(
            "
Show where you were, as a marker following you around the factory?",
            false,
        )?;
    let overlay_clock = video
        && ask_yes_no(
            "
Show the in-game clock, so the video says how long the factory took?",
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
    // No extension here: the viewer appends `.avi` or `.mp4` for a video and
    // treats the path as a folder for an image sequence, so the same argument
    // serves both.
    let target = root.join(build::as_folder_name(&chosen.name));

    println!("\nRendering. A window opens while this runs; leave it alone until it closes.\n");
    // Blocking: every other mode hands off to the viewer and exits, but an
    // export finishes, and returning to the menu before it does would leave
    // its progress printing over a fresh prompt.
    build::video(&build::VideoRequest {
        timelapse: chosen.path.clone(),
        target,
        width: size.0,
        height: size.1,
        surface,
        video,
        fps,
        mp4,
        overlay_players,
        overlay_clock,
    })?;

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

    let chosen_surfaces = ask_surface_choice(&surfaces)?;
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
    let out = build::timelapses_root().join(build::as_folder_name(&name));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    // Terrain is fixed the instant the baseline loads, so it is written once
    // here rather than per emitted frame. Only finds anything for a capture
    // recorded before ground moved into its own scan. Kept first because
    // `offer_terrain_for_capture` overwrites it afterwards, which is the right
    // precedence, a scan covering the factory's final extent.
    match chosen_surfaces.as_slice() {
        [one] => println!("\n  Building your timelapse of {}.\n", pretty_place(one)),
        many if many.len() == surfaces.len() => println!("\n  Building your timelapse.\n"),
        many => println!("\n  Building your timelapse of {}.\n", describe_places(many)),
    }

    let plan = build::Plan {
        surfaces: chosen_surfaces,
        options: Options { interval: frame_seconds * TICKS_PER_SECOND, max_frames: MAX_FRAMES },
    };
    // A console reports by overwriting one line, and only every twenty-fifth
    // frame: the work is fast enough that printing each would spend more time
    // on the terminal than on the timelapse. Nothing here cancels, there being
    // nothing to press while this blocks.
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut on_frame = |written: usize| {
        if written.is_multiple_of(25) {
            print!("\r  {written} frames");
            io::stdout().flush().ok();
        }
    };
    let built = build::timelapse(
        &mut replay_state,
        &chosen.session_dir,
        &out,
        &plan,
        &mut build::Watch { on: &mut on_frame, cancel: &never },
    )?;
    let emitted = built.emitted;
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
    // Recovered rather than lost, but the recording was damaged and saying so
    // is how somebody learns that resetting mid-playthrough has a cost.
    if replay_state.headerless_segments > 0 {
        println!(
            "
  Part of this recording had lost its header and was recovered."
        );
        println!("  This happens if you reset a recording and then load a save from before it.");
    }
    // The events exist and were readable; they simply describe a moment the
    // snapshot is already past, so nothing can be done with them.
    if replay_state.pre_baseline_events > 20 {
        println!(
            "
  {} recorded changes happened before this recording's snapshot and could not be used.",
            with_thousands(replay_state.pre_baseline_events as u64)
        );
        println!("  That usually means a save from before the last reset was loaded and played.");
        println!("  Resetting again from where you are now starts a clean recording.");
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
    add_icons_for_capture(settings, &user_dir, &out);

    Ok(out)
}

/// Icons for a live capture's timelapse. Separate from the ground, which is
/// asked about and often declined: icons are unconditional, being 10 MB and,
/// after the first modpack build, no time at all.
///
/// Silent when Factorio cannot be found. Somebody who has never pointed this
/// tool at an install is not going to be helped by being asked here, and the
/// timelapse is still perfectly usable without artwork.
fn add_icons_for_capture(settings: &mut Settings, user_dir: &Path, out: &Path) {
    let Some(factorio) = settings.factorio_exe.clone().or_else(locate_factorio) else {
        return;
    };
    let Ok(mod_source) = mod_source_dir() else {
        return;
    };
    let config = export::ExportConfig {
        factorio,
        user_mods: user_dir.join("mods"),
        mod_source,
        include_resources: false,
        capture_terrain: false,
        terrain_scan: false,
    };
    add_icons(out, &config);
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

    // Read once per playthrough. A rebuild otherwise launches Factorio again
    // to be told the same ground, which is now most of what a rebuild costs.
    let cached = cached_terrain(user_dir, session_id);
    // Ground cached before the scan collected scenery would be reused forever,
    // leaving out the scenery this exists to supply. One file answering yes
    // settles it; a capture with no scenery anywhere rescans, which is the safe
    // way to be wrong.
    let cached = match cached.iter().any(|p| frame::read_has_entities(p).unwrap_or(false)) {
        true => cached,
        false => Vec::new(),
    };
    println!();
    let wanted = ask_yes_no(
        match cached.is_empty() {
            true => "  Add the grass, water and trees under your factory?\n  It looks much better.",
            false => {
                "  Add the grass, water and trees under your factory?\n  Already read for this playthrough, so this is instant."
            }
        },
        settings.capture_terrain.unwrap_or(false),
    )?;
    settings.capture_terrain = Some(wanted);
    remember(settings);
    if !wanted {
        return Ok(());
    }

    if !cached.is_empty() {
        let copied =
            cached.iter().filter(|file| file.file_name().is_some_and(|name| std::fs::copy(file, out.join(name)).is_ok())).count();
        if copied > 0 {
            println!("  Reused the ground already read for this playthrough.");
            return Ok(());
        }
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

/// Where a scan's ground is kept for next time: this playthrough's own capture
/// folder, which survives a timelapse being rebuilt. `None` when the caller
/// does not know which playthrough it is, in which case there is nothing safe
/// to key the ground to.
fn keep_terrain_beside_capture(session_id: Option<u32>) -> Option<PathBuf> {
    let dir = locate_factorio()?.join("script-output").join("save-timelapse").join(format!("{:08x}", session_id?));
    dir.is_dir().then_some(dir)
}

/// Ground already read for this playthrough, newest first.
///
/// Kept beside the capture rather than only in the timelapse, which is deleted
/// and rebuilt every time. Natural ground does not change, so reading it again
/// means launching Factorio to be told the same thing, which on a megabase is
/// most of what a rebuild costs.
///
/// Empty for a playthrough nobody has said yes to yet.
fn cached_terrain(user_dir: &Path, session_id: u32) -> Vec<PathBuf> {
    let dir = user_dir.join("script-output").join("save-timelapse").join(format!("{session_id:08x}"));
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("terrain_") && n.ends_with(".stfr")))
        .collect();
    found.sort();
    found
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
/// What the icons on disk were dumped for: every mod and its version, taken
/// from the filenames rather than by reading each one.
///
/// A mod's icons change only when the mod does, so this is what decides
/// whether the cached dump still answers. Folders as well as zips, a mod being
/// installable either way.
fn mod_set_stamp(user_mods: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(user_mods)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|item| item.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "mod-settings.dat" && name != "mod-list.json")
        .collect();
    names.sort();
    let mut hash: u64 = 1469598103934665603;
    for byte in names.join(",").bytes() {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    format!("{hash:016x}")
}

/// Icons already dumped for this exact set of mods, or `None` to dump them.
///
/// Kept next to the timelapses rather than inside one, because the answer
/// depends on the mods rather than on the playthrough: two timelapses from one
/// modpack share it, and a timelapse folder is deleted and rebuilt every time.
fn cached_icons(user_mods: &Path) -> PathBuf {
    build::output_dir_next_to_exe("icons").join(mod_set_stamp(user_mods))
}

/// Every entity name the built timelapse actually draws.
///
/// Read from the frames rather than from `prototypes.json`, which lists every
/// prototype the game has rather than the few hundred a factory is made of.
/// Frames are deltas, so a name can first appear in any of them and all are
/// read; they are small for exactly that reason.
fn names_in_timelapse(out: &Path) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    let mut frames: Vec<PathBuf> = std::fs::read_dir(out)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|item| item.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("stfr"))
        .collect();
    frames.sort();
    for path in frames {
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let Ok(frame) = frame::read_binary(&bytes) else { continue };
        for entity in &frame.entities {
            if !names.contains(&*entity.n) {
                names.insert(entity.n.to_string());
            }
        }
    }
    names
}

/// Every entity's icon as the recording game draws it, copied into `out`.
///
/// Without this a modded prototype has no artwork at all and falls back to a
/// flat colour, because its icon lives inside its mod zip under a name that
/// need not match the prototype and is often several layers that the game
/// composites. Factorio will dump them all if asked, so it is asked.
///
/// Asking is the expensive part and is skipped whenever it can be. A vanilla
/// playthrough draws nothing the install cannot already answer for, so it never
/// launches Factorio here at all. Beyond that the dump is cached against the
/// mod set, so a second timelapse from one modpack pays nothing either.
///
/// Note what cannot be optimised: 45 of the 60 seconds a dump takes is loading
/// the mods, not writing the icons, and every icon needs those same prototypes
/// loaded. Asking for eight instead of thirteen hundred would save about a
/// second, and `--dump-icon-sprites` takes no filter regardless. Skipping the
/// run is the only real saving there is.
///
/// Best effort throughout: a timelapse without icons is the one everybody had
/// until now.
fn add_icons(out: &Path, config: &export::ExportConfig) -> bool {
    let Some(data_dir) = export::install_data_dir(&config.factorio) else {
        return false;
    };

    let names = names_in_timelapse(out);
    let missing: Vec<&String> =
        names.iter().filter(|name| save_timelapse::icons::icon_path(None, &data_dir, name).is_none()).collect();
    if missing.is_empty() {
        // Every name is one the install can draw, so there is nothing this
        // game could add. True of any unmodded playthrough.
        return true;
    }

    let cache = cached_icons(&config.user_mods);
    let cached = std::fs::read_dir(&cache).map(|entries| entries.count()).unwrap_or(0);

    if cached == 0 {
        step(&format!(
            "Reading this game's icons, once for this set of mods ({} of {} buildings need them)",
            missing.len(),
            names.len()
        ));
        let staged = std::env::temp_dir().join(format!("save-timelapse-icons-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staged);
        let started = std::time::Instant::now();
        let result = export::dump_entity_icons(&staged, &cache, config);
        let _ = std::fs::remove_dir_all(&staged);
        if let Err(e) = result {
            eprintln!("warning: could not read this game's icons ({e}); modded buildings will draw as flat colours");
            return false;
        }
        println!("read in {:.1}s, kept for next time", started.elapsed().as_secs_f32());
    }

    // Only what this timelapse draws, not all thirteen hundred: a factory is
    // made of a few dozen kinds of thing, and the folder travels with the
    // timelapse.
    let into = out.join("icons");
    if std::fs::create_dir_all(&into).is_err() {
        return false;
    }
    let mut copied = 0usize;
    for name in &names {
        let file = format!("{name}.png");
        if std::fs::copy(cache.join(&file), into.join(&file)).is_ok() {
            copied += 1;
        }
    }
    println!("{copied} icons for this timelapse's buildings");
    copied > 0
}

/// Copies the rail shapes a scan sampled into a timelapse's description.
///
/// Only `rails`, and only where there are none: the scan ran later than the
/// capture and its frames were written against the capture's own answers.
/// Edited as JSON so unknown keys survive.
fn adopt_scanned_rails(scanned: &Path, description: &Path) -> io::Result<bool> {
    let has_rails = |value: &serde_json::Value| value.get("rails").and_then(|r| r.as_array()).is_some_and(|r| !r.is_empty());

    let scanned: serde_json::Value = serde_json::from_slice(&std::fs::read(scanned)?)?;
    if !has_rails(&scanned) {
        return Ok(false);
    }
    let mut current: serde_json::Value = serde_json::from_slice(&std::fs::read(description)?)?;
    if has_rails(&current) {
        return Ok(false);
    }
    let Some(object) = current.as_object_mut() else {
        return Ok(false);
    };
    object.insert("rails".to_string(), scanned["rails"].clone());
    std::fs::write(description, serde_json::to_vec(&current)?)?;
    Ok(true)
}

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
                // Kept beside the capture as well as in the timelapse, so a
                // rebuild reuses it instead of launching Factorio to read the
                // same unchanging ground again. Best effort: failing to keep a
                // copy costs a scan next time, not this timelapse.
                let keep = keep_terrain_beside_capture(expect_session);
                let mut copied = 0usize;
                for file in &scan.files {
                    let Some(name) = file.file_name() else { continue };
                    match std::fs::copy(file, out.join(name)) {
                        Ok(_) => copied += 1,
                        Err(e) => eprintln!("warning: could not copy {}: {e}", name.to_string_lossy()),
                    }
                    if let Some(dir) = &keep {
                        let _ = std::fs::copy(file, dir.join(name));
                    }
                }
                println!("{copied} surface(s) of ground in {:.1}s", scan.seconds);

                if let Some(described) = &scan.prototypes {
                    match adopt_scanned_rails(described, &out.join("prototypes.json")) {
                        Ok(true) => println!("  Rail corners recovered from that save."),
                        Ok(false) => {}
                        Err(e) => eprintln!("warning: could not read rail shapes from the scan: {e}"),
                    }
                }

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
    let out = build::timelapses_root().join(build::as_folder_name(&name));
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

    // One line per save, written as it starts and completed as it finishes,
    // which is what makes a long run readable: the name is on screen while
    // Factorio is loading it rather than only once it is done.
    let never = std::sync::atomic::AtomicBool::new(false);
    let mut on_save = |step: build::SaveStep| match step {
        build::SaveStep::Started { index, total, label } => {
            print!("[{:>3}/{total}] {label} ... ", index + 1);
            io::stdout().flush().ok();
        }
        build::SaveStep::Exported { bytes, seconds, .. } => println!("ok, {} KiB in {seconds:.1}s", bytes / 1024),
        build::SaveStep::Failed { error, .. } => println!("failed: {error}"),
    };
    let exported = build::from_saves(&chosen, &out, &workspace, &config, &mut build::Watch { on: &mut on_save, cancel: &never })?;

    let _ = std::fs::remove_dir_all(&workspace);
    println!("\n{} of {} exported to {}", exported.frames.len(), chosen.len(), out.display());

    if exported.frames.is_empty() {
        return Err(io::Error::other("none of the selected saves exported successfully"));
    }
    let milestone_states = exported.milestones;
    let exported = exported.frames;

    match write_as_delta_chain(&exported) {
        Ok((before, after)) if after < before => println!(
            "  frames reduced from {} MiB to {} MiB, each one keeping only what changed",
            before / (1024 * 1024),
            after / (1024 * 1024)
        ),
        Ok(_) => {}
        // A frame is only rewritten once the one before it has been, so a
        // failure partway leaves a chain followed by full pictures. The viewer
        // reads a full frame as a fresh statement of everything standing and
        // closes whatever it does not mention, so what is left is larger than
        // intended rather than wrong.
        Err(err) => println!("  could not reduce the frames ({err}); they are still usable, just larger"),
    }

    // The last save chosen: ground is scanned once for the whole timelapse,
    // and the latest save's map is generated furthest out.
    if capture_terrain {
        if let Some(last) = chosen.last() {
            add_terrain(last, &out, &config, None, None);
        }
    }

    // Unconditional and unasked, unlike ground: it costs 10 MB and no time
    // at all once cached, where ground is a real size and time decision.
    add_icons(&out, &config);

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
    println!("\n  Opening the viewer.\n");
    // The viewer narrates its loading on stdout and inherits this console, so
    // it would land on top of the menu. Discarding stdout and keeping stderr
    // silences the narration without silencing a viewer that failed.
    build::viewer_command()?.arg(path).stdout(Stdio::null()).spawn()?;
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
        let built = build::list_timelapses();
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
                manage(&dir.join("script-output").join("save-timelapse"))
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
/// One rendered video or image sequence in `videos/`. The viewer writes a file
/// for a video and a folder of numbered frames for a sequence, so both shapes
/// are listed the same way and weighed the same way.
struct BuiltVideo {
    name: String,
    path: PathBuf,
    bytes: u64,
    modified: SystemTime,
}

/// Bytes under `path`, whether it is one file or a folder of frames. Best
/// effort: anything unreadable counts as nothing rather than failing a listing
/// somebody is only trying to read.
fn size_on_disk(path: &Path) -> u64 {
    let Ok(meta) = std::fs::metadata(path) else { return 0 };
    if meta.is_file() {
        return meta.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.filter_map(Result::ok).map(|entry| size_on_disk(&entry.path())).sum()
}

fn list_videos() -> Vec<BuiltVideo> {
    list_videos_in(&videos_root())
}

/// Split from [`list_videos`] for the same reason as [`list_timelapses_in`]:
/// the real root is derived from the running executable's own location.
fn list_videos_in(root: &Path) -> Vec<BuiltVideo> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut found: Vec<BuiltVideo> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some(BuiltVideo {
                name: entry.file_name().to_string_lossy().into_owned(),
                bytes: size_on_disk(&path),
                path,
                modified,
            })
        })
        .collect();
    found.sort_by_key(|v| std::cmp::Reverse(v.modified));
    found
}

/// Deletes `path`, file or folder, so a video and an image sequence are the
/// same operation to the caller.
fn delete_path(path: &Path) -> io::Result<()> {
    match std::fs::metadata(path)?.is_dir() {
        true => std::fs::remove_dir_all(path),
        false => std::fs::remove_file(path),
    }
}

/// The three things this tool leaves on disk, split by what losing one costs.
///
/// The log data is a playthrough's recorded history and the only copy of it: it
/// cannot be got back from a save, because Factorio keeps no record of when
/// anything was built. A timelapse and a video are both made from that log, so
/// deleting either is giving up disk space and a rebuild, not the history.
/// Putting all three on one screen with one delete made the recoverable and the
/// irreversible look alike.
fn manage(capture_dir: &Path) -> io::Result<()> {
    loop {
        let sessions = replay::discover_sessions(capture_dir).unwrap_or_default();
        let recorded: u64 = sessions.iter().map(replay::Session::size_on_disk).sum();
        let timelapses = build::list_timelapses();
        let built: u64 = timelapses.iter().map(|t| t.bytes).sum();
        let videos = list_videos();
        let rendered: u64 = videos.iter().map(|v| v.bytes).sum();

        let input = prompt(&format!(
            "\n  What would you like to manage?\n\n\
             \x20   1  Log data            {:>3}, {}\n\
             \x20   2  Built timelapses    {:>3}, {}\n\
             \x20   3  Videos              {:>3}, {}\n\
             \x20   4  Back\n\n\
             \x20 Type a number:",
            sessions.len(),
            describe_size(recorded),
            timelapses.len(),
            describe_size(built),
            videos.len(),
            describe_size(rendered),
        ))?;

        match input.trim() {
            "1" => manage_captures(capture_dir)?,
            "2" => manage_timelapses()?,
            "3" => manage_videos()?,
            "4" | "" => return Ok(()),
            _ => println!("\n  Please type a number from 1 to 4.\n"),
        }
    }
}

/// Built timelapses. Deleting one costs the time to build it again and nothing
/// else, which is why this asks once and says so rather than spelling out a
/// warning the way [`manage_captures`] has to.
fn manage_timelapses() -> io::Result<()> {
    loop {
        let built = build::list_timelapses();
        if built.is_empty() {
            println!("\n  No timelapses built yet.\n");
            return Ok(());
        }

        let Some(index) = ask_timelapse_choice(&built, "Which would you like to delete?")? else {
            return Ok(());
        };
        let chosen = &built[index];
        let question = format!("Delete \"{}\"? You can build it again from the log data", chosen.name);
        if ask_yes_no(&question, false)? {
            match std::fs::remove_dir_all(&chosen.path) {
                Ok(()) => println!("Deleted."),
                Err(e) => println!("Could not delete it: {e}"),
            }
        } else {
            println!("Left alone.");
        }
    }
}

/// Videos and image sequences. Cheapest of the three to lose: the timelapse it
/// was rendered from is still there.
fn manage_videos() -> io::Result<()> {
    loop {
        let videos = list_videos();
        if videos.is_empty() {
            println!("\n  No videos saved yet.\n");
            return Ok(());
        }

        println!("\n  Your videos:\n");
        for (i, video) in videos.iter().enumerate() {
            let age = video.modified.elapsed().map(describe_age).unwrap_or_else(|_| "unknown".to_string());
            println!("  {}  {}", i + 1, video.name);
            println!("     {}, saved {age}\n", describe_size(video.bytes));
        }

        let input = prompt("  Type a number to delete one, or press Enter to go back:")?;
        if input.trim().is_empty() {
            return Ok(());
        }
        let Some(index) = parse_session_index(&input, videos.len()) else {
            println!("\n  Please type a number from 1 to {}.\n", videos.len());
            continue;
        };
        let chosen = &videos[index];
        let question = format!("Delete \"{}\"? You can save it again from the timelapse", chosen.name);
        if ask_yes_no(&question, false)? {
            match delete_path(&chosen.path) {
                Ok(()) => println!("Deleted."),
                Err(e) => println!("Could not delete it: {e}"),
            }
        } else {
            println!("Left alone.");
        }
    }
}

fn manage_captures(capture_dir: &Path) -> io::Result<()> {
    loop {
        let mut sessions = replay::discover_sessions(capture_dir).unwrap_or_default();
        if sessions.is_empty() {
            println!(
                "
No log data found in {}.",
                capture_dir.display()
            );
            return Ok(());
        }

        let now = SystemTime::now();
        let total: u64 = sessions.iter().map(replay::Session::size_on_disk).sum();
        println!("\n  Log data from {} playthroughs, {} in total:\n", sessions.len(), describe_size(total));
        for (i, session) in sessions.iter().enumerate() {
            println!("  {}  {}\n", i + 1, describe_session_with_size(session, now));
        }

        let action = prompt(
            "  1  Rename one\n\
             \x20 2  Delete one permanently\n\n\
             \x20 Type a number, or press Enter to go back:",
        )?;
        let action = action.trim().to_string();
        if action.is_empty() {
            return Ok(());
        }
        if action != "1" && action != "2" {
            println!("\n  Please type 1 or 2, or press Enter to go back.\n");
            continue;
        }

        let which = prompt(&format!("  Which one? Type a number from 1 to {}:", sessions.len()))?;
        let Some(index) = parse_session_index(&which, sessions.len()) else {
            println!("\n  Please type a number from 1 to {}.\n", sessions.len());
            continue;
        };

        if action == "2" {
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
    // The window, in its own process, launched by `build::viewer_command`.
    // Handled before anything else so the menu's console handling, panic hook
    // and settings loading never run for a process that is only going to draw.
    let args: Vec<String> = std::env::args().skip(1).collect();

    // The window that replaces this menu, still opt in while it is being
    // built: every flow it does not have yet is one the console still does,
    // so both have to work at once until the last screen lands.
    if args.first().is_some_and(|first| first == "--gui") {
        macroquad::Window::from_config(save_timelapse::gui::window_conf(), save_timelapse::gui::run());
        return;
    }

    if args.first().is_some_and(|first| first == build::VIEW_FLAG) {
        let rest: Vec<String> = args[1..].to_vec();
        macroquad::Window::from_config(save_timelapse::viewer::app::window_conf(), async move {
            save_timelapse::viewer::app::run(&rest).await
        });
        return;
    }

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
        assert_eq!(build::as_folder_name("My Megabase"), "My Megabase");
        assert_eq!(build::as_folder_name("Nauvis/Run:2"), "Nauvis_Run_2");
        assert_eq!(build::as_folder_name("  spaced  "), "spaced");
        // Empty or punctuation-only would otherwise produce a folder named
        // "" or ".", one of which cannot be created and the other of which
        // is the parent directory.
        assert_eq!(build::as_folder_name(""), "timelapse");
        assert_eq!(build::as_folder_name("..."), "timelapse");
        assert_eq!(build::as_folder_name("///"), "___");
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

        let found = build::list_timelapses_in(root.path());
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

        let found = build::list_timelapses_in(root.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "real");
    }

    /// Nothing built yet is an ordinary state on a first run, not an error.
    #[test]
    fn a_missing_root_lists_nothing_rather_than_failing() {
        let root = tempfile::tempdir().unwrap();
        assert!(build::list_timelapses_in(&root.path().join("never-created")).is_empty());
    }

    #[test]
    fn a_list_of_numbers_can_be_separated_by_spaces_or_commas() {
        assert_eq!(parse_index_list("1 3", 4), Some(vec![0, 2]));
        assert_eq!(parse_index_list("1,3", 4), Some(vec![0, 2]));
        assert_eq!(parse_index_list("  2 ,3,  1 ", 4), Some(vec![1, 2, 0]));
    }

    /// One bad entry rejects the line. Selecting the part that parsed would
    /// mean a typo silently builds a different set of places than was asked
    /// for, and the build is long enough that nobody would catch it.
    #[test]
    fn one_bad_entry_rejects_the_whole_list() {
        assert_eq!(parse_index_list("1 9", 4), None);
        assert_eq!(parse_index_list("1 nauvis", 4), None);
        assert_eq!(parse_index_list("0", 4), None, "the list is 1 based");
        assert_eq!(parse_index_list("", 4), None, "blank is the caller's business, not a selection");
    }

    /// Typing the same place twice is a slip, not a request for two copies of
    /// it, and the writer would key both to one surface anyway.
    #[test]
    fn repeats_collapse_and_order_is_kept_as_typed() {
        assert_eq!(parse_index_list("3 1 3", 3), Some(vec![2, 0]));
    }

    fn rendered(root: &Path, name: &str, bytes: &[u8]) {
        std::fs::write(root.join(name), bytes).unwrap();
    }

    /// The viewer writes one file for a video and a folder of numbered frames
    /// for an image sequence, so both have to be listed and weighed.
    #[test]
    fn videos_and_image_sequences_are_both_listed() {
        let root = tempfile::tempdir().unwrap();
        rendered(root.path(), "alpha.mp4", b"pretend video");
        let sequence = root.path().join("beta");
        std::fs::create_dir_all(&sequence).unwrap();
        std::fs::write(sequence.join("frame_0000.png"), b"pretend frame").unwrap();
        std::fs::write(sequence.join("frame_0001.png"), b"pretend frame").unwrap();

        let found = list_videos_in(root.path());
        assert_eq!(found.len(), 2);
        let beta = found.iter().find(|v| v.name == "beta").expect("the sequence is listed");
        assert_eq!(beta.bytes, b"pretend frame".len() as u64 * 2, "a folder weighs what is inside it");
    }

    #[test]
    fn no_videos_yet_lists_nothing_rather_than_failing() {
        let root = tempfile::tempdir().unwrap();
        assert!(list_videos_in(&root.path().join("never-created")).is_empty());
    }

    /// Deleting a video has to work on both shapes, since which one it is
    /// depends on an answer given at export time.
    #[test]
    fn deleting_covers_a_file_and_a_folder_alike() {
        let root = tempfile::tempdir().unwrap();
        rendered(root.path(), "alpha.avi", b"pretend video");
        let sequence = root.path().join("beta");
        std::fs::create_dir_all(&sequence).unwrap();
        std::fs::write(sequence.join("frame_0000.png"), b"pretend frame").unwrap();

        delete_path(&root.path().join("alpha.avi")).unwrap();
        delete_path(&sequence).unwrap();
        assert!(list_videos_in(root.path()).is_empty());
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

        let copied = build::copy_sidecars(&session, &out).unwrap();
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

        assert!(build::copy_sidecars(&session, &out).unwrap().is_empty());

        std::fs::write(session.join("milestones.jsonl"), "").unwrap();
        assert_eq!(build::copy_sidecars(&session, &out).unwrap(), ["milestones.jsonl"]);
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

    /// One save's worth of factory, as the mod would have exported it.
    fn snapshot(tick: u64, names: &[&str]) -> frame::Frame {
        frame::Frame {
            tick,
            surface: "nauvis".to_string(),
            count: names.len(),
            entities: names
                .iter()
                .enumerate()
                .map(|(i, n)| frame::Entity { n: std::sync::Arc::from(*n), x: i as f32 + 0.5, y: 0.5, d: 0, w: 1, h: 1 })
                .collect(),
            ..Default::default()
        }
    }

    /// A name can first appear in any frame, deltas carrying only what
    /// changed, so every frame has to be read rather than just the first.
    #[test]
    fn names_in_timelapse_finds_things_built_after_the_first_frame() {
        let dir = tempfile::tempdir().unwrap();
        for (i, frame) in [snapshot(100, &["pipe", "belt"]), snapshot(200, &["assembler"])].iter().enumerate() {
            std::fs::write(dir.path().join(format!("frame_{i:04}.stfr")), frame::write_binary(&frame.as_out())).unwrap();
        }

        let names = names_in_timelapse(dir.path());
        assert_eq!(names.len(), 3, "got {names:?}");
        assert!(names.contains("assembler"), "the one that only exists in a later delta");
    }

    /// Anything that is not a frame is ignored rather than failing the read,
    /// and a timelapse folder holds several such files.
    #[test]
    fn names_in_timelapse_ignores_the_sidecars_beside_the_frames() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("frame_0000.stfr"), frame::write_binary(&snapshot(100, &["pipe"]).as_out())).unwrap();
        std::fs::write(dir.path().join("players.jsonl"), b"{}").unwrap();
        std::fs::write(dir.path().join("prototypes.json"), b"{}").unwrap();
        std::fs::write(dir.path().join("frame_0001.stfr"), b"not a frame at all").unwrap();

        let names = names_in_timelapse(dir.path());
        assert_eq!(names.len(), 1);
        assert!(names.contains("pipe"));
    }

    /// The saving that matters. A vanilla playthrough draws nothing the
    /// install cannot already answer for, so nothing needs dumping and
    /// Factorio is never launched. Checked through the same resolution the
    /// viewer uses, so the two cannot disagree about what counts as missing.
    #[test]
    fn an_unmodded_timelapse_needs_nothing_dumped() {
        let data = tempfile::tempdir().unwrap();
        let icons = data.path().join("base").join("graphics/icons");
        std::fs::create_dir_all(&icons).unwrap();
        for name in ["pipe", "belt"] {
            std::fs::write(icons.join(format!("{name}.png")), b"icon").unwrap();
        }

        let names: Vec<String> = vec!["pipe".to_string(), "belt".to_string()];
        let missing: Vec<&String> =
            names.iter().filter(|n| save_timelapse::icons::icon_path(None, data.path(), n).is_none()).collect();
        assert!(missing.is_empty(), "nothing to ask this game for");

        let modded = "kr-quarry-drill".to_string();
        assert!(
            save_timelapse::icons::icon_path(None, data.path(), &modded).is_none(),
            "and a modded name is what makes the dump worth its minute"
        );
    }

    /// Everything standing at each frame, accumulated the way the viewer
    /// accumulates: a full frame states the world, a delta changes it.
    fn replay_chain(paths: &[std::path::PathBuf]) -> Vec<Vec<String>> {
        let mut ordered: Vec<frame::Frame> =
            paths.iter().filter_map(|p| std::fs::read(p).ok()).filter_map(|b| frame::read_binary(&b).ok()).collect();
        ordered.sort_by_key(|f| f.tick);

        let mut standing: std::collections::HashMap<(i32, i32), String> = std::collections::HashMap::new();
        let mut out = Vec::new();
        for frame in &ordered {
            if !frame.delta {
                standing.clear();
            }
            for pos in &frame.removed_entities {
                standing.remove(pos);
            }
            for e in &frame.entities {
                standing.insert(save_timelapse::world::pos_key(e.x, e.y), e.n.to_string());
            }
            let mut names: Vec<String> = standing.values().cloned().collect();
            names.sort();
            out.push(names);
        }
        out
    }

    /// Factorio's autosaves rotate, so `_autosave1` is as likely to be the
    /// newest as the oldest and the digits in a filename are only a guess at
    /// order. A chain built in the wrong order is not merely mis-sorted: every
    /// frame after the mistake describes changes against the wrong world.
    #[test]
    fn a_delta_chain_is_built_in_tick_order_not_filename_order() {
        let dir = tempfile::tempdir().unwrap();
        // Written newest first, which is what a rotated autosave set looks
        // like once `ordering_key` has sorted it by the digits in the name.
        let written = [snapshot(300, &["pipe", "belt", "lamp"]), snapshot(100, &["pipe"]), snapshot(200, &["pipe", "belt"])];
        let paths: Vec<std::path::PathBuf> = written
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let path = dir.path().join(format!("frame_{i:04}.stfr"));
                std::fs::write(&path, frame::write_binary(&f.as_out())).unwrap();
                path
            })
            .collect();

        write_as_delta_chain(&paths).unwrap();

        let full: Vec<u64> = paths
            .iter()
            .map(|p| frame::read_binary(&std::fs::read(p).unwrap()).unwrap())
            .filter(|f| !f.delta)
            .map(|f| f.tick)
            .collect();
        assert_eq!(full, vec![100], "the earliest tick is the picture, whatever it is called");

        assert_eq!(
            replay_chain(&paths),
            vec![
                vec!["pipe".to_string()],
                vec!["belt".to_string(), "pipe".to_string()],
                vec!["belt".to_string(), "lamp".to_string(), "pipe".to_string()],
            ],
            "replaying the chain rebuilds each save exactly"
        );
    }

    /// A delta the viewer dropped would take every frame after it along with
    /// it, so a duplicated moment is resolved here instead.
    #[test]
    fn two_saves_of_one_moment_leave_one_frame() {
        let dir = tempfile::tempdir().unwrap();
        let written = [snapshot(100, &["pipe"]), snapshot(200, &["pipe", "belt"]), snapshot(200, &["pipe", "belt"])];
        let paths: Vec<std::path::PathBuf> = written
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let path = dir.path().join(format!("frame_{i:04}.stfr"));
                std::fs::write(&path, frame::write_binary(&f.as_out())).unwrap();
                path
            })
            .collect();

        write_as_delta_chain(&paths).unwrap();

        let left: Vec<&std::path::PathBuf> = paths.iter().filter(|p| p.exists()).collect();
        assert_eq!(left.len(), 2, "the repeated moment is gone");
        assert_eq!(replay_chain(&paths).last().unwrap(), &vec!["belt".to_string(), "pipe".to_string()]);
    }

    /// The whole point: a frame that restates an unchanged factory should cost
    /// almost nothing.
    #[test]
    fn an_unchanged_factory_costs_almost_nothing_per_frame() {
        let dir = tempfile::tempdir().unwrap();
        let names: Vec<&str> = vec!["pipe"; 2000];
        let paths: Vec<std::path::PathBuf> = (0..5)
            .map(|i| {
                let path = dir.path().join(format!("frame_{i:04}.stfr"));
                std::fs::write(&path, frame::write_binary(&snapshot(100 * (i + 1), &names).as_out())).unwrap();
                path
            })
            .collect();

        let (before, after) = write_as_delta_chain(&paths).unwrap();
        assert!(after * 4 < before, "five copies of one factory became one copy and four near-empty frames: {before} to {after}");
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

    fn write_json(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn scanned_rails_fill_in_a_description_that_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let scanned = write_json(dir.path(), "scanned.json", r#"{"rails":[{"n":"curved-rail-a"}]}"#);
        let current = write_json(dir.path(), "prototypes.json", r#"{"rails":[],"types":{"a":"b"}}"#);

        assert!(adopt_scanned_rails(&scanned, &current).unwrap());
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&current).unwrap()).unwrap();
        assert_eq!(after["rails"].as_array().unwrap().len(), 1);
        // The capture's own answers are what its frames were written against.
        assert_eq!(after["types"]["a"], "b");
    }

    #[test]
    fn a_description_that_already_has_rails_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let scanned = write_json(dir.path(), "scanned.json", r#"{"rails":[{"n":"new"}]}"#);
        let current = write_json(dir.path(), "prototypes.json", r#"{"rails":[{"n":"old"}]}"#);

        assert!(!adopt_scanned_rails(&scanned, &current).unwrap());
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&current).unwrap()).unwrap();
        assert_eq!(after["rails"][0]["n"], "old");
    }

    #[test]
    fn a_scan_that_sampled_no_rails_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let scanned = write_json(dir.path(), "scanned.json", r#"{"rails":[]}"#);
        let current = write_json(dir.path(), "prototypes.json", r#"{"rails":[]}"#);
        assert!(!adopt_scanned_rails(&scanned, &current).unwrap());
    }
}
