//! Mojo: `generator: mojo` (experimental). blaze derives the graph from the
//! `import` lines; this backend only finds the project's parts, so a
//! `pixi.toml` needs no Mojo configuration:
//!
//! - packages: directories with an `__init__.mojo` (outermost only, outside
//!   `test`/`tests`),
//! - programs: other files with a top-level `fn main(` / `def main(`
//!   (`main.mojo` is named after the package),
//! - Python modules: files that define `PyInit_<name>`,
//! - tests: `tests/**/test_*.mojo` (or `test/`), one cached build + run each.
//!
//! Everything can be set explicitly in `[package.build.config]` instead.
use std::path::{Path, PathBuf};

use pixi_build_blaze::{
    Generated, RecipeContext, RecipeGenerator, base_recipe, ensure,
    recipe::{Cmd, Generator, InBuildTests, MojoFile, MojoPackage, Task, TaskDef, Test},
};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", default)]
struct Config {
    /// Package directories (default: discovered).
    packages: Option<Vec<String>>,
    /// Program files (default: discovered).
    binaries: Option<Vec<String>>,
    /// Python extension module files (default: discovered).
    python_modules: Option<Vec<String>>,
    /// Test file globs (default: `tests/**/test_*.mojo`).
    tests: Option<Vec<String>>,
    /// Extra `mojo` arguments.
    extra_args: Vec<String>,
    /// The compiler package.
    compiler: String,
    /// Add `fmt` / `lint` tasks (mblack).
    default_tasks: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            packages: None,
            binaries: None,
            python_modules: None,
            tests: None,
            extra_args: Vec::new(),
            compiler: "mojo-compiler".into(),
            default_tasks: true,
        }
    }
}

const SKIP: &[&str] = &[".git", ".pixi", ".blaze", "target", "build", "node_modules"];

fn is_test_dir(p: &Path) -> bool {
    p.components()
        .any(|c| matches!(c.as_os_str().to_str(), Some("test" | "tests")))
}

/// Everything below `root` that looks like part of a Mojo project.
struct Layout {
    packages: Vec<PathBuf>,
    binaries: Vec<PathBuf>,
    python_modules: Vec<PathBuf>,
    test_dir: Option<&'static str>,
}

fn discover(root: &Path) -> Layout {
    let mut files = Vec::new();
    let mut walk = vec![root.to_path_buf()];
    while let Some(dir) = walk.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if p.is_dir() {
                if !name.starts_with('.') && !SKIP.contains(&name.as_str()) {
                    walk.push(p);
                }
            } else if p.extension().is_some_and(|x| x == "mojo" || x == "🔥") {
                files.push(p.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    files.sort();
    // Outermost package directories.
    let mut packages: Vec<PathBuf> = files
        .iter()
        .filter(|f| f.file_stem().is_some_and(|s| s == "__init__") && !is_test_dir(f))
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .collect();
    packages.sort();
    let outer: Vec<PathBuf> = packages
        .iter()
        .filter(|p| !packages.iter().any(|q| q != *p && p.starts_with(q)))
        .cloned()
        .collect();
    let in_package = |f: &Path| outer.iter().any(|p| f.starts_with(p));
    let mut binaries = Vec::new();
    let mut python_modules = Vec::new();
    for f in &files {
        if in_package(f) || is_test_dir(f) {
            continue;
        }
        let text = std::fs::read_to_string(root.join(f)).unwrap_or_default();
        if text.contains("PyInit_") {
            python_modules.push(f.clone());
        } else if text
            .lines()
            .any(|l| l.starts_with("fn main(") || l.starts_with("def main("))
        {
            binaries.push(f.clone());
        }
    }
    let test_dir = ["tests", "test"]
        .into_iter()
        .find(|d| files.iter().any(|f| f.starts_with(d)));
    Layout {
        packages: outer,
        binaries,
        python_modules,
        test_dir,
    }
}

struct Mojo;

impl RecipeGenerator for Mojo {
    type Config = Config;
    fn generate(&self, cx: &RecipeContext<'_>, c: &Config) -> miette::Result<Generated> {
        let mut r = base_recipe(cx, Generator::Mojo)?;
        let found = discover(cx.source_dir);
        let s = |p: &Path| p.to_string_lossy().replace('\\', "/");
        let pkg_name = r.package.name.clone();
        let m = &mut r.build.mojo;
        m.packages = match &c.packages {
            Some(p) => p.iter().cloned().map(MojoPackage::Path).collect(),
            None => found
                .packages
                .iter()
                .map(|p| MojoPackage::Path(s(p)))
                .collect(),
        };
        let file = |path: String| {
            let stem = Path::new(&path)
                .file_stem()
                .map(|x| x.to_string_lossy().into_owned());
            // `main.mojo` is the package's program.
            let name = (stem.as_deref() == Some("main")).then(|| pkg_name.clone());
            MojoFile { path, name }
        };
        m.binaries = match &c.binaries {
            Some(b) => b.iter().cloned().map(file).collect(),
            None => found.binaries.iter().map(|b| file(s(b))).collect(),
        };
        m.python_modules = match &c.python_modules {
            Some(p) => p
                .iter()
                .cloned()
                .map(|path| MojoFile { path, name: None })
                .collect(),
            None => found
                .python_modules
                .iter()
                .map(|p| MojoFile {
                    path: s(p),
                    name: None,
                })
                .collect(),
        };
        m.tests.include = match &c.tests {
            Some(t) => t.clone(),
            None => found
                .test_dir
                .map(|d| vec![format!("{d}/**/test_*.mojo")])
                .unwrap_or_default(),
        };
        m.args = c.extra_args.clone();
        let has_python = !m.python_modules.is_empty();
        let has_tests = !m.tests.include.is_empty();

        ensure(&mut r.requirements.build, &c.compiler);
        // The Mojo runtime libraries ship with the compiler package.
        ensure(&mut r.requirements.run, &c.compiler);
        if has_python {
            ensure(&mut r.requirements.host, "python");
            ensure(&mut r.requirements.run, "python");
        }
        if has_tests {
            r.tests.push(Test::InBuild {
                ctest: InBuildTests::default(),
            });
        }
        if c.default_tasks {
            // mblack ships with the full `mojo` package.
            r.environments.insert(
                "mblack".into(),
                pixi_build_blaze::recipe::EnvironmentDef {
                    dependencies: pixi_build_blaze::recipe::Dependencies::List(vec!["mojo".into()]),
                },
            );
            let task = |cmd: &str, desc: &str, in_place: bool| {
                TaskDef::Full(Task {
                    cmd: Some(Cmd::Shell(cmd.into())),
                    description: Some(desc.into()),
                    environment: Some("mblack".into()),
                    foreach: Some("**/*.mojo".into()),
                    in_place,
                    ..Default::default()
                })
            };
            r.tasks.entry("lint-mojo".into()).or_insert_with(|| {
                task(
                    "mblack --check --diff --quiet \"{{ input }}\"",
                    "check Mojo formatting (mblack, cached per file)",
                    false,
                )
            });
            r.tasks.entry("fmt-mojo".into()).or_insert_with(|| {
                task(
                    "mblack --quiet \"{{ input }}\"",
                    "format Mojo sources (mblack)",
                    true,
                )
            });
            for (alias, dep) in [("lint", "lint-mojo"), ("fmt", "fmt-mojo")] {
                r.tasks.entry(alias.into()).or_insert_with(|| {
                    TaskDef::Full(Task {
                        depends_on: vec![pixi_build_blaze::recipe::DependsOn::Name(dep.into())],
                        ..Default::default()
                    })
                });
            }
        }
        Ok(Generated {
            recipe: pixi_build_blaze::RecipeSource::Typed(Box::new(r)),
            variants: Default::default(),
            input_globs: vec!["**/*.mojo".into()],
            source_dependencies: Default::default(),
        })
    }
}

fn main() {
    pixi_build_blaze::main(Mojo)
}
