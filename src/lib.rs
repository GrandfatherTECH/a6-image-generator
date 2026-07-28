//! Reusable core for the A6 Image Studio desktop and command-line clients.

pub mod api;
pub mod app;
pub mod config;
pub mod domain;
pub mod generation;
pub mod storage;
pub mod xdg;

slint::include_modules!();
