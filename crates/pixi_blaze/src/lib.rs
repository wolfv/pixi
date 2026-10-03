//! pixi <-> rattler-blaze.
//!
//! Build backends that implement `conda/recipe` only describe a package (a
//! rattler-blaze recipe). This crate does the rest inside pixi:
//!
//! - [`outputs`]: expand the recipe into variants and derive the
//!   `conda/outputs` metadata pixi needs for solving;
//! - [`build`]: build one output with blaze against the build/host prefixes
//!   pixi already solved and installed, every compile/link/test a cached
//!   action; all builds of a pixi process share one [`blaze::Session`] (one
//!   action cache, one pool of job slots, in-flight dedup).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use anyhow::{Context as _, bail};
use blaze::{ExternalEnvs, Session, SessionOptions, Target, Unit, recipe::Variant};
use pixi_build_types::{
    BinaryPackageSpec, NamedSpec, PackageSpec, ProjectModel, SourcePackageName, VariantValue,
    procedures::{
        conda_build_v1::{CondaBuildV1Params, CondaBuildV1Result},
        conda_outputs::{
            CondaOutput, CondaOutputDependencies, CondaOutputIgnoreRunExports, CondaOutputMetadata,
            CondaOutputRunExports, CondaOutputsParams, CondaOutputsResult,
        },
        conda_recipe::{CondaRecipeParams, CondaRecipeResult},
    },
};
use rattler_conda_types::{
    MatchSpec, NoArchType, PackageName, ParseMatchSpecOptions, Platform, VersionWithSource,
};
use tokio::sync::Mutex;

pub use blaze;

pub mod engine;

/// Recipe params for the same request as `conda/outputs`.
pub fn recipe_params_from_outputs(p: &CondaOutputsParams) -> CondaRecipeParams {
    CondaRecipeParams {
        channels: p.channels.clone(),
        host_platform: p.host_platform,
        build_platform: p.build_platform,
        variant_configuration: p.variant_configuration.clone(),
        variant_files: p.variant_files.clone(),
        work_directory: p.work_directory.clone(),
    }
}

pub fn recipe_params_from_build(p: &CondaBuildV1Params) -> CondaRecipeParams {
    let build_platform = p
        .build_prefix
        .as_ref()
        .map(|b| b.platform)
        .unwrap_or(p.output.subdir);
    CondaRecipeParams {
        channels: p.channels.clone(),
        // A noarch output is built on, and for, the build platform: the
        // backend sees the same platform as for `conda/outputs`.
        host_platform: if p.output.subdir == Platform::NoArch {
            build_platform
        } else {
            p.output.subdir
        },
        build_platform,
        variant_configuration: None,
        variant_files: None,
        work_directory: p.work_directory.clone(),
    }
}

fn variant_value_str(v: &VariantValue) -> String {
    match v {
        VariantValue::String(s) => s.clone(),
        VariantValue::Int(i) => i.to_string(),
        VariantValue::Bool(b) => b.to_string(),
    }
}

/// Backend defaults < recipe's variants.yaml < workspace variant files <
/// workspace variant configuration.
fn variant_config(
    recipe: &CondaRecipeResult,
    platform: Platform,
    files: Option<&[PathBuf]>,
    workspace: Option<&BTreeMap<String, Vec<VariantValue>>>,
) -> anyhow::Result<BTreeMap<String, Vec<String>>> {
    let mut config = recipe.variant_configuration.clone();
    config.extend(blaze::recipe::load_variant_config(
        &recipe.recipe_directory.join("variants.yaml"),
        platform.as_str(),
    )?);
    for f in files.into_iter().flatten() {
        config.extend(blaze::recipe::load_variant_config(f, platform.as_str())?);
    }
    for (k, vs) in workspace.into_iter().flatten() {
        config.insert(k.clone(), vs.iter().map(variant_value_str).collect());
    }
    Ok(config)
}

/// All variants of a recipe for `platform` under the workspace's variant
/// configuration (for `pixi run <pkg>//<task>`).
pub fn variants(
    recipe: &CondaRecipeResult,
    platform: Platform,
    variant_files: &[PathBuf],
    variant_configuration: &BTreeMap<String, Vec<VariantValue>>,
) -> anyhow::Result<Vec<Variant>> {
    let config = variant_config(
        recipe,
        platform,
        Some(variant_files),
        Some(variant_configuration),
    )?;
    expand(recipe, platform, &config)
}

fn expand(
    recipe: &CondaRecipeResult,
    platform: Platform,
    config: &BTreeMap<String, Vec<String>>,
) -> anyhow::Result<Vec<Variant>> {
    blaze::recipe::expand(
        &recipe.recipe,
        &recipe.recipe_directory,
        platform.as_str(),
        config,
    )
    .context("expanding the recipe from the build backend")
}

/// Source dependencies by name: the backend's (`source_dependencies`, e.g.
/// ROS workspace siblings) and the project model's (the recipe only carries
/// names for them).
fn source_specs(
    recipe: &CondaRecipeResult,
    model: Option<&ProjectModel>,
) -> BTreeMap<String, PackageSpec> {
    let mut out = BTreeMap::new();
    for (name, path) in &recipe.source_dependencies {
        out.insert(
            name.clone(),
            PackageSpec::Source(pixi_build_types::SourcePackageSpec {
                location: pixi_build_types::SourcePackageLocationSpec::Path(
                    pixi_build_types::PathSpec { path: path.clone() },
                ),
                version: None,
                build: None,
                build_number: None,
                extras: None,
                flags: None,
                subdir: None,
                license: None,
                condition: None,
            }),
        );
    }
    let Some(targets) = model.and_then(|m| m.targets.as_ref()) else {
        return out;
    };
    // The default target and the conditional ones: a requirement only names
    // the package, and whichever condition holds, it's that source.
    let all = targets
        .default_target
        .iter()
        .chain(targets.conditional.iter().flat_map(|c| c.values()));
    for deps in all
        .flat_map(|t| {
            [
                &t.build_dependencies,
                &t.host_dependencies,
                &t.run_dependencies,
            ]
        })
        .flatten()
    {
        for (name, spec) in deps {
            if matches!(spec, PackageSpec::Source(_)) {
                out.insert(name.as_str().to_string(), spec.clone());
            }
        }
    }
    out
}

fn named_spec(
    s: &str,
    sources: &BTreeMap<String, PackageSpec>,
) -> anyhow::Result<NamedSpec<PackageSpec>> {
    let m = MatchSpec::from_str(s, ParseMatchSpecOptions::lenient())
        .with_context(|| format!("invalid requirement `{s}`"))?;
    let name = m
        .name
        .as_exact()
        .with_context(|| format!("requirement `{s}` needs an exact package name"))?
        .clone();
    let spec = match sources.get(name.as_normalized()) {
        Some(src) => src.clone(),
        None => PackageSpec::Binary(Box::new(BinaryPackageSpec {
            version: m.version.clone(),
            build: m.build.clone(),
            ..Default::default()
        })),
    };
    Ok(NamedSpec {
        name: SourcePackageName::from(name),
        spec,
    })
}

fn deps(
    specs: &[String],
    sources: &BTreeMap<String, PackageSpec>,
) -> anyhow::Result<CondaOutputDependencies> {
    Ok(CondaOutputDependencies {
        depends: specs
            .iter()
            .map(|s| named_spec(s, sources))
            .collect::<anyhow::Result<_>>()?,
        constraints: Vec::new(),
    })
}

/// Build tools blaze itself needs (RPATH rewriting while packaging).
fn build_specs(v: &Variant) -> Vec<String> {
    let mut specs = v.recipe.requirements.build.clone();
    if v.target_platform.starts_with("linux") && !specs.iter().any(|s| s.starts_with("patchelf")) {
        specs.push("patchelf".into());
    }
    specs
}

/// `conda/outputs` computed from a recipe.
pub fn outputs(
    recipe: &CondaRecipeResult,
    params: &CondaOutputsParams,
    model: Option<&ProjectModel>,
) -> anyhow::Result<CondaOutputsResult> {
    let config = variant_config(
        recipe,
        params.host_platform,
        params.variant_files.as_deref(),
        params.variant_configuration.as_ref(),
    )?;
    let sources = source_specs(recipe, model);
    let mut outputs = Vec::new();
    for v in expand(recipe, params.host_platform, &config)? {
        let r = &v.recipe;
        let variant: BTreeMap<String, VariantValue> = v
            .used
            .iter()
            .map(|(k, val)| (k.clone(), VariantValue::String(val.clone())))
            .collect();
        let build = deps(&build_specs(&v), &sources)?;
        let host = deps(&r.requirements.host, &sources)?;
        let mut names = vec![(r.package.name.clone(), r.requirements.run.clone())];
        names.extend(
            r.outputs
                .iter()
                .map(|o| (o.name.clone(), o.requirements.run.clone())),
        );
        let siblings: Vec<String> = names.iter().map(|(n, _)| n.clone()).collect();
        for (name, run) in names {
            // Declared dependencies on sibling outputs are exact pins (they
            // come from the same build).
            let run: Vec<String> = run
                .iter()
                .map(|spec| {
                    let dep = spec.split_whitespace().next().unwrap_or(spec);
                    if dep != name && siblings.iter().any(|s| s == dep) && !spec.contains(' ') {
                        format!("{dep} =={} {}", r.package.version, v.build_string)
                    } else {
                        spec.clone()
                    }
                })
                .collect();
            outputs.push(CondaOutput {
                metadata: CondaOutputMetadata {
                    name: PackageName::try_from(name.as_str())?,
                    version: VersionWithSource::from_str(&r.package.version)?,
                    build: v.build_string.clone(),
                    build_number: r.build.number,
                    // noarch packages are built here but published to noarch/.
                    subdir: if r.build.noarch.is_some() {
                        Platform::NoArch
                    } else {
                        params.host_platform
                    },
                    license: r.about.license.clone(),
                    license_family: None,
                    flags: Vec::new(),
                    track_features: Vec::new(),
                    noarch: match r.build.noarch {
                        Some(blaze::recipe::NoArch::Python) => NoArchType::python(),
                        Some(blaze::recipe::NoArch::Generic) => NoArchType::generic(),
                        None => NoArchType::none(),
                    },
                    purls: None,
                    python_site_packages_path: None,
                    variant: variant.clone(),
                },
                build_dependencies: Some(build.clone()),
                host_dependencies: Some(host.clone()),
                run_dependencies: deps(&run, &sources)?,
                extra_dependencies: BTreeMap::new(),
                ignore_run_exports: CondaOutputIgnoreRunExports::default(),
                run_exports: CondaOutputRunExports::default(),
                input_globs: None,
                input_glob_sets: None,
            });
        }
    }
    Ok(CondaOutputsResult {
        outputs,
        input_globs: metadata_globs(recipe),
        input_glob_sets: None,
    })
}

/// What the outputs (metadata, variants, build strings) are derived from:
/// the package manifest, the variant files next to it, the backend's own
/// inputs, and step/task files pulled in with `uses:`.
fn metadata_globs(recipe: &CondaRecipeResult) -> Vec<String> {
    let mut g: Vec<String> = ["pixi.toml", "pyproject.toml", "variants.yaml"]
        .into_iter()
        .map(String::from)
        .collect();
    g.extend(recipe.input_globs.iter().cloned());
    g.extend(uses_files(&recipe.recipe));
    g.sort();
    g.dedup();
    g
}

/// `uses: ./steps/lint.yaml` in the recipe's steps and tasks.
fn uses_files(recipe: &str) -> Vec<String> {
    let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(recipe) else {
        return Vec::new();
    };
    ["steps", "tasks"]
        .into_iter()
        .filter_map(|k| doc.get(k).and_then(|m| m.as_mapping()))
        .flat_map(|m| m.values())
        .filter_map(|t| t.get("uses").and_then(|u| u.as_str()))
        .map(|u| u.trim_start_matches("./").to_string())
        .collect()
}

/// Everything below the package directory except build products: blaze
/// re-running on an unchanged tree is a no-op anyway (all actions cached).
/// Only top-level build trees are left out: a source directory that happens
/// to be called `build` (a Python subpackage, say) is a source.
fn build_globs() -> Vec<String> {
    [
        "**",
        "!.pixi/**",
        "!.blaze/**",
        "!target/**",
        "!build/**",
        "!**/.git/**",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// The embedded engine of one pixi process: one blaze session per channel
/// list (one action cache and pool of job slots for every package built),
/// plus per-variant build dedup. Owned by the command dispatcher.
pub struct Runtime {
    root: PathBuf,
    jobs: Option<usize>,
    sessions: Mutex<BTreeMap<Vec<String>, Arc<Session>>>,
    builds: std::sync::Mutex<BTreeMap<String, Arc<tokio::sync::OnceCell<BuiltVariant>>>>,
}

type BuiltVariant = Result<Vec<blaze::BuiltPackage>, String>;

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("root", &self.root).finish()
    }
}

impl Runtime {
    /// `root` holds the action cache, CAS and shared environments
    /// (`PIXI_BLAZE_ROOT` overrides it); `jobs`: parallel actions.
    pub fn new(root: PathBuf, jobs: Option<usize>) -> Arc<Self> {
        let root = std::env::var_os("PIXI_BLAZE_ROOT")
            .map(PathBuf::from)
            .unwrap_or(root);
        let jobs = std::env::var("PIXI_BLAZE_JOBS")
            .ok()
            .and_then(|j| j.parse().ok())
            .or(jobs);
        Arc::new(Runtime {
            root,
            jobs,
            sessions: Mutex::new(BTreeMap::new()),
            builds: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    /// The process-wide runtime for `root` (the dispatcher and `pixi run`
    /// share it, and with it the action cache and job slots).
    pub fn shared(root: PathBuf, jobs: Option<usize>) -> Arc<Self> {
        static RUNTIMES: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, Arc<Runtime>>>> =
            std::sync::OnceLock::new();
        RUNTIMES
            .get_or_init(Default::default)
            .lock()
            .unwrap()
            .entry(root.clone())
            .or_insert_with(|| Runtime::new(root, jobs))
            .clone()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn session(&self, channels: Vec<String>) -> anyhow::Result<Arc<Session>> {
        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get(&channels) {
            return Ok(s.clone());
        }
        let mut o = SessionOptions::new(&self.root);
        o.channels = channels.clone();
        o.quiet = std::env::var_os("PIXI_BLAZE_VERBOSE").is_none();
        o.declared_output_deps = true;
        // Package tasks' commands mean what they mean in [tasks]: deno's
        // task shell, run through the pixi executable (`shim_main`).
        o.task_shell = blaze::TaskShell::Deno;
        if let Some(j) = self.jobs {
            o.jobs = j;
        }
        let s = Arc::new(Session::new(o).await?);
        sessions.insert(channels, s.clone());
        Ok(s)
    }
}

fn padded_host(work: &Path) -> PathBuf {
    let mut host = String::from("host_env");
    if cfg!(unix) {
        while work.join(&host).as_os_str().len() < 255 {
            host.push_str("_placehold");
        }
        let excess = work.join(&host).as_os_str().len().saturating_sub(255);
        host.truncate(host.len() - excess);
    }
    work.join(host)
}

/// `conda/build_v1` for a recipe: build the requested output with blaze.
/// All outputs of the variant are built once (concurrent and later calls for
/// sibling outputs reuse the result).
/// With `ctx`, the steps run on pixi's compute engine (one key per step, see
/// [`engine`]); without, on blaze's own scheduler.
pub async fn build(
    runtime: &Runtime,
    recipe: &CondaRecipeResult,
    params: &CondaBuildV1Params,
    sink: Option<blaze::report::LineSink>,
    ctx: Option<&mut pixi_compute_engine::ComputeCtx>,
) -> anyhow::Result<CondaBuildV1Result> {
    let out = &params.output;
    // A noarch output is built on (and for) the build platform.
    let platform = if out.subdir == Platform::NoArch {
        params
            .build_prefix
            .as_ref()
            .map(|b| b.platform)
            .unwrap_or_else(Platform::current)
    } else {
        out.subdir
    };
    // Pin exactly the requested variant.
    let mut config = variant_config(recipe, platform, None, None)?;
    for (k, v) in &out.variant {
        config.insert(k.clone(), vec![variant_value_str(v)]);
    }
    let name = out.name.as_normalized().to_string();
    let variants = expand(recipe, platform, &config)?;
    let variant = variants
        .into_iter()
        .find(|v| {
            let r = &v.recipe;
            (r.package.name == name || r.outputs.iter().any(|o| o.name == name))
                && out.build.as_ref().is_none_or(|b| *b == v.build_string)
        })
        .with_context(|| {
            format!(
                "the recipe has no output {name} {}",
                out.build.as_deref().unwrap_or("")
            )
        })?;
    let version = variant.recipe.package.version.clone();
    let build_string = variant.build_string.clone();
    let main_name = variant.recipe.package.name.clone();
    let work = params.work_directory.join("blaze");

    let key = format!("{}#{}", work.display(), variant.tag());
    let cell = runtime
        .builds
        .lock()
        .unwrap()
        .entry(key)
        .or_default()
        .clone();
    let built = cell
        .get_or_init(|| async {
            build_variant(runtime, variant, params, work, &main_name, sink, ctx)
                .await
                .map_err(|e| format!("{e:#}"))
        })
        .await
        .clone()
        .map_err(|e| anyhow::anyhow!(e))?;

    let Some(pkg) = built.iter().find(|p| p.name == name) else {
        bail!("blaze did not produce {name}");
    };
    let mut output_file = pkg.path.clone();
    if let Some(dir) = &params.output_directory {
        fs_err::create_dir_all(dir)?;
        let dest = dir.join(output_file.file_name().unwrap());
        fs_err::copy(&output_file, &dest)?;
        output_file = dest;
    }
    Ok(CondaBuildV1Result {
        output_file,
        input_globs: build_globs(),
        input_glob_sets: None,
        name,
        version: VersionWithSource::from_str(&version)?,
        build: build_string,
        subdir: out.subdir,
    })
}

async fn build_variant(
    runtime: &Runtime,
    variant: Variant,
    params: &CondaBuildV1Params,
    work: PathBuf,
    main_name: &str,
    sink: Option<blaze::report::LineSink>,
    ctx: Option<&mut pixi_compute_engine::ComputeCtx>,
) -> anyhow::Result<Vec<blaze::BuiltPackage>> {
    let (build_prefix, build_records) = match &params.build_prefix {
        Some(p) => (
            p.prefix.clone(),
            p.packages
                .iter()
                .map(|p| p.repodata_record.clone())
                .collect(),
        ),
        None => (work.join("build_env"), Vec::new()),
    };
    let (host_prefix, host_records) = match &params.host_prefix {
        Some(p) => (
            p.prefix.clone(),
            p.packages
                .iter()
                .map(|p| p.repodata_record.clone())
                .collect(),
        ),
        None => (padded_host(&work), Vec::new()),
    };
    let run_exports: Vec<String> = params
        .run_dependencies
        .iter()
        .flatten()
        .filter(|d| d.source.is_some())
        .map(|d| d.spec.to_string())
        .collect();
    let unit = Unit {
        variant,
        envs: Some(ExternalEnvs {
            build_prefix,
            build_records,
            host_prefix,
            host_records,
            run_exports: Some(run_exports),
            // The dispatcher only solved these (it skips installing prefixes
            // for recipe backends): blaze installs them into its shared,
            // content-addressed environments.
            materialize: true,
        }),
        locked: None,
        work_dir: Some(work),
        env_records: BTreeMap::new(),
        provided_envs: BTreeMap::new(),
    };
    let channels = params.channels.iter().map(|c| c.to_string()).collect();
    let session = runtime.session(channels).await?;
    let task = if std::env::var_os("PIXI_BLAZE_TEST").is_some() {
        "all"
    } else {
        "package"
    };
    let targets = [Target {
        package: Some(main_name.to_string()),
        task: task.into(),
    }];
    let options = blaze::RunOptions { sink };
    let outcome = match ctx {
        Some(ctx) => {
            let prepared = session.prepare(vec![unit], &targets, &options)?;
            let result = engine::execute(ctx, &prepared).await;
            session.finish_external(prepared, result)?
        }
        None => session.run_with(vec![unit], &targets, &options).await?,
    };
    tracing::info!(
        "blaze: {} steps in {:.1}s ({} actions executed, {} cached)",
        outcome.steps,
        outcome.wall.as_secs_f64(),
        outcome.executed,
        outcome.cached
    );
    Ok(outcome.packages)
}

// ---------------------------------------------------------------------------
// [package.steps] / [package.tasks]

/// Steps and tasks from a package manifest, as recipe YAML values.
#[derive(Debug, Default, Clone)]
pub struct ManifestTasks {
    pub steps: serde_yaml::Mapping,
    pub tasks: serde_yaml::Mapping,
}

impl ManifestTasks {
    /// `[package.steps]` / `[package.tasks]` of a `pixi.toml`, or
    /// `[tool.pixi.package.*]` of a `pyproject.toml`. (A platform-specific
    /// command uses the recipe's selectors: `${{ 'nmake' if win else 'make' }}`;
    /// pixi deprecates `[package.target.*]` tables.)
    pub fn read(manifest: &Path) -> anyhow::Result<Self> {
        // A `package.xml` source (whose manifest path is the package.xml or
        // its directory) has no pixi manifest.
        if manifest.is_dir() || manifest.extension().is_none_or(|e| e != "toml") {
            return Ok(Self::default());
        }
        let text = fs_err::read_to_string(manifest)
            .with_context(|| format!("reading {}", manifest.display()))?;
        let doc: toml::Table =
            toml::from_str(&text).with_context(|| format!("parsing {}", manifest.display()))?;
        let pyproject = manifest.file_name().is_some_and(|n| n == "pyproject.toml");
        let (package, prefix) = if pyproject {
            (
                doc.get("tool")
                    .and_then(|t| t.get("pixi"))
                    .and_then(|t| t.get("package")),
                "tool.pixi.package",
            )
        } else {
            (doc.get("package"), "package")
        };
        let mut out = Self::default();
        let Some(package) = package else {
            return Ok(out);
        };
        let section =
            |table: &toml::Value, key: &str, name: &str| -> anyhow::Result<serde_yaml::Mapping> {
                match table.get(key) {
                    None => Ok(serde_yaml::Mapping::new()),
                    Some(v) => match serde_yaml::to_value(serde_json::to_value(v)?)? {
                        serde_yaml::Value::Mapping(m) => Ok(m),
                        _ => bail!("[{name}.{key}] in {} must be a table", manifest.display()),
                    },
                }
            };
        out.steps = section(package, "steps", prefix)?;
        out.tasks = section(package, "tasks", prefix)?;
        Ok(out)
    }

    /// The manifest tasks that steps depend on, directly or through other
    /// tasks: they are part of the build.
    pub fn steps_depend_on(&self) -> std::collections::BTreeSet<String> {
        let mut out = std::collections::BTreeSet::new();
        let mut todo: Vec<String> = self.steps.values().flat_map(depends_on_names).collect();
        while let Some(name) = todo.pop() {
            if let Some(task) = self.tasks.get(name.as_str())
                && out.insert(name)
            {
                todo.extend(depends_on_names(task));
            }
        }
        out
    }

    /// Names of tasks from the manifest (they replace backend defaults).
    pub fn task_names(&self) -> Vec<String> {
        self.tasks
            .keys()
            .filter_map(|k| k.as_str().map(String::from))
            .collect()
    }
}

/// Merge the manifest's `[package.steps]` (always: they change the package)
/// and `[package.tasks]` into the backend's recipe, before it is expanded:
/// manifest entries replace recipe entries of the same name, and
/// build-affecting ones feed the build string. Without `with_tasks` (building
/// the package), only the tasks that steps depend on are merged: the others
/// may use workspace environments, which only `pixi run` provides.
pub fn merge_manifest(
    recipe: &mut CondaRecipeResult,
    tasks: &ManifestTasks,
    with_tasks: bool,
) -> anyhow::Result<()> {
    let tasks_to_merge: serde_yaml::Mapping = if with_tasks {
        tasks.tasks.clone()
    } else {
        tasks
            .tasks
            .iter()
            .filter(|(k, _)| {
                k.as_str()
                    .is_some_and(|k| tasks.steps_depend_on().contains(k))
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };
    if tasks.steps.is_empty() && tasks_to_merge.is_empty() {
        return Ok(());
    }
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&recipe.recipe).context("parsing the backend's recipe")?;
    let map = doc
        .as_mapping_mut()
        .context("the backend's recipe is not a mapping")?;
    let mut merge = |key: &str, entries: &serde_yaml::Mapping| {
        let section = map
            .entry(serde_yaml::Value::String(key.into()))
            .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
        if let serde_yaml::Value::Mapping(m) = section {
            for (k, v) in entries {
                m.insert(k.clone(), v.clone());
            }
        }
    };
    merge("steps", &tasks.steps);
    merge("tasks", &tasks_to_merge);
    recipe.recipe = serde_yaml::to_string(&doc)?;
    Ok(())
}

/// The names in a task's `depends-on` (`["a", {task = "b"}]`).
fn depends_on_names(task: &serde_yaml::Value) -> Vec<String> {
    let Some(deps) = task.get("depends-on").and_then(|d| d.as_sequence()) else {
        return Vec::new();
    };
    deps.iter()
        .filter_map(|d| {
            d.as_str()
                .or_else(|| d.get("task").and_then(|t| t.as_str()))
                .map(String::from)
        })
        .collect()
}
