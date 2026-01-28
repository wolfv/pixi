//! PKL manifest parsing implementation

use std::path::Path;

use rpkl_parser::parse_module;
use rpkl_runtime::Evaluator;
use rpkl_stdlib::stdlib_registry;
use toml_span::Deserialize;

use super::error::PklError;
use crate::toml::TomlManifest;

/// Parse a PKL manifest file and convert it to a WorkspaceManifest.
///
/// This function:
/// 1. Parses the PKL source into an AST
/// 2. Evaluates the AST to produce a VmValue
/// 3. Serializes the VmValue to JSON
/// 4. Converts JSON to TOML string
/// 5. Parses the TOML with toml_span into TomlManifest
/// 6. Converts TomlManifest to WorkspaceManifest
///
/// # Arguments
///
/// * `source` - The PKL source code to parse
/// * `manifest_path` - Optional path to the manifest file, used for resolving imports
///
/// # Returns
///
/// The parsed manifest as a `TomlManifest`, or an error if parsing/evaluation fails.
pub fn parse_pkl_manifest(
    source: &str,
    manifest_path: Option<&Path>,
) -> Result<TomlManifest, PklError> {
    // Step 1: Parse PKL source to AST
    let module = parse_module(source)?;

    // Step 2: Create evaluator with stdlib
    let evaluator = Evaluator::with_externals(stdlib_registry());

    // Set base path for imports if manifest path is provided
    if let Some(path) = manifest_path {
        if let Some(parent) = path.parent() {
            evaluator.set_base_path(parent);
        }
    }

    // Step 3: Evaluate the module
    let value = evaluator.eval_module(&module)?;

    // Step 4: Convert VmValue -> JSON -> TOML string
    let json_value: serde_json::Value = serde_json::to_value(&value)?;
    let toml_string = toml::to_string_pretty(&json_value).map_err(|e| {
        PklError::ConversionError(format!("Failed to convert to TOML: {}", e))
    })?;

    // Step 5: Parse TOML with toml_span
    let mut toml = toml_span::parse(&toml_string).map_err(|e| {
        PklError::ConversionError(format!("Failed to parse generated TOML: {}", e))
    })?;

    // Step 6: Deserialize into TomlManifest
    let manifest = TomlManifest::deserialize(&mut toml).map_err(|e| {
        PklError::ConversionError(format!("Failed to deserialize manifest: {}", e))
    })?;

    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_pkl_manifest() {
        let pkl_source = r#"
workspace {
    name = "test-project"
    channels = new Listing { "conda-forge" }
    platforms = new Listing { "linux-64"; "osx-64" }
}
"#;
        let result = parse_pkl_manifest(pkl_source, None);
        assert!(result.is_ok(), "Failed to parse: {:?}", result.err());
    }
}
