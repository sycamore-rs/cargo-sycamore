use super::*;

const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

fn locked(version: &str) -> String {
    format!(
        "[[package]]\nname = \"wasm-bindgen\"\nversion = \"{version}\"\nsource = \"{REGISTRY}\"\n"
    )
}

#[test]
fn reads_exact_resolved_version() {
    let contents = format!(
        "version = 4\n{}\n[[package]]\nname = \"wasm-bindgen-shared\"\nversion = \"0.2.99\"\n",
        locked("0.2.100")
    );
    assert_eq!(parse_wasm_bindgen_version(&contents).unwrap(), "0.2.100");
}

#[test]
fn accepts_sparse_registry_sources() {
    let contents = locked("0.2.100").replace(REGISTRY, "sparse+https://example.com/index/");
    assert_eq!(parse_wasm_bindgen_version(&contents).unwrap(), "0.2.100");
}

#[test]
fn rejects_missing_ambiguous_or_unreleased_bindgen() {
    for contents in [
        "version = 4".into(),
        "invalid toml".into(),
        format!("{}\n{}", locked("0.2.99"), locked("0.2.100")),
        "[[package]]\nname = \"wasm-bindgen\"\nversion = \"0.2.100\"".into(),
        locked("0.2.100").replace(REGISTRY, "git+https://github.com/example/wasm-bindgen"),
    ] {
        assert!(parse_wasm_bindgen_version(&contents).is_err(), "{contents}");
    }
}

fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("src")).unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"test-app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(temp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
    temp
}

fn metadata(root: &Path) -> Metadata {
    MetadataCommand::new()
        .manifest_path(root.join("Cargo.toml"))
        .no_deps()
        .exec()
        .unwrap()
}

#[test]
fn selects_binary_or_cdylib_and_validates_names() {
    let temp = fixture();
    let meta = metadata(temp.path());
    let package = select_package(&meta, None).unwrap();
    assert_eq!(select_target(package, None).unwrap().name, "test-app");
    assert!(select_package(&meta, Some("absent")).is_err());
    assert!(select_target(package, Some("absent")).is_err());
    fs::create_dir(temp.path().join("src/bin")).unwrap();
    fs::write(temp.path().join("src/bin/other.rs"), "fn main() {}\n").unwrap();
    let meta = metadata(temp.path());
    let package = select_package(&meta, None).unwrap();
    assert!(select_target(package, None).is_err());
    assert_eq!(select_target(package, Some("other")).unwrap().name, "other");
    let manifest = temp.path().join("Cargo.toml");
    let contents = fs::read_to_string(&manifest).unwrap();
    fs::write(
        &manifest,
        format!("{contents}\n[lib]\ncrate-type = [\"cdylib\"]\n"),
    )
    .unwrap();
    fs::write(temp.path().join("src/lib.rs"), "pub fn app() {}\n").unwrap();
    let meta = metadata(temp.path());
    assert!(
        select_target(select_package(&meta, None).unwrap(), None)
            .unwrap()
            .is_cdylib()
    );
}

#[test]
fn respects_workspace_defaults_and_member_manifest() {
    let temp = fixture();
    let root = temp.path();
    let manifest = root.join("Cargo.toml");
    let contents = fs::read_to_string(&manifest).unwrap().replace(
        "[workspace]",
        "[workspace]\nmembers = [\"member\"]\ndefault-members = [\"member\"]",
    );
    fs::write(&manifest, &contents).unwrap();
    fs::create_dir_all(root.join("member/src")).unwrap();
    fs::write(
        root.join("member/Cargo.toml"),
        "[package]\nname = \"member\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(root.join("member/src/main.rs"), "fn main() {}\n").unwrap();
    for path in [root.to_path_buf(), root.join("member")] {
        let meta = metadata(&path);
        assert_eq!(select_package(&meta, None).unwrap().name.as_str(), "member");
    }
    // Cargo's metadata defaults reflect the invocation manifest even when the
    // workspace root is itself a package and has no explicit default-members.
    fs::write(
        &manifest,
        contents.replace("default-members = [\"member\"]\n", ""),
    )
    .unwrap();
    assert_eq!(
        select_package(&metadata(root), None).unwrap().name.as_str(),
        "test-app"
    );
    assert_eq!(
        select_package(&metadata(&root.join("member")), None)
            .unwrap()
            .name
            .as_str(),
        "member"
    );
}

#[test]
fn virtual_workspace_requires_an_unambiguous_package() {
    let temp = tempfile::tempdir().unwrap();
    fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for name in ["a", "b"] {
        fs::create_dir_all(temp.path().join(name).join("src")).unwrap();
        fs::write(
            temp.path().join(name).join("Cargo.toml"),
            format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        )
        .unwrap();
        fs::write(temp.path().join(name).join("src/main.rs"), "fn main() {}\n").unwrap();
    }
    let meta = metadata(temp.path());
    assert!(select_package(&meta, None).is_err());
    assert_eq!(select_package(&meta, Some("b")).unwrap().name.as_str(), "b");
}

#[cfg(unix)]
mod pipeline {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn quote(value: impl AsRef<OsStr>) -> String {
        format!(
            "'{}'",
            value.as_ref().to_string_lossy().replace('\'', "'\\''")
        )
    }

    fn script(path: &Path, contents: &str) {
        fs::write(path, format!("#!/bin/sh\nset -eu\n{contents}\n")).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn mock_cargo(root: &Path, fail: bool) -> PathBuf {
        let meta = metadata(root);
        let package = select_package(&meta, None).unwrap();
        let target = select_target(package, None).unwrap();
        let wasm = root.join("custom-target/test_app.wasm");
        fs::create_dir_all(wasm.parent().unwrap()).unwrap();
        fs::write(&wasm, b"fake wasm").unwrap();
        let artifact = serde_json::json!({
            "reason": "compiler-artifact", "package_id": package.id,
            "manifest_path": package.manifest_path, "target": target,
            "profile": {"opt_level":"0", "debuginfo":0, "debug_assertions":true, "overflow_checks":true, "test":false},
            "features":[], "filenames":[wasm], "executable":null, "fresh":false,
        });
        let real_cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let cargo = root.join("mock-cargo");
        script(
            &cargo,
            &format!(
                "if [ \"$1\" = metadata ]; then exec {} \"$@\"; fi\nprintf '%s\\n' \"$@\" > {}\n{}\nprintf '%s\\n' {}",
                quote(real_cargo),
                quote(root.join("cargo-args")),
                if fail { "exit 1" } else { "true" },
                quote(artifact.to_string()),
            ),
        );
        cargo
    }

    #[test]
    fn builds_then_downloads_matching_cli_and_runs_web_bindings() {
        let temp = fixture();
        let root = temp.path();
        let cargo = mock_cargo(root, false);
        fs::write(
            root.join("Cargo.lock"),
            format!("version = 4\n{}", locked("0.2.100")),
        )
        .unwrap();
        let bindgen = root.join("wasm-bindgen");
        script(
            &bindgen,
            &format!(
                "printf '%s\\n' \"$@\" > {}",
                quote(root.join("bindgen-args"))
            ),
        );
        let options = BuildOptions {
            manifest_path: Some(root.join("Cargo.toml")),
            release: true,
            ..Default::default()
        };
        build_with(&options, cargo.as_os_str(), |version| {
            assert_eq!(version, "0.2.100");
            assert!(root.join("cargo-args").exists());
            Ok(bindgen)
        })
        .unwrap();
        let args = fs::read_to_string(root.join("cargo-args")).unwrap();
        assert!(args.contains("--target\nwasm32-unknown-unknown\n"));
        assert!(args.contains("--release\n"));
        let args = fs::read_to_string(root.join("bindgen-args")).unwrap();
        assert!(args.contains("custom-target/test_app.wasm\n"));
        assert!(args.contains("--target\nweb\n--out-dir\n"));
        assert!(root.join("dist").is_dir());
    }

    #[test]
    fn failed_cargo_does_not_download_tools() {
        let temp = fixture();
        let cargo = mock_cargo(temp.path(), true);
        let options = BuildOptions {
            manifest_path: Some(temp.path().join("Cargo.toml")),
            ..Default::default()
        };
        let error = build_with(&options, cargo.as_os_str(), |_| {
            panic!("must not download after failed build")
        })
        .unwrap_err();
        assert!(error.to_string().contains("cargo build failed"));
    }

    #[test]
    fn bindgen_failure_is_reported() {
        let temp = fixture();
        let root = temp.path();
        let cargo = mock_cargo(root, false);
        fs::write(root.join("Cargo.lock"), locked("0.2.100")).unwrap();
        let bindgen = root.join("wasm-bindgen");
        script(&bindgen, "exit 1");
        let options = BuildOptions {
            manifest_path: Some(root.join("Cargo.toml")),
            out_dir: Some(root.join("output")),
            ..Default::default()
        };
        let error = build_with(&options, cargo.as_os_str(), |_| Ok(bindgen)).unwrap_err();
        assert!(error.to_string().contains("wasm-bindgen 0.2.100 failed"));
        assert!(root.join("output").is_dir());
    }
}
