//! Directory-scoped ignore rules, loaded lazily without walking the workspace.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::options::Options;

use super::normalize;

#[derive(Debug)]
pub(super) struct IgnoreRules {
    configured: Gitignore,
    config_base: PathBuf,
    git: Option<GitRules>,
}

#[derive(Debug)]
struct GitRules {
    root: PathBuf,
    cache: RefCell<BTreeMap<PathBuf, Gitignore>>,
}

impl IgnoreRules {
    pub(super) fn new(options: &Options, workspace: &Path) -> Result<Self> {
        let config_base = normalize(
            options
                .config_path
                .as_deref()
                .and_then(Path::parent)
                .unwrap_or(workspace),
        );
        let mut builder = GitignoreBuilder::new(&config_base);
        for pattern in &options.watch_ignore {
            builder
                .add_line(options.config_path.clone(), pattern)
                .with_context(|| format!("invalid watch.ignore pattern {pattern:?}"))?;
        }
        let configured = builder.build().context("compiling watch.ignore patterns")?;
        let git = options.watch_use_gitignore.then(|| GitRules {
            // A .git file also identifies linked worktrees and submodules.
            root: workspace
                .ancestors()
                .find(|path| path.join(".git").exists())
                .unwrap_or(workspace)
                .to_owned(),
            cache: RefCell::new(BTreeMap::new()),
        });
        Ok(Self {
            configured,
            config_base,
            git,
        })
    }

    pub(super) fn parent_roots(&self, workspace: &Path) -> Vec<PathBuf> {
        let Some(git) = &self.git else {
            return Vec::new();
        };
        workspace
            .ancestors()
            .skip(1)
            .take_while(|path| path.starts_with(&git.root))
            .map(Path::to_owned)
            .collect()
    }

    pub(super) fn is_control(&self, workspace: &Path, path: &Path) -> bool {
        self.git.as_ref().is_some_and(|git| {
            path.file_name().is_some_and(|name| name == ".gitignore")
                && path.parent().is_some_and(|parent| {
                    parent.starts_with(&git.root)
                        && (parent.starts_with(workspace) || workspace.starts_with(parent))
                        && !self.configured(parent, true)
                        && !self.git(parent, true)
                })
        })
    }

    pub(super) fn invalidate(&self) {
        if let Some(git) = &self.git {
            git.cache.borrow_mut().clear();
        }
    }

    pub(super) fn configured(&self, path: &Path, is_dir: bool) -> bool {
        let nodes = ancestors(&self.config_base, path);
        // Check parents first: a negated child cannot resurrect an ignored directory.
        nodes.iter().any(|node| {
            self.configured
                .matched(node, *node != path || is_dir)
                .is_ignore()
        })
    }

    pub(super) fn git(&self, path: &Path, is_dir: bool) -> bool {
        let Some(git) = &self.git else { return false };
        let mut cache = git.cache.borrow_mut();
        let mut directories: Vec<&Path> = Vec::new();
        for node in ancestors(&git.root, path) {
            let directory = node != path || is_dir;
            // A deeper .gitignore takes precedence, including its negations.
            let ignored = directories
                .iter()
                .rev()
                .find_map(|parent| {
                    let matched = cache[*parent].matched(node, directory);
                    (!matched.is_none()).then(|| matched.is_ignore())
                })
                .unwrap_or(false);
            if ignored {
                return true;
            }
            if directory {
                cache
                    .entry(node.to_owned())
                    .or_insert_with(|| load_gitignore(node));
                directories.push(node);
            }
        }
        false
    }
}

fn ancestors<'a>(root: &Path, path: &'a Path) -> Vec<&'a Path> {
    if !path.starts_with(root) {
        return Vec::new();
    }
    let mut nodes: Vec<_> = path
        .ancestors()
        .take_while(|node| node.starts_with(root))
        .collect();
    nodes.reverse();
    nodes
}

fn load_gitignore(directory: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(directory);
    if let Some(error) = builder.add(directory.join(".gitignore"))
        && !error
            .io_error()
            .is_some_and(|error| error.kind() == ErrorKind::NotFound)
    {
        crate::logging::warn(format_args!("reading watch ignore rules: {error}"));
    }
    match builder.build() {
        Ok(matcher) => matcher,
        Err(error) => {
            crate::logging::warn(format_args!(
                "compiling watch ignore rules in {}: {error}",
                directory.display()
            ));
            Gitignore::empty()
        }
    }
}
