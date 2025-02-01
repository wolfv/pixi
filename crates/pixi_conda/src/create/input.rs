use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

use itertools::Itertools;
use miette::Diagnostic;
use rattler_conda_types::{
    EnvironmentYaml, ExplicitEnvironmentSpec, MatchSpec, ParseExplicitEnvironmentSpecError,
};
use thiserror::Error;

pub enum EnvironmentInput {
    /// The input of the environment is a set of match specs
    Specs(Vec<MatchSpec>),

    /// The input of the environment is an environment yaml.
    EnvironmentYaml(EnvironmentYaml, PathBuf),

    /// The input of the environment is a set of files.
    Files(Vec<ExplicitEnvironmentSpec>),
}

#[derive(Debug, Error, Diagnostic)]
pub enum InputError {
    #[error("could not determine the type of environment file for '{0}'")]
    InvalidInputFile(PathBuf),

    #[error("only a single environment yaml file can be provided")]
    MultipleEnvironmentYamlFiles,

    #[error("could not parse '{0}'")]
    ParseExplicitSpecError(PathBuf, #[source] ParseExplicitEnvironmentSpecError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl EnvironmentInput {
    pub fn from_files_or_specs(
        files: Vec<PathBuf>,
        specs: Vec<MatchSpec>,
    ) -> Result<Self, InputError> {
        if !specs.is_empty() {
            return Ok(EnvironmentInput::Specs(specs));
        }

        let first_file = files
            .first()
            .expect("either files are provided or match specs");
        let Some(first_file_kind) = InputFileKind::from_path(&first_file) else {
            return Err(InputError::InvalidInputFile(first_file.clone()));
        };

        match first_file_kind {
            InputFileKind::EnvironmentYaml => Self::from_environment_yaml(files),
            InputFileKind::ExplicitFile => Self::from_explicit_files(files),
        }
    }

    fn from_environment_yaml(files: Vec<PathBuf>) -> Result<Self, InputError> {
        let Ok(path) = files.into_iter().exactly_one() else {
            return Err(InputError::MultipleEnvironmentYamlFiles);
        };

        Ok(Self::EnvironmentYaml(
            EnvironmentYaml::from_path(&path)?,
            path,
        ))
    }

    fn from_explicit_files(files: Vec<PathBuf>) -> Result<Self, InputError> {
        let specs = files
            .into_iter()
            .map(|path| match ExplicitEnvironmentSpec::from_path(&path) {
                Ok(spec) => Ok(spec),
                Err(e) => Err(InputError::ParseExplicitSpecError(path, e)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::Files(specs))
    }
}

/// An enum representing the kind of input file.
enum InputFileKind {
    EnvironmentYaml,
    ExplicitFile,
}

impl InputFileKind {
    /// Guess the kind of input file from the file extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path
            .extension()
            .and_then(OsStr::to_str)
            .map(str::to_ascii_lowercase);
        match ext.as_deref() {
            Some("yaml") => Some(Self::EnvironmentYaml),
            Some("txt") => Some(Self::ExplicitFile),
            _ => None,
        }
    }
}
