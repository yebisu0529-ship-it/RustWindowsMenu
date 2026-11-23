pub mod classic;
pub mod com_shell;
pub mod config;
pub mod engine;
pub mod icons;
pub mod model;
pub mod registry;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
