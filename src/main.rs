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
use save_timelapse::replay::{self, Options};

/// Default game time per frame during live-capture replay, asked about
/// interactively so a longer playthrough can trade a larger export for
/// smoother, more finely-spaced playback.
const DEFAULT_FRAME_SECONDS: u64 = 60;

/// Factorio's normal game speed: one real second is 60 ticks.
const TICKS_PER_SECOND: u64 = 60;

const MAX_FRAMES: usize = 100_000;

enum Mode {
    LiveCapture,
    FromSaves,
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

fn ask_mode() -> io::Result<Mode> {
    loop {
        let input = prompt(
            "What would you like to do?\n\
             \x20 1) Update my timelapse from live capture (recommended if capture is on)\n\
             \x20 2) Build a timelapse from existing save files\n\
             Enter 1 or 2:",
        )?;
        match input.as_str() {
            "1" => return Ok(Mode::LiveCapture),
            "2" => return Ok(Mode::FromSaves),
            _ => println!("Please enter 1 or 2.\n"),
        }
    }
}

/// Auto-detects Factorio's data folder (saves/mods/script-output), falling
/// back to asking for it. Validated by checking for a `mods` subfolder
/// rather than just accepting any path, since a wrong answer here would
/// otherwise only surface as a confusing failure several steps later.
fn locate_factorio_user_dir_interactive() -> io::Result<PathBuf> {
    if let Some(dir) = factorio_user_dir() {
        if dir.join("mods").is_dir() {
            return Ok(dir);
        }
    }
    loop {
        let input = prompt(
            "Could not find your Factorio folder automatically.\n\
             Please enter the path to it (the folder containing \"mods\" and \"saves\", \
             usually %APPDATA%\\Factorio on Windows):",
        )?;
        let path = PathBuf::from(&input);
        if path.join("mods").is_dir() {
            return Ok(path);
        }
        println!("That doesn't look right: no \"mods\" folder inside {}.\n", path.display());
    }
}

/// Same idea for the actual game executable, only needed by the from-saves
/// flow, which launches it headless.
fn locate_factorio_exe_interactive() -> io::Result<PathBuf> {
    if let Some(exe) = locate_factorio() {
        return Ok(exe);
    }
    loop {
        let input = prompt(
            "Could not find your Factorio install automatically.\n\
             Please enter the full path to factorio.exe (usually inside a Steam \
             library, under Factorio\\bin\\x64\\factorio.exe):",
        )?;
        let path = PathBuf::from(&input);
        if path.is_file() {
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
        println!(
            "  {}) baseline tick {} ({} entities, {} tiles), surfaces: {} ({})",
            i + 1,
            session.baseline.tick,
            session.baseline.entities,
            session.baseline.tiles,
            session.baseline.surfaces.join(", "),
            describe_age(age)
        );
    }
    loop {
        let input =
            prompt("\nWhich playthrough do you want to update the timelapse for? Enter a number:")?;
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
        .filter(|path| {
            path.file_name().and_then(|n| n.to_str()).is_some_and(|name| name.to_lowercase().contains(&needle))
        })
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
    std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
        .join(name)
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

fn run_live_capture() -> io::Result<PathBuf> {
    let user_dir = locate_factorio_user_dir_interactive()?;

    let capture = user_dir.join("script-output").join("save-timelapse");
    // A missing capture folder (nothing has ever been captured) and one that
    // exists but names no finished baseline are the same "not started yet"
    // state from here, so a read_dir failure is folded into the empty case
    // rather than surfaced as a raw IO error.
    let sessions = replay::discover_sessions(&capture).unwrap_or_default();
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

    let mut replay_state = replay::load_baseline(&chosen.baseline_path)?;
    println!(
        "\nbaseline tick {} ({} entities, {} tiles)",
        replay_state.baseline.tick,
        replay_state.world.entity_count(),
        replay_state.world.tile_count()
    );
    let surfaces = replay::discover_surfaces(&chosen.session_dir, &replay_state)?;
    println!("surfaces: {}\n", surfaces.join(", "));

    let chosen_surface = ask_surface_choice(&surfaces)?;
    let frame_seconds = ask_frame_seconds(DEFAULT_FRAME_SECONDS)?;

    // Fixed and owned entirely by this tool, so it's safe to clear before
    // every run: a shorter capture than last time can't leave stale,
    // higher-numbered frames behind for the viewer to mix in with current
    // data.
    let out = output_dir_next_to_exe("save-timelapse-frames");
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

    // A straight copy, not a re-parse: the mod's raw log and what the
    // viewer reads are the exact same shape by design (see
    // src/player_log.rs), so there's nothing to convert. Absent entirely
    // is normal, not an error, e.g. nobody was connected during capture.
    let players_log = chosen.session_dir.join("players.jsonl");
    if players_log.exists() {
        std::fs::copy(&players_log, out.join("players.jsonl"))?;
    }

    let options = Options { interval: frame_seconds * TICKS_PER_SECOND, max_frames: MAX_FRAMES };
    let mut written = 0usize;
    let mut error: Option<io::Error> = None;

    let emitted = match &chosen_surface {
        None => {
            println!("rendering every surface, one frame per {frame_seconds}s of game time\n");
            replay::run(&mut replay_state, &chosen.session_dir, &options, |world, tick| {
                if error.is_some() {
                    return;
                }
                if let Err(e) = replay::write_all_surfaces(world, tick, &out, written) {
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

    // Its own line rather than folded into the message above, since unlike a
    // plain reload this one is worth naming: it only happens in captures
    // recorded before the mod rolled over on a same-save reload, and it is
    // also the shape a hand-deleted script-output would leave behind.
    if replay_state.restarted_segments > 0 {
        println!(
            "{} segment(s) contained more than one attempt at the same stretch of play, from \
             reloading the same save more than once before this version; only the last attempt \
             at each was used\n",
            replay_state.restarted_segments
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

fn run_from_saves() -> io::Result<PathBuf> {
    let factorio = locate_factorio_exe_interactive()?;
    let user_dir = locate_factorio_user_dir_interactive()?;
    let user_mods = user_dir.join("mods");
    let saves_dir = user_dir.join("saves");

    let mut saves: Vec<PathBuf> = std::fs::read_dir(&saves_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();
    saves.sort_by_key(|p| ordering_key(p));

    if saves.is_empty() {
        return Err(io::Error::other(format!("No .zip saves found in {}.", saves_dir.display())));
    }

    println!("\nFound {} save(s) in {}:", saves.len(), saves_dir.display());
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
        chosen
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let capture_terrain = ask_yes_no(
        "Include natural terrain (grass, water, trees, cliffs) around the base? This can \
         significantly increase export size and time (roughly 5x in testing)",
        false,
    )?;

    let out = output_dir_next_to_exe("frames");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;

    let workspace = std::env::temp_dir().join(format!("save-timelapse-{}", std::process::id()));
    let config = export::ExportConfig {
        factorio,
        user_mods,
        mod_source: mod_source_dir()?,
        include_resources: false,
        capture_terrain,
    };

    let mut done = 0usize;
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
                    let mut combined =
                        std::fs::OpenOptions::new().create(true).append(true).open(out.join("players.jsonl"))?;
                    combined.write_all(&std::fs::read(log)?)?;
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

    Ok(out)
}

fn run() -> io::Result<PathBuf> {
    println!("save-timelapse\n");
    match ask_mode()? {
        Mode::LiveCapture => run_live_capture(),
        Mode::FromSaves => run_from_saves(),
    }
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
        let mut saves =
            vec![PathBuf::from("base2.zip"), PathBuf::from("base10.zip"), PathBuf::from("base1.zip")];
        saves.sort_by_key(|p| ordering_key(p));
        assert_eq!(
            saves,
            vec![PathBuf::from("base1.zip"), PathBuf::from("base2.zip"), PathBuf::from("base10.zip")]
        );
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
