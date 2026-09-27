//! Describes the `conda/recipe` request (experimental).
//!
//! Instead of building packages itself (`conda/outputs` + `conda/build_v1`), a
//! backend that implements this procedure only *describes* the build: it
//! returns a recipe in the rattler-blaze format (`build.generator: cmake`,
//! requirements, tests, ...). Pixi expands the recipe into variants, derives
//! the output metadata from it, and builds it with its embedded fine-grained
//! build engine. The recipe is the unit of work.

use std::{collections::BTreeMap, path::PathBuf};

use rattler_conda_types::{ChannelUrl, Platform};
use serde::{Deserialize, Serialize};

use crate::VariantValue;

pub const METHOD_NAME: &str = "conda/recipe";

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CondaRecipeParams {
    #[serde(default)]
    pub channels: Vec<ChannelUrl>,

    /// The platform the package is built for.
    pub host_platform: Platform,

    /// The platform build tools run on.
    pub build_platform: Platform,

    /// The variant configuration of the workspace.
    pub variant_configuration: Option<BTreeMap<String, Vec<VariantValue>>>,

    /// Variant files of the workspace.
    pub variant_files: Option<Vec<PathBuf>>,

    /// A scratch directory unique to this source package.
    pub work_directory: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CondaRecipeResult {
    /// The recipe as YAML (rattler-blaze format, `${{ jinja }}` allowed).
    pub recipe: String,

    /// Directory that relative paths in the recipe (`source.path`,
    /// `variants.yaml`) are relative to.
    pub recipe_directory: PathBuf,

    /// Extra variant configuration the backend contributes (e.g. defaults for
    /// `c_compiler`); the workspace configuration takes precedence.
    #[serde(default)]
    pub variant_configuration: BTreeMap<String, Vec<String>>,

    /// Files that determine the recipe (hashed into the lock file, like
    /// `conda/outputs`' input globs).
    #[serde(default)]
    pub input_globs: Vec<String>,
}
