# cargo-sycamore

A starting point for a Cargo subcommand for building [Sycamore](https://sycamore.dev) applications.

## Install locally

With a Rust toolchain supporting edition 2024 installed:

```sh
cargo install --path .
cargo sycamore --help
cargo sycamore --version
```

The binary also supports direct invocation as `cargo-sycamore`.

## Building an application

Install the Rust WASM target once, then run from your app's directory:

```sh
rustup target add wasm32-unknown-unknown
cargo sycamore build
cargo sycamore build --release
```

Options:

```sh
cargo sycamore build --manifest-path path/to/Cargo.toml
cargo sycamore build --package my-app --bin client --out-dir public
cargo sycamore build --help
```

Settings can also come from `Sycamore.toml`; see [Configuration](#configuration).

The build command:

1. Selects a workspace package using Cargo's default members (or `--package`).
2. Builds its `cdylib` library, or its sole binary, for `wasm32-unknown-unknown`.
   Use `--bin` to explicitly select a binary when needed.
3. Reads the exact `wasm-bindgen` dependency version from the workspace's
   `Cargo.lock` **after** Cargo creates/updates it.
4. Automatically downloads/caches that version of `wasm-bindgen-cli` and runs
   it with `--target web` on the actual WASM artifact reported by Cargo.

Generated HTML, JavaScript, and WASM go into the package's `dist/` directory by default.
TypeScript declarations are disabled with wasm-bindgen's `--no-typescript` flag. An explicit `--out-dir` is relative to the current
working directory. Custom Cargo target directories are supported.

The app must depend (directly or transitively) on a released registry version of
`wasm-bindgen`. Missing dependencies, multiple locked `wasm-bindgen` versions,
and path/git overrides produce an error rather than guessing an incompatible
CLI version. Multiple versions anywhere in the workspace lockfile must currently
be unified, even if only one is used by the selected app.

Only `wasm-bindgen-cli` is downloaded by `build` for now. The command generates
an HTML entrypoint and WASM web bindings, but does not bundle/copy CSS or other
static assets. Use `serve` for the development server. Existing files in the output directory
are not cleaned. Set `GITHUB_TOKEN` if needed for GitHub API
rate limits; it is passed to the downloader for API requests only.

### HTML entrypoint

Every successful build writes `index.html` into the output directory. If an
`index.html` exists beside the selected package's `Cargo.toml`, it is used as the
template; otherwise the bundled `templates/index.html` is used. In a workspace,
this means the selected app package's root, not necessarily the workspace root.

The template is parsed as HTML5 using `scraper` (backed by `html5ever`), and a
`<script type="module">` is appended to
`<head>` to import the generated JavaScript and initialize WASM automatically:

```html
<script type="module" data-cargo-sycamore>
import init from "./my_app.js";
init().catch(error => console.error("Failed to initialize WASM:", error));
</script>
```

The import filename matches the actual compiled WASM artifact. You don't need to
add placeholders or write startup JavaScript. Missing `<head>`/`<body>` elements
are supplied by the parser, and user content/scripts are preserved (formatting
may be normalized). A previous `script[data-cargo-sycamore]` is replaced rather
than duplicated; that marker is reserved for generated startup scripts.

The source template is never overwritten. Choosing an output path that would
overwrite it produces an error. Changes to the app's `index.html`, including its
creation or deletion, trigger regeneration in watch mode. Serve the output using
an HTTP server rather than opening it as a `file://` URL.

## Watching for changes

```sh
cargo sycamore watch
cargo sycamore watch --release
cargo sycamore watch --package my-app --out-dir public
cargo sycamore watch --config path/to/Sycamore.toml
```

`watch` accepts the same flags and configuration as `build`. It builds immediately,
then watches the entire Cargo workspace recursively. File creations, edits,
deletions, and renames trigger a rebuild after a 250 ms quiet period. Builds run
one at a time; edits during a build schedule a follow-up build rather than being
lost. Compile errors are printed, and watching continues so fixing the source
triggers another attempt. Stop with **Ctrl-C**.

Cargo's target directory, each workspace package's default `dist/`, the configured
output directory, and `.git` paths are ignored to avoid self-triggered rebuilds.
Use a dedicated output directory, not the workspace root. Ignored paths are not
available as watch inputs.

Common editor artifacts are excluded from rebuild triggers and change logs:

- Vim: `4913`, `*~`, and hidden swap files (`.*.swp` through `.*.swa`, plus `.swp`).
- Emacs: `*~`, `#*#` autosaves, and `.#*` lock files.
- JetBrains: `*___jb_tmp___`, `*___jb_old___`, and `*___jb_bak___`.
- Kate: `*.kate-swp`.
- GIO/GNOME editors: `.goutputstream-*` atomic-save files.

Atomic saves that rename a temporary file onto a real source file still trigger
rebuilds and log the real destination. Ordinary dotfiles, `*.tmp`, and `*.bak`
inputs are not broadly excluded. An explicitly loaded config file remains watched
regardless of its filename.

The active `Sycamore.toml` is watched even when outside the workspace. Options are
resolved again on each rebuild, so config changes take effect without restarting;
invalid config is reported while watching continues. Watch roots and output
exclusions are updated when valid configuration changes the project or output
path. Local dependencies outside the workspace and other external assets are not
watched. Native filesystem watcher errors are fatal rather than silently missing
future edits.

## Development server

```sh
cargo sycamore serve
cargo sycamore serve --port 9000
cargo sycamore s --port 9000 # shorthand for serve
cargo sycamore serve --release --package my-app --out-dir public
```

`serve` binds to **http://127.0.0.1:8080** by default and serves the selected app's
`dist/` directory (or the output directory configured via CLI/TOML). It starts the
same initial build and file watcher as `watch`, including automatic tool downloads,
change logging, debounce, config reloads, and recovery from compilation errors.
Requests return 404 until output exists; after a build failure, any existing output
remains available. Refresh the browser to load rebuilt files—automatic browser
reload is not implemented. Stop the server and watcher with **Ctrl-C**.

Configure the port in `Sycamore.toml`:

```toml
[serve]
port = 9000
```

`--port` overrides `[serve].port`; otherwise the default is 8080. The port is fixed
at startup, so restart `serve` after changing it. Port 0 asks the OS for a free
port, which is printed at startup. All regular build flags are supported too.
Successful rebuilds update the served directory if build configuration changes it.

The server supports GET/HEAD, directory `index.html`, and MIME types including
`application/wasm`. Responses disable caching for development. Traversal and
symlinks pointing outside the output directory are rejected. There is no directory
listing or SPA-route fallback. This is a **local development server**, not a
production server; it deliberately listens only on loopback.

## Configuration

Create a `Sycamore.toml` in your project (a commented template is included here):

```toml
[build]
# May also be supplied as CLI flags:
manifest-path = "Cargo.toml"
package = "my-app"
# bin = "client"
out-dir = "dist"

# Config only; these are not CLI flags:
features = ["csr"]
default-features = true
```

The precedence is **explicit CLI flags → configuration → built-in defaults**.
For example, `cargo sycamore b --out-dir public --release` overrides the configured
output directory and selects the release profile.

| Option | CLI | `Sycamore.toml` (`[build]`) | Default |
| --- | --- | --- | --- |
| Manifest | `--manifest-path` | `manifest-path` | Cargo's current project |
| Package | `--package` / `-p` | `package` | Cargo's default members |
| Binary | `--bin` | `bin` | cdylib or sole binary |
| Output | `--out-dir` | `out-dir` | `<package>/dist` |
| Features | — | `features` | `[]` |
| Default features | — | `default-features` | `true` |
| Release profile | `--release` | — | debug |
| Config file | `--config` | — | nearest `Sycamore.toml` |

Server settings live under `[serve]`: `port` corresponds to the serve-only
`--port` flag and defaults to 8080.

Configuration is optional. Discovery starts in the directory containing the
CLI-supplied `--manifest-path`, or the current directory when that flag is absent,
then searches ancestors for the nearest `Sycamore.toml`. Only that file is loaded;
parent and child files are not layered. Use `--config path/to/Sycamore.toml` to
select a file explicitly (a missing explicit file is an error).

CLI paths, including `--config`, are relative to your current working directory.
Paths written **inside** the config are relative to the directory containing that
file. A config file doesn't change the current project unless it sets
`manifest-path`. Without an output override, output still defaults to the selected
package's `dist/` directory.

Unknown keys, malformed values, and CLI-only keys such as `release` in the config
are errors. Set `default-features = false` to pass `--no-default-features` to Cargo;
`features` is passed as Cargo's feature list.

`src/options.rs` keeps three separate types:

- `BuildArgs` / `ServeArgs`: clap CLI input, preserving absent flags as `Option` values.
- `Config` / `BuildConfig` / `ServeConfig`: the supported TOML schema, without CLI-only fields.
- `Options`: the final merged values consumed by commands, independent
  of where each value came from.

## Other commands

```sh
cargo sycamore new my-app
cargo sycamore new --help
```

`new <PATH>` is currently a placeholder: it reports that project generation is
not implemented, exits with a failure status, and does not create any files.

## Development

```sh
cargo run -- --help
cargo run -- sycamore --help
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

CLI definitions and argument-parsing tests live in `src/cli.rs`; command dispatch
lives in `src/main.rs`.

## Downloading build tools

The library's `tools` module downloads version-pinned GitHub release assets on
first use and returns cached executable paths on subsequent calls:

```rust,no_run
use cargo_sycamore::tools::{BuiltinTool, ToolCache};

fn main() -> anyhow::Result<()> {
    let cache = ToolCache::system()?;
    // Use the same version as the application's wasm-bindgen crate.
    let bindgen = cache.ensure(&BuiltinTool::WasmBindgenCli, "0.2.100")?;
    let wasm_opt = cache.ensure(&BuiltinTool::WasmOpt, "123")?;
    let tailwind = cache.ensure(&BuiltinTool::TailwindCss, "4.0.0")?;
    println!("{}\n{}\n{}", bindgen.display(), wasm_opt.display(), tailwind.display());
    Ok(())
}
```

- Supports x86-64 and ARM64 Linux/macOS, plus x86-64 Windows. Binaryen also
  supports ARM64 Windows; other unsupported combinations return an error.
- Host OS, architecture, and libc select the asset. Binaryen's Linux builds
  require glibc. A configured asset must actually exist in the requested release;
  historical releases may have fewer platforms.
- `ToolCache::new(path)` selects a custom cache directory. The default is the
  OS user cache directory under `cargo-sycamore/tools`.
- Downloads are blocking, use HTTPS and timeouts, and have size limits. HTTP,
  missing-asset, checksum, and extraction failures are returned with context.
- SHA-256 digests from GitHub are verified when present. Older releases without
  a digest rely on HTTPS unless a trusted `Release::sha256` is supplied. Cached
  files are not rehashed on every use; the cache must be writable only by trusted
  users.
- Per-release locks prevent concurrent downloads, and installations are published
  only after extraction succeeds. Tar archives retain runtime libraries, but
  links, special entries, and unsafe paths are rejected. Downloaded tools are
  **not** executed automatically or added to `PATH`.
- Pass a GitHub token explicitly with `cache.with_github_token(token)` to increase
  API rate limits. Tokens are sent only to GitHub's API and are not cached.

To add another tool, implement `Tool::release(version, platform)` to return a
`Release`, or call `ToolCache::ensure_release(&release)` directly. Release definitions
specify the repository, exact tag, asset name, executable path, optional checksum,
and format (`ArchiveFormat::Binary` or `ArchiveFormat::TarGz`). No downloader
changes are needed for another tool using either format.

Tests use local synthetic assets and do not need network access. An optional
GitHub smoke test downloads all three tools without executing them:

```sh
cargo test --lib downloads_builtin_releases -- --ignored
```
