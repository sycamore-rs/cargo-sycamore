# Usage

## CLI output

Tool diagnostics use Cargo-style status labels on stderr, without timestamps,
module names, or spans. Color is enabled only for a compatible terminal and is
disabled when stderr is redirected or `NO_COLOR` is set.

```sh
cargo sycamore build -v     # Include additional debug diagnostics.
cargo sycamore serve -q     # Hide tool status messages.
NO_COLOR=1 cargo sycamore build
```

`-v` / `--verbose` and `-q` / `--quiet` are global flags and can appear before or
after the subcommand; they cannot be combined. Quiet mode still shows warnings
and errors. Cargo/compiler text and child-process stdout/stderr are not filtered
or reformatted by these flags. Command output and clap help remain separate from
tool diagnostics.

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
cargo sycamore build --package my-app --bin client --out-dir dist/site
cargo sycamore build --help
```

Settings can also come from `Sycamore.toml` or `[package.metadata.sycamore]` in
`Cargo.toml`; see [Configuration](#configuration).

A build produces a browser client (WASM plus wasm-bindgen web bindings), the
`<output_name>.bootstrap.js` bootstrap and `sycamore-assets.json` manifest, and—when
configured—a separately compiled native server executable. Client and server are
Cargo builds with independent target selection and feature sets. The default `[build].mode` is
`spa`; it generates an `index.html` shell. `ssr` builds the same client assets and
server executable but does not generate a tool-owned HTML shell. See
[SPA and SSR builds](#spa-and-ssr-builds) for the architecture and examples.

The client build selects a workspace package using Cargo's default members (or
`--package`), then builds its `cdylib` library or selected binary/example for
`wasm32-unknown-unknown`. The exact `wasm-bindgen` dependency version is read from
the workspace `Cargo.lock` after Cargo updates it; that version of
`wasm-bindgen-cli` is downloaded/cached and run against the Cargo-reported artifact.
Release builds also optimize generated `*_bg.wasm` with `wasm-opt` by default.
When configured, Tailwind CSS runs as part of this client build.

`[build].out-dir` is the build root and defaults to `dist/`. Browser-facing files
(HTML when generated, JavaScript, WASM, bootstrap, stylesheet and copied assets)
are first generated under `<out-dir>/.staging/public/`; the private manifest is
staged at `<out-dir>/.staging/sycamore-assets.json`. After the
client and (if configured) server builds both succeed, staged contents are
published at the build root as `public/` and `sycamore-assets.json`, and
`.staging/` is removed. The build root is not a location for the backend
executable; Cargo places the executable in its target directory. TypeScript
declarations are disabled with wasm-bindgen's `--no-typescript` flag. An explicit
`--out-dir` is relative to the current working directory. Custom Cargo target
directories are supported.

The resulting layout is:

```text
dist/
├── sycamore-assets.json       # private build/backend manifest
└── public/                    # static dev server root; browser-facing files
    ├── index.html             # SPA mode only
    ├── <output_name>.js
    ├── <output_name>_bg.wasm
    ├── <output_name>.bootstrap.js
    ├── index.css              # when Tailwind is enabled
    └── ...                    # copied assets
```

The app must depend (directly or transitively) on a released registry version of
`wasm-bindgen`. Missing dependencies, multiple locked `wasm-bindgen` versions,
and path/git overrides produce an error rather than guessing an incompatible
CLI version. Multiple versions anywhere in the workspace lockfile must currently
be unified, even if only one is used by the selected app.

Debug builds download `wasm-bindgen-cli`; release builds also download
`wasm-opt` unless disabled. When configured, Tailwind CSS is downloaded and run
in either mode. The command generates browser bindings and a bootstrap in
staging, plus the manifest; in SPA mode it also generates an HTML entrypoint. It copies static
files/directories listed in `[build].assets` into the staged `public/` directory
(see below). In SSR mode, HTML is app-owned and no build-time shell is generated.
On successful publication, existing entries in `<out-dir>` other than `.staging/`
are deleted before staged contents are copied into place. Use a dedicated output
directory: all existing files there are discarded on success, and manual files
are not preserved. Build guards reject output directories that would contain
workspace/package sources or protected build inputs such as Cargo's target
directory or a configured input/config file.

Failures before publication leave previously published output untouched; a
`.staging/` directory left by such a failure may remain until the next build resets
it. A failure to clean staging after successful publication is reported as a warning.
Publication itself uses delete-then-copy and is not filesystem-atomic: a
failure during that final step can leave incomplete output. The development server
continues serving its previous successful snapshot after a failed rebuild. Set
`GITHUB_TOKEN` if needed for GitHub API rate limits; it is passed to the downloader
for API requests only.

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
# Optional: defaults to index.css inside the resolved public/ directory.
output = "index.css" # written under <out-dir>/public/
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
- Output paths are always relative to the resolved `public/` directory and
  default to `index.css`. For example, `output = "output.css"` writes
  `dist/public/output.css`; `output = "assets/site.css"` writes
  `dist/public/assets/site.css`. Both follow `--out-dir` / `[build].out-dir`
  overrides automatically. Absolute output paths and `..` components are rejected;
  do not include the build-root or `public/` prefix.
- Version defaults to **4.3.5**. The matching standalone executable is downloaded
  through the normal tool cache, so npm/Node installation is not required.
- The compiler runs once for each successful Cargo/wasm-bindgen build, including
  examples and watch/serve rebuilds, with its working directory set to the selected
  package root for source scanning. Release builds add `--minify`; debug builds do
  not. Use Tailwind's `@source` directives if you need additional scan locations.
- Missing inputs, download errors, and compiler failures fail the build and prevent
  successful browser reloads. A failure before publication leaves the previous
  output intact; the final delete-and-copy publication is not atomic and can leave
  incomplete output if that step itself fails.
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

### HTML entrypoint and private asset manifest

In the default SPA build mode, each successful build publishes `index.html` into
`<build-root>/public/` (it is generated in staging before publication). If an
`index.html` exists beside the selected client
package's `Cargo.toml`, it is used as the template; otherwise the bundled
`templates/index.html` is used. In a workspace, this means the selected client
package's root, not necessarily the workspace root. SSR mode deliberately skips
this generated shell: the application server owns its HTML responses.

The SPA template is parsed as HTML5 using `scraper` (backed by `html5ever`), and a
`<script type="module">` is appended to `<head>` to import the generated
JavaScript and initialize WASM automatically:

```html
<script type="module" data-cargo-sycamore>
import init from "/my_app.js";
init().catch(error => console.error("Failed to initialize WASM:", error));
</script>
```

The import filename matches the actual compiled WASM artifact. You don't need to
add placeholders or write startup JavaScript. Missing `<head>`/`<body>` elements
are supplied by the parser, and user content/scripts are preserved (formatting
may be normalized). A previous `script[data-cargo-sycamore]` is replaced rather
than duplicated; that marker is reserved for generated startup scripts.

Every build mode also publishes `<output_name>.bootstrap.js` under
`<build-root>/public/` and `sycamore-assets.json` at the build root (for example,
`dist/sycamore-assets.json`); the manifest is staged at
`<build-root>/.staging/sycamore-assets.json` first. It contains the fields
`public_url`, `bootstrap`, `js`, and `wasm`, plus `stylesheet` when Tailwind CSS is enabled.
Values are root-relative public URLs (for example, `/my_app.bootstrap.js`,
`/my_app.js`, `/my_app_bg.wasm`, and optionally `/index.css`), never filesystem
paths. `public_url` records the normalized deployment prefix (default `/`), and
all asset URLs include it. The `bootstrap` value points to the generated bootstrap filename, which is
derived from the client artifact name to avoid colliding with wasm-bindgen output.
The bootstrap imports the client JS and initializes WASM. SSR applications can
read this manifest to emit their own HTML and asset links.

The SPA source template is never overwritten. Choosing an output path that would
overwrite it produces an error. Changes to the client package's `index.html`,
including its creation or deletion, trigger regeneration in watch mode. Serve
browser output using HTTP rather than opening it as a `file://` URL.

### Deployment prefix

For an app deployed below the site root, set a root-relative public URL:

```toml
[build]
public-url = "/my-app/"
```

The default is `/`; a missing trailing slash is added automatically.
Generated HTML imports, stylesheet links, and asset-manifest URLs use this prefix,
for example `/my-app/my_app.js`. Generated SPA HTML also gets a
`<base href="/my-app/">` unless the template already supplies a base element.
The bootstrap's own imports remain module-relative, so wasm-bindgen can resolve
its JS, WASM, and auxiliary files under the same prefix.

This changes URLs, not the filesystem layout: files still live directly under
`dist/public/`, not `dist/public/my-app/`. Links and URLs in copied files or
custom application code are not rewritten.

When serving the frontend, the development server mounts `public/` at that prefix.
Requests outside it return 404 unless matched by `forward-routes`; the bare
`/my-app` redirects to `/my-app/`, preserving the query string. SPA history
fallback stays within the prefix. Forwarded requests retain their original paths:
forward routes are expressed against the full browser-facing path and do not
automatically gain the prefix. Reload endpoints remain at `/__sycamore/*`.

With `serve-frontend = false`, the backend still owns all routing. It must serve
its public assets at the prefix recorded in the manifest; the proxy does not
strip that prefix. Production hosting must mount `public/` the same way.

Only root-relative deployment paths are supported, not absolute/CDN URLs.
Queries, fragments, traversal components, repeated slashes, and unsafe or
malformed escapes are rejected.

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
by Cargo, and required features can be enabled with `[build.client].features`.

You can also select an example in `Sycamore.toml`:

```toml
[build.client]
example = "counter"
```

`--bin` and `--example` are mutually exclusive, as are their config equivalents.
An explicit CLI target selector overrides the configured target selector, so
`--example counter` can replace a configured `bin` and `--bin client` can replace
a configured `example`. Without an example selection, normal library/binary target
selection is unchanged.

SPA examples use the same package-root `index.html` template and default package
`dist/` build root (with published browser files under `dist/public/`) as normal builds; SSR
examples do not generate a shell. Use
`--out-dir` to keep separate outputs when switching targets. `serve --example` includes watching and browser autoreload
according to your usual serve settings.

## Watching for changes

```sh
cargo sycamore watch
cargo sycamore watch --release
cargo sycamore watch --package my-app --out-dir dist/site
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
path. Configured assets and Tailwind inputs outside the workspace are watched;
other local dependencies outside the workspace are not watched. Native filesystem watcher errors are fatal rather than silently missing
future edits.

## Development server

```sh
cargo sycamore serve
cargo sycamore serve --port 9000
cargo sycamore s --port 9000 # shorthand for serve
cargo sycamore serve --release --package my-app --out-dir dist/site
```

`serve` binds to **http://127.0.0.1:8080** by default and runs the selected build
and watcher. In SPA mode it serves the public asset directory and can proxy chosen
routes to a separately built native application server. In SSR mode it runs the
native server and proxies application requests; it does not serve a frontend. The
build root (`dist/` by default) contains `public/` (the browser files served by the
static dev server) and the sibling private `sycamore-assets.json` manifest, never
the backend executable. Build and watch do not need development-server ports; `serve`
requires `[serve].backend-port` whenever `[build.server]` is configured, even if
the application is reached directly on a separate origin.

A configured backend must honor the runtime contract supplied by the supervisor:
`SYCAMORE_SERVER_ADDR` is its loopback listen address,
`SYCAMORE_PUBLIC_DIR` is `<snapshot>/public`, and
`SYCAMORE_ASSET_MANIFEST` is `<snapshot>/sycamore-assets.json`. Backend snapshots
contain the manifest beside `public/`. The backend must bind the supplied address
and use the public directory/manifest as appropriate. The static development
server serves only `public/`; public URLs remain root-relative (for example,
`/app.js`), with no `/public` URL prefix. No framework integration crate is
included yet. The tool does not prescribe production deployment topology; static
generation/export is application-owned.

For a typical SPA with selected backend routes proxied by the same-origin dev
server:

```toml
[build]
mode = "spa"

[build.client]
package = "web"
features = ["csr"]

[build.server]
package = "backend"
bin = "http"
features = ["http"]

[serve]
port = 8080
backend-port = 9000
forward-routes = ["/api", "/auth"]
```

The frontend serves missing GET/HEAD paths with `index.html` when
`[serve].spa = true`; configured forwarded routes are handled by the backend
first. Prefixes match complete path segments (`/api` matches `/api` and
`/api/users`, but not `/apiary`). `forward-routes` defaults to `[]`.

For SSR, build the client assets and backend independently but run the same
supervised backend in `serve`; it owns all HTML and static asset responses:

```toml
[build]
mode = "ssr"

[build.client]
package = "web"
features = ["hydrate"]

[build.server]
package = "backend"
bin = "http"
features = ["ssr", "http"]

[serve]
backend-port = 9000
# serve-frontend defaults to false in SSR mode.
```

`serve-frontend = true` is invalid in SSR mode. With `serve-frontend = false`,
all application requests go to the backend except reserved `/__sycamore/`
development endpoints; explicit `spa` and nonempty `forward-routes` are rejected
in this backend-only mode. The backend port must be nonzero and different from
the public port. A configured server always requires `backend-port` for `serve`,
whether or not frontend traffic is proxied.

Set `[serve].serve-frontend = true` to serve the frontend in SPA mode (the default
there); it defaults to `false` in SSR mode. `[serve].forward-routes` selects
backend prefixes when the frontend is served and requires `[build.server]`.
Forwarding occurs before static file lookup and SPA fallback. When frontend
serving is disabled, all routes except the reserved `/__sycamore/` development
endpoints go to the backend. `spa` and
nonempty `forward-routes` are rejected with frontend serving disabled.
The public `port` defaults to 8080 (`0` asks the OS for a free port, printed at
startup); `backend-port` is required with a configured server, must be nonzero,
and must differ from the public port. Public port changes require restarting
`serve`.

Autoreload defaults to `true`; `[serve].autoreload = false` disables browser
reloads while watching and rebuilding continues. Override with `--autoreload`,
`--autoreload=true`, or `--autoreload=false`. `[serve].spa` defaults to `false`
(and `--spa` / `--spa=false` overrides it). SPA fallback handles missing GET/HEAD
paths within `build.public-url` via public `index.html`, after route forwarding,
and adds a `<base>` element using that prefix
to the response if needed. Existing files and directory indexes take precedence;
reserved endpoints do not fall back. These settings and `serve-frontend`,
`backend-port`, and `forward-routes` can be reloaded when configuration changes.
Build/watch do not require these runtime ports, but serve validation applies them.

The development supervisor snapshots the backend executable and the build root,
including sibling `public/` and `sycamore-assets.json`, waits for TCP readiness,
and restores the previous backend generation if a restart fails. It manages and kills only the direct child process; backend grandchildren
are not supported. Public assets are also published as successful frontend
snapshots. Compilation/configuration failures preserve the last good served output.

The dev proxy supports streaming HTTP and application WebSockets. Application
response statuses and headers are preserved, except hop-by-hop headers; with
autoreload, eligible HTML responses have reload metadata adjusted as described
below. It requests identity encoding for reload injection. Only an identity,
`text/html`, status-200 response gets the reload module appended at the end of the
stream; content length and validators are removed. Compressed HTML is passed through
untouched. A Content Security Policy may block the injected development script.

The Axum frontend supports GET/HEAD, directory `index.html`, byte ranges for static
assets, and MIME types including `application/wasm`. Traversal and symlinks outside
the public output are rejected. The server is local-development infrastructure,
not a production server, and listens only on loopback. Host headers are restricted
to the local server, and reload WebSocket connections require a matching
same-origin browser Origin header. Application WebSocket authorization remains
the backend's responsibility.

The backend's loopback port is also directly reachable; the proxy's Host/Origin
guards do not protect that port. The application must apply appropriate Host,
Origin, CORS, and authentication checks, especially for state-changing endpoints.
Loopback binding alone is not protection against requests from untrusted websites.

The reload endpoints are `/__sycamore/reload.js` and `/__sycamore/reload`. The
client reconnects after disconnections and reloads after successful builds or
server restarts. A page carries its build version to detect missed rebuilds. All
tabs reload only after a complete successful build; failed builds never reload the
page. A status pill reports builds and errors. Its UI lives in `src/reload.js` and
is absent when autoreload is disabled. A custom CSP may need to allow the
same-origin module, local WebSocket connection, and indicator inline styles.

## Configuration

Create a `Sycamore.toml` in your project (a commented template is included here):

```toml
[build]
mode = "spa" # default; use "ssr" for server-rendered HTML
manifest-path = "Cargo.toml"
out-dir = "dist" # build root; browser files are under dist/public/
# assets = ["favicon.ico", "images"] # copied under dist/public/

[build.client]
package = "web"
# bin = "client" # or example = "counter"
features = ["hydrate"]
default-features = true

# Optional native host executable; built separately from the client.
[build.server]
package = "backend"
bin = "http"
features = ["http"]
default-features = true

[serve]
port = 8080
autoreload = true
serve-frontend = true # defaults to true for SPA, false for SSR
backend-port = 9000 # required by serve whenever [build.server] exists
forward-routes = ["/api"] # optional; default []
spa = false
```

The precedence is **explicit CLI flags → configuration → built-in defaults**.
For example, `cargo sycamore b --out-dir dist/site --release` overrides the
configured build root and selects the release profile. Client selectors and feature
settings are configured under `[build.client]`; client CLI target selectors
override configured target selectors. The client and server package/feature
settings are otherwise independent.

### SPA and SSR builds

`[build].mode` is `"spa"` by default or `"ssr"`. SSR mode requires
`[build.server]`. `[build.client]` accepts optional `package`, `bin`, `example`,
`features`, and `default-features`. `[build.server]` accepts optional `package`,
`bin`, `features`, and `default-features`; its binary is compiled for the native
host in a separate Cargo invocation, not into the public output directory. Both
feature sets default to `[]` and default features to `true`; the server uses the
same `--release` selection as the client.

The shared `[build]` keys are `mode`, `manifest-path`, `out-dir`, `public-url`, and `assets`.
The `out-dir` is the build root (`dist/` by default). Output is staged in
`.staging/` and published as its `public/` child plus `sycamore-assets.json` beside
that child. Successful publication clears all other existing entries in `out-dir`;
use a dedicated output directory and do not keep manual files there. All modes
emit `<output_name>.bootstrap.js` and `sycamore-assets.json`; SPA also generates
the HTML shell under `public/`, while SSR leaves HTML generation to the
application. Static site generation/export is not performed by the tool.

`[serve]` keys are `port` (8080), `autoreload` (`true`), `spa` (`false`),
`serve-frontend` (defaults by build mode), `backend-port` (required at serve time
when `[build.server]` exists), and `forward-routes` (`[]`). The backend port must
be nonzero and differ from the public port. See
[Development server](#development-server) for routing and runtime details.

| Option | CLI | `Sycamore.toml` config key | Default |
| --- | --- | --- | --- |
| Manifest | `--manifest-path` | `build.manifest-path` | Cargo's current project |
| Client package | `--package` / `-p` | `build.client.package` | Cargo's default members |
| Client binary | `--bin` | `build.client.bin` | cdylib or sole binary |
| Client example | `--example` | `build.client.example` | none |
| Output | `--out-dir` | `build.out-dir` | `<package>/dist` (build root; browser files in `public/`) |
| Public URL prefix | — | `build.public-url` | `/` |
| Static assets | — | `build.assets` | `[]` |
| Client features | — | `build.client.features` | `[]` |
| Client default features | — | `build.client.default-features` | `true` |
| Build mode | — | `build.mode` | `spa` |
| Native server | — | `build.server` | not built |
| Release profile | `--release` | — | debug |
| Config file | `--config` | — | nearest config source |

### Static assets

List files or directories to copy under the build root's `public/` directory:

```toml
[build]
assets = [
    "favicon.ico",                                # dist/public/favicon.ico
    "images",                                     # dist/public/images/ (recursive)
    { from = "logo.svg", to = "img/brand.svg" }, # dist/public/img/brand.svg
    { from = "fonts", to = "webfonts" },          # dist/public/webfonts/ (recursive)
    { from = "public", to = "." },                # public/* → dist/public/*
]
```

Source paths are relative to the selected config file (or the containing
`Cargo.toml` for package metadata). Absolute source paths are also supported.
Destinations (`to`) are relative to `public/`, not the build root; absolute paths,
empty paths, and `..` components are rejected. Parent directories are created as
needed. Using `to = "."` copies a directory’s contents directly into `public/`,
preserving nested directories. Using `to = "."` with a file is an error.

Assets are copied in array order, overwriting existing files at the same paths
in staged `public/`. Missing sources and copy failures are errors. Generated HTML,
JavaScript, WASM, and Tailwind output take precedence over assets with the same
paths under `public/`. Each successful publication clears previous output before
copying the new staged contents, including old copied assets; no manual files in
`out-dir` are preserved. A source directory must not contain the output directory.
Asset changes trigger rebuilds in watch/serve mode, including sources outside the
workspace.

WASM optimization settings live under `[tools.wasm-opt]`:

| Option | CLI | Config key | Default |
| --- | --- | --- | --- |
| Enable release optimization | `--wasm-opt` | `enable` | `true` (release only) |
| Binaryen version | — | `version` | `"133"` |

The old `[build].wasm-opt` and `[build].wasm-opt-version` keys are no longer
accepted; move them to this tools section.

Serve-only flags remain unchanged: `--port` overrides `[serve].port` and
`--autoreload` overrides `[serve].autoreload`; `--spa` / `--spa=false` overrides
`[serve].spa`. `serve-frontend`, `backend-port`, and `forward-routes` are
configuration-only. Build and watch ignore serving ports; serve validates them.

Alternatively, use the same schema inside the package's `Cargo.toml`:

```toml
[package.metadata.sycamore.build]
mode = "ssr"
out-dir = "site-output"

[package.metadata.sycamore.build.client]
package = "web"
features = ["hydrate"]

[package.metadata.sycamore.build.server]
package = "backend"
bin = "http"
features = ["ssr"]

[package.metadata.sycamore.tools.wasm-opt]
enable = true
version = "133"

[package.metadata.sycamore.serve]
port = 8080
backend-port = 9000
# serve-frontend defaults to false for SSR.
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
`manifest-path`. Without an output override, the build root still defaults to the
selected package's `dist/` directory, with published browser files under
`dist/public/`. Choose a dedicated output directory: successful builds remove its
existing entries before publishing, so manual files are not retained.

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
