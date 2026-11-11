use anyhow::{Result, bail};

use super::{ArchiveFormat, Platform, Release, Tool};

/// Tools whose GitHub release layouts are known to cargo-sycamore.
#[derive(Clone, Copy, Debug)]
pub enum BuiltinTool {
    WasmBindgenCli,
    WasmOpt,
    TailwindCss,
}

impl Tool for BuiltinTool {
    fn release(&self, version: &str, platform: &Platform) -> Result<Release> {
        let os = platform.os.as_str();
        let arch = platform.arch.as_str();
        let exe = if os == "windows" { ".exe" } else { "" };
        let (repository, tag, asset, executable, format) = match self {
            Self::WasmBindgenCli => {
                let version = version.strip_prefix('v').unwrap_or(version);
                let target = match (os, arch) {
                    ("linux", "x86_64") => "x86_64-unknown-linux-musl",
                    ("linux", "aarch64") if platform.env == "musl" => "aarch64-unknown-linux-musl",
                    ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
                    ("macos", "x86_64") => "x86_64-apple-darwin",
                    ("macos", "aarch64") => "aarch64-apple-darwin",
                    ("windows", "x86_64") => "x86_64-pc-windows-msvc",
                    _ => bail!("wasm-bindgen-cli has no configured release for {platform:?}"),
                };
                let stem = format!("wasm-bindgen-{version}-{target}");
                (
                    "wasm-bindgen/wasm-bindgen",
                    version.to_owned(),
                    format!("{stem}.tar.gz"),
                    format!("{stem}/wasm-bindgen{exe}"),
                    ArchiveFormat::TarGz,
                )
            }
            Self::WasmOpt => {
                let version = version.strip_prefix("version_").unwrap_or(version);
                // Binaryen's Linux builds target glibc, not musl.
                if os == "linux" && platform.env == "musl" {
                    bail!("Binaryen's prebuilt Linux tools require glibc");
                }
                let target = match (os, arch) {
                    ("linux", "x86_64") => "x86_64-linux",
                    ("linux", "aarch64") => "aarch64-linux",
                    ("macos", "x86_64") => "x86_64-macos",
                    ("macos", "aarch64") => "arm64-macos",
                    ("windows", "x86_64") => "x86_64-windows",
                    ("windows", "aarch64") => "arm64-windows",
                    _ => bail!("wasm-opt has no configured release for {platform:?}"),
                };
                let tag = format!("version_{version}");
                (
                    "WebAssembly/binaryen",
                    tag.clone(),
                    format!("binaryen-{tag}-{target}.tar.gz"),
                    format!("binaryen-{tag}/bin/wasm-opt{exe}"),
                    ArchiveFormat::TarGz,
                )
            }
            Self::TailwindCss => {
                let version = version.strip_prefix('v').unwrap_or(version);
                let target = match (os, arch) {
                    ("linux", "x86_64") => "linux-x64",
                    ("linux", "aarch64") => "linux-arm64",
                    ("macos", "x86_64") => "macos-x64",
                    ("macos", "aarch64") => "macos-arm64",
                    ("windows", "x86_64") => "windows-x64",
                    _ => bail!("tailwindcss has no configured release for {platform:?}"),
                };
                let musl = if os == "linux" && platform.env == "musl" {
                    "-musl"
                } else {
                    ""
                };
                let asset = format!("tailwindcss-{target}{musl}{exe}");
                (
                    "tailwindlabs/tailwindcss",
                    format!("v{version}"),
                    asset,
                    format!("tailwindcss{exe}"),
                    ArchiveFormat::Binary,
                )
            }
        };
        Ok(Release {
            repository: repository.into(),
            tag,
            asset,
            executable: executable.into(),
            format,
            sha256: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform(os: &str, arch: &str, env: &str) -> Platform {
        Platform {
            os: os.into(),
            arch: arch.into(),
            env: env.into(),
        }
    }

    #[test]
    fn release_layouts() {
        let linux = platform("linux", "x86_64", "gnu");
        let bindgen = BuiltinTool::WasmBindgenCli
            .release("0.2.100", &linux)
            .unwrap();
        assert_eq!(bindgen.tag, "0.2.100");
        assert_eq!(
            bindgen.asset,
            "wasm-bindgen-0.2.100-x86_64-unknown-linux-musl.tar.gz"
        );
        let opt = BuiltinTool::WasmOpt.release("version_123", &linux).unwrap();
        assert_eq!(opt.asset, "binaryen-version_123-x86_64-linux.tar.gz");
        assert_eq!(
            opt.executable.to_str().unwrap(),
            "binaryen-version_123/bin/wasm-opt"
        );
        let tailwind = BuiltinTool::TailwindCss
            .release("v4.0.0", &platform("windows", "x86_64", "msvc"))
            .unwrap();
        assert_eq!(tailwind.tag, "v4.0.0");
        assert_eq!(tailwind.asset, "tailwindcss-windows-x64.exe");
    }

    #[test]
    fn arm_macos_and_musl_layouts() {
        let mac = platform("macos", "aarch64", "");
        assert_eq!(
            BuiltinTool::WasmOpt.release("123", &mac).unwrap().asset,
            "binaryen-version_123-arm64-macos.tar.gz"
        );
        let musl = platform("linux", "aarch64", "musl");
        assert_eq!(
            BuiltinTool::TailwindCss
                .release("4.0.0", &musl)
                .unwrap()
                .asset,
            "tailwindcss-linux-arm64-musl"
        );
        assert!(BuiltinTool::WasmOpt.release("123", &musl).is_err());
    }

    #[test]
    fn unsupported_platforms_fail() {
        for tool in [
            BuiltinTool::WasmBindgenCli,
            BuiltinTool::WasmOpt,
            BuiltinTool::TailwindCss,
        ] {
            assert!(
                tool.release("1", &platform("freebsd", "riscv64", ""))
                    .is_err()
            );
        }
    }
}
