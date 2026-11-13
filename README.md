# cargo-sycamore

A Cargo subcommand for building and locally developing [Sycamore](https://sycamore.dev) applications with WebAssembly and optional native SPA/SSR backends.

## Install locally

With a Rust toolchain supporting edition 2024 installed:

```sh
cargo install cargo-sycamore
cargo sycamore --help
cargo sycamore --version
```

For prebuilt binaries from GitHub releases, install with:

```sh
cargo binstall cargo-sycamore
```

## Usage

See [USAGE.md](USAGE.md) for build, watch, and development-server workflows,
configuration, Tailwind CSS and WASM optimization, examples, and tool downloads.

To see default configuration options, see [Sycamore.toml](Sycamore.toml).
