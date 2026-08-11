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
pub mod wire;
pub mod world;
