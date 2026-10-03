//! Python (PEP 517 via pip): `generator: python`. C/C++ extensions compile
//! through blaze's compiler shim (setuptools) or ninja shim (scikit-build-core,
//! meson-python), one cached action each. Name/version default to
//! pyproject.toml's `[project]`.
use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure, ensure_compilers,
    recipe::{Generator, Test, TestRequirements},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Add default `fmt` / `lint` tasks (overridable in [package.tasks]).
    default_tasks: bool,
    /// Run `tests/` against the installed package (pytest if the project
    /// uses it, else unittest) as part of `<pkg>//test`.
    tests: bool,
    /// Languages to add compilers for (`[]` for pure Python).
    compilers: Option<Vec<String>>,
    /// Extra `pip install` arguments (e.g. `-Ccmake.define.FOO=ON`).
    extra_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            default_tasks: true,
            tests: true,
            compilers: None,
            extra_args: Vec::new(),
        }
    }
}

struct Python;

impl RecipeGenerator for Python {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut model = cx.model.clone();
        if let Ok(text) = fs_err::read_to_string(cx.source_dir.join("pyproject.toml"))
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
        if c.tests
            && let Some(test) = default_test(cx.source_dir)
        {
            r.tests.push(test);
        }
        if c.default_tasks {
            pixi_build_blaze::add_ruff_tasks(&mut r);
            if langs.iter().any(|l| *l == "c" || *l == "cxx") {
                pixi_build_blaze::add_clang_format_tasks(&mut r, cx.source_dir);
            }
        }
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs: vec!["pyproject.toml".into()],
            source_dependencies: Default::default(),
        })
    }
}

/// A package test running `tests/` against the installed package.
fn default_test(dir: &std::path::Path) -> Option<Test> {
    let tests = dir.join("tests");
    let files: Vec<std::path::PathBuf> = fs_err::read_dir(&tests)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "py"))
        .collect();
    if files.is_empty() {
        return None;
    }
    let pyproject = fs_err::read_to_string(dir.join("pyproject.toml")).unwrap_or_default();
    // unittest only when the suite is written for it (TestCase classes);
    // plain `def test_*()` functions need pytest.
    let uses_unittest = files
        .iter()
        .any(|f| fs_err::read_to_string(f).is_ok_and(|t| t.contains("unittest")));
    let uses_pytest = pyproject.contains("[tool.pytest")
        || dir.join("pytest.ini").exists()
        || dir.join("conftest.py").exists()
        || !uses_unittest;
    let (script, reqs) = if uses_pytest {
        ("python -m pytest -q tests", vec!["pytest".to_string()])
    } else {
        ("python -m unittest discover -s tests -v", Vec::new())
    };
    Some(Test::Package {
        script: vec![script.into()],
        requirements: TestRequirements { run: reqs },
        files: vec!["tests/**".into()],
    })
}

fn main() {
    pixi_build_blaze::main(Python)
}
