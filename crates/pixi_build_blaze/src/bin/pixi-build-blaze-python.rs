//! Python (PEP 517 via pip): `generator: python`. C/C++ extensions compile
//! through blaze's compiler shim (setuptools) or ninja shim (scikit-build-core,
//! meson-python), one cached action each. Name/version default to
//! pyproject.toml's `[project]`.
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure, ensure_compilers,
    recipe::Generator,
};
use serde::Deserialize;

#[derive(Deserialize, Default)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Languages to add compilers for (`[]` for pure Python).
    compilers: Option<Vec<String>>,
    /// Extra `pip install` arguments (e.g. `-Ccmake.define.FOO=ON`).
    extra_args: Vec<String>,
}

struct Python;

impl RecipeGenerator for Python {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut model = cx.model.clone();
        if let Ok(text) = std::fs::read_to_string(cx.source_dir.join("pyproject.toml"))
            && let Ok(v) = text.parse::<toml::Table>()
            && let Some(project) = v.get("project")
        {
            if model.name.is_none() {
                model.name = project
                    .get("name")
                    .and_then(|n| n.as_str())
                    .map(String::from);
            }
            if model.version.is_none() {
                model.version = project
                    .get("version")
                    .and_then(|n| n.as_str())
                    .and_then(|v| v.parse().ok());
            }
        }
        let cx = RecipeContext {
            model: &model,
            ..*cx
        };
        let mut r = base_recipe(&cx, Generator::Python)?;
        r.build.python.args = c.extra_args.clone();
        let langs: Vec<&str> = c
            .compilers
            .as_ref()
            .map(|l| l.iter().map(String::as_str).collect())
            .unwrap_or_else(|| vec!["c"]);
        ensure_compilers(&mut r.requirements.build, &langs);
        ensure(&mut r.requirements.host, "python");
        ensure(&mut r.requirements.host, "pip");
        ensure(&mut r.requirements.run, "python");
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs: vec!["pyproject.toml".into()],
        })
    }
}

fn main() {
    pixi_build_blaze::main(Python)
}
