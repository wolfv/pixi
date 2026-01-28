//! PKL-specific error types

use miette::Diagnostic;
use thiserror::Error;

/// Errors that can occur when parsing or evaluating PKL manifests.
#[derive(Debug, Error, Diagnostic)]
pub enum PklError {
    /// Failed to parse PKL source code.
    #[error("Failed to parse PKL: {0}")]
    #[diagnostic(code(pixi::pkl::parse_error))]
    ParseError(String),

    /// Failed to evaluate PKL module.
    #[error("Failed to evaluate PKL: {0}")]
    #[diagnostic(code(pixi::pkl::eval_error))]
    EvalError(String),

    /// Failed to convert PKL value to manifest structure.
    #[error("Failed to convert PKL value to manifest: {0}")]
    #[diagnostic(code(pixi::pkl::conversion_error))]
    ConversionError(String),

    /// IO error occurred while reading the manifest.
    #[error("IO error: {0}")]
    #[diagnostic(code(pixi::pkl::io_error))]
    IoError(#[from] std::io::Error),
}

impl From<rpkl_parser::ParseError> for PklError {
    fn from(err: rpkl_parser::ParseError) -> Self {
        PklError::ParseError(err.to_string())
    }
}

impl From<rpkl_runtime::EvalError> for PklError {
    fn from(err: rpkl_runtime::EvalError) -> Self {
        PklError::EvalError(err.to_string())
    }
}

impl From<serde_json::Error> for PklError {
    fn from(err: serde_json::Error) -> Self {
        PklError::ConversionError(format!("JSON deserialization failed: {}", err))
    }
}
