# Migrating `pixi_config` onto `rattler_config`

`rattler_config` (in the `rattler` repo) provides an extensible
`ConfigBase<T>` struct that already covers most of what `pixi_config`
currently re-implements. The plan is to gradually replace pixi's bespoke
config types with the upstream ones, then collapse `pixi_config::Config`
to a typealias for `ConfigBase<PixiConfigExt>`.

## Iteration setup

While the migration is in flight, `rattler_config` is path-patched in the
workspace `Cargo.toml`:

```toml
[patch.crates-io]
rattler_config = { path = "../rattler/crates/rattler_config" }
```

Anything missing in `rattler_config` should be added upstream there, not
worked around in pixi. Remove the patch once a release covers everything
pixi depends on.

## Migration plan

Field-by-field swap, smallest reviewable PRs:

1. **PR 1 (this one)** — add the dependency + path patch + this doc +
   inline `TODO(rattler-config)` markers. No behavior change.
2. **Per-field PRs** — replace one pixi type at a time with the rattler
   equivalent: `S3Options` → `S3OptionsMap`, `RepodataConfig` → upstream
   `RepodataConfig`, `ConcurrencyConfig` → upstream, `ProxyConfig` →
   upstream, `BuildConfig` → upstream, `RunPostLinkScripts` → upstream.
   Each PR adjusts the call sites that touch the swapped field.
3. **Structural collapse** — once the overlapping fields are all upstream
   types, replace the outer `Config` struct with
   `pub type Config = rattler_config::ConfigBase<PixiConfigExt>;` and
   move the remaining pixi-only fields into `PixiConfigExt`.

## Deferred: needs upstream/alignment work first

(None at the moment — `LinkConfig` resolved via deprecation in PR 8.)

### Architectural finding: `#[serde(flatten)]` + `serde_ignored`

While prototyping the `LinkConfig` adoption we hit a hard
serde-ergonomic limitation. When any field on the outer struct uses
`#[serde(flatten)]`, **`serde_ignored` stops reporting unknown
top-level keys** — flatten absorbs unmatched keys into an internal
buffer that `serde_ignored` cannot inspect.

This matters for the eventual structural collapse:
`ConfigBase<T>` flattens its `extensions: T` field by design. The
moment pixi's `Config` becomes `ConfigBase<PixiConfigExt>`, typo
detection on every `PixiConfigExt` field (and every flattened
sibling such as `RepodataConfig`) silently breaks.

Options before doing the collapse:

1. Accept the regression — silent failure on typos in
   extension/flattened fields.
2. Embed extension-specific fields under named tables
   (e.g. `[pixi]`) — non-breaking only if every flattened sub-config
   stays flattened (which contradicts the option that resolves it).
3. Move ignored-field tracking *into*
   `rattler_config::load_from_files` so the deserializer at that
   layer captures the unknowns and exposes them through a method;
   pixi consumes that instead of wrapping with `serde_ignored`.

`LinkConfig` sidestepped the issue with the deprecation pattern
(option 2 applied to *one* field, with legacy aliases). The same
pattern won't scale to every extension field, so a real decision
is still needed before the structural collapse.

## Pending upstream

Fields currently in `pixi_config::Config` that are not pixi-specific and
should graduate to `rattler_config::ConfigBase` (or to a sibling crate
under the rattler workspace):

- [x] `tls_root_certs: Option<TlsRootCerts>` — TLS root cert selection
  (webpki / system). Promoted in PR 9. See the "Done" section below.
- [ ] `allow_symbolic_links: Option<bool>` — package-install link
  strategy.
- [ ] `allow_hard_links: Option<bool>` — package-install link strategy.
- [ ] `allow_ref_links: Option<bool>` — package-install link strategy
  (copy-on-write).
- [ ] `pinning_strategy: Option<PinningStrategy>` — useful for any tool
  that adds/updates conda dependencies; `rattler_config` already calls
  this out as missing in its module-level TODO.

## Stays in `PixiConfigExt`

Fields that are pixi-specific and will live in the extension struct,
not upstream:

- `pypi_config` — PyPI integration knobs.
- `shell` — pixi shell behavior.
- `experimental` — pixi feature flags.
- `detached_environments` — pixi env layout.
- `cache` (`CacheConfig`) — per-cache-kind redirection used by pixi's
  cache layout. The generic parts (default cache root, netfs detection)
  may be worth promoting later, but the API surface isn't stable.
- `tool_platform` — pixi-build tool resolution.
- CLI structs (`ConfigCli`, `ConfigCliPrompt`, `ConfigCliActivation`).

## Done

- `s3_options` → `rattler_config::S3OptionsMap` (PR 2). Pixi's local
  `S3Options` struct is now a re-export of the upstream one. The
  `pixi_manifest::S3Options` copy (in `crates/pixi_manifest/src/s3.rs`)
  still exists separately because it has `deny_unknown_fields` and lives
  on a different config surface (`[workspace.s3-options]` in
  `pixi.toml`); folding it into the upstream type is deferred to a later
  PR.
  - Upstream change: added a small inherent `S3OptionsMap::is_empty()`
    method to keep `#[serde(skip_serializing_if = ...)]` ergonomic.

- `concurrency` → `rattler_config::ConcurrencyConfig` (PR 3). Pixi's
  local `ConcurrencyConfig` struct plus `default_max_concurrent_solves`
  / `default_max_concurrent_downloads` helpers are now re-exports of
  the upstream items. The merge call in `Config::merge_config` now goes
  through the `rattler_config::config::Config` trait
  (`merge_config(&Self) -> Result<Self, MergeError>`), with an
  `.expect("infallible")` since concurrency merging cannot fail.

- `repodata_config` → `rattler_config::RepodataConfig` (PR 6).
  Required an upstream port: pixi's tolerant `Deserialize` visitor
  (silently ignores unknown top-level keys like deprecated
  `disable-jlap`, surfacing them through `serde_ignored`) was moved
  into `rattler_config` and replaces the default
  flatten-based deserializer. Also dropped `deny_unknown_fields` from
  `RepodataChannelConfig` so per-channel sub-tables tolerate unknown
  keys the same way. Fixed an unrelated bug in
  `RepodataConfig::validate` that rejected the default-empty state.
  - Pixi-side: the foreign `impl From<RepodataChannelConfig> for SourceConfig`
    is now an orphan-rule violation, so it was rewritten as a private
    free function `repodata_channel_to_source(...)`. The two call sites
    in `impl From<&Config> for rattler_repodata_gateway::ChannelConfig`
    were updated.
  - All 29 `rattler_config` tests and all 51 `pixi_config` tests pass.

- `proxy_config` → `rattler_config::ProxyConfig` (PR 7). Adopted
  rattler's eager `Default` (reads `HTTP_PROXY` / `HTTPS_PROXY` /
  `NO_PROXY` env vars into the struct). Pixi keeps its local
  `ENV_*_PROXY` / `USE_PROXY_FROM_ENV` `LazyLock` statics — they're
  used by `get_proxies()` to short-circuit and defer to reqwest's
  env-var handling, and by the post-load warning to detect
  config/env divergence. End result: env vars get read twice per
  process (once cached in rattler, once cached in pixi). That's the
  accepted trade-off per the migration ask.
  - Fixed an unrelated bug in `ProxyConfig::validate` upstream that
    rejected the default-empty state.

- `tls_root_certs` → `rattler_config::config::tls::TlsRootCerts` (PR 9).
  Promoted as a single top-level field on `ConfigBase` (no sub-table)
  next to `tls_no_verify`. Upstream enum is just `Webpki` + `System`;
  legacy spellings `"native"` and `"all"` are accepted as serde
  aliases on `System`. Pixi-side cleanup:
  - Deleted local `TlsRootCerts` enum + `Display` + `FromStr`.
  - Deleted `warn_deprecated_tls_root_certs` (no separate variants
    to detect — legacy values silently resolve to `System`).
  - Simplified `should_use_system_certs_for_uv` and `for_mode` in
    `pixi_utils` to just match on `System` / `Webpki`.
  - **Small behavior change for `"all"`**: previously
    `tls-root-certs = "all"` mapped to `webpki` certs at runtime
    (the variant's documentation said `system`, so the code
    contradicted the doc). Now `"all"` consistently resolves to
    `system`, which is the closer fit for users who originally
    wanted broad trust including corporate CAs. Users who
    specifically wanted webpki-only need to change the value to
    `"webpki"` explicitly.
  - Behavior preserved for `"native"` (always meant `system`).

- `allow_*_links` → `rattler_config::LinkConfig` (PR 8) — adopted via
  deprecation pattern (same shape as the existing `change_ps1` →
  `shell.change-ps1` migration). The new canonical location is the
  `[link-config]` TOML table; the three flat legacy fields are still
  accepted but `from_toml` migrates them into `link_config` and emits
  a deprecation warning per field. Save-load roundtrip drops the
  legacy spelling. `LinkConfig` itself lives upstream as a standalone
  module on the `rattler-config-link-config` branch (not yet wired
  into `ConfigBase` — see the architectural finding below).

- `run_post_link_scripts` → `rattler_config::RunPostLinkScripts`
  (PR 5). Drop-in swap — pixi's enum was byte-for-byte identical
  (variants `Insecure` / `False`, same kebab-case serde, same
  `FromStr` impl). External crates (`pixi_core`, `pixi_global`) match
  on the variants through the re-export and need no edits.

- `build` + `PackageFormatAndCompression` → `rattler_config::BuildConfig`
  (PR 4). About 130 lines deleted from pixi (the duplicated
  `FromStr`/`Serialize`/`Deserialize` impls and the struct). Re-exported
  for back-compat.
  - Upstream change: added a small inherent `BuildConfig::is_default()`
    method (same pattern as `S3OptionsMap::is_empty()` in PR 2) so
    `#[serde(skip_serializing_if = ...)]` resolves to a free path.
  - **Caveat — dual `rattler_conda_types` copies**: rattler_config
    pulls `rattler_conda_types` from the local rattler workspace via
    the path patch; pixi pulls it from crates.io. The dep graph
    therefore contains two copies of `rattler_conda_types` even though
    the versions match. Types from `PackageFormatAndCompression`
    (i.e. `CondaArchiveType`, `CompressionLevel`) **do not unify**
    between pixi's direct imports and the wrapped type. Tests that
    used direct struct construction now compare against the canonical
    `Serialize` string instead. Path-patching `rattler_conda_types`
    itself is **not viable** — the local copy has small API drift
    that breaks crates.io `rattler_repodata_gateway`. The dup will
    resolve naturally once rattler_config is consumed from crates.io
    again.
