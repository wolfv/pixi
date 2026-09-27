//! `pixi run <package>//<task>` and `pixi run //<task>` (experimental).
//!
//! Package tasks live in `[package.tasks]` of a package's manifest. Every
//! package also has the built-in targets `configure`, `build`, `test`,
//! `package` and `all`. The packages of the workspace are loaded as one graph
//! by pixi's embedded build engine (rattler-blaze): their build backends emit
//! recipes, source dependencies between them become edges, and every compile,
//! link, test and cacheable task is a cached action, so `pixi run //test`
//! builds and tests the whole workspace in parallel.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    str::FromStr,
};

use miette::{Context, IntoDiagnostic};
use pixi_blaze::blaze::{
    self, Target, Unit,
    recipe::{EnvironmentDef, TaskDef, Variant},
};
use pixi_build_types::procedures::conda_recipe::CondaRecipeParams;
use pixi_command_dispatcher::InstantiateBackendKey;
use pixi_core::Workspace;
use pixi_manifest::FeaturesExt;
use pixi_spec::SourceAnchor;
use rattler_conda_types::Platform;

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
        let dir = path.parent().unwrap().to_path_buf();
        if table.get("package").is_none() {
            continue;
        }
        out.push(PackageDir {
            dir,
            manifest: path.to_path_buf(),
        });
    }
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    Ok(out)
}

/// `[package.tasks]`, read into blaze's task model (it mirrors pixi's task
/// schema: `cmd`, `depends-on`, `inputs`, `outputs`, `env`, `cwd`, ...).
fn package_tasks(manifest: &Path) -> miette::Result<BTreeMap<String, TaskDef>> {
    let text = fs_err::read_to_string(manifest).into_diagnostic()?;
    let doc: toml::Table = toml::from_str(&text).into_diagnostic()?;
    let Some(tasks) = doc.get("package").and_then(|p| p.get("tasks")) else {
        return Ok(BTreeMap::new());
    };
    tasks
        .clone()
        .try_into()
        .into_diagnostic()
        .with_context(|| format!("invalid [package.tasks] in {}", manifest.display()))
}

/// The conda dependencies of a workspace environment, as match specs (used by
/// package tasks with `environment = "<name>"`).
fn environment_specs(
    workspace: &Workspace,
    name: &str,
    platform: Platform,
) -> miette::Result<Vec<String>> {
    let env = workspace
        .environment(name)
        .ok_or_else(|| miette::miette!("unknown environment `{name}`"))?;
    let pixi_platform = pixi_manifest::PixiPlatform::from_subdir(platform);
    let mut specs = Vec::new();
    for (pkg, spec) in env.combined_dependencies(Some(&pixi_platform)).into_specs() {
        let channel_config = workspace.channel_config();
        match spec.try_into_nameless_match_spec(&channel_config) {
            Ok(Some(nameless)) => specs.push(format!("{} {}", pkg.as_normalized(), nameless)),
            _ => specs.push(pkg.as_normalized().to_string()),
        }
    }
    Ok(specs)
}

pub async fn execute(workspace: &Workspace, args: &[String]) -> miette::Result<()> {
    // `pixi run //` or `pixi run pkg//`: list what can be run.
    let listing: Option<Option<String>> = match args {
        [one] if one.ends_with("//") => {
            let pkg = one.trim_end_matches("//");
            Some((!pkg.is_empty()).then(|| pkg.to_string()))
        }
        _ => None,
    };
    let targets: Vec<Target> = if listing.is_some() {
        Vec::new()
    } else {
        args.iter()
            .map(|a| Target::from_str(a))
            .collect::<Result<_, _>>()
            .map_err(|e| miette::miette!("{e:#}"))?
    };

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

    let mut units = Vec::new();
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
        let recipe = backend
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
                    "the build backend of {} ({}) does not implement `conda/recipe`; package tasks need a recipe backend",
                    pkg.manifest.display(),
                    backend.identifier()
                )
            })?
            .into_diagnostic()?;
        let tasks = package_tasks(&pkg.manifest)?;
        for mut variant in pixi_blaze::variants(
            &recipe,
            platform,
            &variants_config.variant_files,
            &variant_configuration,
        )
        .map_err(|e| miette::miette!("{e:#}"))?
        {
            add_tasks(workspace, &mut variant, &tasks, platform)?;
            units.push(Unit::from(variant));
        }
    }
    if units.is_empty() {
        miette::bail!("no packages found below {}", root.display());
    }
    if let Some(filter) = listing {
        list_tasks(&units, filter.as_deref());
        return Ok(());
    }

    let session = pixi_blaze::session(channels)
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
    let outcome = session
        .run(units, &targets)
        .await
        .map_err(|e| miette::miette!("{e:#}"))?;
    blaze::session::print_outcome(&outcome, session.output_dir());
    Ok(())
}

fn list_tasks(units: &[Unit], filter: Option<&str>) {
    use console::style;
    let mut seen = std::collections::BTreeSet::new();
    for u in units {
        let r = &u.variant.recipe;
        if filter.is_some_and(|f| f != r.package.name) || !seen.insert(r.package.name.clone()) {
            continue;
        }
        println!("{}", style(&r.package.name).bold());
        for (name, desc) in blaze::tasks::BUILTIN_TARGETS {
            println!(
                "  {:<22} {}",
                style(format!("{}//{name}", r.package.name)).cyan(),
                style(desc).dim()
            );
        }
        for (name, def) in &r.tasks {
            let t = def.task();
            let mut desc = t.description.clone().unwrap_or_default();
            if t.cmd.is_none() && !t.depends_on.is_empty() {
                let deps: Vec<&str> = t.depends_on.iter().map(|d| d.name()).collect();
                desc = format!("{desc} [{}]", deps.join(", "));
            }
            println!(
                "  {:<22} {desc}",
                style(format!("{}//{name}", r.package.name)).green()
            );
        }
    }
}

fn add_tasks(
    workspace: &Workspace,
    variant: &mut Variant,
    tasks: &BTreeMap<String, TaskDef>,
    platform: Platform,
) -> miette::Result<()> {
    let recipe = &mut variant.recipe;
    // Tasks from pixi.toml win over the backend's defaults of the same name,
    // and their environments are the workspace's.
    for (name, def) in tasks {
        if let Some(env) = def.task().environment {
            let specs = environment_specs(workspace, &env, platform)?;
            recipe.environments.insert(
                env,
                EnvironmentDef {
                    dependencies: blaze::recipe::Dependencies::List(specs),
                },
            );
        }
        recipe.tasks.insert(name.clone(), def.clone());
    }
    Ok(())
}
