//! Slint desktop application wiring.

mod actions;
mod controller;
mod state;

pub use controller::{GuiError, run};
