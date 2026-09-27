//! Steps: no meta build system at all. The package's `[package.steps]`
//! (`configure`, `compile`, `install`) *are* the build; they install into
//! `$PREFIX`, and every compiler call inside them is a cached action. For
//! bespoke builds (Makefiles, code generators, emscripten, ...).
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure_compilers, recipe::Generator,
};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Languages to add compilers for (default: none).
    compilers: Vec<String>,
}

struct Steps;

impl RecipeGenerator for Steps {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut r = base_recipe(cx, Generator::Steps)?;
        let langs: Vec<&str> = c.compilers.iter().map(String::as_str).collect();
        ensure_compilers(&mut r.requirements.build, &langs);
        Ok(r.into())
    }
}

fn main() {
    pixi_build_blaze::main(Steps)
}
