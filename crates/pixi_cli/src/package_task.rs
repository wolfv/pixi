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
use pixi_command_dispatcher::{CommandDispatcher, InstantiateBackendKey};
use pixi_core::{Workspace, lock_file::LockFileDerivedData};
use pixi_manifest::FeaturesExt;
use pixi_record::{LockFileResolver, UnresolvedPixiRecord};
use pixi_spec::SourceAnchor;
use rattler_conda_types::{Platform, RepoDataRecord};

/// A package's source: a directory with a pixi manifest (`pixi.toml`, or a
/// `pyproject.toml` with `[tool.pixi.package]`), or a ROS `package.xml`.
struct PackageSource {
    /// What the build backend is instantiated for (the directory, or the
    /// package.xml).
    source: PathBuf,
    dir: PathBuf,
    /// The pixi manifest, if the package has one.
    manifest: Option<PathBuf>,
}

impl PackageSource {
    fn at(path: &Path) -> Option<Self> {
        let path = dunce::canonicalize(path).ok()?;
        if path.file_name().is_some_and(|n| n == "package.xml") {
            let dir = path.parent()?.to_path_buf();
            let manifest = pixi_manifest_in(&dir);
            return Some(PackageSource {
                source: path,
                dir,
                manifest,
            });
        }
        let dir = if path.is_file() {
            path.parent()?.to_path_buf()
        } else {
            path
        };
        let manifest = pixi_manifest_in(&dir);
        let source = if manifest.is_none() && dir.join("package.xml").is_file() {
            dir.join("package.xml")
        } else {
            dir.clone()
        };
        Some(PackageSource {
            source,
            dir,
            manifest,
        })
    }
}

/// The package manifest of a directory: `pixi.toml` with `[package]`, or a
/// `pyproject.toml` with `[tool.pixi.package]`.
fn pixi_manifest_in(dir: &Path) -> Option<PathBuf> {
    let table = |p: &Path| -> Option<toml_edit::DocumentMut> {
        fs_err::read_to_string(p).ok()?.parse().ok()
    };
    let pixi = dir.join("pixi.toml");
    if table(&pixi).is_some_and(|t| t.get("package").is_some()) {
        return Some(pixi);
    }
    let pyproject = dir.join("pyproject.toml");
    if table(&pyproject).is_some_and(|t| {
        t.get("tool")
            .and_then(|t| t.get("pixi"))
            .and_then(|t| t.get("package"))
            .is_some()
    }) {
        return Some(pyproject);
    }
    None
}

/// Path dependencies in a package manifest's dependency tables (also in
/// `[package.target.*]`), resolved against its directory.
fn manifest_path_dependencies(manifest: &Path) -> Vec<PathBuf> {
    let Some(doc) = fs_err::read_to_string(manifest)
        .ok()
        .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
    else {
        return Vec::new();
    };
    let package = if manifest.file_name().is_some_and(|n| n == "pyproject.toml") {
        doc.get("tool")
            .and_then(|t| t.get("pixi"))
            .and_then(|t| t.get("package"))
    } else {
        doc.get("package")
    };
    let Some(package) = package.and_then(|p| p.as_table_like()) else {
        return Vec::new();
    };
    let mut tables = vec![package];
    if let Some(targets) = package.get("target").and_then(|t| t.as_table_like()) {
        tables.extend(targets.iter().filter_map(|(_, t)| t.as_table_like()));
    }
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for table in tables {
        for key in [
            "build-dependencies",
            "host-dependencies",
            "run-dependencies",
        ] {
            let Some(deps) = table.get(key).and_then(|d| d.as_table_like()) else {
                continue;
            };
            for (_, spec) in deps.iter() {
                if let Some(path) = spec
                    .as_table_like()
                    .and_then(|t| t.get("path"))
                    .and_then(|p| p.as_str())
                {
                    out.push(dir.join(path));
                }
            }
        }
    }
    out
}

/// The workspace's packages: its own package, the path sources its
/// environments depend on, and, transitively, theirs (from their manifests
/// and from their backends' `source_dependencies`, e.g. ROS siblings).
fn workspace_sources(workspace: &Workspace, platform: Platform) -> Vec<PathBuf> {
    let root = workspace.root();
    let mut out = Vec::new();
    // The workspace's own package, and the one pixi was started in.
    if pixi_manifest_in(root).is_some() {
        out.push(root.to_path_buf());
    }
    if let Some(dir) = workspace
        .package
        .as_ref()
        .and_then(|p| p.provenance.path.parent())
    {
        out.push(dir.to_path_buf());
    }
    let pixi_platform = pixi_manifest::PixiPlatform::from_subdir(platform);
    for env in workspace.environments() {
        for (_, spec) in env.combined_dependencies(Some(&pixi_platform)).into_specs() {
            if let Some(p) = spec.as_path_source() {
                out.push(root.join(p.path.as_str()));
            }
        }
    }
    out
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

/// Build/host records from the lock file: for a package variant, the source
/// record of that name whose environments are for `platform` and whose
/// variant is the same. Source packages inside those environments are left
/// out (blaze builds them in the same run).
struct Locked<'a> {
    lock: &'a rattler_lock::LockFile,
    resolver: &'a LockFileResolver,
}

impl Locked<'_> {
    /// The locked packages of workspace environment `name` for `platform`,
    /// if it's locked and has no source packages (those would need building).
    fn environment(&self, name: &str, platform: Platform) -> Option<Vec<RepoDataRecord>> {
        let lock_platform = self.lock.platform(platform.as_str())?;
        let mut out = Vec::new();
        for pkg in self.lock.environment(name)?.packages(lock_platform)? {
            match self.resolver.get_for_package(pkg)? {
                UnresolvedPixiRecord::Binary(b) => out.push(b.as_ref().clone()),
                UnresolvedPixiRecord::Source(_) => return None,
            }
        }
        Some(out)
    }

    /// `only_variant`: the package has no other variant here.
    fn envs(&self, v: &Variant, platform: Platform, only_variant: bool) -> Option<LockedEnvs> {
        let binaries = |records: &[UnresolvedPixiRecord]| -> Vec<RepoDataRecord> {
            records
                .iter()
                .filter_map(|r| match r {
                    UnresolvedPixiRecord::Binary(b) => Some(b.as_ref().clone()),
                    UnresolvedPixiRecord::Source(_) => None,
                })
                .collect()
        };
        // Every record is for `platform` (or noarch), and at least one is for
        // `platform` itself: noarch packages alone say nothing.
        let on_platform = |records: &[RepoDataRecord]| {
            records.is_empty()
                || (records.iter().all(|r| {
                    r.package_record.subdir == platform.as_str()
                        || r.package_record.subdir == "noarch"
                }) && records
                    .iter()
                    .any(|r| r.package_record.subdir == platform.as_str()))
        };
        for pkg in self.lock.packages() {
            let Some(UnresolvedPixiRecord::Source(src)) = self.resolver.get_for_package(pkg) else {
                continue;
            };
            if src.name().as_normalized() != v.recipe.package.name {
                continue;
            }
            // This variant: another variant's environments (another Python,
            // say) would build the wrong thing. The lock records the keys
            // that tell a package's variants apart; with none recorded, the
            // record is only good for a package with one variant.
            let mut shared = v
                .used
                .iter()
                .filter_map(|(k, val)| src.variants.get(k).map(|x| x.to_string() == *val))
                .peekable();
            let same_variant = if shared.peek().is_none() {
                only_variant
            } else {
                shared.all(|eq| eq)
            };
            if !same_variant {
                continue;
            }
            let build = binaries(&src.build_packages);
            let host = binaries(&src.host_packages);
            if on_platform(&build) && on_platform(&host) {
                return Some(LockedEnvs { build, host });
            }
        }
        None
    }
}

/// The workspace's packages as blaze units, with what `pixi run` reports
/// about them.
struct Packages {
    units: Vec<Unit>,
    /// Per package: names contributed by its pixi manifest (listing, explain).
    from_manifest: BTreeMap<String, BTreeSet<String>>,
    /// Packages built without lock-file environments.
    unlocked: Vec<String>,
    /// Sources whose backend isn't a recipe backend (left out).
    skipped: Vec<(PathBuf, String)>,
    /// Directory name -> package name (`pendulum_msgs//build` for
    /// `ros-humble-pendulum-msgs`); `None` when two packages share one.
    aliases: BTreeMap<String, Option<String>>,
    channels: Vec<String>,
}

impl Packages {
    async fn discover(
        workspace: &Workspace,
        dispatcher: &CommandDispatcher,
        locked: Option<Locked<'_>>,
    ) -> miette::Result<Self> {
        let root = workspace.root().to_path_buf();
        let platform = workspace
            .default_environment()
            .best_declared_platform()
            .map(|p| p.subdir())
            .unwrap_or_else(Platform::current);
        let channel_config = workspace.channel_config();
        let channel_urls = workspace
            .default_environment()
            .channel_urls(&channel_config)
            .into_diagnostic()?;
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
        let scratch = root.join(".pixi").join("blaze");

        let mut out = Packages {
            units: Vec::new(),
            from_manifest: BTreeMap::new(),
            unlocked: Vec::new(),
            skipped: Vec::new(),
            aliases: BTreeMap::new(),
            channels: channel_urls.iter().map(|c| c.to_string()).collect(),
        };
        let mut queue = workspace_sources(workspace, platform);
        let mut seen = BTreeSet::new();
        while let Some(path) = queue.pop() {
            let Some(pkg) = PackageSource::at(&path) else {
                continue;
            };
            if !seen.insert(pkg.source.clone()) {
                continue;
            }
            let build_dir = pixi_path::AbsPathBuf::new(pkg.dir.clone())
                .map_err(|_| miette::miette!("{} is not absolute", pkg.dir.display()))?
                .into_assume_dir();
            let backend = dispatcher
                .engine()
                .compute(&InstantiateBackendKey::new(
                    &pkg.source,
                    None,
                    SourceAnchor::Workspace,
                    build_dir,
                    None,
                ))
                .await
                .map_err(|e| miette::miette!("{e}"))
                .and_then(|r| r.map_err(|e| miette::miette!("{e}")))
                .with_context(|| {
                    format!(
                        "instantiating the build backend of {}",
                        pkg.source.display()
                    )
                })?;
            let backend = backend.lock().await;
            let Some(recipe) = backend
                .conda_recipe(CondaRecipeParams {
                    channels: channel_urls.clone(),
                    host_platform: platform,
                    build_platform: platform,
                    variant_configuration: Some(variant_configuration.clone()),
                    variant_files: Some(variants_config.variant_files.clone()),
                    work_directory: scratch.join("backends"),
                })
                .await
            else {
                out.skipped
                    .push((pkg.source.clone(), backend.identifier().to_string()));
                continue;
            };
            let mut recipe = recipe.into_diagnostic()?;
            // Other packages this one builds against.
            for rel in recipe.source_dependencies.values() {
                queue.push(pkg.dir.join(rel));
            }
            let manifest = match &pkg.manifest {
                Some(m) => {
                    queue.extend(manifest_path_dependencies(m));
                    ManifestTasks::read(m).map_err(|e| miette::miette!("{e:#}"))?
                }
                None => ManifestTasks::default(),
            };
            pixi_blaze::merge_manifest(&mut recipe, &manifest, true)
                .map_err(|e| miette::miette!("{e:#}"))?;
            let variants = pixi_blaze::variants(
                &recipe,
                platform,
                &variants_config.variant_files,
                &variant_configuration,
            )
            .map_err(|e| miette::miette!("{e:#}"))
            .with_context(|| format!("in {}", pkg.source.display()))?;
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
            let only_variant = variants.len() == 1;
            for mut variant in variants {
                if let Some(d) = pkg.dir.file_name().and_then(|d| d.to_str()) {
                    let name = &variant.recipe.package.name;
                    out.aliases
                        .entry(d.to_string())
                        .and_modify(|a| {
                            if a.as_ref() != Some(name) {
                                *a = None
                            }
                        })
                        .or_insert_with(|| Some(name.clone()));
                }
                let env_records = workspace_environments(
                    workspace,
                    &mut variant,
                    &manifest.task_names(),
                    platform,
                    locked.as_ref(),
                )?;
                out.from_manifest
                    .insert(variant.recipe.package.name.clone(), names.clone());
                let envs = locked
                    .as_ref()
                    .and_then(|l| l.envs(&variant, platform, only_variant));
                if locked.is_some() && envs.is_none() {
                    out.unlocked.push(variant.recipe.package.name.clone());
                }
                let mut unit = Unit::from(variant);
                unit.locked = envs;
                unit.env_records = env_records;
                out.units.push(unit);
            }
        }
        if out.units.is_empty() {
            let mut msg = format!(
                "the workspace at {} has no packages with a recipe backend",
                root.display()
            );
            for (source, backend) in &out.skipped {
                msg.push_str(&format!("\n  {} uses {backend}", source.display()));
            }
            miette::bail!(
                help = "package targets need a source package built by a `pixi-build-blaze-*` \
                        backend, that the workspace depends on (`{ path = \"...\" }`)",
                "{msg}"
            );
        }
        out.units.sort_by(|a, b| {
            a.variant
                .recipe
                .package
                .name
                .cmp(&b.variant.recipe.package.name)
        });
        Ok(out)
    }

    /// `pkg` as a package name: a package name, or a directory name.
    fn resolve(&self, pkg: &str) -> miette::Result<String> {
        if self
            .units
            .iter()
            .any(|u| u.variant.recipe.package.name == pkg)
        {
            return Ok(pkg.to_string());
        }
        match self.aliases.get(pkg) {
            Some(Some(name)) => Ok(name.clone()),
            Some(None) => {
                miette::bail!("`{pkg}` is the directory of several packages; use the package name")
            }
            None => {
                let mut msg = format!("the workspace has no package `{pkg}`");
                if !self.skipped.is_empty() {
                    msg.push_str(" (packages without a recipe backend aren't package targets:");
                    for (source, backend) in &self.skipped {
                        msg.push_str(&format!(" {} uses {backend};", source.display()));
                    }
                    msg.push(')');
                }
                let known: BTreeSet<&str> = self
                    .units
                    .iter()
                    .map(|u| u.variant.recipe.package.name.as_str())
                    .collect();
                miette::bail!(
                    help = format!(
                        "packages: {}",
                        known.into_iter().collect::<Vec<_>>().join(", ")
                    ),
                    "{msg}"
                )
            }
        }
    }

    fn targets(&self, args: &[String]) -> miette::Result<Vec<Target>> {
        args.iter()
            .map(|a| {
                let mut t = Target::from_str(a).map_err(|e| miette::miette!("{e:#}"))?;
                t.package = t.package.map(|p| self.resolve(&p)).transpose()?;
                Ok(t)
            })
            .collect()
    }
}

/// `pixi run pkg//build //test`: run package targets as one build graph,
/// with build/host environments from the (updated) lock file.
pub async fn run_targets(
    lock_file: &LockFileDerivedData<'_>,
    args: &[String],
) -> miette::Result<()> {
    let workspace = lock_file.workspace;
    // blaze installs environments with rattler, which uses the global rayon
    // pool: let uv claim it first, as pixi does before installing (uv
    // insists on configuring the pool itself).
    uv_configuration::initialize_rayon_once();
    let runtime = workspace
        .blaze_runtime()?
        .ok_or_else(|| miette::miette!("package targets need the `pixi-build-blaze` preview"))?;
    let resolver = lock_file.resolver()?;
    let locked = Locked {
        lock: lock_file.as_lock_file(),
        resolver: &resolver,
    };
    let packages =
        Packages::discover(workspace, &lock_file.command_dispatcher, Some(locked)).await?;
    let targets = packages.targets(args)?;
    let session = runtime
        .session(packages.channels.clone())
        .await
        .map_err(|e| miette::miette!("{e:#}"))?;
    let names: Vec<String> = packages
        .units
        .iter()
        .map(|u| format!("{} [{}]", u.variant.tag(), u.variant.describe()))
        .collect();
    eprintln!(
        "{}{} {} over {} package variant(s): {}",
        console::Emoji("✨ ", ""),
        console::style("Package targets").bold(),
        console::style(args.join(" ")).green().bold(),
        packages.units.len(),
        names.join(", ")
    );
    if !packages.unlocked.is_empty() {
        eprintln!(
            "  {} not in the lock file for this variant, so their environments are solved: {}",
            console::style("note:").yellow(),
            packages.unlocked.join(", ")
        );
    }
    // Every step (each compile, link, test, task) is a key of pixi's compute
    // engine, which runs them.
    let prepared = session
        .prepare(packages.units, &targets, &Default::default())
        .map_err(|e| miette::miette!("{e:#}"))?;
    let result = lock_file
        .command_dispatcher
        .engine()
        .with_ctx(async |ctx| pixi_blaze::engine::execute(ctx, &prepared).await)
        .await
        .map_err(|e| miette::miette!("{e}"))?;
    let outcome = session
        .finish_external(prepared, result)
        .map_err(|e| miette::miette!("{e:#}"))?;
    blaze::session::print_outcome(&outcome, session.output_dir());
    Ok(())
}

/// `pixi run //` or `pixi run pkg//`: list what can be run.
pub async fn list(workspace: &Workspace, arg: &str) -> miette::Result<()> {
    let dispatcher = workspace.command_dispatcher_builder(None)?.finish();
    let packages = Packages::discover(workspace, &dispatcher, None).await?;
    let filter = arg.trim_end_matches("//");
    let filter = (!filter.is_empty())
        .then(|| packages.resolve(filter))
        .transpose()?;
    list_tasks(&packages.units, filter.as_deref(), &packages.from_manifest);
    Ok(())
}

/// `pixi task list`: the package targets after the workspace's tasks.
pub async fn print_task_list(workspace: &Workspace) -> miette::Result<()> {
    let dispatcher = workspace.command_dispatcher_builder(None)?.finish();
    let packages = Packages::discover(workspace, &dispatcher, None).await?;
    use console::style;
    println!(
        "\n{} {}",
        style("Package targets").bold(),
        style("(run one with `pixi run <package>//<name>`; `pixi run <package>//` explains)").dim()
    );
    let mut seen = BTreeSet::new();
    let width = packages
        .units
        .iter()
        .map(|u| u.variant.recipe.package.name.len() + 2)
        .max()
        .unwrap_or(0);
    for u in &packages.units {
        let r = &u.variant.recipe;
        if !seen.insert(r.package.name.clone()) {
            continue;
        }
        let mut names: Vec<String> = blaze::tasks::BUILTIN_TARGETS
            .iter()
            .map(|(n, _)| n.to_string())
            .collect();
        names.extend(
            r.tasks
                .iter()
                .filter(|(_, d)| d.task().required_by.is_empty())
                .filter(|(n, _)| !blaze::recipe::is_build_step(n))
                .map(|(n, _)| n.clone()),
        );
        println!(
            "  {}  {}",
            style(format!("{:<width$}", format!("{}//", r.package.name)))
                .cyan()
                .bold(),
            names.join(", ")
        );
    }
    Ok(())
}

/// The workspace's package targets for shell completion, without starting
/// any build backend: the built-in targets and `[package.tasks]` of every
/// package (by manifest name, or directory name for a package.xml).
pub fn completion_names(workspace: &Workspace) -> Vec<String> {
    let platform = Platform::current();
    let mut queue = workspace_sources(workspace, platform);
    let mut seen = BTreeSet::new();
    let mut out = BTreeSet::new();
    while let Some(path) = queue.pop() {
        let Some(pkg) = PackageSource::at(&path) else {
            continue;
        };
        if !seen.insert(pkg.source.clone()) {
            continue;
        }
        let mut name = pkg
            .dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from);
        let mut tasks = Vec::new();
        if let Some(manifest) = &pkg.manifest {
            queue.extend(manifest_path_dependencies(manifest));
            if let Some(doc) = fs_err::read_to_string(manifest)
                .ok()
                .and_then(|t| t.parse::<toml_edit::DocumentMut>().ok())
            {
                let package = if manifest.file_name().is_some_and(|n| n == "pyproject.toml") {
                    doc.get("tool")
                        .and_then(|t| t.get("pixi"))
                        .and_then(|t| t.get("package"))
                        .cloned()
                } else {
                    doc.get("package").cloned()
                };
                if let Some(n) = package
                    .as_ref()
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                {
                    name = Some(n.to_string());
                }
                if let Some(t) = package
                    .as_ref()
                    .and_then(|p| p.get("tasks"))
                    .and_then(|t| t.as_table_like())
                {
                    tasks.extend(t.iter().map(|(k, _)| k.to_string()));
                }
            }
        }
        let Some(name) = name else { continue };
        for (target, _) in blaze::tasks::BUILTIN_TARGETS {
            out.insert(format!("{name}//{target}"));
        }
        for task in tasks {
            out.insert(format!("{name}//{task}"));
        }
    }
    out.into_iter().collect()
}

/// `pixi task explain <pkg//name>`
pub async fn explain(workspace: &Workspace, target: &str) -> miette::Result<()> {
    if !workspace.package_targets_enabled() {
        miette::bail!(
            help = "add it to your workspace: `preview = [\"pixi-build\", \"pixi-build-blaze\"]`",
            "package targets (`pkg//task`) need the `pixi-build-blaze` preview"
        );
    }
    let dispatcher = workspace.command_dispatcher_builder(None)?.finish();
    let packages = Packages::discover(workspace, &dispatcher, None).await?;
    let t = packages
        .targets(&[target.to_string()])?
        .pop()
        .expect("one target");
    let mut seen = BTreeSet::new();
    for u in &packages.units {
        let r = &u.variant.recipe;
        if t.package.as_ref().is_some_and(|p| *p != r.package.name)
            || !seen.insert(r.package.name.clone())
        {
            continue;
        }
        let names = packages
            .from_manifest
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
/// environment (it replaces a backend environment of the same name): with its
/// locked packages where the lock file has them, else solved from its specs.
fn workspace_environments(
    workspace: &Workspace,
    variant: &mut Variant,
    manifest_tasks: &[String],
    platform: Platform,
    locked: Option<&Locked<'_>>,
) -> miette::Result<BTreeMap<String, Vec<RepoDataRecord>>> {
    let mut records = BTreeMap::new();
    let recipe = &mut variant.recipe;
    for name in manifest_tasks {
        let Some(def) = recipe.tasks.get(name) else {
            continue;
        };
        if let Some(env) = def.task().environment {
            let specs = environment_specs(workspace, &env, platform)?;
            if let Some(r) = locked.and_then(|l| l.environment(&env, platform)) {
                records.insert(env.clone(), r);
            }
            recipe.environments.insert(
                env,
                EnvironmentDef {
                    dependencies: blaze::recipe::Dependencies::List(specs),
                },
            );
        }
    }
    Ok(records)
}
