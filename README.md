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

The build command:

1. Selects a workspace package using Cargo's default members (or `--package`).
2. Builds its `cdylib` library, or its sole binary, for `wasm32-unknown-unknown`.
   Use `--bin` to explicitly select a binary when needed.
3. Reads the exact `wasm-bindgen` dependency version from the workspace's
   `Cargo.lock` **after** Cargo creates/updates it.
4. Automatically downloads/caches that version of `wasm-bindgen-cli` and runs
   it with `--target web` on the actual WASM artifact reported by Cargo.

Generated JavaScript, WASM, and TypeScript declarations go into the package's
`dist/` directory by default. An explicit `--out-dir` is relative to the current
working directory. Custom Cargo target directories are supported.

The app must depend (directly or transitively) on a released registry version of
`wasm-bindgen`. Missing dependencies, multiple locked `wasm-bindgen` versions,
and path/git overrides produce an error rather than guessing an incompatible
CLI version. Multiple versions anywhere in the workspace lockfile must currently
be unified, even if only one is used by the selected app.

Only `wasm-bindgen-cli` is downloaded by `build` for now. This is a WASM + web
bindings build, not an HTML/CSS bundler or development server. Existing files in
the output directory are not cleaned. Set `GITHUB_TOKEN` if needed for GitHub API
rate limits; it is passed to the downloader for API requests only.

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
