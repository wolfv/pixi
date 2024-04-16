use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::{Config, ConfigCli};
use crate::install::execute_transaction;
use crate::{config, prefix::Prefix, progress::await_in_progress};
use clap::Parser;
use itertools::Itertools;
use miette::IntoDiagnostic;
use rattler::install::Transaction;
use rattler::package_cache::PackageCache;
use rattler_conda_types::{
    MatchSpec, PackageName, ParseStrictness, Platform, PrefixRecord, RepoDataRecord,
};
use rattler_shell::{
    activation::{ActivationVariables, Activator, PathModificationBehavior},
    shell::Shell,
    shell::ShellEnum,
};
use reqwest_middleware::ClientWithMiddleware;

use super::common::{
    channel_name_from_prefix, find_designated_package, get_client_and_sparse_repodata,
    load_package_records, package_name, BinDir, BinEnvDir,
};

/// Installs the defined package in a global accessible location.
#[derive(Parser, Debug)]
#[clap(arg_required_else_help = true)]
pub struct Args {
    /// Specifies the package(s) that is to be installed.
    #[arg(num_args = 1..)]
    package: Vec<String>,

    /// Represents the channels from which the package will be installed.
    /// Multiple channels can be specified by using this field multiple times.
    ///
    /// When specifying a channel, it is common that the selected channel also
    /// depends on the `conda-forge` channel.
    /// For example: `pixi global install --channel conda-forge --channel bioconda`.
    ///
    /// By default, if no channel is provided, `conda-forge` is used.
    #[clap(short, long)]
    channel: Vec<String>,

    #[clap(flatten)]
    config: ConfigCli,
}

/// Create the environment activation script
fn create_activation_script(prefix: &Prefix, shell: ShellEnum) -> miette::Result<String> {
    let activator =
        Activator::from_path(prefix.root(), shell, Platform::current()).into_diagnostic()?;
    let result = activator
        .activation(ActivationVariables {
            conda_prefix: None,
            path: None,
            path_modification_behavior: PathModificationBehavior::Prepend,
        })
        .into_diagnostic()?;

    // Add a shebang on unix based platforms
    let script = if cfg!(unix) {
        format!("#!/bin/sh\n{}", result.script)
    } else {
        result.script
    };

    Ok(script)
}

fn is_executable(prefix: &Prefix, relative_path: &Path) -> bool {
    // Check if the file is in a known executable directory.
    let binary_folders = if cfg!(windows) {
        &([
            "",
            "Library/mingw-w64/bin/",
            "Library/usr/bin/",
            "Library/bin/",
            "Scripts/",
            "bin/",
        ][..])
    } else {
        &(["bin"][..])
    };

    let parent_folder = match relative_path.parent() {
        Some(dir) => dir,
        None => return false,
    };

    if !binary_folders
        .iter()
        .any(|bin_path| Path::new(bin_path) == parent_folder)
    {
        return false;
    }

    // Check if the file is executable
    let absolute_path = prefix.root().join(relative_path);
    is_executable::is_executable(absolute_path)
}

/// Find the executable scripts within the specified package installed in this conda prefix.
fn find_executables<'a>(prefix: &Prefix, prefix_package: &'a PrefixRecord) -> Vec<&'a Path> {
    prefix_package
        .files
        .iter()
        .filter(|relative_path| is_executable(prefix, relative_path))
        .map(|buf| buf.as_ref())
        .collect()
}

/// Mapping from an executable in a package environment to its global binary script location.
#[derive(Debug)]
pub struct BinScriptMapping<'a> {
    pub original_executable: &'a Path,
    pub global_binary_path: PathBuf,
}

/// For each executable provided, map it to the installation path for its global binary script.
async fn map_executables_to_global_bin_scripts<'a>(
    package_executables: &[&'a Path],
    bin_dir: &BinDir,
) -> miette::Result<Vec<BinScriptMapping<'a>>> {
    #[cfg(target_family = "windows")]
    let extensions_list: Vec<String> = if let Ok(pathext) = std::env::var("PATHEXT") {
        pathext.split(';').map(|s| s.to_lowercase()).collect()
    } else {
        tracing::debug!("Could not find 'PATHEXT' variable, using a default list");
        [
            ".COM", ".EXE", ".BAT", ".CMD", ".VBS", ".VBE", ".JS", ".JSE", ".WSF", ".WSH", ".MSC",
            ".CPL",
        ]
        .iter()
        .map(|&s| s.to_lowercase())
        .collect()
    };

    #[cfg(target_family = "unix")]
    // TODO: Find if there are more relevant cases, these cases are generated by our big friend GPT-4
    let extensions_list: Vec<String> = vec![
        ".sh", ".bash", ".zsh", ".csh", ".tcsh", ".ksh", ".fish", ".py", ".pl", ".rb", ".lua",
        ".php", ".tcl", ".awk", ".sed",
    ]
    .iter()
    .map(|&s| s.to_owned())
    .collect();

    let BinDir(bin_dir) = bin_dir;
    let mut mappings = vec![];

    for exec in package_executables.iter() {
        // Remove the extension of a file if it is in the list of known extensions.
        let Some(file_name) = exec
            .file_name()
            .and_then(OsStr::to_str)
            .map(str::to_lowercase)
        else {
            continue;
        };
        let file_name = extensions_list
            .iter()
            .find_map(|ext| file_name.strip_suffix(ext))
            .unwrap_or(file_name.as_str());

        let mut executable_script_path = bin_dir.join(file_name);

        if cfg!(windows) {
            executable_script_path.set_extension("bat");
        };
        mappings.push(BinScriptMapping {
            original_executable: exec,
            global_binary_path: executable_script_path,
        });
    }
    Ok(mappings)
}

/// Find all executable scripts in a package and map them to their global install paths.
///
/// (Convenience wrapper around `find_executables` and `map_executables_to_global_bin_scripts` which
/// are generally used together.)
pub(super) async fn find_and_map_executable_scripts<'a>(
    prefix: &Prefix,
    prefix_package: &'a PrefixRecord,
    bin_dir: &BinDir,
) -> miette::Result<Vec<BinScriptMapping<'a>>> {
    let executables = find_executables(prefix, prefix_package);
    map_executables_to_global_bin_scripts(&executables, bin_dir).await
}

/// Create the executable scripts by modifying the activation script
/// to activate the environment and run the executable.
pub(super) async fn create_executable_scripts(
    mapped_executables: &[BinScriptMapping<'_>],
    prefix: &Prefix,
    shell: &ShellEnum,
    activation_script: String,
) -> miette::Result<()> {
    for BinScriptMapping {
        original_executable: exec,
        global_binary_path: executable_script_path,
    } in mapped_executables
    {
        let mut script = activation_script.clone();
        shell
            .run_command(
                &mut script,
                [
                    format!("\"{}\"", prefix.root().join(exec).to_string_lossy()).as_str(),
                    get_catch_all_arg(shell),
                ],
            )
            .expect("should never fail");

        if matches!(shell, ShellEnum::CmdExe(_)) {
            // wrap the script contents in `@echo off` and `setlocal` to prevent echoing the script
            // and to prevent leaking environment variables into the parent shell (e.g. PATH would grow longer and longer)
            script = format!("@echo off\nsetlocal\n{}\nendlocal", script);
        }

        tokio::fs::write(&executable_script_path, script)
            .await
            .into_diagnostic()?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                executable_script_path,
                std::fs::Permissions::from_mode(0o744),
            )
            .into_diagnostic()?;
        }
    }
    Ok(())
}

/// Install a global command
pub async fn execute(args: Args) -> miette::Result<()> {
    // Figure out what channels we are using
    let config = Config::with_cli_config(&args.config);
    let channels = config.compute_channels(&args.channel).into_diagnostic()?;

    // Find the MatchSpec we want to install
    let specs = args
        .package
        .into_iter()
        .map(|package_str| MatchSpec::from_str(&package_str, ParseStrictness::Strict))
        .collect::<Result<Vec<_>, _>>()
        .into_diagnostic()?;

    // Fetch sparse repodata
    let (authenticated_client, sparse_repodata) =
        get_client_and_sparse_repodata(&channels, &config).await?;

    // Install the package(s)
    let mut executables = vec![];
    for package_matchspec in specs {
        let package_name = package_name(&package_matchspec)?;
        let records = load_package_records(package_matchspec, &sparse_repodata)?;

        let (prefix_package, scripts, _) =
            globally_install_package(&package_name, records, authenticated_client.clone()).await?;
        let channel_name = channel_name_from_prefix(&prefix_package, config.channel_config());
        let record = &prefix_package.repodata_record.package_record;

        // Warn if no executables were created for the package
        if scripts.is_empty() {
            eprintln!(
                "{}No executable entrypoint found in package {}, are you sure it exists?",
                console::style(console::Emoji("⚠️", "")).yellow().bold(),
                console::style(record.name.as_source()).bold()
            );
        }

        eprintln!(
            "{}Installed package {} {} {} from {}",
            console::style(console::Emoji("✔ ", "")).green(),
            console::style(record.name.as_source()).bold(),
            console::style(record.version.version()).bold(),
            console::style(record.build.as_str()).bold(),
            channel_name,
        );

        executables.extend(scripts);
    }

    if !executables.is_empty() {
        print_executables_available(executables).await?;
    }

    Ok(())
}

async fn print_executables_available(executables: Vec<PathBuf>) -> miette::Result<()> {
    let BinDir(bin_dir) = BinDir::from_existing().await?;
    let whitespace = console::Emoji("  ", "").to_string();
    let executable = executables
        .into_iter()
        .map(|path| {
            path.strip_prefix(&bin_dir)
                .expect("script paths were constructed by joining onto BinDir")
                .to_string_lossy()
                .to_string()
        })
        .join(&format!("\n{whitespace} -  "));

    if is_bin_folder_on_path().await {
        eprintln!(
            "{whitespace}These executables are now globally available:\n{whitespace} -  {executable}",
        )
    } else {
        eprintln!("{whitespace}These executables have been added to {}\n{whitespace} -  {executable}\n\n{} To use them, make sure to add {} to your PATH",
                  console::style(&bin_dir.display()).bold(),
                  console::style("!").yellow().bold(),
                  console::style(&bin_dir.display()).bold()
        )
    }

    Ok(())
}

/// Install given package globally, with all its dependencies
pub(super) async fn globally_install_package(
    package_name: &PackageName,
    records: Vec<RepoDataRecord>,
    authenticated_client: ClientWithMiddleware,
) -> miette::Result<(PrefixRecord, Vec<PathBuf>, bool)> {
    // Create the binary environment prefix where we install or update the package
    let BinEnvDir(bin_prefix) = BinEnvDir::create(package_name).await?;
    let prefix = Prefix::new(bin_prefix);
    let prefix_records = prefix.find_installed_packages(None).await?;

    // Create the transaction that we need
    let transaction =
        Transaction::from_current_and_desired(prefix_records.clone(), records, Platform::current())
            .into_diagnostic()?;

    let has_transactions = !transaction.operations.is_empty();

    // Execute the transaction if there is work to do
    if has_transactions {
        let package_cache = Arc::new(PackageCache::new(config::get_cache_dir()?.join("pkgs")));

        // Execute the operations that are returned by the solver.
        await_in_progress("creating virtual environment", |pb| {
            execute_transaction(
                package_cache,
                &transaction,
                &prefix_records,
                prefix.root().to_path_buf(),
                authenticated_client,
                pb,
            )
        })
        .await?;
    }

    // Find the installed package in the environment
    let prefix_package = find_designated_package(&prefix, package_name).await?;

    // Determine the shell to use for the invocation script
    let shell: ShellEnum = if cfg!(windows) {
        rattler_shell::shell::CmdExe.into()
    } else {
        rattler_shell::shell::Bash.into()
    };

    // Construct the reusable activation script for the shell and generate an invocation script
    // for each executable added by the package to the environment.
    let activation_script = create_activation_script(&prefix, shell.clone())?;

    let bin_dir = BinDir::create().await?;
    let script_mapping =
        find_and_map_executable_scripts(&prefix, &prefix_package, &bin_dir).await?;
    create_executable_scripts(&script_mapping, &prefix, &shell, activation_script).await?;

    let scripts: Vec<_> = script_mapping
        .into_iter()
        .map(
            |BinScriptMapping {
                 global_binary_path: path,
                 ..
             }| path,
        )
        .collect();

    Ok((prefix_package, scripts, has_transactions))
}

/// Returns the string to add for all arguments passed to the script
fn get_catch_all_arg(shell: &ShellEnum) -> &str {
    match shell {
        ShellEnum::CmdExe(_) => "%*",
        ShellEnum::PowerShell(_) => "@args",
        _ => "\"$@\"",
    }
}

/// Returns true if the bin folder is available on the PATH.
async fn is_bin_folder_on_path() -> bool {
    let bin_path = match BinDir::from_existing().await.ok() {
        Some(BinDir(bin_dir)) => bin_dir,
        None => return false,
    };

    std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect_vec())
        .unwrap_or_default()
        .into_iter()
        .contains(&bin_path)
}
