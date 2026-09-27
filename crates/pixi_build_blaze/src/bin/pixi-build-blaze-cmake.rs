//! CMake: `generator: cmake` + compilers, cmake, ninja; CTest tests in-build.
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure, ensure_compilers,
    recipe::{Generator, InBuildTests, Test},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Extra `cmake` configure arguments.
    extra_args: Vec<String>,
    build_type: String,
    /// Languages to add compilers for.
    compilers: Vec<String>,
    /// Run CTest tests (one cached action per test).
    ctest: bool,
    /// Add default `fmt` / `lint` tasks (overridable in [package.tasks]).
    default_tasks: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            extra_args: Vec::new(),
            build_type: "Release".into(),
            compilers: vec!["c".into(), "cxx".into()],
            ctest: true,
            default_tasks: true,
        }
    }
}

struct CMake;

impl RecipeGenerator for CMake {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut r = base_recipe(cx, Generator::Cmake)?;
        r.build.cmake.args = c.extra_args.clone();
        r.build.cmake.build_type = c.build_type.clone();
        let langs: Vec<&str> = c.compilers.iter().map(String::as_str).collect();
        ensure_compilers(&mut r.requirements.build, &langs);
        ensure(&mut r.requirements.build, "cmake");
        ensure(&mut r.requirements.build, "ninja");
        if c.ctest {
            r.tests.push(Test::InBuild {
                ctest: InBuildTests::default(),
            });
        }
        if c.default_tasks {
            pixi_build_blaze::add_clang_format_tasks(&mut r, cx.source_dir);
        }
        Ok(r.into())
    }
}

fn main() {
    pixi_build_blaze::main(CMake)
}
