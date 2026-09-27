use std::fmt::{Debug, Formatter};

use rattler_conda_types::VersionWithSource;

pub mod in_memory;

use in_memory::InMemoryBackend;
use pixi_build_types::{
    BackendCapabilities, PixiBuildApiVersion,
    procedures::{
        conda_build_v1::{CondaBuildV1Params, CondaBuildV1Result},
        conda_outputs::{CondaOutputsParams, CondaOutputsResult},
    },
};

mod stderr;

use crate::json_rpc::CommunicationError;

pub mod json_rpc;

#[derive(Debug)]
pub struct Backend {
    /// The backend that is used to communicate with the build server.
    inner: BackendImplementation,

    /// The API version that the backend supports.
    api_version: PixiBuildApiVersion,

    /// The backend capabilities that the backend support also taking into
    /// account the API version.
    capabilities: BackendCapabilities,
}

pub enum BackendImplementation {
    /// The backend is a JSON-RPC backend.
    JsonRpc(Box<json_rpc::JsonRpcBackend>),

    /// An in memory backend.
    InMemory(Box<dyn InMemoryBackend>),
}

impl Debug for BackendImplementation {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendImplementation::JsonRpc(json_rpc) => json_rpc.fmt(f),
            BackendImplementation::InMemory(backend) => f
                .debug_struct("InMemoryBackend")
                .field("identifier", &backend.identifier())
                .finish(),
        }
    }
}

impl BackendImplementation {
    pub fn capabilities(&self) -> BackendCapabilities {
        match self {
            BackendImplementation::JsonRpc(json_rpc) => json_rpc.capabilities().clone(),
            BackendImplementation::InMemory(in_memory) => in_memory.capabilities(),
        }
    }

    pub fn identifier(&self) -> &str {
        match self {
            BackendImplementation::JsonRpc(json_rpc) => json_rpc.identifier(),
            BackendImplementation::InMemory(in_memory) => in_memory.identifier(),
        }
    }

    pub fn version(&self) -> Option<&VersionWithSource> {
        match self {
            BackendImplementation::JsonRpc(json_rpc) => json_rpc.version(),
            BackendImplementation::InMemory(_) => None,
        }
    }
}

impl From<json_rpc::JsonRpcBackend> for BackendImplementation {
    fn from(json_rpc: json_rpc::JsonRpcBackend) -> Self {
        BackendImplementation::JsonRpc(Box::new(json_rpc))
    }
}

impl From<Box<dyn in_memory::InMemoryBackend>> for BackendImplementation {
    fn from(in_memory: Box<dyn in_memory::InMemoryBackend>) -> Self {
        BackendImplementation::InMemory(in_memory)
    }
}

impl Backend {
    pub fn new(inner: BackendImplementation, api_version: PixiBuildApiVersion) -> Self {
        let mut capabilities = inner.capabilities().mask_with_api_version(&api_version);
        if capabilities.provides_conda_recipe() {
            // Recipe backends only describe the package; pixi derives the
            // outputs and builds them itself (see `pixi_blaze`).
            capabilities.provides_conda_outputs = Some(true);
            capabilities.provides_conda_build_v1 = Some(true);
        }
        Self {
            inner,
            api_version,
            capabilities,
        }
    }

    /// Returns an identifier for the backend. This is useful for debugging
    /// purposes mostly.
    pub fn identifier(&self) -> &str {
        self.inner.identifier()
    }

    /// Returns the version of the backend, if available. This is useful for
    /// debugging purposes mostly.
    pub fn version(&self) -> Option<&VersionWithSource> {
        self.inner.version()
    }

    /// Returns the capabilities of the backend. This takes into account both
    /// the actual capabilities of the backend and the API version that is in
    /// use.
    ///
    /// Sometimes backends provide more capabilities that the API version that
    /// we established. This can happen when the backend already implemented
    /// some capabilities both not all for a particular API version.
    pub fn capabilities(&self) -> &BackendCapabilities {
        &self.capabilities
    }

    /// Returns the capabilities of the backend, without taking into account the
    /// API version. This is only useful for debugging purposes. In most cases
    /// [`Self::capabilities`] should be used instead.
    pub fn backend_capabilities(&self) -> BackendCapabilities {
        self.inner.capabilities()
    }

    /// Returns the API version that was used to establish the backend.
    pub fn api_version(&self) -> PixiBuildApiVersion {
        self.api_version
    }

    /// The recipe of a `conda/recipe` backend, if this is one.
    pub async fn conda_recipe(
        &self,
        params: pixi_build_types::procedures::conda_recipe::CondaRecipeParams,
    ) -> Option<
        Result<pixi_build_types::procedures::conda_recipe::CondaRecipeResult, CommunicationError>,
    > {
        if !self.capabilities.provides_conda_recipe() {
            return None;
        }
        match &self.inner {
            BackendImplementation::JsonRpc(json_rpc) => Some(json_rpc.conda_recipe(params).await),
            BackendImplementation::InMemory(_) => None,
        }
    }

    fn project_model(&self) -> Option<&pixi_build_types::ProjectModel> {
        match &self.inner {
            BackendImplementation::JsonRpc(json_rpc) => json_rpc.project_model(),
            BackendImplementation::InMemory(_) => None,
        }
    }

    pub async fn conda_build_v1<W: BackendOutputStream + Send + 'static>(
        &self,
        params: CondaBuildV1Params,
        output_stream: W,
    ) -> Result<CondaBuildV1Result, CommunicationError> {
        if let Some(recipe) = self
            .conda_recipe(pixi_blaze::recipe_params_from_build(&params))
            .await
        {
            let recipe = recipe?;
            return pixi_blaze::build(&recipe, &params)
                .await
                .map_err(|e| CommunicationError::Blaze(e.into()));
        }
        assert!(
            self.inner.capabilities().provides_conda_build_v1(),
            "This backend does not support the conda build v1 procedure"
        );
        match &self.inner {
            BackendImplementation::JsonRpc(json_rpc) => {
                json_rpc.conda_build_v1(params, output_stream).await
            }
            BackendImplementation::InMemory(in_memory) => in_memory
                .conda_build_v1(params, &output_stream)
                .map_err(|e| *e),
        }
    }

    /// Returns the outputs that this backend can produce.
    pub async fn conda_outputs<W: BackendOutputStream + Send + 'static>(
        &self,
        params: CondaOutputsParams,
        output_stream: W,
    ) -> Result<CondaOutputsResult, CommunicationError> {
        if let Some(recipe) = self
            .conda_recipe(pixi_blaze::recipe_params_from_outputs(&params))
            .await
        {
            let recipe = recipe?;
            return pixi_blaze::outputs(&recipe, &params, self.project_model())
                .map_err(|e| CommunicationError::Blaze(e.into()));
        }
        assert!(
            self.inner.capabilities().provides_conda_outputs(),
            "This backend does not support the conda outputs procedure"
        );
        match &self.inner {
            BackendImplementation::JsonRpc(json_rpc) => {
                json_rpc.conda_outputs(params, output_stream).await
            }
            BackendImplementation::InMemory(in_memory) => in_memory
                .conda_outputs(params, &output_stream)
                .map_err(|e| *e),
        }
    }
}

pub trait BackendOutputStream {
    fn on_line(&mut self, line: String);
}

impl BackendOutputStream for () {
    fn on_line(&mut self, _line: String) {
        // No-op implementation
    }
}

impl<F: FnMut(String)> BackendOutputStream for F {
    fn on_line(&mut self, line: String) {
        self(line);
    }
}
