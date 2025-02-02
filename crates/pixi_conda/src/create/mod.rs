pub mod input;

use std::{collections::HashMap, io, io::Write, path::PathBuf, str::FromStr, time::Instant};

use clap::{Parser, ValueEnum};
use futures::future::TryFutureExt;
use input::EnvironmentInput;
use itertools::Itertools;
use miette::{Context, IntoDiagnostic, Report};
use pixi_config::{get_cache_dir, Config};
use pixi_consts::consts;
use pixi_progress::{await_in_progress, wrap_in_progress};
use pixi_utils::reqwest::build_reqwest_clients;
use rattler::{install::Installer, package_cache::PackageCache};
use rattler_conda_types::{
    ChannelConfig, EnvironmentYaml, MatchSpec, MatchSpecOrSubSection, NamedChannelOrUrl,
    PackageName, ParseChannelError, Platform, RepoDataRecord,
};
use rattler_solve::{SolverImpl, SolverTask};
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};
use tabwriter::TabWriter;

use crate::{registry::Registry, EnvironmentName};

/// Create a new conda environment from a list of specified packages.
#[derive(Parser, Debug)]
pub struct Args {
    /// List of packages to install or update in the conda environment.
    #[clap( conflicts_with = "file", num_args = 1.., required = true)]
    package_spec: Vec<MatchSpec>,

    /// Read package versions from the given file. Repeated file specifications
    /// can be passed (e.g. --file=file1 --file=file2).
    #[clap(long,  conflicts_with = "package_spec", num_args = 1.., required = true)]
    file: Vec<PathBuf>,

    /// Name of environment.
    #[clap(
        long,
        short,
        help_heading = "Target Environment Specification",
        conflicts_with = "prefix"
    )]
    name: Option<EnvironmentName>,

    /// Path to environment location (i.e. prefix).
    #[clap(long, short, help_heading = "Target Environment Specification")]
    prefix: Option<PathBuf>,

    /// Sets any confirmation values to 'yes' automatically. Users will not be
    /// asked to confirm any adding, deleting, backups, etc.
    #[clap(long, short, help_heading = "Output, Prompt, and Flow Control Options")]
    yes: bool,

    /// Only display what would have been done.
    #[clap(long, short, help_heading = "Output, Prompt, and Flow Control Options")]
    dry_run: bool,

    /// Choose which solver backend to use.
    #[clap(
        long,
        help_heading = "Solver Mode Modifiers",
        default_value = "resolvo"
    )]
    solver: SolverBackend,

    #[clap(flatten)]
    channel_customization: ChannelCustomization,
}

#[derive(Default, ValueEnum, Debug, Copy, Clone)]
enum SolverBackend {
    #[default]
    Resolvo,
    Libsolv,
}

#[derive(Parser, Debug)]
struct ChannelCustomization {
    /// Additional channel to search for packages.
    #[clap(long, short, help_heading = "Channel customization")]
    channel: Vec<NamedChannelOrUrl>,

    /// Do not search default channels.
    #[clap(
        long,
        help_heading = "Channel customization",
        default_value = "false",
        requires = "channel"
    )]
    override_channels: bool,

    /// Use packages built for this platform. The new environment will be
    /// configured to remember this choice. Should be formatted like
    /// 'osx-64', 'linux-32', 'win-64', and so on. Defaults to the
    /// current (native) platform.
    #[clap(long, visible_alias = "subdir", help_heading = "Channel customization")]
    platform: Option<Platform>,
}

pub async fn execute(config: Config, args: Args) -> miette::Result<()> {
    // Convert the input into a canonical form.
    let (mut input, input_path) =
        match EnvironmentInput::from_files_or_specs(args.file, args.package_spec)? {
            EnvironmentInput::EnvironmentYaml(environment, path) => (environment, Some(path)),
            EnvironmentInput::Specs(specs) => (
                EnvironmentYaml {
                    dependencies: specs
                        .into_iter()
                        .map(MatchSpecOrSubSection::MatchSpec)
                        .collect(),
                    ..EnvironmentYaml::default()
                },
                None,
            ),
            EnvironmentInput::Files(_) => {
                unimplemented!("explicit environment files are not yet supported")
            }
        };

    // Determine the path to the environment.
    let prefix = if let Some(prefix) = &args.prefix {
        if input.prefix.is_some() {
            tracing::warn!("--prefix is specified, but the input file also contains a prefix, the input file will be ignored");
        } else if input.name.is_some() {
            tracing::warn!("--prefix is specified, but the input file also contains a name, the input file will be ignored");
        }
        prefix
    } else if let Some(name) = args.name {
        if input.prefix.is_some() {
            tracing::warn!("--name is specified, but the input file also contains a prefix, the input file will be ignored");
        } else if input.name.is_some() {
            tracing::warn!("--name is specified, but the input file also contains a name, the input file will be ignored");
        }
        let registry = Registry::from_env();
        &registry.root().join(name.as_ref())
    } else if let Some(prefix) = &input.prefix {
        if input.name.is_some() {
            tracing::warn!(
                "the input file contains both a 'name' and a 'prefix', the 'name' will be ignored"
            );
        }
        prefix
    } else if let Some(name) = &input.name {
        let registry = Registry::from_env();
        &registry.root().join(name)
    } else {
        miette::bail!("either --name or --prefix must be specified");
    };

    let prefix = if prefix.is_relative() {
        &std::env::current_dir()
            .into_diagnostic()
            .context("failed to determine the current directory")?
            .join(prefix)
    } else {
        prefix
    };
    let prefix = dunce::simplified(&prefix);

    // Remove the prefix if it already exists
    if prefix.is_dir() && args.dry_run {
        miette::bail!("The prefix already exists, and --dry-run is specified, so the operation will not be performed");
    } else if prefix.is_dir() {
        let allow_remove = args.yes
            || dialoguer::Confirm::new()
                .with_prompt(format!(
                    "{} The prefix '{}' already exists, do you want to remove it?",
                    console::style("?").blue(),
                    prefix.display()
                ))
                .report(false)
                .default(false)
                .show_default(true)
                .interact()
                .into_diagnostic()?;
        if allow_remove {
            fs_err::remove_dir_all(&prefix).into_diagnostic()?;
        } else {
            eprintln!("Aborting");
            return Ok(());
        }
    }

    // Construct a channel configuration to resolve channel names.
    let mut channel_config = config.global_channel_config().clone();
    if let Some(input_path) = &input_path {
        channel_config.root_dir = input_path
            .parent()
            .expect("a file must have a parent")
            .to_path_buf();
    }

    // Determine the channels to use for package resolution.
    let mut channels = args.channel_customization.channel;
    if args.channel_customization.override_channels {
        if !input.channels.is_empty() {
            tracing::warn!("--override-channels is specified, but the input also contains channels, these will be ignored");
        }
    } else {
        channels.append(&mut input.channels);
        channels.append(&mut config.default_channels());
    }

    let channels = channels
        .into_iter()
        .map(|channel| channel.into_channel(&channel_config))
        .collect::<Result<Vec<_>, ParseChannelError>>()
        .into_diagnostic()?;

    // Determine the platform to use for package resolution.
    let platform = args
        .channel_customization
        .platform
        .unwrap_or_else(Platform::current);

    // Load the repodata for specs.
    // TODO: Add progress reporting
    let (_client, client_with_middleware) = build_reqwest_clients(Some(&config));
    let gateway = config.gateway(client_with_middleware.clone());
    let repo_data_duration = Instant::now();
    let available_packages = await_in_progress("fetching repodata", |_| {
        gateway
            .query(
                channels.into_iter().unique(),
                [platform, Platform::NoArch],
                input.match_specs().cloned(),
            )
            .recursive(true)
            .execute()
            .map_err(|e| Report::from_err(e))
    })
    .await?;
    let repo_data_record_count = available_packages
        .iter()
        .map(|records| records.len())
        .sum::<usize>();
    let repo_data_duration = repo_data_duration.elapsed();
    eprintln!(
        "{}Fetched {repo_data_record_count} records {}",
        console::style(console::Emoji("✔ ", "")).green(),
        console::style(format!(
            "in {}",
            humantime::format_duration(repo_data_duration)
        ))
        .dim()
    );

    // Determine the virtual packages
    let virtual_packages =
        VirtualPackages::detect(&VirtualPackageOverrides::from_env()).into_diagnostic()?;

    // Solve the environment
    let solver_task = SolverTask {
        virtual_packages: virtual_packages.into_generic_virtual_packages().collect(),
        specs: input.match_specs().cloned().collect(),
        ..SolverTask::from_iter(&available_packages)
    };

    let solver_duration = Instant::now();
    let solver_result = wrap_in_progress("solving", move || match args.solver {
        SolverBackend::Resolvo => rattler_solve::resolvo::Solver.solve(solver_task),
        SolverBackend::Libsolv => rattler_solve::libsolv_c::Solver.solve(solver_task),
    })
    .into_diagnostic()?;
    let solver_duration = solver_duration.elapsed();
    eprintln!(
        "{}Solved environment {}",
        console::style(console::Emoji("✔ ", "")).green(),
        console::style(format!(
            "in {}",
            humantime::format_duration(solver_duration)
        ))
        .dim()
    );

    // Print the result
    eprintln!("\nThe following packages will be installed:\n");
    print_transaction(
        &solver_result.records,
        &solver_result.features,
        &channel_config,
    )
    .into_diagnostic()?;

    if args.dry_run {
        // This is the point where we would normally ask the user for confirmation for
        // installation.
        return Ok(());
    }

    eprintln!(); // Add a newline after the transaction

    let do_install = args.yes
        || dialoguer::Confirm::new()
            .with_prompt(format!(
                "{} Do you want to proceed with the installation?",
                console::style("?").blue()
            ))
            .default(true)
            .show_default(true)
            .report(false)
            .interact()
            .into_diagnostic()?;
    if !do_install {
        eprintln!("Aborting");
        return Ok(());
    }

    // Install the environment
    let cache_dir =
        get_cache_dir().expect("cache dir is already available because it was used by the gateway");
    let package_count = solver_result.records.len();
    let installation_duration = Instant::now();
    await_in_progress("installing", |_| {
        Installer::new()
            .with_package_cache(PackageCache::new(
                cache_dir.join(consts::CONDA_PACKAGE_CACHE_DIR),
            ))
            .with_download_client(client_with_middleware)
            .with_execute_link_scripts(true)
            .with_installed_packages(vec![])
            .with_target_platform(platform)
            .install(prefix, solver_result.records)
    })
    .await
    .into_diagnostic()?;
    let installation_duration = installation_duration.elapsed();

    eprintln!(
        "{}Installed {package_count} packages into {} {}",
        console::style(console::Emoji("✔ ", "")).green(),
        prefix.display(),
        console::style(format!(
            "in {}",
            humantime::format_duration(installation_duration)
        ))
        .dim()
    );

    Ok(())
}

fn print_transaction(
    records: &[RepoDataRecord],
    features: &HashMap<PackageName, Vec<String>>,
    channel_config: &ChannelConfig,
) -> io::Result<()> {
    let heading_style = console::Style::new().bold().white().bright();
    let seperator_style = console::Style::new().dim();

    let output = std::io::stderr();
    let mut writer = TabWriter::new(output);
    writeln!(
        writer,
        "  {package}\t{version}\t{build}\t{channel}\t{size}",
        package = heading_style.apply_to("Package"),
        version = heading_style.apply_to("Version"),
        build = heading_style.apply_to("Build"),
        channel = heading_style.apply_to("Channel"),
        size = heading_style.apply_to("Size")
    )?;
    writeln!(
        writer,
        "{}",
        seperator_style.apply_to("  -------\t-------\t-----\t-------\t----")
    )?;

    let format_record = |writer: &mut TabWriter<_>, r: &RepoDataRecord| -> io::Result<()> {
        let channel = r
            .channel
            .as_deref()
            .and_then(|c| NamedChannelOrUrl::from_str(c).ok())
            .and_then(|c| c.into_base_url(channel_config).ok())
            .map(|c| channel_config.canonical_name(c.as_ref()))
            .map(|name| {
                name.split_once("://")
                    .map(|(_, name)| name.to_string())
                    .unwrap_or(name)
            })
            .map(|name| name.trim_end_matches('/').to_string());

        write!(writer, "{} ", console::style("+").green())?;

        if let Some(features) = features.get(&r.package_record.name) {
            write!(
                writer,
                "{}[{}]\t",
                r.package_record.name.as_normalized(),
                features.join(", "),
            )?
        } else {
            write!(writer, "{}\t", r.package_record.name.as_normalized(),)?;
        }

        writeln!(
            writer,
            "{}\t{}\t{}\t{}",
            r.package_record.version.to_string(),
            &r.package_record.build,
            channel.as_deref().unwrap_or_default(),
            r.package_record
                .size
                .map(|bytes| human_bytes::human_bytes(bytes as f64))
                .unwrap_or_default()
        )
    };

    for package in records.iter().sorted_by_key(|r| &r.package_record.name) {
        format_record(&mut writer, package)?;
    }

    writer.flush()
}
