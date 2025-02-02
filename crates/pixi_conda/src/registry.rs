use std::path::PathBuf;

use pixi_config::pixi_home;

pub struct Registry {
    root: PathBuf,
}

impl Registry {
    /// Constructs a new instance rooted at the given path.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Constructs a new registry by reading the environment.
    ///
    /// By default, this will look for the `PIXI_HOME` environment variable.
    pub fn from_env() -> Self {
        let env_dir = pixi_home()
            .map(|pixi_home| pixi_home.join("envs"))
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("envs"));
        Self::new(env_dir)
    }

    /// Returns the root directory of the registry.
    pub fn root(&self) -> &PathBuf {
        &self.root
    }
}
