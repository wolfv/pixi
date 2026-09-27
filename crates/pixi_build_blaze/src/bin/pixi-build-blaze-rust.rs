//! Cargo: `generator: cargo`; every rustc/cc call is a cached action.
//! Name and version default to Cargo.toml's `[package]`.
use std::collections::BTreeMap;

use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure, ensure_compilers,
    recipe::{Generator, InBuildTests, Test},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Extra `cargo install` arguments (e.g. `--features foo`).
    extra_args: Vec<String>,
    env: BTreeMap<String, String>,
    /// Build and run `cargo test` binaries (one cached action per binary).
    tests: bool,
    /// Add default `fmt` / `lint` tasks (overridable in [package.tasks]).
    default_tasks: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            extra_args: Vec::new(),
            env: BTreeMap::new(),
            tests: true,
            default_tasks: true,
        }
    }
}

struct Rust;

impl RecipeGenerator for Rust {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut model = cx.model.clone();
        let cargo_toml = cx.source_dir.join("Cargo.toml");
        if let Ok(text) = std::fs::read_to_string(&cargo_toml)
            && let Ok(v) = text.parse::<toml::Table>()
            && let Some(pkg) = v.get("package")
        {
            if model.name.is_none() {
                model.name = pkg.get("name").and_then(|n| n.as_str()).map(String::from);
            }
            if model.version.is_none() {
                model.version = pkg
                    .get("version")
                    .and_then(|n| n.as_str())
                    .and_then(|v| v.parse().ok());
            }
        }
        let cx = RecipeContext {
            model: &model,
            ..*cx
        };
        let mut r = base_recipe(&cx, Generator::Cargo)?;
        r.build.cargo.args = c.extra_args.clone();
        r.build.cargo.env = c.env.clone();
        ensure_compilers(&mut r.requirements.build, &["c"]); // linker
        ensure(&mut r.requirements.build, "rust");
        if c.tests {
            r.tests.push(Test::InBuild {
                ctest: InBuildTests::default(),
            });
        }
        if c.default_tasks {
            pixi_build_blaze::add_rust_tasks(&mut r);
        }
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs: vec!["Cargo.toml".into()],
        })
    }
}

fn main() {
    pixi_build_blaze::main(Rust)
}
