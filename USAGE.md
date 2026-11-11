# Usage

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

Settings can also come from `Sycamore.toml` or `[package.metadata.sycamore]` in
`Cargo.toml`; see [Configuration](#configuration).

The build command:

1. Selects a workspace package using Cargo's default members (or `--package`).
2. Builds its `cdylib` library, or its sole binary, for `wasm32-unknown-unknown`.
   Use `--bin` to explicitly select a binary when needed.
3. Reads the exact `wasm-bindgen` dependency version from the workspace's
   `Cargo.lock` **after** Cargo creates/updates it.
4. Automatically downloads/caches that version of `wasm-bindgen-cli` and runs
   it with `--target web` on the actual WASM artifact reported by Cargo.
5. For release builds, downloads/caches Binaryen's `wasm-opt` and optimizes the
   generated `*_bg.wasm` with `-Oz` before generating HTML or announcing success.

Generated HTML, JavaScript, and WASM go into the package's `dist/` directory by default.
TypeScript declarations are disabled with wasm-bindgen's `--no-typescript` flag. An explicit `--out-dir` is relative to the current
working directory. Custom Cargo target directories are supported.

The app must depend (directly or transitively) on a released registry version of
`wasm-bindgen`. Missing dependencies, multiple locked `wasm-bindgen` versions,
and path/git overrides produce an error rather than guessing an incompatible
CLI version. Multiple versions anywhere in the workspace lockfile must currently
be unified, even if only one is used by the selected app.

Debug builds download `wasm-bindgen-cli`; release builds also download
`wasm-opt` unless disabled. When configured, Tailwind CSS is downloaded and run
in either mode. The command generates an HTML entrypoint and WASM web bindings,
but does not otherwise bundle/copy CSS or other static assets. Use `serve` for the development server. Existing files in the output directory
are not cleaned. Set `GITHUB_TOKEN` if needed for GitHub API
rate limits; it is passed to the downloader for API requests only.

### Release WASM optimization

`--release` enables `wasm-opt` by default, including in `watch`, `serve`, and
example builds. Debug builds never download or run it, even with `--wasm-opt=true`.

```sh
cargo sycamore build --release                      # Optimize with wasm-opt.
cargo sycamore build --release --wasm-opt=false     # Skip optimization.
cargo sycamore s --release --wasm-opt               # Override disabled config.
```

Configuration supports an enable/disable setting and a config-only version pin:

```toml
[tools.wasm-opt]
enable = true
version = "133"
```

CLI `--wasm-opt[=true|false]` overrides `[tools.wasm-opt].enable`; omitting the flag
preserves the configured value (default `true`). The default Binaryen version is
pinned to `133`, not a moving `latest` release. It uses the existing tool cache and
platform support; for platforms without compatible Binaryen binaries, disable it
with `--wasm-opt=false`.

Optimization runs after wasm-bindgen using
`-Oz --all-features --disable-compact-imports` on its generated module, leaving
Cargo's original artifact unchanged. Compact imports are explicitly disabled
because browser runtimes do not yet support that encoding. The optimizer writes to a
temporary file and replaces the module only after success. Download or optimizer
failures fail the build; watch/serve report an error and do not trigger a successful
browser reload. Optimization may increase release rebuild times.

### Tailwind CSS

Enable the standalone Tailwind CSS compiler explicitly in `Sycamore.toml`:

```toml
[tools.tailwindcss]
enable = true
input = "styles/input.css"
# Optional: defaults to index.css inside the resolved build output directory.
output = "index.css"
# Optional: pinned default, not a moving latest release.
version = "4.3.5"
```

The same settings work in Cargo metadata:

```toml
[package.metadata.sycamore.tools.tailwindcss]
enable = true
input = "styles/input.css"
```

- `enable = true` is required to run Tailwind. It defaults to `false`; setting
  `input`, `output`, or `version` alone does not activate it. A warning is printed
  if these options are set while `enable` is false or omitted. There are no
  additional CLI flags. Disabling Tailwind also removes the generated stylesheet
  link from subsequent HTML output.
- The default input is `index.css` beside the selected config file or manifest.
  Create the input file; for Tailwind v4, start with `@import "tailwindcss";`.
- Input paths are relative to the config file (or metadata's `Cargo.toml`), not
  the invocation directory. Absolute input paths are preserved.
- Output paths are always relative to the resolved build output directory and
  default to `index.css`. For example, `output = "output.css"` writes
  `dist/output.css`; `output = "assets/site.css"` writes `dist/assets/site.css`.
  Both follow `--out-dir` / `[build].out-dir` overrides automatically. Absolute
  output paths and `..` components are rejected; do not include the `dist/` prefix.
- Version defaults to **4.3.5**. The matching standalone executable is downloaded
  through the normal tool cache, so npm/Node installation is not required.
- The compiler runs once for each successful Cargo/wasm-bindgen build, including
  examples and watch/serve rebuilds, with its working directory set to the selected
  package root for source scanning. Release builds add `--minify`; debug builds do
  not. Use Tailwind's `@source` directives if you need additional scan locations.
- Missing inputs, download errors, and compiler failures fail the build and prevent
  successful browser reloads. CSS is published atomically, so a failed compiler
  does not replace the previously generated stylesheet.
- Watch mode tracks the configured input even outside the workspace and excludes
  generated CSS and temporary output files to prevent rebuild loops. Other files
  within the workspace also trigger compilation as usual.

The generated HTML automatically includes the compiled stylesheet in `<head>`:

```html
<link rel="stylesheet" href="./index.css" data-cargo-sycamore-tailwind>
```

The URL follows the actual CSS output path, including nested directories, with
path segments URL-encoded. The reserved `data-cargo-sycamore-tailwind` marker
allows replacement/removal without changing user-authored links. No link is added
when Tailwind is disabled. This works with the bundled HTML or a custom template;
the source template is never modified. Remove any manually added Tailwind link
from earlier setups to avoid duplicates. Keep input/source files outside generated
output directories so watch can see edits.

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

### Running Cargo examples

Use `--example <NAME>` to build, watch, or serve a Cargo example in the browser:

```sh
cargo sycamore build --example counter
cargo sycamore w --example counter
cargo sycamore s --example counter
cargo sycamore s --package my-app --example counter --release
```

Examples are selected by Cargo target name: `examples/counter.rs`,
`examples/counter/main.rs`, or a custom `[[example]]` target in `Cargo.toml`.
Binary and `cdylib` examples are supported; Rust-only library example crate types
cannot produce the required WASM executable. Example dev-dependencies are handled
by Cargo, and required features can be enabled with `[build].features`.

You can also select an example in `Sycamore.toml`:

```toml
[build]
example = "counter"
```

`--bin` and `--example` are mutually exclusive, as are their config equivalents.
An explicit CLI target selector overrides the configured target selector, so
`--example counter` can replace a configured `bin` and `--bin client` can replace
a configured `example`. Without an example selection, normal library/binary target
selection is unchanged.

Examples use the same package-root `index.html` template and default package
`dist/` output as normal builds. Use `--out-dir` to keep separate outputs when
switching targets. `serve --example` includes watching and browser autoreload
according to your usual serve settings.

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
remains available. Connected browsers automatically reload after successful builds;
failed builds do not trigger a reload. Stop the server and watcher with **Ctrl-C**.

Configure the port in `Sycamore.toml`:

```toml
[serve]
port = 9000
autoreload = true
spa = false
```

Autoreload defaults to `true`. Set `[serve].autoreload = false` to disable browser
reloads while continuing to watch and rebuild. Override either config value with
`--autoreload` (enable), `--autoreload=true`, or `--autoreload=false` (disable).
Like the port, this setting takes effect when `serve` starts; restart after changing it.
When disabled, HTML is served without a reload client and the reload endpoints are
not registered.

Set `[serve].spa = true` for single-page applications with client-side routing.
Missing GET/HEAD paths fall back to the output directory's `index.html` with a 200
HTML response. Existing files and directory indexes still take precedence; unsafe
paths and reserved `/__sycamore/` endpoints do not fall back. If `index.html` is
missing, the response remains 404. The fallback also applies to asset-like paths
that are missing, not just extensionless routes.

For nested routes, fallback HTML gets `<base href="/">` unless the template already
has a `<base href>`; this keeps relative JS/WASM and asset URLs rooted correctly.
Only the HTTP response is modified, never the source template or files on disk.
SPA fallback works with or without autoreload and defaults to `false`. Use `--spa`
(or `--spa=true`) to enable it, or `--spa=false` to override an enabled config
setting. CLI values take precedence; an omitted flag preserves the config value.
The setting takes effect at server startup (restart after changing it).

`--port` overrides `[serve].port`; otherwise the default is 8080. The port is fixed
at startup, so restart `serve` after changing it. Port 0 asks the OS for a free
port, which is printed at startup. All regular build flags are supported too.
Successful rebuilds update the served directory if build configuration changes it.

The Axum server supports GET/HEAD, directory `index.html`, byte ranges for static
assets, and MIME types including `application/wasm`. Responses disable caching for
development. Traversal and symlinks pointing outside the output directory are rejected.
Host headers are restricted to the local server, and WebSocket connections must
have a matching same-origin browser Origin header.

The reload client is injected into **served HTML responses only**—normal build
output and source templates never contain development-server code. The reserved
paths `/__sycamore/reload.js` and `/__sycamore/reload` provide the client module
and WebSocket endpoint. The client reconnects after disconnections and reloads
when the server restarts or a successful build was missed while disconnected.
Each served page carries its build version to catch rebuilds between loading the
HTML and connecting the socket. All connected tabs reload only after the complete
successful build, including HTML generation, finishes.

While rebuilding, a small bottom-of-page pill warns that the page may be stale.
If compilation or configuration fails, it becomes a red error pill and points you
to the terminal; failed builds never reload the page. New connections receive the
current status too. The indicator uses isolated Shadow DOM styles, doesn't move
page content or capture pointer events, and respects reduced-motion preferences.
All indicator UI lives in `src/reload.js` and is absent when autoreload is disabled.

If your custom HTML has a Content Security Policy, allow the same-origin client
script, local WebSocket connections, and the indicator's inline styles during
development. There is no directory listing; SPA-route fallback is opt-in via
`[serve].spa`. This is a **local development server**, not a
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
| Example | `--example` | `example` | none |
| Output | `--out-dir` | `out-dir` | `<package>/dist` |
| Features | — | `features` | `[]` |
| Default features | — | `default-features` | `true` |
| Release profile | `--release` | — | debug |
| Config file | `--config` | — | nearest config source |

WASM optimization settings live under `[tools.wasm-opt]`:

| Option | CLI | Config key | Default |
| --- | --- | --- | --- |
| Enable release optimization | `--wasm-opt` | `enable` | `true` (release only) |
| Binaryen version | — | `version` | `"133"` |

The old `[build].wasm-opt` and `[build].wasm-opt-version` keys are no longer
accepted; move them to this tools section.

Server settings live under `[serve]`: `port` corresponds to the serve-only
`--port` flag and defaults to 8080. `autoreload` corresponds to `--autoreload` and
defaults to `true`. Both CLI overrides preserve config values when omitted.
`spa` corresponds to the serve-only `--spa` flag and defaults to `false`.

Alternatively, use the same schema inside the package's `Cargo.toml`:

```toml
[package.metadata.sycamore.build]
out-dir = "dist"
features = ["csr"]

[package.metadata.sycamore.tools.wasm-opt]
enable = true
version = "133"

[package.metadata.sycamore.serve]
port = 8080
spa = true
```

The `build`, `serve`, and `tools` tables are nested under `package.metadata.sycamore`;
all settings and validation rules are the same as in `Sycamore.toml`. Relative
paths in metadata are relative to the directory containing that `Cargo.toml`.
Changes to the selected manifest are picked up by watch/serve just like changes
to a standalone config.

Configuration is optional. An explicit `--config path/to/Sycamore.toml` selects
that file and bypasses automatic discovery (a missing explicit file is an error).
Otherwise:

- With CLI `--package <NAME>`, Cargo metadata locates that workspace package and
  discovery starts at its root, allowing that package's standalone config or
  Cargo metadata to take precedence over ancestor configs.
- Without `--package`, discovery starts in the directory containing the
  CLI-supplied `--manifest-path`, or the current directory when it is absent.

At each directory, discovery checks `Sycamore.toml` first, then
`[package.metadata.sycamore]` in `Cargo.toml`, before searching the parent.
If both sources exist in the same directory, a warning identifies the selected
standalone file and the ignored metadata. Ignored metadata is not validated as
Sycamore configuration. A manifest without Sycamore metadata does not stop the
search. Only one source is loaded; sources are not layered, and CLI options still
override its values. A virtual workspace does not automatically search its members;
use `--package` to select a member's config. Package discovery uses the CLI manifest
path or current Cargo project;
a package set inside a config does not trigger another round of discovery.
Unknown packages and malformed selected configs are errors, not reasons to fall
back to another config.

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
- `Config` / `BuildConfig` / `ServeConfig` / `ToolsConfig` / `WasmOptConfig` / `TailwindConfig`: the supported TOML schema, without CLI-only fields.
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
