//! Go: `generator: go` (GOCACHEPROG backed by blaze's CAS).
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure,
    recipe::{Generator, InBuildTests, Test},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    packages: Vec<String>,
    ldflags: Option<String>,
    tests: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            packages: vec![".".into()],
            ldflags: None,
            tests: true,
        }
    }
}

struct Go;

impl RecipeGenerator for Go {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut r = base_recipe(cx, Generator::Go)?;
        r.build.go.packages = c.packages.clone();
        r.build.go.ldflags = c.ldflags.clone();
        ensure(&mut r.requirements.build, "go");
        if c.tests {
            r.tests.push(Test::InBuild {
                ctest: InBuildTests::default(),
            });
        }
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs: vec!["go.mod".into(), "go.sum".into()],
        })
    }
}

fn main() {
    pixi_build_blaze::main(Go)
}
