//! Passthrough: the package directory has a hand-written rattler-blaze
//! `recipe.yaml`; the backend just hands it (and its variants.yaml) to pixi.
use pixi_build_blaze::{Generated, RecipeContext, RecipeGenerator};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Recipe file relative to the package directory.
    recipe: Option<String>,
}

struct Passthrough;

impl RecipeGenerator for Passthrough {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let rel = c.recipe.as_deref().unwrap_or("recipe.yaml");
        let path = cx.source_dir.join(rel);
        let text = fs_err::read_to_string(&path)
            .map_err(|e| miette::miette!("reading {}: {e}", path.display()))?;
        let variants = pixi_build_blaze::recipe::load_variant_config(
            &cx.source_dir.join("variants.yaml"),
            cx.params.host_platform.as_str(),
        )
        .map_err(|e| miette::miette!("{e:#}"))?;
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Yaml(text),
            variants,
            input_globs: vec![rel.to_string(), "variants.yaml".into()],
            source_dependencies: Default::default(),
        })
    }
}

fn main() {
    pixi_build_blaze::main(Passthrough)
}
