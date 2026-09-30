//! ROS: the recipe comes from the package's `package.xml` (RoboStack
//! naming, `ros-<distro>-<name>`, and its rosdep mapping). Used for
//! `{ path = "src/my_pkg/package.xml" }` dependencies, or a `pixi.toml` with
//! this backend next to a `package.xml`.
//!
//! Other ROS packages of the workspace that this one depends on become
//! source dependencies, so a whole colcon workspace builds from source in
//! one graph, in dependency order.
use std::{
    collections::BTreeMap,
    path::{Component, Path, PathBuf},
};

use pixi_build_blaze::{Generated, RecipeContext, RecipeGenerator};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// ROS distribution (default: from a `robostack-<distro>` channel).
    distro: Option<String>,
    /// Extra rosdep → conda mappings (YAML files in RoboStack's format,
    /// relative to the package directory).
    extra_package_mappings: Vec<PathBuf>,
    /// Add `test_depend`s and build the package's tests.
    tests: bool,
}

/// `robostack-humble`, `robostack-jazzy`, ... in the channel list.
fn distro_from_channels<'a>(channels: impl IntoIterator<Item = &'a str>) -> Option<String> {
    channels.into_iter().find_map(|c| {
        let last = c.trim_end_matches('/').rsplit('/').next()?;
        last.strip_prefix("robostack-")
            .filter(|d| !d.is_empty() && !d.contains('-'))
            .map(str::to_string)
    })
}

/// `to` relative to `from` (both absolute), with `/` separators.
fn relative(from: &Path, to: &Path) -> String {
    let (f, t): (Vec<Component>, Vec<Component>) =
        (from.components().collect(), to.components().collect());
    let common = f.iter().zip(&t).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = f[common..].iter().map(|_| "..".to_string()).collect();
    parts.extend(
        t[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    }
}

/// Does `dir` have a `pixi.toml` with a `[package]` table?
fn has_pixi_package(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join("pixi.toml"))
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .is_some_and(|t| t.contains_key("package"))
}

struct Ros;

impl RecipeGenerator for Ros {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let err = |e: anyhow::Error| miette::miette!("{e:#}");
        let dir = std::fs::canonicalize(cx.source_dir)
            .map_err(|e| miette::miette!("{}: {e}", cx.source_dir.display()))?;
        let xml = dir.join("package.xml");
        let text = std::fs::read_to_string(&xml)
            .map_err(|e| miette::miette!("reading {}: {e}", xml.display()))?;
        let p = blaze_ros::PackageXml::parse(&text).map_err(err)?;
        let distro = c
            .distro
            .clone()
            .or_else(|| distro_from_channels(cx.params.channels.iter().map(|c| c.as_str())))
            .ok_or_else(|| {
                miette::miette!(
                    "no ROS distro: add a `robostack-<distro>` channel, or set \
                     `distro` in [package.build.config]"
                )
            })?;
        let mut o = blaze_ros::Options::new(&distro, cx.params.host_platform.as_str());
        o.tests = c.tests;
        for m in &c.extra_package_mappings {
            let path = dir.join(m);
            let yaml = std::fs::read_to_string(&path)
                .map_err(|e| miette::miette!("reading {}: {e}", path.display()))?;
            o.mapping.extend_from_yaml(&yaml).map_err(err)?;
        }
        let r = blaze_ros::recipe(&p, &dir, &o).map_err(err)?;

        // ROS packages of the same workspace are built from source.
        let mut source_dependencies = BTreeMap::new();
        if let Some(ws) = cx.workspace_dir {
            let siblings: BTreeMap<String, PathBuf> = blaze_ros::discover(ws)
                .map_err(err)?
                .into_iter()
                .map(|(d, sp)| (blaze_ros::conda_name(&distro, &sp.name), d))
                .collect();
            let reqs = &r.requirements;
            for spec in reqs.build.iter().chain(&reqs.host).chain(&reqs.run) {
                let name = spec.split_whitespace().next().unwrap_or(spec);
                if name == r.package.name {
                    continue;
                }
                if let Some(d) = siblings.get(name) {
                    let d = std::fs::canonicalize(d).unwrap_or_else(|_| d.clone());
                    // The same source as the workspace names it: the
                    // package's pixi manifest if it has one, else its
                    // package.xml.
                    let rel = relative(&dir, &d);
                    let location = if has_pixi_package(&d) {
                        rel
                    } else {
                        format!("{rel}/package.xml")
                    };
                    source_dependencies.insert(name.to_string(), location);
                }
            }
        }
        let mut input_globs = vec!["package.xml".to_string(), "setup.cfg".into()];
        input_globs.extend(
            c.extra_package_mappings
                .iter()
                .map(|m| m.to_string_lossy().replace('\\', "/")),
        );
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs,
            source_dependencies,
        })
    }
}

fn main() {
    pixi_build_blaze::main(Ros)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distro_from_robostack_channel() {
        assert_eq!(
            distro_from_channels([
                "https://prefix.dev/conda-forge",
                "https://prefix.dev/robostack-jazzy/"
            ]),
            Some("jazzy".into())
        );
        assert_eq!(
            distro_from_channels(["https://prefix.dev/conda-forge"]),
            None
        );
    }

    #[test]
    fn relative_paths() {
        assert_eq!(
            relative(Path::new("/ws/a/b"), Path::new("/ws/c")),
            "../../c"
        );
        assert_eq!(relative(Path::new("/ws/a"), Path::new("/ws/a/x")), "x");
        assert_eq!(relative(Path::new("/ws"), Path::new("/ws")), ".");
    }
}
