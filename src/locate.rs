//! Auto-detecting a local Factorio install. Shared by the CLI (to find what
//! to export from) and the viewer (to find sprite icons to render).

use std::path::PathBuf;

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Where Factorio keeps saves, mods and config for the current user.
pub fn factorio_user_dir() -> Option<PathBuf> {
    if cfg!(windows) {
        return std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Factorio"));
    }
    let home = home_dir()?;
    if cfg!(target_os = "macos") {
        return Some(home.join("Library/Application Support/factorio"));
    }
    Some(home.join(".factorio"))
}

pub fn locate_factorio() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if cfg!(windows) {
        candidates.extend(
            [
                r"C:\Program Files (x86)\Steam\steamapps\common\Factorio\bin\x64\factorio.exe",
                r"C:\Program Files\Factorio\bin\x64\factorio.exe",
                r"D:\SteamLibrary\steamapps\common\Factorio\bin\x64\factorio.exe",
                r"E:\SteamLibrary\steamapps\common\Factorio\bin\x64\factorio.exe",
            ]
            .iter()
            .map(PathBuf::from),
        );
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from("/Applications/factorio.app/Contents/MacOS/factorio"));
    } else {
        candidates.push(PathBuf::from("/usr/share/factorio/bin/x64/factorio"));
        candidates.push(PathBuf::from("/opt/factorio/bin/x64/factorio"));
    }

    if let Some(home) = home_dir() {
        for prefix in [
            ".steam/steam/steamapps/common/Factorio",
            ".local/share/Steam/steamapps/common/Factorio",
            "Library/Application Support/Steam/steamapps/common/factorio.app/Contents",
            "factorio",
        ] {
            candidates.push(home.join(prefix).join("bin/x64/factorio"));
        }
    }

    candidates.into_iter().find(|path| path.exists())
}
