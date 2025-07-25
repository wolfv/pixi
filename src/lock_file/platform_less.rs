use super::{IoConcurrencyLimit, LockFileDerivedData};
use crate::{
    Workspace,
    environment::{CondaPrefixUpdated, CondaPrefixUpdater, PythonStatus},
    prefix::Prefix,
    workspace::grouped_environment::GroupedEnvironment,
};
use async_once_cell::OnceCell;
use dashmap::DashMap;
use indicatif::ProgressBar;
use miette::IntoDiagnostic;
use pixi_command_dispatcher::{BuildEnvironment, PixiEnvironmentSpec};
use pixi_glob::GlobHashCache;
use pixi_manifest::{EnvironmentName, FeaturesExt};
use pixi_record::PixiRecord;
use pixi_spec::PixiSpec;
use pixi_spec_containers::DependencyMap;
use rattler_conda_types::PrefixRecord;
use rattler_conda_types::{GenericVirtualPackage, MatchSpec, Matches, PackageName};
use rattler_lock::LockFile;
use std::sync::Arc;

impl Workspace {
    /// In platform-less mode, solve and install packages directly without a lock file
    pub async fn solve_and_install_platform_less(&self) -> miette::Result<LockFileDerivedData<'_>> {
        let glob_hash_cache = GlobHashCache::default();

        // Construct a command dispatcher
        let multi_progress = pixi_progress::global_multi_progress();
        let anchor_pb = multi_progress.add(ProgressBar::hidden());
        let command_dispatcher = self
            .command_dispatcher_builder()?
            .with_reporter(crate::reporters::TopLevelProgress::new(
                pixi_progress::global_multi_progress(),
                anchor_pb,
            ))
            .finish();

        let package_cache = command_dispatcher.package_cache().clone();
        let mut lock_file = LockFile::default();
        let updated_conda_prefixes: DashMap<
            EnvironmentName,
            Arc<OnceCell<(Prefix, PythonStatus)>>,
        > = DashMap::new();
        let updated_pypi_prefixes: DashMap<EnvironmentName, Arc<OnceCell<Prefix>>> = DashMap::new();

        tracing::info!("Platform-less mode: solving and installing without lock file");

        // Process each environment
        for environment in self.environments() {
            let platform = environment.best_platform();
            let env_name = environment.name();

            tracing::info!(
                "processing environment '{}' for platform {}",
                env_name,
                platform
            );

            // Get virtual packages for the platform
            let virtual_packages = environment.virtual_packages(platform);

            // Get the dependencies
            let conda_deps = environment.combined_dependencies(Some(platform));
            let pypi_deps = environment.pypi_dependencies(Some(platform));

            tracing::debug!(
                "Processing environment '{}' with {} conda deps, {} pypi deps",
                env_name,
                conda_deps.iter().count(),
                pypi_deps.iter().count()
            );

            // Convert conda dependencies to PixiSpec
            let mut pixi_dependencies = DependencyMap::default();
            for (name, specs) in conda_deps.iter() {
                for spec in specs {
                    pixi_dependencies.insert(name.clone(), PixiSpec::from(spec.clone()));
                }
            }

            if !pixi_dependencies.is_empty() {
                // Check if environment already exists and satisfies requirements
                let env_dir = environment.dir();
                let prefix = Prefix::new(&env_dir);
                let mut needs_update = true;

                if env_dir.exists() {
                    if let Ok(installed_packages) = prefix.find_installed_packages() {
                        // Check if installed packages satisfy the current dependencies
                        needs_update = !dependencies_satisfied(
                            &pixi_dependencies,
                            &installed_packages,
                            &self.channel_config(),
                        );
                        if !needs_update {
                            tracing::info!(
                                "Environment '{}' already satisfies requirements, skipping solve/install",
                                env_name
                            );
                        }
                    }
                }

                // Read the installed packages to build lock file data regardless
                if let Ok(installed_packages) = prefix.find_installed_packages() {
                    let mut builder = LockFile::builder();

                    // Set channels for the environment
                    let channels = environment.channels();
                    let channel_urls: Vec<String> = channels
                        .iter()
                        .map(|c| c.clone().clone().into_base_url(&self.channel_config()))
                        .collect::<Result<Vec<_>, _>>()
                        .into_diagnostic()?
                        .iter()
                        .map(|url| url.to_string())
                        .collect();

                    builder.set_channels(env_name.as_str(), channel_urls);

                    // Add the installed packages to the lock file
                    for record in installed_packages {
                        let pixi_record = PixiRecord::Binary(record.repodata_record);
                        builder.add_conda_package(env_name.as_str(), platform, pixi_record.into());
                    }

                    lock_file = builder.finish();
                }

                if needs_update {
                    tracing::info!("solving dependencies for environment '{}'", env_name);

                    // Get channels
                    let channels = environment.channels();
                    let channel_urls = channels
                        .iter()
                        .map(|c| c.clone().clone().into_base_url(&self.channel_config()))
                        .collect::<Result<Vec<_>, _>>()
                        .into_diagnostic()?;

                    // Build the PixiEnvironmentSpec
                    let pixi_env_spec = PixiEnvironmentSpec {
                        name: Some(env_name.to_string()),
                        dependencies: pixi_dependencies,
                        constraints: Default::default(),
                        installed: vec![],
                        build_environment: BuildEnvironment::simple(
                            platform,
                            virtual_packages
                                .clone()
                                .into_iter()
                                .map(GenericVirtualPackage::from)
                                .collect(),
                        ),
                        channels: channel_urls,
                        strategy: environment.solve_strategy(),
                        channel_priority: environment
                            .channel_priority()?
                            .unwrap_or_default()
                            .into(),
                        exclude_newer: environment.exclude_newer(),
                        channel_config: self.channel_config().clone(),
                        variants: Some(self.variants(platform)),
                        enabled_protocols: Default::default(),
                    };

                    // Solve the environment
                    let solved_records = command_dispatcher
                        .solve_pixi_environment(pixi_env_spec)
                        .await?;

                    tracing::info!(
                        "solved to {} packages for environment '{}'",
                        solved_records.len(),
                        env_name
                    );

                    // Install packages using CondaPrefixUpdater
                    let env_dir = environment.dir();
                    let _prefix = Prefix::new(&env_dir);
                    let group = GroupedEnvironment::Environment(environment.clone());

                    // Create the conda prefix updater
                    let conda_updater = CondaPrefixUpdater::builder(
                        group,
                        platform,
                        virtual_packages
                            .into_iter()
                            .map(GenericVirtualPackage::from)
                            .collect(),
                        command_dispatcher.clone(),
                    )
                    .finish()?;

                    // Update the prefix
                    let CondaPrefixUpdated {
                        prefix,
                        python_status,
                        ..
                    } = conda_updater.update(solved_records, None).await?;

                    // Store the updated prefix
                    let once_cell = Arc::new(OnceCell::new());
                    once_cell
                        .get_or_init(async { (prefix.clone(), *python_status.clone()) })
                        .await;
                    updated_conda_prefixes.insert(env_name.clone(), once_cell);

                    // Read the installed packages from the prefix to build lock file data
                    let env_dir = environment.dir();
                    let prefix = Prefix::new(&env_dir);

                    if let Ok(installed_packages) = prefix.find_installed_packages() {
                        let mut builder = LockFile::builder();

                        // Set channels for the environment
                        let channels = environment.channels();
                        let channel_urls: Vec<String> = channels
                            .iter()
                            .map(|c| c.clone().clone().into_base_url(&self.channel_config()))
                            .collect::<Result<Vec<_>, _>>()
                            .into_diagnostic()?
                            .iter()
                            .map(|url| url.to_string())
                            .collect();

                        builder.set_channels(env_name.as_str(), channel_urls);

                        // Add the installed packages to the lock file
                        for record in installed_packages {
                            // Convert PrefixRecord to PixiRecord using the repodata_record
                            let pixi_record = PixiRecord::Binary(record.repodata_record);
                            builder.add_conda_package(
                                env_name.as_str(),
                                platform,
                                pixi_record.into(),
                            );
                        }

                        lock_file = builder.finish();
                    } else {
                        tracing::warn!(
                            "Could not read installed packages from prefix for environment '{}'",
                            env_name
                        );
                    }

                    // TODO: Handle PyPI dependencies
                    if !pypi_deps.is_empty() {
                        tracing::warn!(
                            "PyPI dependencies in platform-less mode not yet implemented for environment '{}'",
                            env_name
                        );
                    }
                }
            } else {
                tracing::info!("no dependencies to install for environment '{}'", env_name);

                // Read any existing packages from the prefix to build lock file data
                let env_dir = environment.dir();
                let prefix = Prefix::new(&env_dir);

                if env_dir.exists() {
                    if let Ok(installed_packages) = prefix.find_installed_packages() {
                        let mut builder = LockFile::builder();

                        // Set channels for the environment
                        let channels = environment.channels();
                        let channel_urls: Vec<String> = channels
                            .iter()
                            .map(|c| c.clone().clone().into_base_url(&self.channel_config()))
                            .collect::<Result<Vec<_>, _>>()
                            .into_diagnostic()?
                            .iter()
                            .map(|url| url.to_string())
                            .collect();

                        builder.set_channels(env_name.as_str(), channel_urls);

                        // Add any existing installed packages to the lock file
                        for record in installed_packages {
                            let pixi_record = PixiRecord::Binary(record.repodata_record);
                            builder.add_conda_package(
                                env_name.as_str(),
                                platform,
                                pixi_record.into(),
                            );
                        }

                        lock_file = builder.finish();

                        tracing::info!(
                            "Found {} existing packages in environment '{}'",
                            lock_file
                                .environment(env_name.as_str())
                                .map(|env| env
                                    .conda_packages(platform)
                                    .map(|packages| packages.count())
                                    .unwrap_or(0))
                                .unwrap_or(0),
                            env_name
                        );
                    }
                } else {
                    // Create empty environment entry if directory doesn't exist
                    let mut builder = LockFile::builder();

                    let channels = environment.channels();
                    let channel_urls: Vec<String> = channels
                        .iter()
                        .map(|c| c.clone().clone().into_base_url(&self.channel_config()))
                        .collect::<Result<Vec<_>, _>>()
                        .into_diagnostic()?
                        .iter()
                        .map(|url| url.to_string())
                        .collect();

                    builder.set_channels(env_name.as_str(), channel_urls);
                    lock_file = builder.finish();
                }
            }
        }

        Ok(LockFileDerivedData {
            workspace: self,
            lock_file,
            package_cache,
            updated_conda_prefixes,
            updated_pypi_prefixes,
            uv_context: Default::default(),
            io_concurrency_limit: IoConcurrencyLimit::default(),
            command_dispatcher,
            glob_hash_cache,
            was_outdated: true, // In platform-less mode, we always update
        })
    }
}

/// Check if the installed packages satisfy the given dependencies
/// This validates both package names and version specifications
fn dependencies_satisfied(
    dependencies: &DependencyMap<PackageName, PixiSpec>,
    installed_packages: &[PrefixRecord],
    channel_config: &rattler_conda_types::ChannelConfig,
) -> bool {
    // For each dependency, check if there's a matching installed package by name and version
    for (dep_name, dep_specs) in dependencies.iter() {
        // Take the first spec from the IndexSet (most common case is single spec)
        let dep_spec = dep_specs.first();
        if dep_spec.is_none() {
            continue;
        }
        let dep_spec = dep_spec.unwrap();

        // Convert PixiSpec to NamelessMatchSpec and then to MatchSpec for proper version checking
        let match_spec = match dep_spec
            .clone()
            .try_into_nameless_match_spec(channel_config)
        {
            Ok(Some(nameless_spec)) => {
                MatchSpec::from_nameless(nameless_spec, Some(dep_name.clone()))
            }
            Ok(None) => {
                // For specs that can't be converted to MatchSpec (like Git sources),
                // just check by name for now
                tracing::debug!(
                    "Cannot convert spec for '{}' to MatchSpec, checking by name only",
                    dep_name.as_source()
                );
                let satisfied = installed_packages
                    .iter()
                    .any(|installed| &installed.repodata_record.package_record.name == dep_name);
                if !satisfied {
                    tracing::debug!(
                        "Dependency '{}' not found in installed packages",
                        dep_name.as_source()
                    );
                    return false;
                }
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to convert spec for '{}' to MatchSpec: {}. Checking by name only.",
                    dep_name.as_source(),
                    e
                );
                let satisfied = installed_packages
                    .iter()
                    .any(|installed| &installed.repodata_record.package_record.name == dep_name);
                if !satisfied {
                    tracing::debug!(
                        "Dependency '{}' not found in installed packages",
                        dep_name.as_source()
                    );
                    return false;
                }
                continue;
            }
        };

        let satisfied = installed_packages.iter().any(|installed| {
            let package_record = &installed.repodata_record.package_record;
            match_spec.matches(package_record)
        });

        if !satisfied {
            tracing::debug!(
                "Dependency '{}' with spec '{:?}' not satisfied by installed packages",
                dep_name.as_source(),
                dep_spec
            );
            return false;
        }
    }

    tracing::debug!(
        "All {} dependencies satisfied by installed packages",
        dependencies.iter().count()
    );
    true
}
