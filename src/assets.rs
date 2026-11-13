//! Configured static assets, copied recursively into the build output.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum AssetConfig {
    Path(PathBuf),
    Options(AssetMapping),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetMapping {
    pub from: PathBuf,
    pub to: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Asset {
    pub from: PathBuf,
    pub to: Option<PathBuf>,
}

impl AssetConfig {
    pub(crate) fn resolve(self, base: &Path) -> Result<Asset> {
        let (from, to) = match self {
            Self::Path(from) => (from, None),
            Self::Options(mapping) => (mapping.from, mapping.to),
        };
        ensure!(
            !from.as_os_str().is_empty(),
            "build.assets source must not be empty"
        );
        if let Some(to) = &to {
            ensure!(
                !to.as_os_str().is_empty()
                    && to
                        .components()
                        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir)),
                "build.assets destination must be a relative path without '..' components"
            );
        }
        Ok(Asset {
            from: base.join(from),
            to,
        })
    }
}

pub(crate) fn copy(assets: &[Asset], out_dir: &Path) -> Result<()> {
    fs::create_dir_all(out_dir)?;
    let output = fs::canonicalize(out_dir)?;
    for asset in assets {
        let source = fs::canonicalize(&asset.from)
            .with_context(|| format!("reading asset {}", asset.from.display()))?;
        ensure!(
            asset.to.as_deref() != Some(Path::new(".")) || source.is_dir(),
            "cannot copy asset {} to '.': source is not a directory",
            asset.from.display()
        );
        let destination = out_dir.join(match &asset.to {
            Some(to) => to.as_path(),
            None => Path::new(asset.from.file_name().with_context(|| {
                format!(
                    "asset {} has no filename; specify 'to'",
                    asset.from.display()
                )
            })?),
        });
        // Reject before traversal: otherwise copying a parent of dist would
        // repeatedly discover its own newly-created output.
        ensure!(
            !source.is_dir() || !output.starts_with(&source),
            "asset directory {} contains the output directory",
            asset.from.display()
        );
        copy_entry(&source, &destination, &mut Vec::new()).with_context(|| {
            format!(
                "copying asset {} to {}",
                asset.from.display(),
                destination.display()
            )
        })?;
    }
    Ok(())
}

fn copy_entry(source: &Path, destination: &Path, ancestors: &mut Vec<PathBuf>) -> Result<()> {
    let source =
        fs::canonicalize(source).with_context(|| format!("reading {}", source.display()))?;
    let metadata = fs::metadata(&source)?;
    if metadata.is_dir() {
        ensure!(
            !ancestors.contains(&source),
            "asset directory cycle at {}",
            source.display()
        );
        // Also catches symlinks in the input pointing back at generated output.
        let existing = destination
            .ancestors()
            .find(|path| path.exists())
            .context("asset destination has no existing ancestor")?;
        ensure!(
            !fs::canonicalize(existing)?.starts_with(&source),
            "cannot copy asset directory {} into itself",
            source.display()
        );
        fs::create_dir_all(destination)?;
        ancestors.push(source.clone());
        for entry in fs::read_dir(&source)? {
            let entry = entry?;
            copy_entry(
                &entry.path(),
                &destination.join(entry.file_name()),
                ancestors,
            )?;
        }
        ancestors.pop();
    } else {
        ensure!(
            metadata.is_file(),
            "asset {} is not a regular file or directory",
            source.display()
        );
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        if destination.exists() {
            ensure!(
                fs::canonicalize(destination)? != source,
                "cannot copy asset {} onto itself",
                source.display()
            );
        }
        fs::copy(&source, destination).with_context(|| {
            format!("copying {} to {}", source.display(), destination.display())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(config: &str, base: &Path) -> Result<Vec<Asset>> {
        let config: crate::options::Config = toml::from_str(config)?;
        config
            .build
            .assets
            .into_iter()
            .map(|asset| asset.resolve(base))
            .collect()
    }

    #[test]
    fn copies_files_directories_renames_and_flattened_contents() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("static/nested/empty")).unwrap();
        fs::write(root.join("static/nested/file.txt"), "nested").unwrap();
        fs::write(root.join("static/.hidden"), "hidden").unwrap();
        fs::write(root.join("favicon.ico"), "icon").unwrap();
        let assets = resolve(r#"[build]
assets = ["favicon.ico", "static", { from = "favicon.ico", to = "images/icon.ico" }, { from = "static", to = "renamed" }, { from = "static", to = "." }]
"#, root).unwrap();
        let out = root.join("dist");
        copy(&assets, &out).unwrap();
        for path in ["favicon.ico", "images/icon.ico"] {
            assert_eq!(fs::read_to_string(out.join(path)).unwrap(), "icon");
        }
        for prefix in ["static/", "renamed/", ""] {
            assert_eq!(
                fs::read_to_string(out.join(format!("{prefix}nested/file.txt"))).unwrap(),
                "nested"
            );
            assert!(out.join(format!("{prefix}nested/empty")).is_dir());
            assert!(out.join(format!("{prefix}.hidden")).is_file());
        }
        fs::write(root.join("favicon.ico"), "updated").unwrap();
        copy(&assets, &out).unwrap();
        assert_eq!(
            fs::read_to_string(out.join("favicon.ico")).unwrap(),
            "updated"
        );
    }

    #[test]
    fn rejects_invalid_mappings() {
        for mapping in [
            r#"{ from = "x", to = "../escape" }"#,
            r#"{ from = "x", to = "/absolute" }"#,
            r#"{ from = "x", to = "" }"#,
            r#"{ from = "x", flatten = true }"#,
            r#"{ from = "" }"#,
            r#"{ from = "x", typo = true }"#,
            r#"{ to = "x" }"#,
        ] {
            assert!(
                resolve(&format!("[build]\nassets = [{mapping}]"), Path::new(".")).is_err(),
                "{mapping}"
            );
        }
    }

    #[test]
    fn rejects_missing_sources_flattened_files_and_recursive_output() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::write(root.join("file"), "data").unwrap();
        for (mapping, message) in [
            (r#""missing""#, "reading asset"),
            (r#"{ from = "file", to = "." }"#, "not a directory"),
            (
                r#"{ from = ".", to = "." }"#,
                "contains the output directory",
            ),
        ] {
            let assets = resolve(&format!("[build]\nassets = [{mapping}]"), root).unwrap();
            let error = copy(&assets, &root.join("dist")).unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");
        }
    }
}
