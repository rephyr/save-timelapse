//! save-timelapse: one interactive tool, no flags to learn.
//!
//! Asks what you want to do (rebuild from live capture, or build from
//! existing saves), asks whatever it couldn't auto-detect (Factorio's
//! folder, which surface, which saves belong to one playthrough), then
//! opens the viewer on the result automatically.
//!
//!     save-timelapse.exe

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use save_timelapse::export;
use save_timelapse::frame;
use save_timelapse::locate::{factorio_user_dir, locate_factorio};
use save_timelapse::milestone;
use save_timelapse::replay::{self, Options};
use save_timelapse::settings::{self, Settings};

/// Default game time per frame during live-capture replay, asked about
/// interactively so a longer playthrough can trade a larger export for
/// smoother, more finely-spaced playback.
const DEFAULT_FRAME_SECONDS: u64 = 60;

/// Factorio's normal game speed: one real second is 60 ticks.
const TICKS_PER_SECOND: u64 = 60;

const MAX_FRAMES: usize = 100_000;

enum Mode {
    OpenExisting,
    LiveCapture,
    FromSaves,
    ManageCaptures,
    Quit,
}

/// Prints `question`, reads one line, and trims it. An empty `Ok` on EOF
/// (stdin closed, e.g. `< /dev/null`) would spin the retry loops below
/// forever with no way to make progress, so that comes back as an error
/// instead, which unwinds out through the same friendly-failure path as
/// everything else.
fn prompt(question: &str) -> io::Result<String> {
    print!("{question} ");
    io::stdout().flush()?;
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line)?;
    if read == 0 {
        return Err(io::Error::other("no more input"));
    }
    Ok(line.trim().to_string())
}

/// Announces a step and leaves the line open, so whatever the step finds
/// finishes the sentence.
///
/// Every one of these was previously silent, which is fine when it takes no
/// time and awful when it does: loading a megabase baseline is tens of
/// seconds of a blank screen, and "is this doing anything?" is not a question
/// a tool should make anyone ask. Flushed explicitly because stdout is line
/// buffered, so without it the announcement would appear *after* the work it
/// is announcing, which is worse than not printing it at all.
fn step(what: &str) {
    print!("{what}... ");
    io::stdout().flush().ok();
}

/// `already_built` only changes what option 1 *says*, never the numbering.
/// Hiding it when there is nothing to open would renumber everything else
/// depending on state, and a menu whose option 2 means different things on
/// different days is worse than one line that sometimes reads "none yet".
fn ask_mode(already_built: usize) -> io::Result<Mode> {
    let opened = match already_built {
        0 => "Open a timelapse I already built (none yet)".to_string(),
        1 => "Open a timelapse I already built (1 available)".to_string(),
        n => format!("Open a timelapse I already built ({n} available)"),
    };
    loop {
        let input = prompt(&format!(
            "What would you like to do?\n\
             \x20 1) {opened}\n\
             \x20 2) Update my timelapse from live capture (recommended if capture is on)\n\
             \x20 3) Build a timelapse from existing save files\n\
             \x20 4) Manage my captures (name them, see their size, delete ones you're done with)\n\
             Enter 1, 2, 3 or 4, or press Enter to quit:"
        ))?;
        match input.as_str() {
            "1" => return Ok(Mode::OpenExisting),
            "2" => return Ok(Mode::LiveCapture),
            "3" => return Ok(Mode::FromSaves),
            "4" => return Ok(Mode::ManageCaptures),
            // Managing captures returns here rather than exiting, so this
            // menu needs its own way out. Blank rather than a fifth number:
            // Enter is what someone already presses to leave the management
            // screen, and it should keep meaning "back" one level up.
            "" => return Ok(Mode::Quit),
            _ => println!("Please enter 1, 2, 3 or 4.\n"),
        }
    }
}

/// Factorio's data folder (saves/mods/script-output): remembered answer
/// first, then auto-detection, then asking.
///
/// Validated by checking for a `mods` subfolder rather than accepting any
/// path, since a wrong answer here would otherwise only surface as a
/// confusing failure several steps later. That applies to the remembered
/// path too (see
/// `Settings::valid_factorio_dir`), so a folder that has since moved falls
/// through to detection rather than becoming a confusing failure several
/// steps later. Whatever ends up resolving is written back, which is what
/// makes this a question asked once rather than every run: the case that
/// actually hurt was auto-detection failing, since that meant retyping the
/// same path forever.
fn locate_factorio_user_dir_interactive(settings: &mut Settings) -> io::Result<PathBuf> {
    step("Finding your Factorio folder");

    // Says which of the three it was. "Remembered" versus "found" is the
    // difference between the tool knowing something about this machine and
    // having just worked it out, and it is the only clue anyone gets that a
    // saved setting is in play at all.
    if let Some(dir) = settings.valid_factorio_dir() {
        println!("remembered from last time\n  {}", dir.display());
        return Ok(dir.to_path_buf());
    }
    if let Some(dir) = factorio_user_dir() {
        if dir.join("mods").is_dir() {
            println!("found at\n  {}", dir.display());
            settings.factorio_dir = Some(dir.clone());
            return Ok(dir);
        }
    }
    println!("not found automatically.\n");
    loop {
        let input = prompt(
            "Please enter the path to it (the folder containing \"mods\" and \"saves\", \
             usually %APPDATA%\\Factorio on Windows):",
        )?;
        let path = PathBuf::from(&input);
        if path.join("mods").is_dir() {
            settings.factorio_dir = Some(path.clone());
            return Ok(path);
        }
        println!("That doesn't look right: no \"mods\" folder inside {}.\n", path.display());
    }
}

/// Same idea for the actual game executable, only needed by the from-saves
/// flow, which launches it headless.
fn locate_factorio_exe_interactive(settings: &mut Settings) -> io::Result<PathBuf> {
    step("Finding your Factorio install");

    if let Some(exe) = settings.valid_factorio_exe() {
        println!("remembered from last time\n  {}", exe.display());
        return Ok(exe.to_path_buf());
    }
    if let Some(exe) = locate_factorio() {
        println!("found at\n  {}", exe.display());
        settings.factorio_exe = Some(exe.clone());
        return Ok(exe);
    }
    println!("not found automatically.\n");
    loop {
        let input = prompt(
            "Please enter the full path to factorio.exe (usually inside a Steam \
             library, under Factorio\\bin\\x64\\factorio.exe):",
        )?;
        let path = PathBuf::from(&input);
        if path.is_file() {
            settings.factorio_exe = Some(path.clone());
            return Ok(path);
        }
        println!("That file doesn't exist: {}\n", path.display());
    }
}

/// Empty input is the caller's job to treat as "every surface"; this only
/// ever gets asked about a genuine name, matched case-insensitively so
/// "Nauvis" and "nauvis" aren't different answers.
fn find_surface<'a>(input: &str, surfaces: &'a [String]) -> Option<&'a str> {
    surfaces.iter().find(|s| s.eq_ignore_ascii_case(input)).map(String::as_str)
}

/// `None` for anything that isn't a recognized yes/no word, including empty
/// input: whether a blank answer means "yes" or "no" is the caller's
/// business (`ask_yes_no` treats it as "use the default"), not something
/// this pure parser should decide on its own.
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
            "How much game time should each frame represent? Fewer seconds means more \
             frames, spaced closer together: smoother scrubbing and playback, at the cost of \
             a larger export and slower load in the viewer. Enter seconds [default {default}]:"
        ))?;
        if input.trim().is_empty() {
            return Ok(default);
        }
        match parse_frame_seconds(&input) {
            Some(seconds) => return Ok(seconds),
            None => println!("Please enter a whole number of seconds greater than 0.\n"),
        }
    }
}

fn ask_surface_choice(surfaces: &[String]) -> io::Result<Option<String>> {
    loop {
        let input = prompt(&format!(
            "Render every surface (so tab in the viewer can switch between worlds), or just \
             one?\nSurfaces found: {}\nEnter a name, or press Enter for every surface:",
            surfaces.join(", ")
        ))?;
        if input.is_empty() {
            return Ok(None);
        }
        if let Some(found) = find_surface(&input, surfaces) {
            return Ok(Some(found.to_string()));
        }
        println!("\"{input}\" doesn't match any surface listed above. Try again.\n");
    }
}

/// A coarse "how long ago" label for a session's `last_modified`, good
/// enough to help someone recognise their own playthrough in a list without
/// a mod-side save name to show instead (Factorio gives mods no way to read
/// one).
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

/// Only reached when more than one playthrough has live capture data
/// waiting: combining them would jump between different bases in one
/// timelapse, exactly what tagging playthroughs by session is meant to
/// prevent, so this always picks exactly one rather than offering "all"
/// the way `ask_surface_choice` and the from-saves flow's selection do.
fn ask_session_choice(sessions: &[replay::Session]) -> io::Result<usize> {
    println!("\nFound {} captured playthrough(s):", sessions.len());
    let now = SystemTime::now();
    for (i, session) in sessions.iter().enumerate() {
        let age = now.duration_since(session.last_modified).unwrap_or_default();
        let _ = age;
        println!("  {}) {}", i + 1, describe_session(session, now));
    }
    loop {
        let input = prompt("\nWhich playthrough do you want to update the timelapse for? Enter a number:")?;
        if let Some(index) = parse_session_index(&input, sessions.len()) {
            return Ok(index);
        }
        println!("Please enter a number between 1 and {}.\n", sessions.len());
    }
}

/// Saves are usually numbered, so order by that number rather than
/// lexicographically, which would place "base10" before "base2".
fn ordering_key(path: &Path) -> (u64, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let digits: String = stem.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(0), stem.to_lowercase())
}

/// `all` (case-insensitive) selects everything. Otherwise, if every
/// comma-separated part parses as a number, those are 1-based indices into
/// `saves` (out-of-range ones are dropped rather than erroring, so a typo'd
/// extra index doesn't throw away the rest of a valid selection).
/// Otherwise, a case-insensitive substring filter against each save's
/// filename, the same matching `--match-name` used to do. Blank input
/// selects nothing rather than matching every filename via an empty
/// substring, since the whole point of asking is to make a blank answer
/// something the caller reprompts on, not a silent "combine everything".
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

/// The Lua mod's source, needed to stage a headless export. Tried next to
/// this program first (how it travels once distributed), then the project
/// root baked in at compile time (how it's found running the built exe
/// straight out of target/release, regardless of the working directory),
/// then the current folder as a last resort.
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

/// Where a mode writes its output: a folder beside the running exe, so the
/// result is easy to find regardless of where Factorio's own user data
/// happens to live. Falls back to the current directory only if the exe's
/// own path can't be determined, which realistically never happens.
fn output_dir_next_to_exe(name: &str) -> PathBuf {
    std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(".")).join(name)
}

/// Where built timelapses are kept, one subfolder each.
///
/// Built results used to go to a single fixed folder that every run deleted
/// and rebuilt, which meant reopening yesterday's timelapse was impossible:
/// the only way to look at one was to make it again. Keeping them apart by
/// name is what lets the tool open one instead of always building.
fn timelapses_root() -> PathBuf {
    output_dir_next_to_exe("timelapses")
}

/// Turns a playthrough name or save name into something safe to use as a
/// folder name, since both come from the user and can hold anything.
fn as_folder_name(raw: &str) -> String {
    // Dots are kept, since a name like "v1.2 run" is perfectly ordinary and
    // mangling it would be gratuitous. They are trimmed from the ends
    // afterwards, which is the case that actually matters: "." and ".." name
    // the current and parent directory, so a name of nothing but dots would
    // produce one of those rather than a folder of its own.
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

/// Every built timelapse, newest first.
///
/// Counts frames and weighs the folder rather than only listing names: which
/// of two timelapses somebody wants is usually answered by how big and how
/// recent it is, and neither is visible from a folder name.
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

fn ask_timelapse_choice(built: &[BuiltTimelapse]) -> io::Result<Option<usize>> {
    println!("\nTimelapses you have already built:");
    for (i, t) in built.iter().enumerate() {
        let age = t.modified.elapsed().map(describe_age).unwrap_or_else(|_| "unknown".to_string());
        println!("  {}) {} ({} frames, {}, built {age})", i + 1, t.name, t.frames, describe_size(t.bytes));
    }
    loop {
        let input = prompt("\nWhich would you like to open? Enter a number, or press Enter to go back:")?;
        if input.is_empty() {
            return Ok(None);
        }
        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= built.len() => return Ok(Some(n - 1)),
            _ => println!("Please enter a number between 1 and {}.", built.len()),
        }
    }
}

/// `viewer` is a sibling binary, not a library this crate can call into
/// directly (it depends on this one, not the other way around, and its
/// `main` is a macroquad event loop besides), so launching it means finding
/// the executable cargo already built next to this one.
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

fn run_live_capture(settings: &mut Settings) -> io::Result<PathBuf> {
    let user_dir = locate_factorio_user_dir_interactive(settings)?;

    let capture = user_dir.join("script-output").join("save-timelapse");
    // A missing capture folder (nothing has ever been captured) and one that
    // exists but names no finished baseline are the same "not started yet"
    // state from here, so a read_dir failure is folded into the empty case
    // rather than surfaced as a raw IO error.
    step("Looking for captured playthroughs");
    let sessions = replay::discover_sessions(&capture).unwrap_or_default();
    println!("{} found", sessions.len());
    if sessions.is_empty() {
        return Err(io::Error::other(format!(
            "No live capture found at {}.\n\n\
             In Factorio, turn on the \"save-timelapse-live-capture\" setting (Settings > Mod \
             Settings > Runtime > Save Timelapse), play for a bit, then run this again.",
            capture.display()
        )));
    }

    // Different playthroughs are tagged separately precisely so they never
    // get combined into one timelapse; only ask which one when there is
    // more than one to choose from, so the common single-playthrough case
    // stays exactly as simple as it always was.
    let chosen = if sessions.len() == 1 {
        sessions.into_iter().next().expect("checked non-empty above")
    } else {
        let index = ask_session_choice(&sessions)?;
        sessions.into_iter().nth(index).expect("ask_session_choice returned a valid index")
    };

    // The slowest silent step in the whole tool: a megabase baseline is tens
    // of megabytes and takes real time to read, with nothing on screen until
    // it finished.
    step("\nReading the baseline snapshot");
    let mut replay_state = replay::load_baseline(&chosen.baseline_path)?;
    println!(
        "tick {} ({} entities, {} tiles)",
        replay_state.baseline.tick,
        replay_state.world.entity_count(),
        replay_state.world.tile_count()
    );

    step("Finding surfaces");
    let surfaces = replay::discover_surfaces(&chosen.session_dir, &replay_state)?;
    println!("{}\n", surfaces.join(", "));

    let chosen_surface = ask_surface_choice(&surfaces)?;
    let frame_seconds = ask_frame_seconds(settings.frame_seconds.unwrap_or(DEFAULT_FRAME_SECONDS))?;

    // Saved here rather than after the export, so an answer survives even if
    // the build that follows fails or is interrupted. Retyping preferences
    // because something else went wrong is exactly the friction this exists
    // to remove.
    //
    // Surface choice is deliberately not remembered: which surfaces a capture
    // has changes as a playthrough reaches new planets, so a remembered one
    // would silently pick a stale answer, and picking the wrong world is far
    // more annoying than being asked.
    settings.frame_seconds = Some(frame_seconds);
    remember(settings);

    // Named after the playthrough, so rebuilding one leaves every other
    // timelapse alone and reopening it later is possible at all. Clearing
    // just this one before writing is still right: a shorter capture than
    // last time cannot leave stale, higher-numbered frames behind for the
    // viewer to mix in with current data.
    let name = chosen.label().unwrap_or_else(|| format!("playthrough-{:08x}", chosen.session_id));
    let out = timelapses_root().join(as_folder_name(&name));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    // Terrain is fixed the instant the baseline loads (see
    // `World::terrain_frame`), so it is written once here rather than once
    // per emitted frame like `replay::run`'s per-tick callback below does.
    // No-op per surface with terrain capture off.
    match &chosen_surface {
        None => replay::write_all_terrain(&replay_state.world, replay_state.baseline.tick, &out)?,
        Some(name) => replay::write_terrain(&replay_state.world, name, replay_state.baseline.tick, &out)?,
    }

    copy_session_sidecars(&chosen.session_dir, &out)?;

    let options = Options { interval: frame_seconds * TICKS_PER_SECOND, max_frames: MAX_FRAMES };
    let mut written = 0usize;
    let mut error: Option<io::Error> = None;
    // Last revision written per surface, carried across the whole run so
    // `write_all_surfaces` can skip a surface nothing has touched since. See
    // its doc comment: this is what turns "every surface, every frame" into
    // "every surface that changed".
    let mut surface_revisions: std::collections::HashMap<String, u64> = Default::default();

    let emitted = match &chosen_surface {
        None => {
            println!("rendering every surface, one frame per {frame_seconds}s of game time\n");
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
                    print!("\r{written} frames");
                    io::stdout().flush().ok();
                }
            })?
        }
        Some(name) => {
            println!("rendering surface {name}, one frame per {frame_seconds}s of game time\n");
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
                    print!("\r{written} frames");
                    io::stdout().flush().ok();
                }
            })?
        }
    };
    if let Some(e) = error {
        return Err(e);
    }
    println!("\r{emitted} frames written to {}\n", out.display());

    if replay_state.catch_ups_applied > 0 {
        println!(
            "{} catch-up baseline(s) applied for surface(s) added to tracking after capture started\n",
            replay_state.catch_ups_applied
        );
    }

    // Informational, deliberately not grouped with the warnings below: this
    // is what a playthrough that was reloaded from an earlier save looks like
    // when it replays correctly, so it reports rather than warns.
    if replay_state.superseded_events > 0 {
        println!(
            "{} event(s) skipped from timeline(s) you reloaded away from; the timelapse follows \
             what you actually played\n",
            replay_state.superseded_events
        );
    }

    // Its own line rather than folded into the message above, since this one
    // names a different thing: not events dropped, but files that turned out
    // to hold more than one attempt at the same stretch of play.
    if replay_state.restarted_segments > 0 {
        println!(
            "{} segment(s) held more than one attempt at the same stretch of play, which is what \
             reloading a save mid-capture looks like; only the last attempt at each was used\n",
            replay_state.restarted_segments
        );
    }

    // The one message here that names something the user can act on, and
    // deliberately not phrased as corruption: a skipped extension record only
    // means the mod that wrote this capture is newer than this build. The
    // replay is correct as far as it goes, it just does not show whatever the
    // newer records described, and updating the tool is what recovers them.
    if replay_state.unknown_extensions > 0 {
        println!(
            "{} record(s) in this capture come from a newer version of the mod than this tool \
             understands, and were skipped. The timelapse is complete apart from whatever they \
             described. Updating Save Timelapse to match the mod will pick them up.\n",
            replay_state.unknown_extensions
        );
    }

    // Both counters are already computed by `replay::run`; surfacing them
    // (and pausing so the message can't be missed the way a mid-run
    // eprintln! can, especially in a double-clicked .exe with no persistent
    // console) is the whole point, not the counting itself.
    let mut warned = false;
    if replay_state.skipped_segments > 0 || replay_state.out_of_order_batches > 0 {
        println!(
            "warning: {} segment(s) could not be read and {} batch(es) were out of tick order. \
             This capture may be missing history, see the warnings above, and run \
             /timelapse-reset-capture in-game before your next capture if this session's \
             files were ever deleted by hand.",
            replay_state.skipped_segments, replay_state.out_of_order_batches
        );
        warned = true;
    }
    let total = replay_state.applied_events + replay_state.no_op_events;
    if total >= 20 && replay_state.no_op_events * 2 > total {
        println!(
            "warning: {} of {total} events did nothing when replayed. This usually means \
             the event log doesn't match this session's baseline.",
            replay_state.no_op_events
        );
        warned = true;
    }
    if warned {
        wait_for_enter();
    }

    Ok(out)
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

    // Named after the first save chosen, which is the earliest one and so
    // usually names the playthrough rather than the moment. Asking for a name
    // would be one more question in a flow that already has several, and this
    // is renameable by renaming the folder.
    let name = chosen
        .first()
        .and_then(|p| p.file_stem())
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "from-saves".to_string());
    let out = timelapses_root().join(as_folder_name(&name));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    let workspace = std::env::temp_dir().join(format!("save-timelapse-{}", std::process::id()));
    let config =
        export::ExportConfig { factorio, user_mods, mod_source: mod_source_dir()?, include_resources: false, capture_terrain };

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
                // one-shot sample (its own real game.tick), so a multi-save
                // playthrough builds up one line per save in the shared
                // output file, the same as a live capture's many samples.
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

    // Milestones are the one output that cannot be derived from a single
    // save, so it is written here rather than per-export: each save reports
    // only totals, and when something first became true is recoverable only
    // by comparing consecutive ones.
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

/// Writes the settings, reporting a failure without treating it as one.
///
/// Not being able to remember an answer costs one prompt next time. Refusing
/// to build somebody's timelapse over it would be absurd, so this never
/// returns an error to a caller who would only have to ignore it.
fn remember(settings: &Settings) {
    if let Err(e) = settings.save() {
        eprintln!("warning: could not save your preferences ({e}); they will be asked again next time");
    }
}

/// `None` when there is nothing to open the viewer on, which is the case for
/// managing captures: it is housekeeping, not a build.
fn run() -> io::Result<Option<PathBuf>> {
    println!("save-timelapse\n");

    // Said once, on the run that has nothing saved yet, so the questions
    // below read as setup rather than as something that will happen forever.
    // Checked before loading, since loading is what would create the file.
    let first_run = Settings::is_first_run();
    let mut settings = Settings::load();
    if first_run {
        println!("First run: the answers below are remembered, so you will not be asked again.");
        if let Some(path) = settings::settings_path() {
            println!("They live in {}, and deleting that file resets them.\n", path.display());
        }
    }

    loop {
        let built = list_timelapses();
        match ask_mode(built.len())? {
            // Straight to the viewer, building nothing. The whole point: a
            // timelapse that already exists should not have to be made again
            // to be looked at.
            Mode::OpenExisting => {
                if built.is_empty() {
                    println!("\nNothing built yet. Use option 2 or 3 first.\n");
                    continue;
                }
                match ask_timelapse_choice(&built)? {
                    Some(index) => return Ok(Some(built[index].path.clone())),
                    // Back to the menu rather than out of the program, the
                    // same as leaving the management screen.
                    None => println!(),
                }
            }
            Mode::LiveCapture => return run_live_capture(&mut settings).map(Some),
            Mode::FromSaves => return run_from_saves(&mut settings).map(Some),
            // Loops back to the menu instead of returning, since managing
            // captures is housekeeping done *before* building something:
            // naming a playthrough and then being dropped out of the program
            // means starting it again to actually use the name.
            Mode::ManageCaptures => {
                let capture = locate_factorio_user_dir_interactive(&mut settings)?.join("script-output").join("save-timelapse");
                // Locating the folder may well have been the thing that
                // needed asking, and it is worth keeping even though nothing
                // was built.
                remember(&settings);
                manage_captures(&capture)?;
                println!();
            }
            Mode::Quit => return Ok(None),
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

/// One line describing a capture, for both the management screen and the
/// picker that runs before a live-capture rebuild.
///
/// Leads with the name when there is one. Without it a capture is identified
/// only by a hex session id and a relative age, which is no help at all in
/// telling two playthroughs apart, and naming them is most of the point of
/// the management screen.
fn describe_session(session: &replay::Session, now: SystemTime) -> String {
    let age = describe_age(now.duration_since(session.last_modified).unwrap_or_default());
    let name = session.label().unwrap_or_else(|| format!("unnamed ({:08x})", session.session_id));
    format!(
        "{name}  |  {}, baseline tick {} ({} entities, {} tiles)  |  surfaces: {}  |  {age}",
        describe_size(session.size_on_disk()),
        session.baseline.tick,
        session.baseline.entities,
        session.baseline.tiles,
        session.baseline.surfaces.join(", "),
    )
}

/// The capture management screen: name a playthrough so it can be told apart
/// from the others, see what each is costing on disk, and delete ones that
/// are finished with.
///
/// Deleting is offered here rather than only in game because the in-game
/// reset only ever removes the playthrough you currently have loaded. There
/// was no way at all to clear an old capture belonging to a save you no
/// longer play, short of finding the folder by hand.
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
        println!(
            "
{} capture(s), {} in total:
",
            sessions.len(),
            describe_size(total)
        );
        for (i, session) in sessions.iter().enumerate() {
            println!("  {}) {}", i + 1, describe_session(session, now));
        }

        let action = prompt(
            "
Enter a number to name that capture, \"d <number>\" to delete one, or press Enter to go back:",
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

            // Spelling out what is lost, not just which folder goes. "Delete
            // this capture" sounds like discarding a rendered video you could
            // rebuild; it is actually the only copy of that playthrough's
            // history, since a Factorio save stores no construction record to
            // reconstruct it from. The freeze is worth naming too: it is the
            // part people are surprised by when they start over.
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

/// Copies the plain-JSON sidecar logs a live capture writes alongside its
/// frames into the rendered output, so the viewer finds them next to the
/// frames it is pointed at.
///
/// A straight copy, not a re-parse: the mod's raw logs and what the viewer
/// reads are the exact same shape by design (see `player_log.rs` and
/// `milestone.rs`), so there is nothing to convert. Each one being absent is
/// normal rather than an error: nobody was connected during the capture, or
/// nothing milestone-worthy happened yet, or the capture predates the file
/// existing at all.
fn copy_session_sidecars(session_dir: &Path, out: &Path) -> io::Result<Vec<&'static str>> {
    let mut copied = Vec::new();
    for name in ["players.jsonl", "milestones.jsonl"] {
        let source = session_dir.join(name);
        if source.exists() {
            std::fs::copy(&source, out.join(name))?;
            copied.push(name);
        }
    }
    Ok(copied)
}

/// A double-clicked console window closes the instant the process exits,
/// which for an error message is worse than useless: the user sees a flash
/// and nothing else. Waiting for Enter is what actually gives a double-click
/// user, who never typed a command to begin with, a chance to read what went
/// wrong.
fn wait_for_enter() {
    print!("\nPress Enter to close this window...");
    io::stdout().flush().ok();
    let mut discard = String::new();
    io::stdin().read_line(&mut discard).ok();
}

fn main() {
    // A panic (an unexpected bug, not one of the friendly errors `run`
    // returns) would otherwise skip the pause below entirely: Rust prints
    // the panic message and unwinds straight past the `Err` handling
    // further down, and Windows closes the console the moment the process
    // exits either way. Installing a hook is what catches that case too, so
    // a bug here fails the same way a handled error does instead of just
    // flashing shut.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("\nsave-timelapse hit an unexpected error: {info}");
        wait_for_enter();
    }));

    let outcome = std::panic::catch_unwind(|| {
        run().and_then(|out| {
            let Some(out) = out else { return Ok(()) };
            let viewer = viewer_path()?;
            println!("opening the viewer...");
            Command::new(&viewer).arg(&out).spawn()?;
            Ok(())
        })
    });

    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            eprintln!("\n{e}");
            wait_for_enter();
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

    /// A capture with no name has to stay identifiable, or the management
    /// screen lists rows nobody can tell apart.
    #[test]
    fn an_unnamed_capture_is_described_by_its_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("0000002a");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("baseline.json"), r#"{"tick":100,"entities":7,"tiles":3,"surfaces":["nauvis"]}"#)
            .unwrap();

        let sessions = replay::discover_sessions(dir.path()).unwrap();
        let line = describe_session(&sessions[0], SystemTime::now());
        assert!(line.contains("unnamed (0000002a)"), "got: {line}");
        assert!(line.contains("nauvis") && line.contains("baseline tick 100"), "got: {line}");

        sessions[0].set_label("Vulcanus run").unwrap();
        let named = describe_session(&replay::discover_sessions(dir.path()).unwrap()[0], SystemTime::now());
        assert!(named.starts_with("Vulcanus run"), "a named capture leads with its name: {named}");
        assert!(!named.contains("unnamed"), "got: {named}");
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
