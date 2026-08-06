//! save-timelapse: build a timelapse from Factorio saves you already have.

use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Parser;
use save_timelapse::export;
use save_timelapse::locate::{factorio_user_dir, locate_factorio};

#[derive(Parser)]
#[command(name = "save-timelapse", version, about)]
struct Args {
    /// Folder of .zip saves to export. Defaults to Factorio's saves folder.
    #[arg(long)]
    saves: Option<PathBuf>,

    /// Where to collect exported frames.
    #[arg(long, default_value = "frames")]
    out: PathBuf,

    /// Path to factorio.exe. Auto-detected when omitted.
    #[arg(long)]
    factorio: Option<PathBuf>,

    /// Factorio mods folder. Auto-detected when omitted.
    #[arg(long)]
    mods: Option<PathBuf>,

    /// Directory holding this mod's Lua.
    #[arg(long, default_value = "mod")]
    mod_source: PathBuf,

    /// Include ore tiles. Every tile is a separate entity, so this typically
    /// multiplies frame size without showing factory growth.
    #[arg(long)]
    include_resources: bool,

    /// Export only the first N saves.
    #[arg(long)]
    limit: Option<usize>,
}

/// Saves are usually numbered, so order by that number rather than
/// lexicographically, which would place "base10" before "base2".
fn ordering_key(path: &Path) -> (u64, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    let digits: String = stem.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(0), stem.to_lowercase())
}

fn run(args: Args) -> std::io::Result<()> {
    let factorio = args
        .factorio
        .or_else(locate_factorio)
        .ok_or_else(|| std::io::Error::other("cannot find factorio.exe, pass --factorio"))?;

    let user_mods = args
        .mods
        .or_else(|| factorio_user_dir().map(|d| d.join("mods")))
        .ok_or_else(|| std::io::Error::other("cannot find the mods folder, pass --mods"))?;

    let saves_dir = args
        .saves
        .or_else(|| factorio_user_dir().map(|d| d.join("saves")))
        .ok_or_else(|| std::io::Error::other("cannot find the saves folder, pass --saves"))?;

    let version = export::factorio_version(&factorio)
        .map(|v| format!("{}.{}.{}", v[0], v[1], v[2]))
        .unwrap_or_else(|| "unknown".to_string());

    println!("factorio {} ({})", version, factorio.display());
    println!("mods     {}", user_mods.display());
    println!("saves    {}", saves_dir.display());

    let mut saves: Vec<PathBuf> = std::fs::read_dir(&saves_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("zip"))
        .collect();

    saves.sort_by_key(|path| ordering_key(path));
    if let Some(n) = args.limit {
        saves.truncate(n);
    }

    if saves.is_empty() {
        return Err(std::io::Error::other(format!(
            "no .zip saves in {}",
            saves_dir.display()
        )));
    }
    println!("{} save(s) to export\n", saves.len());

    std::fs::create_dir_all(&args.out)?;
    let workspace = std::env::temp_dir().join(format!("save-timelapse-{}", std::process::id()));

    let config = export::ExportConfig {
        factorio,
        user_mods,
        mod_source: args.mod_source,
        include_resources: args.include_resources,
    };

    let mut done = 0usize;
    for (index, save) in saves.iter().enumerate() {
        let label = save.file_name().unwrap_or_default().to_string_lossy().into_owned();
        print!("[{:>3}/{}] {label} ... ", index + 1, saves.len());
        std::io::stdout().flush().ok();

        let staged = workspace.join(format!("stage_{index}"));
        match export::export_save(save, &staged, &config) {
            Ok(outcome) => {
                let target = args.out.join(format!("frame_{index:04}.json"));
                let primary = &outcome.frames[0];
                std::fs::rename(primary, &target)
                    .or_else(|_| std::fs::copy(primary, &target).map(drop))?;

                let kib = target.metadata().map(|m| m.len()).unwrap_or(0) / 1024;
                println!("ok, {kib} KiB in {:.1}s", outcome.seconds);
                done += 1;
            }
            Err(err) => println!("failed: {err}"),
        }
        let _ = std::fs::remove_dir_all(&staged);
    }

    let _ = std::fs::remove_dir_all(&workspace);
    println!("\n{done} of {} exported to {}", saves.len(), args.out.display());
    Ok(())
}

fn main() {
    if let Err(err) = run(Args::parse()) {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_key_sorts_numerically_not_lexicographically() {
        let mut saves = vec![
            PathBuf::from("base2.zip"),
            PathBuf::from("base10.zip"),
            PathBuf::from("base1.zip"),
        ];
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
}
