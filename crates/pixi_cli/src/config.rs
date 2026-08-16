use crate::cli_config::WorkspaceConfig;
use clap::Parser;
use miette::{IntoDiagnostic, WrapErr};
use pixi_config;
use pixi_config::Config;
use pixi_consts::consts;
use pixi_core::WorkspaceLocator;
use pixi_core::workspace::WorkspaceLocatorError;
use rattler_conda_types::NamedChannelOrUrl;
use std::{io::Write, path::PathBuf, str::FromStr};

#[derive(Parser, Debug)]
enum Subcommand {
    /// Edit the configuration file
    #[clap(alias = "e")]
    Edit(EditArgs),

    /// List configuration values
    ///
    /// Example: `pixi config list default-channels`
    #[clap(visible_alias = "ls", alias = "l")]
    List(ListArgs),

    /// Prepend a value to a list configuration key
    ///
    /// Example: `pixi config prepend default-channels bioconda`
    Prepend(PendArgs),

    /// Append a value to a list configuration key
    ///
    /// Example: `pixi config append default-channels bioconda`
    Append(PendArgs),

    /// Set a configuration value
    ///
    /// Example: `pixi config set default-channels '["conda-forge", "bioconda"]'`
    Set(SetArgs),

    /// Unset a configuration value
    ///
    /// Example: `pixi config unset default-channels`
    Unset(UnsetArgs),
}

#[derive(Parser, Debug, Clone)]
struct CommonArgs {
    /// Operation on project-local configuration
    #[arg(long, short, conflicts_with_all = &["global", "system"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    local: bool,

    /// Operation on global configuration
    #[arg(long, short, conflicts_with_all = &["local", "system"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    global: bool,

    /// Operation on system configuration
    #[arg(long, short, conflicts_with_all = &["local", "global"], help_heading = consts::CLAP_CONFIG_OPTIONS)]
    system: bool,

    #[clap(flatten)]
    pub workspace_config: WorkspaceConfig,
}

#[derive(Parser, Debug, Clone)]
struct EditArgs {
    #[clap(flatten)]
    common: CommonArgs,

    /// The editor to use, defaults to `EDITOR` environment variable or `nano` on Unix and `notepad` on Windows
    #[arg(env = "EDITOR")]
    pub editor: Option<String>,
}

#[derive(Parser, Debug, Clone)]
struct ListArgs {
    /// Configuration key to show (all if not provided)
    key: Option<String>,

    /// Output in JSON format
    #[arg(long)]
    json: bool,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct PendArgs {
    /// Configuration key to set
    key: String,

    /// Configuration value to (pre|ap)pend
    value: String,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct SetArgs {
    /// Configuration key to set
    key: String,

    /// Configuration value to set (key will be unset if value not provided)
    value: Option<String>,

    #[clap(flatten)]
    common: CommonArgs,
}

#[derive(Parser, Debug, Clone)]
struct UnsetArgs {
    /// Configuration key to unset
    key: String,

    #[clap(flatten)]
    common: CommonArgs,
}

enum AlterMode {
    Prepend,
    Append,
    Set,
    Unset,
}

/// Configuration management
#[derive(Parser, Debug)]
#[clap(arg_required_else_help = true)]
pub struct Args {
    #[clap(subcommand)]
    subcommand: Subcommand,
}

pub async fn execute(args: Args) -> miette::Result<()> {
    match args.subcommand {
        Subcommand::Edit(args) => {
            let config_path = determine_config_write_path(&args.common)?;

            let editor = args.editor.unwrap_or_else(|| {
                if cfg!(windows) {
                    "notepad".to_string()
                } else {
                    "nano".to_string()
                }
            });

            let mut child = if cfg!(windows) {
                std::process::Command::new("cmd")
                    .arg("/C")
                    .arg(editor.as_str())
                    .arg(&config_path)
                    .spawn()
                    .into_diagnostic()?
            } else {
                std::process::Command::new(editor.as_str())
                    .arg(&config_path)
                    .spawn()
                    .into_diagnostic()?
            };
            child.wait().into_diagnostic()?;
        }
        Subcommand::List(args) => {
            let config = load_config(&args.common)?;

            let out = match args.key {
                // Narrowing to a key goes through the serialized config, so
                // every key `Config` has is listable; printing the whole
                // config keeps serializing `Config` itself so the field order
                // of the output is unchanged.
                Some(key) => match partial_config(&config, &key)? {
                    Some(value) if args.json => {
                        serde_json::to_string_pretty(&value).into_diagnostic()?
                    }
                    Some(value) => toml_edit::ser::to_string_pretty(&value).into_diagnostic()?,
                    None => String::new(),
                },
                None if args.json => serde_json::to_string_pretty(&config).into_diagnostic()?,
                None => toml_edit::ser::to_string_pretty(&config).into_diagnostic()?,
            };

            if out.is_empty() {
                eprintln!("Configuration not set");
            }
            writeln!(std::io::stdout(), "{out}")
                .map_err(|e| {
                    if e.kind() == std::io::ErrorKind::BrokenPipe {
                        std::process::exit(0);
                    }
                    e
                })
                .into_diagnostic()?;
        }
        Subcommand::Prepend(args) => alter_config(
            &args.common,
            &args.key,
            Some(args.value),
            AlterMode::Prepend,
        )?,
        Subcommand::Append(args) => {
            alter_config(&args.common, &args.key, Some(args.value), AlterMode::Append)?
        }
        Subcommand::Set(args) => alter_config(&args.common, &args.key, args.value, AlterMode::Set)?,
        Subcommand::Unset(args) => alter_config(&args.common, &args.key, None, AlterMode::Unset)?,
    };
    Ok(())
}

fn determine_project_root(common_args: &CommonArgs) -> miette::Result<Option<PathBuf>> {
    let workspace = WorkspaceLocator::default()
        .with_closest_package(false) // Dont care about the package
        .with_emit_warnings(false) // No reason to emit warnings
        .with_consider_environment(true)
        .with_search_start(common_args.workspace_config.workspace_locator_start())
        .with_ignore_pixi_version_check(true)
        .locate();
    match workspace {
        Err(WorkspaceLocatorError::WorkspaceNotFound(_)) => {
            if common_args.local {
                return Err(miette::miette!(
                    "--local flag can only be used inside a pixi workspace but no workspace could be found",
                ));
            }
            Ok(None)
        }
        Err(e) => {
            if common_args.local {
                return Err(e).into_diagnostic().context("--local flag can only be used inside a pixi workspace but loading the workspace failed",);
            }
            Ok(None)
        }
        Ok(project) => Ok(Some(project.root().to_path_buf())),
    }
}

fn load_config(common_args: &CommonArgs) -> miette::Result<Config> {
    let ret = if common_args.system {
        Config::load_system()
    } else if common_args.global {
        Config::load_global()
    } else if let Some(root) = determine_project_root(common_args)? {
        Config::load(&root)
    } else {
        Config::load_global()
    };

    Ok(ret)
}

fn determine_config_write_path(common_args: &CommonArgs) -> miette::Result<PathBuf> {
    let write_path = if common_args.system {
        pixi_config::config_path_system()
    } else {
        if let Some(root) = determine_project_root(common_args)?
            && !common_args.global
        {
            return Ok(root.join(consts::PIXI_DIR).join(consts::CONFIG_FILE));
        }

        let mut global_locations = pixi_config::config_path_global();
        let mut to = global_locations
            .pop()
            .expect("should have at least one global config path");

        for p in global_locations {
            if p.exists() {
                to = p;
                break;
            }
        }

        to
    };

    Ok(write_path)
}

fn alter_config(
    common_args: &CommonArgs,
    key: &str,
    value: Option<String>,
    mode: AlterMode,
) -> miette::Result<()> {
    let mut config = load_config(common_args)?;
    let to = determine_config_write_path(common_args)?;

    match mode {
        AlterMode::Prepend | AlterMode::Append => {
            let is_prepend = matches!(mode, AlterMode::Prepend);

            match key {
                "default-channels" => {
                    let input = value.expect("value must be provided");
                    let channel = NamedChannelOrUrl::from_str(&input)
                        .into_diagnostic()
                        .context("invalid channel name")?;
                    let mut new_channels = config.default_channels.clone();
                    if is_prepend {
                        new_channels.insert(0, channel);
                    } else {
                        new_channels.push(channel);
                    }
                    config.default_channels = new_channels;
                }
                "pypi-config.extra-index-urls" => {
                    let input = url::Url::parse(&value.expect("value must be provided"))
                        .map_err(|e| miette::miette!("Invalid URL: {}", e))?;
                    let mut new_urls = config.pypi_config().extra_index_urls.clone();
                    if is_prepend {
                        new_urls.insert(0, input);
                    } else {
                        new_urls.push(input);
                    }
                    config.pypi_config.extra_index_urls = new_urls;
                }
                _ => {
                    let list_keys = ["default-channels", "pypi-config.extra-index-urls"];
                    let msg_cmd = if is_prepend { "prepend" } else { "append" };
                    return Err(miette::miette!(
                        "{} is only supported for list keys: {}",
                        msg_cmd,
                        list_keys.join(", ")
                    ));
                }
            }
        }
        AlterMode::Set | AlterMode::Unset => config.set(key, value)?,
    }

    config.save(&to)?;
    eprintln!("✅ Updated config at {}", to.display());
    Ok(())
}

/// Narrow the configuration down to the sub-tree addressed by a dotted `key`,
/// so `pixi config list <key>` shows only that part.
///
/// Returns `None` when `key` is a valid key that is simply not set, which the
/// caller reports as "Configuration not set".
///
/// The accepted keys are the ones [`Config::get_keys`] advertises, and the
/// value is taken from the serialized config rather than copied field by
/// field. A hand-written list of fields drifted out of step with `Config` and
/// rejected valid keys such as `tls-root-certs`, `cache.*` and
/// `pinning-strategy` even though the config file and `pixi config set`
/// accepted them.
fn partial_config(config: &Config, key: &str) -> miette::Result<Option<serde_json::Value>> {
    if !is_known_key(config, key) {
        return Err(miette::miette!(
            "Unknown key: {}\nSupported keys:\n\t{}",
            console::style(key).red(),
            config.get_keys().join(",\n\t")
        ));
    }

    // Every `Config` field skips serialization when unset, so a missing
    // segment means the key carries no value.
    let mut value = serde_json::to_value(config).into_diagnostic()?;
    for segment in key.split('.') {
        value = match value {
            serde_json::Value::Object(mut map) => match map.remove(segment) {
                Some(value) => value,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }

    // Wrap the value back into the tables it was addressed through, so the
    // output reads like the config file it came from.
    for segment in key.split('.').rev() {
        value = serde_json::json!({ segment: value });
    }
    Ok(Some(value))
}

/// Whether `key` is one of the keys [`Config::get_keys`] advertises.
///
/// Segments written as a `<placeholder>` there (e.g. `s3-options.<bucket>`)
/// stand for a user-chosen name and match any single segment.
fn is_known_key(config: &Config, key: &str) -> bool {
    let key = key.split('.').collect::<Vec<_>>();
    config.get_keys().iter().any(|known| {
        let known = known.split('.').collect::<Vec<_>>();
        known.len() == key.len()
            && known.iter().zip(&key).all(|(known, segment)| {
                known == segment || (known.starts_with('<') && known.ends_with('>'))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(toml: &str) -> Config {
        Config::from_toml(toml, None).unwrap().0
    }

    fn list(config: &Config, key: &str) -> String {
        match partial_config(config, key).unwrap() {
            Some(value) => toml_edit::ser::to_string_pretty(&value).unwrap(),
            None => String::new(),
        }
    }

    #[test]
    fn lists_every_advertised_key() {
        // `pixi config list <key>` used to accept a hand-written subset of the
        // keys `pixi config set` and the config file understand.
        let config = Config::default();
        for key in config.get_keys() {
            let key = key.replace("<bucket>", "my-bucket");
            partial_config(&config, &key)
                .unwrap_or_else(|err| panic!("`config list {key}` was rejected: {err}"));
        }
    }

    #[test]
    fn lists_tls_root_certs() {
        assert_eq!(
            list(&config("tls-root-certs = \"webpki\""), "tls-root-certs"),
            "tls-root-certs = \"webpki\"\n"
        );
    }

    #[test]
    fn lists_nested_key_under_its_own_table() {
        let config = config("[concurrency]\ndownloads = 7\nsolves = 3\n");
        assert_eq!(
            list(&config, "concurrency.downloads"),
            "[concurrency]\ndownloads = 7\n"
        );
    }

    #[test]
    fn lists_whole_table_for_a_parent_key() {
        let config = config("[proxy-config]\nhttps = \"https://proxy.example\"\n");
        let out = list(&config, "proxy-config");
        assert!(out.starts_with("[proxy-config]"), "{out}");
        assert!(out.contains("proxy.example"), "{out}");
    }

    #[test]
    fn lists_nested_keys_from_every_table() {
        let config = config(
            r#"
tls-root-certs = "webpki"
pinning-strategy = "semver"

[cache]
repodata = "/tmp/repodata"

[pypi-config]
index-url = "https://example.invalid/simple"

[s3-options.my-bucket]
endpoint-url = "https://s3.example.invalid"
region = "eu-central-1"
force-path-style = false
"#,
        );

        for key in [
            "tls-root-certs",
            "pinning-strategy",
            "cache",
            "cache.repodata",
            "pypi-config.index-url",
            "s3-options.my-bucket",
            "s3-options.my-bucket.region",
        ] {
            assert!(
                partial_config(&config, key).unwrap().is_some(),
                "`config list {key}` found no value"
            );
        }
    }

    #[test]
    fn unset_key_lists_nothing() {
        assert!(
            partial_config(&Config::default(), "tls-root-certs")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn unknown_key_is_rejected() {
        let err = partial_config(&Config::default(), "not-a-key").unwrap_err();
        assert!(err.to_string().contains("Unknown key"), "{err}");
    }
}
