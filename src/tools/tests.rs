use super::*;
use anyhow::bail;
use flate2::{Compression, write::GzEncoder};

fn release(format: ArchiveFormat) -> Release {
    Release {
        repository: "example/tool".into(),
        tag: "v1.0.0".into(),
        asset: "tool.tar.gz".into(),
        executable: "tool/bin/tool".into(),
        format,
        sha256: None,
    }
}

fn archive(path: &Path, entries: &[(&str, &[u8])]) {
    let gzip = GzEncoder::new(File::create(path).unwrap(), Compression::default());
    let mut tar = tar::Builder::new(gzip);
    for (name, contents) in entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        tar.append_data(&mut header, name, *contents).unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap();
}

#[test]
fn installs_binary_and_reuses_cache_without_downloading() {
    let temp = tempfile::tempdir().unwrap();
    let cache = ToolCache::new(temp.path()).unwrap();
    let spec = release(ArchiveFormat::Binary);
    let path = cache
        .install(&spec, |path| Ok(fs::write(path, b"binary")?))
        .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"binary");
    assert!(is_executable(&path));
    assert_eq!(
        path,
        cache
            .install(&spec, |_| panic!("cache hit must not download"))
            .unwrap()
    );
    fs::write(&path, b"").unwrap();
    cache
        .install(&spec, |path| Ok(fs::write(path, b"repaired")?))
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"repaired");
}

#[test]
fn concurrent_installers_only_download_once() {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    let temp = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(4));
    let downloads = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let root = temp.path().to_owned();
            let barrier = Arc::clone(&barrier);
            let downloads = Arc::clone(&downloads);
            std::thread::spawn(move || {
                let cache = ToolCache::new(root).unwrap();
                barrier.wait();
                cache
                    .install(&release(ArchiveFormat::Binary), |path| {
                        downloads.fetch_add(1, Ordering::SeqCst);
                        Ok(fs::write(path, b"binary")?)
                    })
                    .unwrap()
            })
        })
        .collect();
    let paths: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(paths.iter().all(|p| p == &paths[0]));
    assert_eq!(downloads.load(Ordering::SeqCst), 1);
}

#[test]
fn extracts_archives_and_keeps_runtime_libraries() {
    let temp = tempfile::tempdir().unwrap();
    let cache = ToolCache::new(temp.path()).unwrap();
    let path = cache
        .install(&release(ArchiveFormat::TarGz), |path| {
            archive(
                path,
                &[
                    ("tool/bin/tool", b"executable"),
                    ("tool/lib/library.so", b"library"),
                ],
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        fs::read(path.parent().unwrap().join("../lib/library.so")).unwrap(),
        b"library"
    );
}

#[test]
fn failures_do_not_publish_cache_and_can_be_retried() {
    let temp = tempfile::tempdir().unwrap();
    let cache = ToolCache::new(temp.path()).unwrap();
    let spec = release(ArchiveFormat::TarGz);
    assert!(
        cache
            .install(&spec, |path| {
                archive(path, &[("wrong-file", b"wrong")]);
                Ok(())
            })
            .is_err()
    );
    assert!(
        cache
            .install(&spec, |path| {
                fs::write(path, b"partial")?;
                bail!("network interrupted")
            })
            .is_err()
    );
    let path = cache
        .install(&spec, |path| {
            archive(path, &[("tool/bin/tool", b"correct")]);
            Ok(())
        })
        .unwrap();
    assert_eq!(fs::read(path).unwrap(), b"correct");
}

#[test]
fn sha256_encoding_matches_standard_test_vector() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("asset");
    fs::write(&path, b"abc").unwrap();
    let expected = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_eq!(hex_digest(Sha256::digest(b"abc")), expected);
    verify_sha256(&path, expected).unwrap();
    verify_sha256(&path, &expected.to_ascii_uppercase()).unwrap();
}

#[test]
fn checksums_are_verified_before_publication() {
    let temp = tempfile::tempdir().unwrap();
    let cache = ToolCache::new(temp.path()).unwrap();
    let mut spec = release(ArchiveFormat::Binary);
    spec.sha256 = Some(hex_digest(Sha256::digest(b"expected")));
    assert!(
        cache
            .install(&spec, |path| Ok(fs::write(path, b"tampered")?))
            .is_err()
    );
    assert!(
        cache
            .install(&spec, |path| Ok(fs::write(path, b"expected")?))
            .is_ok()
    );
}

#[test]
fn cache_keys_include_release_identity() {
    let temp = tempfile::tempdir().unwrap();
    let cache = ToolCache::new(temp.path()).unwrap();
    let first = release(ArchiveFormat::Binary);
    let mut second = first.clone();
    second.tag = "v2.0.0".into();
    let one = cache
        .install(&first, |path| Ok(fs::write(path, b"one")?))
        .unwrap();
    let two = cache
        .install(&second, |path| Ok(fs::write(path, b"two")?))
        .unwrap();
    assert_ne!(one, two);
    assert_eq!(fs::read(one).unwrap(), b"one");
}

#[test]
fn rejects_unsafe_release_definitions_and_paths() {
    for path in [
        "",
        "../escape",
        "/absolute",
        "dir/../../escape",
        "C:\\escape",
        "dir\\escape",
    ] {
        assert!(validate_path(Path::new(path)).is_err(), "{path}");
    }
    for value in ["", "..", "../v1", "v1?query", "v1#fragment"] {
        assert!(validate_segment(value).is_err());
    }
    let mut spec = release(ArchiveFormat::Binary);
    spec.repository = "owner/repo/extra".into();
    assert!(spec.validate().is_err());
    spec.repository = "owner/repo".into();
    spec.tag = "latest".into();
    assert!(spec.validate().is_err());
}

#[test]
fn rejects_corrupt_gzip_trailers() {
    let temp = tempfile::tempdir().unwrap();
    let asset = temp.path().join("asset.tar.gz");
    archive(&asset, &[("tool", b"executable")]);
    let mut bytes = fs::read(&asset).unwrap();
    let crc_start = bytes.len() - 8;
    bytes[crc_start] ^= 0xff;
    fs::write(&asset, bytes).unwrap();
    let dest = temp.path().join("out");
    fs::create_dir(&dest).unwrap();
    assert!(extract_tar_gz(&asset, &dest).is_err());
}

#[test]
fn accepts_dot_prefixed_archive_entries() {
    let temp = tempfile::tempdir().unwrap();
    let asset = temp.path().join("asset.tar.gz");
    archive(&asset, &[("./tool/bin/tool", b"executable")]);
    let dest = temp.path().join("out");
    fs::create_dir(&dest).unwrap();
    extract_tar_gz(&asset, &dest).unwrap();
    assert_eq!(fs::read(dest.join("tool/bin/tool")).unwrap(), b"executable");
}

#[test]
fn rejects_links_in_archives() {
    let temp = tempfile::tempdir().unwrap();
    let asset = temp.path().join("asset.tar.gz");
    let gzip = GzEncoder::new(File::create(&asset).unwrap(), Compression::default());
    let mut tar = tar::Builder::new(gzip);
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Symlink);
    header.set_size(0);
    header.set_mode(0o777);
    tar.append_link(&mut header, "link", "../outside").unwrap();
    tar.into_inner().unwrap().finish().unwrap();
    let dest = temp.path().join("out");
    fs::create_dir(&dest).unwrap();
    assert!(extract_tar_gz(&asset, &dest).is_err());
    assert!(!dest.join("link").exists());
}

/// Optional smoke test against actual GitHub assets. Never executes downloaded code.
#[test]
#[ignore = "requires network access and downloads large release assets"]
fn downloads_builtin_releases() {
    let temp = tempfile::tempdir().unwrap();
    let mut cache = ToolCache::new(temp.path()).unwrap();
    if let Ok(token) = std::env::var("GITHUB_TOKEN") {
        cache = cache.with_github_token(token);
    }
    for (tool, version) in [
        (BuiltinTool::WasmBindgenCli, "0.2.100"),
        (BuiltinTool::WasmOpt, "123"),
        (BuiltinTool::TailwindCss, "4.0.0"),
    ] {
        let executable = cache.ensure(&tool, version).unwrap();
        assert!(is_executable(&executable));
        assert_eq!(cache.ensure(&tool, version).unwrap(), executable);
    }
}

#[test]
fn enforces_stream_size_limits() {
    assert!(copy_limited(&b"12345"[..], &mut Vec::new(), 4).is_err());
    let mut out = Vec::new();
    copy_limited(&b"1234"[..], &mut out, 4).unwrap();
    assert_eq!(out, b"1234");
}
