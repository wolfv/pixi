//! Meson: `generator: meson` + compilers, meson, ninja; `meson test` in-build.
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure, ensure_compilers,
    recipe::{Generator, InBuildTests, Test},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    extra_args: Vec<String>,
    build_type: String,
    compilers: Vec<String>,
    tests: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            extra_args: Vec::new(),
            build_type: "release".into(),
            compilers: vec!["c".into(), "cxx".into()],
            tests: true,
        }
    }
}

struct Meson;

impl RecipeGenerator for Meson {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut r = base_recipe(cx, Generator::Meson)?;
        r.build.meson.args = c.extra_args.clone();
        r.build.meson.build_type = c.build_type.clone();
        let langs: Vec<&str> = c.compilers.iter().map(String::as_str).collect();
        ensure_compilers(&mut r.requirements.build, &langs);
        ensure(&mut r.requirements.build, "meson");
        ensure(&mut r.requirements.build, "ninja");
        if c.tests {
            r.tests.push(Test::InBuild {
                ctest: InBuildTests::default(),
            });
        }
        Ok(r.into())
    }
}

fn main() {
    pixi_build_blaze::main(Meson)
}
