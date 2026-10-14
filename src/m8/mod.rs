//! An M8-style tracker: plays Dirtywave M8 song files, with CLAP plugins
//! standing in for the M8's built-in instruments.

pub mod app;
pub mod format;
pub mod input;
pub mod instruments;
pub mod rack;
pub mod recorder;
pub mod sampler;
pub mod screen;
pub mod sequencer;
pub mod ui;
#[cfg(feature = "window")]
pub mod window;
