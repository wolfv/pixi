//! `pixi run <package>//<task>` and `pixi run //<task>` (preview
//! `pixi-build-blaze`).
//!
//! A package has
//! - **build steps** from its backend (`configure`, `compile`, `install`,
//!   `in-build-tests`), which `[package.steps]` can override or extend;
//! - **targets** (`build`, `test`, `package`, `all`);
//! - **tasks**: the backend's defaults (`fmt`, `lint`, ...) and
//!   `[package.tasks]`, which replace defaults of the same name.
//!
//! The packages of the workspace run as one graph on pixi's embedded build
//! engine (rattler-blaze): source dependencies between them are edges, and
//! every compile, link, test and cacheable task is a cached action. Build and
//! host environments come from the lock file where it has them.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    str::FromStr,
};

use miette::{Context, IntoDiagnostic};
use pixi_blaze::{
    ManifestTasks,
    blaze::{
        self, LockedEnvs, Target, Unit,
        explain::Origin,
        recipe::{EnvironmentDef, Variant},
    },
};
use pixi_build_types::procedures::conda_recipe::CondaRecipeParams;
use pixi_command_dispatcher::InstantiateBackendKey;
use pixi_core::Workspace;
use pixi_manifest::FeaturesExt;
use pixi_record::{LockFileResolver, UnresolvedPixiRecord};
use pixi_spec::SourceAnchor;
use rattler_conda_types::{Platform, RepoDataRecord};

/// Does this `pixi run` argument name a package target?
pub fn is_package_target(arg: &str) -> bool {
    arg.contains("//")
}

struct PackageDir {
    dir: PathBuf,
    manifest: PathBuf,
}

/// Directories below the workspace root with a `pixi.toml` that has a
/// `[package]` table (including packages that are also their own nested
/// workspace, a common layout for `pixi run --manifest-path lib/...`).
fn discover_packages(root: &Path) -> miette::Result<Vec<PackageDir>> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root)
        .require_git(false)
        .git_global(false)
        .git_exclude(false)
        .build()
        .flatten()
    {
        if entry.file_name() != "pixi.toml" {
            continue;
        }
        let path = entry.path();
        let Ok(text) = fs_err::read_to_string(path) else {
            continue;
        };
        let Ok(table) = text.parse::<toml_edit::DocumentMut>() else {
            continue;
        };
        if table.get("package").is_none() {
            continue;
        }
        out.push(PackageDir {
            dir: path.parent().unwrap().to_path_buf(),
            manifest: path.to_path_buf(),
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(out)
}

/// The conda dependencies of a workspace environment, as match specs (for
/// package tasks with `default-environment = "<name>"`).
fn environment_specs(
    workspace: &Workspace,
    name: &str,
    platform: Platform,
) -> miette::Result<Vec<String>> {
    let env = workspace
        .environment(name)
        .ok_or_else(|| miette::miette!("unknown environment `{name}`"))?;
    let pixi_platform = pixi_manifest::PixiPlatform::from_subdir(platform);
    let channel_config = workspace.channel_config();
    let mut specs = Vec::new();
    for (pkg, spec) in env.combined_dependencies(Some(&pixi_platform)).into_specs() {
        match spec.try_into_nameless_match_spec(&channel_config) {
            Ok(Some(nameless)) => specs.push(format!("{} {}", pkg.as_normalized(), nameless)),
            _ => specs.push(pkg.as_normalized().to_string()),
        }
    }
    Ok(specs)
}

/// Build/host records from the lock file: for a package, the source record
/// of that name whose environments are for `platform` and whose variant
/// matches best. Source packages inside those environments are left out
/// (blaze builds them in the same run).
struct Locked {
    lock: rattler_lock::LockFile,
    resolver: Option<LockFileResolver>,
}

impl Locked {
    async fn load(workspace: &Workspace) -> Self {
        let lock = match workspace.load_lock_file().await {
            Ok(l) => l.into_lock_file_or_empty(),
            Err(_) => rattler_lock::LockFile::default(),
        };
        let resolver = LockFileResolver::build(&lock, workspace.root()).ok();
        Locked { lock, resolver }
    }

    fn envs(&self, v: &Variant, platform: Platform) -> Option<LockedEnvs> {
        let resolver = self.resolver.as_ref()?;
        let binaries = |records: &[UnresolvedPixiRecord]| -> Vec<RepoDataRecord> {
            records
                .iter()
                .filter_map(|r| match r {
                    UnresolvedPixiRecord::Binary(b) => Some(b.as_ref().clone()),
                    UnresolvedPixiRecord::Source(_) => None,
                })
                .collect()
        };
        let on_platform = |records: &[RepoDataRecord], allow_noarch: bool| {
            records.is_empty()
                || records.iter().any(|r| {
                    r.package_record.subdir == platform.as_str()
                        || (allow_noarch && r.package_record.subdir == "noarch")
                })
        };
        let mut best: Option<(usize, LockedEnvs)> = None;
        for pkg in self.lock.packages() {
            let Some(UnresolvedPixiRecord::Source(src)) = resolver.get_for_package(pkg) else {
                continue;
            };
            if src.name().as_normalized() != v.recipe.package.name {
                continue;
            }
            let build = binaries(&src.build_packages);
            let host = binaries(&src.host_packages);
            if !on_platform(&build, true) || !on_platform(&host, false) {
                continue;
            }
            // Prefer the record whose variant agrees with ours.
            let score = v
                .used
                .iter()
                .filter(|(k, val)| src.variants.get(*k).is_some_and(|x| x.to_string() == **val))
                .count();
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, LockedEnvs { build, host }));
            }
        }
        best.map(|(_, e)| e)
    }
}

enum Mode {
    Run(Vec<Target>),
    List(Option<String>),
    Explain(Target),
}

pub async fn execute(workspace: &Workspace, args: &[String]) -> miette::Result<()> {
    // `pixi run //` or `pixi run pkg//`: list what can be run.
    let mode = match args {
        [one] if one.ends_with("//") => {
            let pkg = one.trim_end_matches("//");
            Mode::List((!pkg.is_empty()).then(|| pkg.to_string()))
        }
        _ => Mode::Run(
            args.iter()
                .map(|a| Target::from_str(a))
                .collect::<Result<_, _>>()
                .map_err(|e| miette::miette!("{e:#}"))?,
        ),
    };
    run(workspace, mode, args).await
}

/// `pixi task explain <pkg//name>`
pub async fn explain(workspace: &Workspace, target: &str) -> miette::Result<()> {
    let t = Target::from_str(target).map_err(|e| miette::miette!("{e:#}"))?;
    run(workspace, Mode::Explain(t), &[target.to_string()]).await
}

async fn run(workspace: &Workspace, mode: Mode, args: &[String]) -> miette::Result<()> {
    let runtime = workspace.blaze_runtime()?.ok_or_else(|| {
        miette::miette!(
            help = "add it to your workspace: `preview = [\"pixi-build\", \"pixi-build-blaze\"]`",
            "package targets (`pkg//task`) need the `pixi-build-blaze` preview"
        )
    })?;
    let root = workspace.root().to_path_buf();
    let platform = workspace
        .default_environment()
        .best_declared_platform()
        .map(|p| p.subdir())
        .unwrap_or_else(Platform::current);
    let channel_config = workspace.channel_config();
    let channels: Vec<String> = workspace
        .default_environment()
        .channel_urls(&channel_config)
        .into_diagnostic()?
        .into_iter()
        .map(|c| c.to_string())
        .collect();
    let variants_config = workspace
        .variants(&pixi_manifest::PixiPlatform::from_subdir(platform))
        .into_diagnostic()?;
    let variant_configuration: BTreeMap<String, Vec<pixi_build_types::VariantValue>> =
        variants_config
            .variant_configuration
            .iter()
            .map(|(k, vs)| {
                (
                    k.clone(),
                    vs.iter()
                        .map(|v| pixi_build_types::VariantValue::String(v.to_string()))
                        .collect(),
                )
            })
            .collect();

    let dispatcher = workspace.command_dispatcher_builder(None)?.finish();
    let scratch = root.join(".pixi").join("blaze");
    let locked = Locked::load(workspace).await;

    let mut units = Vec::new();
    // Per package: names contributed by pixi.toml (for listing / explain).
    let mut from_manifest: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut unlocked = Vec::new();
    for pkg in discover_packages(&root)? {
        let dir = dunce::canonicalize(&pkg.dir).into_diagnostic()?;
        let build_dir = pixi_path::AbsPathBuf::new(dir.clone())
            .map_err(|_| miette::miette!("{} is not absolute", dir.display()))?
            .into_assume_dir();
        let backend = dispatcher
            .engine()
            .compute(&InstantiateBackendKey::new(
                &dir,
                None,
                SourceAnchor::Workspace,
                build_dir,
                None,
            ))
            .await
            .map_err(|e| miette::miette!("{e}"))
            .with_context(|| format!("instantiating the build backend of {}", dir.display()))?
            .map_err(|e| miette::miette!("{e}"))
            .with_context(|| format!("instantiating the build backend of {}", dir.display()))?;
        let backend = backend.lock().await;
        let mut recipe = backend
            .conda_recipe(CondaRecipeParams {
                channels: Vec::new(),
                host_platform: platform,
                build_platform: platform,
                variant_configuration: Some(variant_configuration.clone()),
                variant_files: Some(variants_config.variant_files.clone()),
                work_directory: scratch.join("backends"),
            })
            .await
            .ok_or_else(|| {
                miette::miette!(
                    "the build backend of {} ({}) does not implement `conda/recipe`; package \
                     targets need a recipe backend",
                    pkg.manifest.display(),
                    backend.identifier()
                )
            })?
            .into_diagnostic()?;
        let manifest = ManifestTasks::read(&pkg.manifest).map_err(|e| miette::miette!("{e:#}"))?;
        pixi_blaze::merge_manifest(&mut recipe, &manifest, true)
            .map_err(|e| miette::miette!("{e:#}"))?;
        let variants = pixi_blaze::variants(
            &recipe,
            platform,
            &variants_config.variant_files,
            &variant_configuration,
        )
        .map_err(|e| miette::miette!("{e:#}"))
        .with_context(|| format!("in {}", pkg.manifest.display()))?;
        let names: BTreeSet<String> = manifest
            .task_names()
            .into_iter()
            .chain(
                manifest
                    .steps
                    .keys()
                    .filter_map(|k| k.as_str().map(String::from)),
            )
            .collect();
        for mut variant in variants {
            workspace_environments(workspace, &mut variant, &manifest.task_names(), platform)?;
            from_manifest.insert(variant.recipe.package.name.clone(), names.clone());
            let envs = locked.envs(&variant, platform);
            if envs.is_none() {
                unlocked.push(variant.recipe.package.name.clone());
            }
            let mut unit = Unit::from(variant);
            unit.locked = envs;
            units.push(unit);
        }
    }
    if units.is_empty() {
        miette::bail!("no packages found below {}", root.display());
    }

    let targets = match mode {
        Mode::List(filter) => {
            list_tasks(&units, filter.as_deref(), &from_manifest);
            return Ok(());
        }
        Mode::Explain(t) => {
            let mut seen = BTreeSet::new();
            for u in &units {
                let r = &u.variant.recipe;
                if t.package.as_ref().is_some_and(|p| *p != r.package.name)
                    || !seen.insert(r.package.name.clone())
                {
                    continue;
                }
                let names = from_manifest
                    .get(&r.package.name)
                    .cloned()
                    .unwrap_or_default();
                let origin = move |n: &str| {
                    if names.contains(n) {
                        Origin::Manifest
                    } else {
                        Origin::Recipe
                    }
                };
                print!("{}", blaze::explain::explain(&u.variant, &t.task, &origin));
            }
            return Ok(());
        }
        Mode::Run(t) => t,
    };

    let session = runtime
        .session(channels)
        .await
        .map_err(|e| miette::miette!("{e:#}"))?;
    let names: Vec<String> = units
        .iter()
        .map(|u| format!("{} [{}]", u.variant.tag(), u.variant.describe()))
        .collect();
    eprintln!(
        "{}blaze: {} over {} package variant(s): {}",
        console::style(console::Emoji("✔ ", "")).green(),
        args.join(" "),
        units.len(),
        names.join(", ")
    );
    if !unlocked.is_empty() {
        eprintln!(
            "  {} not in the lock file, so blaze solves their environments: {}",
            console::style("note:").yellow(),
            unlocked.join(", ")
        );
    }
    let outcome = session
        .run(units, &targets)
        .await
        .map_err(|e| miette::miette!("{e:#}"))?;
    blaze::session::print_outcome(&outcome, session.output_dir());
    Ok(())
}

fn list_tasks(
    units: &[Unit],
    filter: Option<&str>,
    from_manifest: &BTreeMap<String, BTreeSet<String>>,
) {
    use console::style;
    let mut seen = BTreeSet::new();
    for u in units {
        let r = &u.variant.recipe;
        if filter.is_some_and(|f| f != r.package.name) || !seen.insert(r.package.name.clone()) {
            continue;
        }
        let manifest = from_manifest
            .get(&r.package.name)
            .cloned()
            .unwrap_or_default();
        let label = |name: &str| format!("{}//{name}", r.package.name);
        println!("{}", style(&r.package.name).bold());
        println!("  {}", style("build steps").dim());
        let has_in_build_tests = r
            .tests
            .iter()
            .any(|t| matches!(t, blaze::recipe::Test::InBuild { .. }));
        for (name, _) in blaze::recipe::BUILD_STEPS {
            let Some(desc) = blaze::explain::default_step(&u.variant, name) else {
                continue;
            };
            let steps_generator = r.build.generator == blaze::recipe::Generator::Steps;
            if (steps_generator && !r.tasks.contains_key(*name))
                || (*name == "in-build-tests"
                    && !has_in_build_tests
                    && !r.tasks.contains_key(*name))
            {
                continue;
            }
            let desc = if steps_generator {
                "your step".to_string()
            } else if desc.chars().count() > 72 {
                format!("{}…", desc.chars().take(71).collect::<String>())
            } else {
                desc
            };
            let over = match r.tasks.get(*name) {
                Some(_) if manifest.contains(*name) => {
                    format!(" {}", style("(overridden in pixi.toml)").yellow())
                }
                Some(_) => format!(" {}", style("(overridden by the recipe)").yellow()),
                None => String::new(),
            };
            println!(
                "    {:<28} {}{over}",
                style(label(name)).cyan(),
                style(desc).dim()
            );
        }
        for (name, def) in r
            .tasks
            .iter()
            .filter(|(n, d)| !blaze::recipe::is_build_step(n) && !d.task().required_by.is_empty())
        {
            println!(
                "    {:<28} {}",
                style(label(name)).cyan(),
                style(format!("added step, before {:?}", def.task().required_by)).dim()
            );
        }
        println!("  {}", style("targets").dim());
        for (name, desc) in blaze::tasks::BUILTIN_TARGETS {
            println!(
                "    {:<28} {}",
                style(label(name)).cyan(),
                style(desc).dim()
            );
        }
        let tasks: Vec<_> = r
            .tasks
            .iter()
            .filter(|(n, d)| !blaze::recipe::is_build_step(n) && d.task().required_by.is_empty())
            .collect();
        if !tasks.is_empty() {
            println!("  {}", style("tasks").dim());
        }
        for (name, def) in tasks {
            let t = def.task();
            let mut desc = t.description.clone().unwrap_or_default();
            if t.cmd.is_none() && !t.depends_on.is_empty() {
                let deps: Vec<&str> = t.depends_on.iter().map(|d| d.name()).collect();
                desc = format!("{desc} [{}]", deps.join(", "));
            }
            let src = if manifest.contains(name.as_str()) {
                format!(" {}", style("(pixi.toml)").yellow())
            } else {
                String::new()
            };
            println!("    {:<28} {desc}{src}", style(label(name)).green());
        }
    }
}

/// Tasks from pixi.toml that name an environment run in that workspace
/// environment (it replaces a backend environment of the same name).
fn workspace_environments(
    workspace: &Workspace,
    variant: &mut Variant,
    manifest_tasks: &[String],
    platform: Platform,
) -> miette::Result<()> {
    let recipe = &mut variant.recipe;
    for name in manifest_tasks {
        let Some(def) = recipe.tasks.get(name) else {
            continue;
        };
        if let Some(env) = def.task().environment {
            let specs = environment_specs(workspace, &env, platform)?;
            recipe.environments.insert(
                env,
                EnvironmentDef {
                    dependencies: blaze::recipe::Dependencies::List(specs),
                },
            );
        }
    }
    Ok(())
}
