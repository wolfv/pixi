pub mod input;

use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use input::EnvironmentInput;
use itertools::Itertools;
use miette::IntoDiagnostic;
use pixi_config::Config;
use pixi_progress::{await_in_progress, wrap_in_progress};
use pixi_utils::reqwest::build_reqwest_clients;
use rattler_conda_types::{
    EnvironmentYaml, MatchSpec, MatchSpecOrSubSection, NamedChannelOrUrl, ParseChannelError,
    Platform,
};
use rattler_solve::{SolverImpl, SolverTask};
use rattler_virtual_packages::{VirtualPackageOverrides, VirtualPackages};

/// Create a new conda environment from a list of specified packages.
#[derive(Parser, Debug)]
#[clap(verbatim_doc_comment)]
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
    name: Option<String>,

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
    let gateway = config.gateway(client_with_middleware);
    let available_packages = await_in_progress("fetching repodata", |_| {
        gateway
            .query(
                channels.into_iter().unique(),
                [platform, Platform::NoArch],
                input.match_specs().cloned(),
            )
            .recursive(true)
            .execute()
    })
    .await
    .into_diagnostic()?;

    // Determine the virtual packages
    let virtual_packages =
        VirtualPackages::detect(&VirtualPackageOverrides::from_env()).into_diagnostic()?;

    // Solve the environment
    let solver_task = SolverTask {
        virtual_packages: virtual_packages.into_generic_virtual_packages().collect(),
        specs: input.match_specs().cloned().collect(),
        ..SolverTask::from_iter(&available_packages)
    };

    let solver_result = wrap_in_progress("solving", move || match args.solver {
        SolverBackend::Resolvo => rattler_solve::resolvo::Solver.solve(solver_task),
        SolverBackend::Libsolv => rattler_solve::libsolv_c::Solver.solve(solver_task),
    })
    .into_diagnostic()?;

    Ok(())
}
