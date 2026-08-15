//! Library half of save-timelapse, so the CLI, the test double and the
//! integration tests can all share the same code.

pub mod build;
/// The window and its screens, which is what replaces the console menu.
pub mod gui;

pub mod describe;
pub mod event;
pub mod export;
pub mod frame;
pub mod icons;
pub mod locate;
pub mod milestone;
pub mod names;
pub mod player_log;
pub mod prototypes;
pub mod replay;
/// What the tool remembers between runs. Distinct from [`settings_dat`],
/// which is Factorio's own `mod-settings.dat` format and nothing to do with
/// this tool's preferences.
pub mod settings;
pub mod settings_dat;
#[cfg(test)]
mod test_support;
/// The interactive viewer and everything it draws with. Was its own crate
/// until the two binaries merged.
pub mod viewer;
pub mod wire;
pub mod world;

/// Whether FFmpeg can be run. Lives here because both binaries need the same
/// answer: the CLI to decide whether offering MP4 is honest, the viewer to say
/// something useful when asked for one. Asked fresh each time rather than
/// cached, so installing it does not require restarting the tool.
///
/// FFmpeg is never required. The built-in AVI writer is what keeps the tool
/// dependency free; MP4 is a bonus for people who happen to have it.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// A count with thousands separators, for anything a person reads. Lives here
/// rather than in either binary because both display counts and the rule is the
/// same one.
pub fn with_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
