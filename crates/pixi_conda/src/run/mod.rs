use clap::Parser;
use miette::IntoDiagnostic;
use pixi_config::Config;
use rattler_conda_types::Platform;
use rattler_shell::shell::ShellEnum;
use std::{path::PathBuf, process::Stdio};

use crate::{registry::Registry, EnvironmentName};

/// Run an executable in a conda environment.
#[derive(Parser, Debug)]
#[clap(trailing_var_arg = true, disable_help_flag = true)]
pub struct Args {
    #[clap(num_args = 1.., allow_hyphen_values = true, required = true)]
    args: Vec<String>,

    /// Print help
    #[clap(long, short, action = clap::ArgAction::Help)]
    help: Option<bool>,

    /// Name of environment.
    #[clap(
        long,
        short,
        help_heading = "Target Environment Specification",
        conflicts_with = "prefix",
        required = true
    )]
    name: Option<EnvironmentName>,

    /// Path to environment location (i.e. prefix).
    #[clap(long, short, help_heading = "Target Environment Specification")]
    prefix: Option<PathBuf>,
}

pub async fn execute(_config: Config, mut args: Args) -> miette::Result<()> {
    // Determine the prefix to use
    let prefix = if let Some(name) = &args.name {
        &Registry::from_env().root().join(name.as_ref())
    } else if let Some(prefix) = &args.prefix {
        prefix
    } else {
        unreachable!("Either a name or a prefix must be provided")
    };

    // Make sure it exists
    if !prefix.is_dir() || !prefix.join("conda-meta").is_dir() {
        let prefix_or_name = if let Some(name) = &args.name {
            format!("--name {name}")
        } else if let Some(prefix) = &args.prefix {
            format!("--prefix {}", prefix.display())
        } else {
            unreachable!("Either a name or a prefix must be provided")
        };
        miette::bail!(
            help = format!(
                "You can create an environment with:\n\n\tpixi-conda create {prefix_or_name} ..."
            ),
            "The environment at '{}' does not appear to be a valid conda environment",
            prefix.display()
        );
    };

    // Collect environment variables for the prefix.
    let activation_variables = rattler_shell::activation::Activator::from_path(
        prefix,
        ShellEnum::default(),
        Platform::current(),
    )
    .into_diagnostic()?
    .run_activation(
        rattler_shell::activation::ActivationVariables::from_env().into_diagnostic()?,
        None,
    )
    .into_diagnostic()?;

    // Spawn the process
    let executable = args.args.remove(0);
    let mut command = std::process::Command::new(&executable);

    // Set the environment variables
    command.envs(activation_variables);

    // Add the arguments
    command.args(args.args);

    // Inherit stdin, stdout, and stderr
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    // Spawn the child process
    #[cfg(target_family = "unix")]
    {
        use std::os::unix::process::CommandExt;

        // Exec replaces the current process with the new one and does not return!
        let err = command.exec();

        // If we get here, the exec failed
        miette::bail!("Failed to execute '{}': {}", executable, err);
    }

    #[cfg(target_os = "windows")]
    {
        use miette::Report;
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                miette::bail!("The executable '{}' could not be found", executable);
            }
            Err(e) => return Err(Report::from_err(e)),
        };

        // Wait for the child process to complete
        let status = child.wait().into_diagnostic()?;

        // Exit with the same status code as the child process
        std::process::exit(status.code().unwrap_or(1));
    }
}
