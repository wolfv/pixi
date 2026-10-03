# pixi-build-blaze examples

Workspaces for pixi with the embedded rattler-blaze build engine (preview
`pixi-build-blaze`): every compile, link, test and task is a cached action
that pixi's compute engine runs. The design and the full docs are in
[rattler-blaze's pixi/README.md](https://github.com/prefix-dev/rattler-blaze/tree/pixi-integration/pixi) (private for now).

Get it from the experiments channel (linux-64, osx-arm64):

```bash
pixi global install -c https://prefix.dev/wolfv/experiments -c conda-forge pixi-blaze
```

`pixi-blaze` is pixi with the recipe backends bundled (it maps
`pixi-build-cmake`, `-rust`, `-python`, ... to them).

| workspace | platforms | what it shows |
|---|---|---|
| [`example/`](example) | linux, macOS, Windows | 5 packages: libgreet and greet-cli (CMake; a source dependency; a `configure` override), rgreet (Rust), pygreet (Python with a C extension), hello-steps (`[package.steps]`); a `check` task depending on `//test`; a `lint` environment |
| [`example-mojo/`](example-mojo) | linux, macOS (arm64) | Mojo packages, programs and tests, discovered by the backend |
| [`example-ros/`](example-ros) | linux | ros2/demos (21 packages, `./fetch.sh` first) and `pendulum_report`, straight from `package.xml`. To try on macOS, add `"osx-arm64"` to `platforms` (untested). |

```bash
cd example
pixi-blaze task list              # tasks, and every package's targets and tasks
pixi-blaze run //test             # build and test every package, one graph
pixi-blaze run check              # a workspace task that depends on //test
pixi-blaze run libgreet//lint     # a package task, in the `lint` environment
pixi-blaze task explain greet-cli//configure
pixi-blaze install && pixi-blaze run demo
```

Edit a file (e.g. `packages/libgreet/src/greet.c`) and run `pixi-blaze run
//build` again: only its compile and what depends on it re-run.
