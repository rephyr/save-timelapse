//! Library half of save-timelapse, so the CLI, the test double and the
//! integration tests can all share the same code.

pub mod event;
pub mod export;
pub mod frame;
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
pub mod wire;
pub mod world;

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
