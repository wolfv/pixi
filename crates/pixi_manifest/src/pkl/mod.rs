//! PKL manifest parsing support
//!
//! This module provides support for parsing `pixi.pkl` manifest files as an
//! alternative to `pixi.toml`.

mod error;
mod parser;

pub use error::PklError;
pub use parser::parse_pkl_manifest;
