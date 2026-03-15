# Artifact Cache

The artifact cache is a remote build cache for pixi source packages, hosted on [prefix.dev](https://prefix.dev).
When your project depends on source packages (packages built from source during `pixi install`), the artifact cache
lets you share built artifacts across machines, so each package only needs to be built once.

## How it works

```
pixi install
  └─ source package needed
       ├─ local cache hit? → use it
       ├─ remote cache hit? → download & use it
       └─ cache miss? → build locally → upload to remote cache
```

1. When pixi encounters a source package, it computes a **cache key** from the build inputs (package name, version,
   platform, channels, build variants, and virtual packages).
2. It checks the **local build cache** first (under `.pixi/builds/`).
3. If no local cache hit, it checks the **remote artifact cache** on prefix.dev.
4. If the artifact is found remotely, it downloads the pre-built `.conda` file — skipping the entire build.
5. If no remote hit either, the package is built locally. The result is then uploaded to the remote cache in the
   background so future builds (on this or other machines) can reuse it.

## Setup

### 1. Authenticate with prefix.dev

If you haven't already, log in to prefix.dev:

```bash
pixi auth login --token <your-api-key> https://prefix.dev
```

You can generate an API key in your [prefix.dev account settings](https://prefix.dev).

### 2. Configure the artifact cache

Add the following to your pixi configuration. You can set this globally (recommended for teams) or per-project:

=== "Global (recommended)"

    ```bash
    # ~/.pixi/config.toml (or platform equivalent)
    ```

    ```toml
    [artifact-cache]
    url = "https://prefix.dev"
    owner = "my-org"
    ```

=== "Per-project"

    ```bash
    # your_project/.pixi/config.toml
    ```

    ```toml
    [artifact-cache]
    url = "https://prefix.dev"
    owner = "my-org"
    ```

Replace `my-org` with your prefix.dev username or organization name. The cache is scoped per-owner, meaning all
projects under the same owner share the same cache.

### 3. Use pixi as normal

No changes to your workflow are needed. `pixi install` will automatically check and populate the remote cache:

```bash
pixi install  # builds source packages, uploads artifacts in the background
```

On the next machine (or a CI runner), the same command will download pre-built artifacts instead of rebuilding:

```bash
pixi install  # downloads cached artifacts, skips building
```

## Configuration reference

| Option   | Type     | Required | Default | Description                                        |
|----------|----------|----------|---------|----------------------------------------------------|
| `url`    | `string` | yes      | —       | Base URL of the artifact cache server               |
| `owner`  | `string` | yes      | —       | User or organization name on the cache server       |
| `upload` | `bool`   | no       | `true`  | Whether to upload built artifacts to the remote cache |

Full example:

```toml title="config.toml"
[artifact-cache]
url = "https://prefix.dev"
owner = "my-org"
upload = true
```

## CI usage

### GitHub Actions

```yaml
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4

      - uses: prefix-dev/setup-pixi@v0.8.0

      - name: Authenticate with prefix.dev
        run: pixi auth login --token ${{ secrets.PREFIX_API_KEY }} https://prefix.dev

      - name: Configure artifact cache
        run: pixi config set artifact-cache.url https://prefix.dev --global
              && pixi config set artifact-cache.owner my-org --global

      - name: Install (uses remote cache)
        run: pixi install
```

### Read-only CI runners

If you want CI runners to only consume cached artifacts (not upload new ones), set `upload = false`:

```toml
[artifact-cache]
url = "https://prefix.dev"
owner = "my-org"
upload = false
```

This is useful for pull request CI jobs that shouldn't modify the shared cache.

## Cache key

The cache key is deterministic and includes:

- Package name, version, and target platform
- Build string and build variants
- Channel URLs used for dependency resolution
- Host and build virtual packages (e.g., `__glibc`, `__cuda`)

This means the same source package built with the same inputs will always produce the same cache key, regardless of
which machine performs the build.

## Security

- The artifact cache is **per-owner** — only authenticated users belonging to the owner can read from or write to it.
- Authentication uses the same API keys as channel access (`pixi auth login`).
- Artifacts are stored in a dedicated storage bucket, separate from channel packages.
- SHA256 checksums are verified on both upload and download.

## Limitations

- The artifact cache is a **premium feature** on prefix.dev. Contact [prefix.dev](https://prefix.dev) to enable it for your account.
- Only `.conda` source package builds are cached. Pre-built binary packages from channels are not affected.
- Cache entries are automatically cleaned up after 90 days of inactivity.
