//! Capabilities that the frontend and backend provide.

use crate::PixiBuildApiVersion;
use serde::{Deserialize, Serialize};

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Capabilities that the backend provides.
pub struct BackendCapabilities {
    /// Whether the backend provides the `conda/outputs` API.
    pub provides_conda_outputs: Option<bool>,

    /// Whether the backend provides the `conda/build_v1` API.
    pub provides_conda_build_v1: Option<bool>,

    /// Whether the backend provides the `conda/recipe` API: it only describes
    /// *what* to build (a blaze recipe) and pixi does all the building.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provides_conda_recipe: Option<bool>,
}

impl BackendCapabilities {
    /// Mask the capabilities with the expected capabilities of a specific API version.
    pub fn mask_with_api_version(&self, version: &PixiBuildApiVersion) -> Self {
        let expected = version.expected_backend_capabilities();
        Self {
            provides_conda_outputs: Some(
                self.provides_conda_outputs() && expected.provides_conda_outputs(),
            ),
            provides_conda_build_v1: Some(
                self.provides_conda_build_v1() && expected.provides_conda_build_v1(),
            ),
            // Experimental: not tied to an API version yet.
            provides_conda_recipe: self.provides_conda_recipe,
        }
    }

    /// Whether the backend provides the `conda/outputs` API.
    pub fn provides_conda_outputs(&self) -> bool {
        self.provides_conda_outputs.unwrap_or(false)
    }

    /// Whether the backend provides the `conda/build_v1` API.
    pub fn provides_conda_build_v1(&self) -> bool {
        self.provides_conda_build_v1.unwrap_or(false)
    }

    /// Whether the backend provides the `conda/recipe` API.
    pub fn provides_conda_recipe(&self) -> bool {
        self.provides_conda_recipe.unwrap_or(false)
    }
}

#[derive(Debug, Serialize, Deserialize)]
/// Capabilities that the frontend provides.
pub struct FrontendCapabilities {}
