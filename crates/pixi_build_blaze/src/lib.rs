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
    /// The workspace root (or git checkout), for backends that look at
    /// sibling packages (ROS).
    pub workspace_dir: Option<&'a Path>,
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
    /// Requirements that are packages of the same workspace: conda name ->
    /// path relative to the manifest directory (built from source).
    pub source_dependencies: BTreeMap<String, String>,
}

impl From<Recipe> for Generated {
    fn from(recipe: Recipe) -> Self {
        Generated {
            recipe: RecipeSource::Typed(Box::new(recipe)),
            variants: BTreeMap::new(),
            input_globs: Vec::new(),
            source_dependencies: BTreeMap::new(),
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
        .or_else(|| {
            // A `package.xml` source passes its directory as the manifest.
            if manifest_path.is_dir() {
                Some(manifest_path.clone())
            } else {
                manifest_path.parent().map(Path::to_path_buf)
            }
        })
        .unwrap_or_default();
    let workspace_dir = init
        .checkout_root
        .clone()
        .or_else(|| init.workspace_directory.clone());
    let cx = RecipeContext {
        model: &model,
        manifest_path: &manifest_path,
        source_dir: &source_dir,
        workspace_dir: workspace_dir.as_deref(),
        params,
    };
    let generated = generator.generate(&cx, &config)?;
    let recipe = match generated.recipe {
        // Recipes generated from the model also get its conditional
        // dependencies; a hand-written recipe has its own.
        RecipeSource::Typed(r) => add_conditional_requirements(
            &serde_yaml::to_string(&r).map_err(|e| miette::miette!("serializing recipe: {e}"))?,
            &model,
        )?,
        RecipeSource::Yaml(y) => y,
    };
    Ok(CondaRecipeResult {
        recipe,
        recipe_directory: source_dir,
        variant_configuration: generated.variants,
        input_globs: generated.input_globs,
        source_dependencies: generated.source_dependencies,
    })
}

/// A requirement as a match spec string (`name >=1.2`, `conda-forge::name
/// 1.2.* py*[md5=...]`), with every field of the manifest's spec. Source
/// specs become plain names: pixi resolves them against the project model.
pub fn spec_string(name: &SourcePackageName, spec: &PackageSpec) -> String {
    use rattler_conda_types::{Channel, MatchSpec, PackageName, PackageNameMatcher, VersionSpec};
    let PackageSpec::Binary(b) = spec else {
        return name.as_str().to_string();
    };
    let b = b.as_ref().clone();
    // A bare `*` is no constraint (and keeps the requirement a variant).
    let constrained = b.build.is_some()
        || b.build_number.is_some()
        || b.file_name.is_some()
        || b.channel.is_some()
        || b.subdir.is_some()
        || b.md5.is_some()
        || b.sha256.is_some()
        || b.url.is_some()
        || b.license.is_some()
        || b.condition.is_some();
    let version = if constrained {
        Some(b.version.unwrap_or(VersionSpec::Any))
    } else {
        b.version.filter(|v| v != &VersionSpec::Any)
    };
    MatchSpec {
        name: PackageNameMatcher::Exact(PackageName::new_unchecked(name.as_str())),
        version,
        build: b.build,
        build_number: b.build_number,
        file_name: b.file_name,
        extras: b.extras,
        channel: b.channel.map(Channel::from_url).map(std::sync::Arc::new),
        subdir: b.subdir,
        namespace: None,
        md5: b.md5,
        sha256: b.sha256,
        url: b.url,
        license: b.license,
        condition: b.condition,
        track_features: None,
        flags: b.flags,
        license_family: None,
    }
    .to_string()
}

/// The model's conditional dependencies (`[package.target.linux-64.*]` and
/// `"if(...)"` tables arrive as `host_platform == 'linux-64'` expressions) as
/// rattler-build style list items in the recipe's requirements:
/// `- if: <expression>  then: [specs]`, which blaze evaluates per variant.
pub fn add_conditional_requirements(recipe: &str, model: &ProjectModel) -> miette::Result<String> {
    let Some(conditional) = model.targets.as_ref().and_then(|t| t.conditional.as_ref()) else {
        return Ok(recipe.to_string());
    };
    if conditional.is_empty() {
        return Ok(recipe.to_string());
    }
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(recipe).map_err(|e| miette::miette!("parsing the recipe: {e}"))?;
    let reqs = doc
        .as_mapping_mut()
        .ok_or_else(|| miette::miette!("the recipe is not a mapping"))?
        .entry("requirements".into())
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
    for (expr, target) in conditional {
        for (key, deps) in [
            ("build", &target.build_dependencies),
            ("host", &target.host_dependencies),
            ("run", &target.run_dependencies),
            ("run_constraints", &target.run_constraints),
        ] {
            let specs = specs(deps.as_ref());
            if specs.is_empty() {
                continue;
            }
            let mut item = serde_yaml::Mapping::new();
            item.insert("if".into(), expr.to_string().into());
            item.insert(
                "then".into(),
                serde_yaml::Value::Sequence(specs.into_iter().map(Into::into).collect()),
            );
            let list = reqs
                .as_mapping_mut()
                .ok_or_else(|| miette::miette!("requirements is not a mapping"))?
                .entry(key.into())
                .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
            if let Some(seq) = list.as_sequence_mut() {
                seq.push(serde_yaml::Value::Mapping(item));
            }
        }
    }
    serde_yaml::to_string(&doc).map_err(|e| miette::miette!("serializing the recipe: {e}"))
}

fn specs(deps: Option<&ordermap::OrderMap<SourcePackageName, PackageSpec>>) -> Vec<String> {
    deps.into_iter()
        .flatten()
        .map(|(n, s)| spec_string(n, s))
        .collect()
}

/// The default target of the model (conditional targets are added to the
/// recipe by [`add_conditional_requirements`]).
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

#[cfg(test)]
mod tests {
    use super::*;
    use pixi_build_types::{BinaryPackageSpec, ConditionalExpression, Target, Targets};

    fn binary(f: impl FnOnce(&mut BinaryPackageSpec)) -> PackageSpec {
        let mut b = BinaryPackageSpec::default();
        f(&mut b);
        PackageSpec::Binary(Box::new(b))
    }

    #[test]
    fn binary_specs_keep_every_field() {
        let name = SourcePackageName::from(rattler_conda_types::PackageName::new_unchecked("zlib"));
        assert_eq!(spec_string(&name, &binary(|_| {})), "zlib");
        assert_eq!(
            spec_string(&name, &binary(|b| b.version = Some("*".parse().unwrap()))),
            "zlib"
        );
        let full = spec_string(
            &name,
            &binary(|b| {
                b.version = Some(">=1.3".parse().unwrap());
                b.channel = Some("https://prefix.dev/conda-forge".parse().unwrap());
                b.build = Some("h*_1".parse().unwrap());
                b.subdir = Some("linux-64".into());
            }),
        );
        assert!(full.contains("zlib") && full.contains(">=1.3"), "{full}");
        assert!(full.contains("conda-forge"), "{full}");
        assert!(full.contains("h*_1") && full.contains("linux-64"), "{full}");
    }

    #[test]
    fn conditional_dependencies_become_selector_items() {
        let mut host = ordermap::OrderMap::new();
        host.insert(
            SourcePackageName::from(rattler_conda_types::PackageName::new_unchecked("libudev")),
            binary(|_| {}),
        );
        let mut conditional = ordermap::OrderMap::new();
        conditional.insert(
            ConditionalExpression::new("host_platform == 'linux-64'"),
            Target {
                host_dependencies: Some(host),
                ..Default::default()
            },
        );
        let model = ProjectModel {
            targets: Some(Targets {
                default_target: None,
                conditional: Some(conditional),
            }),
            ..Default::default()
        };
        let recipe = "package: {name: p, version: '1'}\nrequirements:\n  host: [zlib]\n";
        let out = add_conditional_requirements(recipe, &model).unwrap();
        let doc: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let host = doc["requirements"]["host"].as_sequence().unwrap();
        assert_eq!(host[0].as_str(), Some("zlib"));
        assert_eq!(host[1]["if"].as_str(), Some("host_platform == 'linux-64'"));
        assert_eq!(host[1]["then"][0].as_str(), Some("libudev"));
        // And blaze evaluates it per platform.
        for (platform, expected) in [("linux-64", true), ("osx-arm64", false)] {
            let v =
                blaze_recipe::expand(&out, Path::new("."), platform, &Default::default()).unwrap();
            let has = v[0].recipe.requirements.host.iter().any(|s| s == "libudev");
            assert_eq!(
                has, expected,
                "{platform}: {:?}",
                v[0].recipe.requirements.host
            );
        }
    }
}
