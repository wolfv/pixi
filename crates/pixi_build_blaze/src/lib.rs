//! A tiny framework for *recipe-only* pixi build backends (experimental).
//!
//! A backend here does one thing: turn the package's project model (from
//! `pixi.toml`) plus its backend configuration into a rattler-blaze recipe
//! (`build.generator: cmake`, requirements, tests). It implements a single
//! procedure, `conda/recipe`. Pixi expands the recipe into variants, derives
//! the output metadata, solves and installs the environments, and builds it
//! with its embedded engine, one cached action per compile/link/test.
//!
//! No rattler-build, no build scripts, no solver: a generator is ~50 lines.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

pub use blaze_recipe as recipe;
use blaze_recipe::{About, Build, Generator, Package, Recipe, Requirements, Source};
use jsonrpc_core::{Error, IoHandler, Params, to_value};
use pixi_build_types::{
    BackendCapabilities, PackageSpec, ProjectModel, SourcePackageName, Target,
    procedures::{
        self,
        conda_recipe::{CondaRecipeParams, CondaRecipeResult},
        initialize::{InitializeParams, InitializeResult},
        negotiate_capabilities::{NegotiateCapabilitiesParams, NegotiateCapabilitiesResult},
    },
};
use serde::de::DeserializeOwned;
use tokio::sync::RwLock;

/// Everything a generator gets to look at.
pub struct RecipeContext<'a> {
    pub model: &'a ProjectModel,
    pub manifest_path: &'a Path,
    /// The package's source directory (where `pixi.toml` lives, or the
    /// configured `build.source`).
    pub source_dir: &'a Path,
    pub params: &'a CondaRecipeParams,
}

/// A typed recipe, or recipe YAML as written by hand (may use `context:`).
pub enum RecipeSource {
    Typed(Box<Recipe>),
    Yaml(String),
}

pub struct Generated {
    pub recipe: RecipeSource,
    /// Default variant values contributed by the backend.
    pub variants: BTreeMap<String, Vec<String>>,
    /// Extra files that determine the recipe (besides the manifest).
    pub input_globs: Vec<String>,
}

impl From<Recipe> for Generated {
    fn from(recipe: Recipe) -> Self {
        Generated {
            recipe: RecipeSource::Typed(Box::new(recipe)),
            variants: BTreeMap::new(),
            input_globs: Vec::new(),
        }
    }
}

pub trait RecipeGenerator: Send + Sync + 'static {
    /// The `[package.build.config]` table.
    type Config: DeserializeOwned + Default + Send + Sync;

    fn generate(&self, cx: &RecipeContext<'_>, config: &Self::Config) -> miette::Result<Generated>;
}

/// Serve `generator` over stdin/stdout. Call this from `main`.
pub fn main<G: RecipeGenerator>(generator: G) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async move {
        let io = setup_io(Arc::new(generator));
        jsonrpc_stdio_server::ServerBuilder::new(io).build().await;
    });
}

struct State {
    init: Option<InitializeParams>,
}

fn rpc_error(e: impl std::fmt::Display) -> Error {
    let mut err = Error::internal_error();
    err.message = e.to_string();
    err
}

fn setup_io<G: RecipeGenerator>(generator: Arc<G>) -> IoHandler {
    let mut io = IoHandler::new();
    io.add_method(
        procedures::negotiate_capabilities::METHOD_NAME,
        |params: Params| async move {
            let _: NegotiateCapabilitiesParams = params.parse()?;
            Ok(to_value(NegotiateCapabilitiesResult {
                capabilities: BackendCapabilities {
                    provides_conda_outputs: Some(false),
                    provides_conda_build_v1: Some(false),
                    provides_conda_recipe: Some(true),
                },
            })
            .unwrap())
        },
    );
    let state = Arc::new(RwLock::new(State { init: None }));
    let s = state.clone();
    io.add_method(
        procedures::initialize::METHOD_NAME,
        move |params: Params| {
            let s = s.clone();
            async move {
                let params: InitializeParams = params.parse()?;
                s.write().await.init = Some(params);
                Ok(to_value(InitializeResult {}).unwrap())
            }
        },
    );
    let s = state.clone();
    io.add_method(
        procedures::conda_recipe::METHOD_NAME,
        move |params: Params| {
            let (s, generator) = (s.clone(), generator.clone());
            async move {
                let params: CondaRecipeParams = params.parse()?;
                let state = s.read().await;
                let init = state.init.as_ref().ok_or_else(Error::invalid_request)?;
                let result = conda_recipe(generator.as_ref(), init, &params).map_err(rpc_error)?;
                Ok(to_value(result).unwrap())
            }
        },
    );
    io
}

fn conda_recipe<G: RecipeGenerator>(
    generator: &G,
    init: &InitializeParams,
    params: &CondaRecipeParams,
) -> miette::Result<CondaRecipeResult> {
    let model = init
        .project_model
        .clone()
        .ok_or_else(|| miette::miette!("this backend needs a pixi package manifest"))?;
    let config: G::Config = match &init.configuration {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| miette::miette!("invalid [package.build.config]: {e}"))?,
        None => G::Config::default(),
    };
    let manifest_path = init.manifest_path.clone();
    let source_dir: PathBuf = init
        .source_directory
        .clone()
        .or_else(|| manifest_path.parent().map(Path::to_path_buf))
        .unwrap_or_default();
    let cx = RecipeContext {
        model: &model,
        manifest_path: &manifest_path,
        source_dir: &source_dir,
        params,
    };
    let generated = generator.generate(&cx, &config)?;
    let recipe = match generated.recipe {
        RecipeSource::Typed(r) => {
            serde_yaml::to_string(&r).map_err(|e| miette::miette!("serializing recipe: {e}"))?
        }
        RecipeSource::Yaml(y) => y,
    };
    Ok(CondaRecipeResult {
        recipe,
        recipe_directory: source_dir,
        variant_configuration: generated.variants,
        input_globs: generated.input_globs,
    })
}

/// `name`, `name >=1.2`, `name 1.2.* py*`. Source specs become plain names:
/// pixi resolves them against the project model.
pub fn spec_string(name: &SourcePackageName, spec: &PackageSpec) -> String {
    let name = name.as_str().to_string();
    match spec {
        PackageSpec::Binary(b) => {
            let mut s = name;
            match (&b.version, &b.build) {
                (Some(v), Some(build)) => s.push_str(&format!(" {v} {build}")),
                (Some(v), None) => s.push_str(&format!(" {v}")),
                (None, Some(build)) => s.push_str(&format!(" * {build}")),
                (None, None) => {}
            }
            s
        }
        _ => name,
    }
}

fn specs(deps: Option<&ordermap::OrderMap<SourcePackageName, PackageSpec>>) -> Vec<String> {
    deps.into_iter()
        .flatten()
        .map(|(n, s)| spec_string(n, s))
        .collect()
}

/// The default target of the model (platform-conditional targets are not
/// mapped yet).
pub fn default_target(model: &ProjectModel) -> Option<&Target> {
    model
        .targets
        .as_ref()
        .and_then(|t| t.default_target.as_ref())
}

/// A recipe with the package metadata, dependencies and `source: path` filled
/// in from the model; generators set `build` and add their toolchain.
pub fn base_recipe(cx: &RecipeContext<'_>, generator: Generator) -> miette::Result<Recipe> {
    let m = cx.model;
    let name = m
        .name
        .clone()
        .ok_or_else(|| miette::miette!("package name missing in the manifest"))?;
    let version = m
        .version
        .as_ref()
        .map(|v| v.to_string())
        .ok_or_else(|| miette::miette!("package version missing in the manifest"))?;
    let t = default_target(m);
    Ok(Recipe {
        schema_version: Some(blaze_recipe::SCHEMA_VERSION),
        package: Package { name, version },
        source: Some(Source::Path {
            path: cx.source_dir.to_path_buf(),
        }),
        build: Build {
            number: m.build_number.unwrap_or(0),
            generator,
            ..Default::default()
        },
        requirements: Requirements {
            build: specs(t.and_then(|t| t.build_dependencies.as_ref())),
            host: specs(t.and_then(|t| t.host_dependencies.as_ref())),
            run: specs(t.and_then(|t| t.run_dependencies.as_ref())),
            run_constraints: specs(t.and_then(|t| t.run_constraints.as_ref())),
        },
        tests: Vec::new(),
        about: About {
            license: m.license.clone(),
            summary: m.description.clone(),
            homepage: m.homepage.as_ref().map(|u| u.to_string()),
        },
        environments: BTreeMap::new(),
        tasks: BTreeMap::new(),
        steps: BTreeMap::new(),
        outputs: Vec::new(),
    })
}

/// Add `spec` unless a requirement for the same package is already there.
pub fn ensure(list: &mut Vec<String>, spec: &str) {
    let name = spec.split_whitespace().next().unwrap_or(spec);
    if !list
        .iter()
        .any(|s| s.split_whitespace().next() == Some(name))
    {
        list.push(spec.to_string());
    }
}

/// Add the C/C++ compilers (`${{ compiler('c') }}`) unless the user listed
/// one of the compiler packages already.
pub fn ensure_compilers(list: &mut Vec<String>, langs: &[&str]) {
    for lang in langs {
        let spec = format!("${{{{ compiler('{lang}') }}}}");
        if !list.contains(&spec) {
            list.push(spec);
        }
    }
}

// ---------------------------------------------------------------------------
// Default tasks
//
// Backends add conventional developer tasks (`fmt`, `lint`) to the recipe.
// They are defaults: a task of the same name in `[package.tasks]` replaces
// them, and `default-tasks = false` in the backend config turns them off.

use blaze_recipe::{Cmd, DependsOn, EnvironmentDef, Task, TaskDef};

/// Globs for C/C++ sources and headers.
pub const CXX_GLOB: &str = "**/*.{c,cc,cpp,cxx,c++,h,hh,hpp,hxx,h++,ipp,tpp,cu,cuh}";

/// Look for `name` in `dir` and its ancestors (e.g. a repository-wide
/// `.clang-format` above the package directory).
pub fn find_up(dir: &Path, name: &str) -> Option<PathBuf> {
    dir.ancestors().map(|d| d.join(name)).find(|p| p.exists())
}

fn task(cmd: &str, description: &str) -> Task {
    Task {
        cmd: Some(Cmd::Shell(cmd.to_string())),
        description: Some(description.to_string()),
        ..Default::default()
    }
}

fn add_task(r: &mut Recipe, name: &str, t: Task) {
    r.tasks.entry(name.to_string()).or_insert(TaskDef::Full(t));
}

fn add_env(r: &mut Recipe, name: &str, specs: &[&str]) {
    r.environments
        .entry(name.to_string())
        .or_insert_with(|| EnvironmentDef {
            dependencies: blaze_recipe::Dependencies::List(
                specs.iter().map(|s| s.to_string()).collect(),
            ),
        });
}

/// Add `dep` to the alias task `name` (`fmt` -> [fmt-cpp, fmt-py]).
fn add_to_alias(r: &mut Recipe, name: &str, dep: &str, description: &str) {
    let entry = r.tasks.entry(name.to_string()).or_insert_with(|| {
        TaskDef::Full(Task {
            description: Some(description.to_string()),
            ..Default::default()
        })
    });
    if let TaskDef::Full(t) = entry
        && t.cmd.is_none()
        && !t.depends_on.iter().any(|d| d.name() == dep)
    {
        t.depends_on.push(DependsOn::Name(dep.to_string()));
    }
}

/// `fmt-cpp` / `lint-cpp` with clang-format, if the project has a
/// `.clang-format` (its style is the project's decision, not ours). One
/// action per file: `lint` only re-checks files that changed.
pub fn add_clang_format_tasks(r: &mut Recipe, source_dir: &Path) {
    if find_up(source_dir, ".clang-format").is_none() {
        return;
    }
    add_env(r, "clang-format", &["clang-format"]);
    add_task(
        r,
        "lint-cpp",
        Task {
            foreach: Some(CXX_GLOB.into()),
            environment: Some("clang-format".into()),
            ..task(
                "clang-format --dry-run --Werror \"{{ input }}\"",
                "check C/C++ formatting (clang-format, cached per file)",
            )
        },
    );
    add_task(
        r,
        "fmt-cpp",
        Task {
            foreach: Some(CXX_GLOB.into()),
            environment: Some("clang-format".into()),
            in_place: true,
            ..task(
                "clang-format -i \"{{ input }}\"",
                "format C/C++ sources (clang-format)",
            )
        },
    );
    add_to_alias(r, "fmt", "fmt-cpp", "format all sources");
    add_to_alias(r, "lint", "lint-cpp", "run all linters");
}

/// `fmt-py` / `lint-py` with ruff (cached on the Python sources).
pub fn add_ruff_tasks(r: &mut Recipe) {
    add_env(r, "ruff", &["ruff"]);
    add_task(
        r,
        "lint-py",
        Task {
            environment: Some("ruff".into()),
            inputs: vec![
                "**/*.py".into(),
                "pyproject.toml".into(),
                "ruff.toml".into(),
                ".ruff.toml".into(),
            ],
            ..task(
                "ruff check . && ruff format --check .",
                "lint + check formatting of Python sources (ruff)",
            )
        },
    );
    add_task(
        r,
        "fmt-py",
        Task {
            environment: Some("ruff".into()),
            in_place: true,
            inputs: vec!["**/*.py".into()],
            ..task("ruff format .", "format Python sources (ruff)")
        },
    );
    add_to_alias(r, "fmt", "fmt-py", "format all sources");
    add_to_alias(r, "lint", "lint-py", "run all linters");
}

/// `fmt-rs` / `lint-rs` with rustfmt and clippy (from the build env's rust).
pub fn add_rust_tasks(r: &mut Recipe) {
    add_task(
        r,
        "lint-rs",
        task(
            "cargo fmt --check && cargo clippy --locked -- -D warnings",
            "check formatting and run clippy",
        ),
    );
    add_task(
        r,
        "fmt-rs",
        Task {
            in_place: true,
            inputs: vec!["**/*.rs".into()],
            ..task("cargo fmt", "format Rust sources (rustfmt)")
        },
    );
    add_to_alias(r, "fmt", "fmt-rs", "format all sources");
    add_to_alias(r, "lint", "lint-rs", "run all linters");
}
