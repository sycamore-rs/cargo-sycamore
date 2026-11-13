//! Snapshot and lifecycle management for the native development backend.
//!
//! The child is killed and reaped directly. This is portable across supported
//! platforms, but does not terminate grandchildren spawned by a backend.

use std::collections::HashSet;
use std::fs;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tempfile::TempDir;

const READINESS_TIMEOUT: Duration = Duration::from_secs(10);
const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);

pub(crate) struct BackendSupervisor {
    running: Option<RunningBackend>,
}

struct RunningBackend {
    child: Child,
    generation: Generation,
    backend_port: u16,
    exit_reported: bool,
}

#[derive(Debug)]
struct Generation {
    // Keeping this alive keeps both the executable and assets stable for the child.
    _directory: TempDir,
    executable: PathBuf,
    public_dir: PathBuf,
    manifest_path: PathBuf,
}

impl BackendSupervisor {
    pub(crate) fn new() -> Self {
        Self { running: None }
    }

    /// Replace the running backend with a snapshot of the newly built app.
    /// If launching or readiness fails, the previous generation is relaunched
    /// and this method still returns the original failure.
    pub(crate) fn restart(
        &mut self,
        executable: &Path,
        output_dir: &Path,
        backend_port: u16,
    ) -> Result<()> {
        let port_is_managed = self
            .running
            .as_ref()
            .is_some_and(|running| running.backend_port == backend_port);
        if !port_is_managed {
            ensure_port_available(backend_port)?;
        }

        // Prepare the generation before disturbing a working server. In particular,
        // a copy error must not take down the currently served build.
        let next = snapshot(executable, output_dir)?;

        let mut previous = self.running.take();
        if let Some(old) = previous.as_mut()
            && let Err(error) = stop_child(&mut old.child)
        {
            self.running = previous;
            return Err(error).context("stopping previous backend");
        }

        match launch(next, backend_port) {
            Ok((child, generation)) => {
                self.running = Some(RunningBackend {
                    child,
                    generation,
                    backend_port,
                    exit_reported: false,
                });
                Ok(())
            }
            Err(start_error) => {
                let rollback = previous.take().and_then(|old| {
                    let old_port = old.backend_port;
                    match launch(old.generation, old_port) {
                        Ok((child, generation)) => {
                            self.running = Some(RunningBackend {
                                child,
                                generation,
                                backend_port: old_port,
                                exit_reported: false,
                            });
                            None
                        }
                        Err(error) => Some(error),
                    }
                });
                if let Some(rollback_error) = rollback {
                    Err(start_error.context(format!(
                        "could not restore the previous backend: {rollback_error:#}"
                    )))
                } else {
                    Err(start_error)
                }
            }
        }
    }

    /// Report an unexpected runtime exit once, without stopping the watcher.
    pub(crate) fn check(&mut self) -> Result<()> {
        if let Some(running) = &mut self.running
            && !running.exit_reported
            && let Some(status) = running
                .child
                .try_wait()
                .context("checking backend process")?
        {
            running.exit_reported = true;
            bail!("backend exited unexpectedly ({})", display_status(status));
        }
        Ok(())
    }

    /// Stop the backend and wait for its process to be reaped.
    pub(crate) fn stop(&mut self) -> Result<()> {
        // Remove the generation snapshot only after the child has exited.
        if let Some(mut running) = self.running.take()
            && let Err(error) = stop_child(&mut running.child)
        {
            self.running = Some(running);
            return Err(error).context("stopping backend");
        }
        Ok(())
    }
}

impl Drop for BackendSupervisor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn snapshot(executable: &Path, output_dir: &Path) -> Result<Generation> {
    let executable = executable
        .canonicalize()
        .with_context(|| format!("resolving backend executable {}", executable.display()))?;
    let executable_metadata = fs::metadata(&executable)
        .with_context(|| format!("reading backend executable {}", executable.display()))?;
    if !executable_metadata.is_file() {
        bail!(
            "backend executable is not a regular file: {}",
            executable.display()
        );
    }

    let output_root = output_dir
        .canonicalize()
        .with_context(|| format!("resolving build output directory {}", output_dir.display()))?;
    if !fs::metadata(&output_root)
        .with_context(|| format!("reading build output directory {}", output_root.display()))?
        .is_dir()
    {
        bail!(
            "build output path is not a directory: {}",
            output_root.display()
        );
    }

    let public_path = output_root.join("public");
    let public_source = public_path
        .canonicalize()
        .with_context(|| format!("resolving public directory {}", public_path.display()))?;
    if !fs::metadata(&public_source)
        .with_context(|| format!("reading public directory {}", public_source.display()))?
        .is_dir()
    {
        bail!(
            "public path is not a directory: {}",
            public_source.display()
        );
    }

    let manifest_source = output_root.join("sycamore-assets.json");
    let manifest_metadata = fs::metadata(&manifest_source)
        .with_context(|| format!("reading asset manifest {}", manifest_source.display()))?;
    if !manifest_metadata.is_file() {
        bail!(
            "asset manifest is not a regular file: {}",
            manifest_source.display()
        );
    }

    let directory = tempfile::Builder::new()
        .prefix("cargo-sycamore-backend-")
        .tempdir()
        .context("creating backend generation snapshot")?;
    let destination_root = directory
        .path()
        .canonicalize()
        .context("resolving backend generation snapshot")?;
    if destination_root.starts_with(&public_source) {
        bail!(
            "public directory {} contains the backend snapshot location",
            public_source.display()
        );
    }

    let executable_name = executable.file_name().ok_or_else(|| {
        anyhow!(
            "backend executable has no filename: {}",
            executable.display()
        )
    })?;
    let executable_copy = directory.path().join(executable_name);
    copy_regular_file(&executable, &executable_copy)?;

    let manifest_copy = directory.path().join("sycamore-assets.json");
    copy_regular_file(&manifest_source, &manifest_copy)?;

    let public_copy = directory.path().join("public");
    fs::create_dir(&public_copy).context("creating snapshot public directory")?;
    copy_public_directory(&public_source, &public_copy)?;
    fs::set_permissions(
        &public_copy,
        fs::metadata(&public_source)
            .context("reading public directory permissions")?
            .permissions(),
    )
    .context("preserving public directory permissions")?;

    Ok(Generation {
        executable: executable_copy
            .canonicalize()
            .context("resolving snapshot executable")?,
        public_dir: public_copy
            .canonicalize()
            .context("resolving snapshot public directory")?,
        manifest_path: manifest_copy
            .canonicalize()
            .context("resolving snapshot asset manifest")?,
        _directory: directory,
    })
}

/// Snapshot public files without turning out-of-tree symlinks into exposed files.
pub(crate) fn copy_public_directory(source: &Path, destination: &Path) -> Result<()> {
    let source = source
        .canonicalize()
        .context("resolving public directory")?;
    let destination = destination
        .canonicalize()
        .context("resolving public snapshot")?;
    anyhow::ensure!(
        !destination.starts_with(&source),
        "public directory contains its snapshot location"
    );
    copy_directory_contents(
        &source,
        &destination,
        &destination,
        &source,
        &mut HashSet::new(),
    )
}

fn copy_directory_contents(
    source: &Path,
    destination: &Path,
    snapshot_root: &Path,
    public_root: &Path,
    ancestors: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical_source = source
        .canonicalize()
        .with_context(|| format!("resolving public asset directory {}", source.display()))?;
    if canonical_source.starts_with(snapshot_root) {
        bail!(
            "public asset source enters the backend snapshot: {}",
            source.display()
        );
    }
    if !ancestors.insert(canonical_source.clone()) {
        bail!(
            "directory symlink cycle in public assets at {}",
            source.display()
        );
    }

    let result = (|| {
        for entry in fs::read_dir(source)
            .with_context(|| format!("reading public directory {}", source.display()))?
        {
            let entry = entry.with_context(|| format!("reading entry in {}", source.display()))?;
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            let canonical_entry = source_path
                .canonicalize()
                .with_context(|| format!("resolving public asset {}", source_path.display()))?;
            anyhow::ensure!(
                canonical_entry.starts_with(public_root),
                "public asset symlink escapes output directory: {}",
                source_path.display()
            );
            if canonical_entry.starts_with(snapshot_root) {
                bail!(
                    "public asset enters the backend snapshot: {}",
                    source_path.display()
                );
            }
            let metadata = fs::metadata(&canonical_entry)
                .with_context(|| format!("reading metadata for {}", source_path.display()))?;
            if metadata.is_dir() {
                fs::create_dir(&destination_path).with_context(|| {
                    format!("creating snapshot directory {}", destination_path.display())
                })?;
                copy_directory_contents(
                    &canonical_entry,
                    &destination_path,
                    snapshot_root,
                    public_root,
                    ancestors,
                )?;
                fs::set_permissions(&destination_path, metadata.permissions()).with_context(
                    || format!("preserving permissions for {}", destination_path.display()),
                )?;
            } else if metadata.is_file() {
                copy_regular_file(&canonical_entry, &destination_path)?;
            } else {
                bail!("unsupported public asset type: {}", source_path.display());
            }
        }
        Ok(())
    })();

    ancestors.remove(&canonical_source);
    result
}

fn copy_regular_file(source: &Path, destination: &Path) -> Result<()> {
    fs::copy(source, destination)
        .with_context(|| format!("copying {} to {}", source.display(), destination.display()))?;
    let permissions = fs::metadata(source)
        .with_context(|| format!("reading permissions for {}", source.display()))?
        .permissions();
    fs::set_permissions(destination, permissions)
        .with_context(|| format!("preserving permissions for {}", destination.display()))?;
    Ok(())
}

fn launch(generation: Generation, backend_port: u16) -> Result<(Child, Generation)> {
    // This check also runs after stopping the managed process: an unsupported
    // grandchild must not masquerade as the newly launched backend.
    ensure_port_available(backend_port)?;
    let address = format!("127.0.0.1:{backend_port}");
    let mut child = Command::new(&generation.executable)
        .env("SYCAMORE_SERVER_ADDR", &address)
        .env("SYCAMORE_PUBLIC_DIR", &generation.public_dir)
        .env("SYCAMORE_ASSET_MANIFEST", &generation.manifest_path)
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting backend {}", generation.executable.display()))?;

    if let Err(error) = wait_until_ready(&mut child, backend_port) {
        let stop_error = stop_child(&mut child).err();
        return Err(match stop_error {
            Some(stop_error) => error.context(format!(
                "also failed to stop failed backend: {stop_error:#}"
            )),
            None => error,
        });
    }
    Ok((child, generation))
}

fn ensure_port_available(port: u16) -> Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(address).with_context(|| {
        format!("backend port {port} is unavailable (already occupied or cannot be bound)")
    })?;
    drop(listener);
    Ok(())
}

fn wait_until_ready(child: &mut Child, port: u16) -> Result<()> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let deadline = Instant::now() + READINESS_TIMEOUT;
    loop {
        if let Some(status) = child
            .try_wait()
            .context("checking backend startup status")?
        {
            bail!(
                "backend exited before becoming ready ({})",
                display_status(status)
            );
        }
        if TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            if let Some(status) = child.try_wait().context("checking ready backend process")? {
                bail!(
                    "backend exited before becoming ready ({})",
                    display_status(status)
                );
            }
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("backend did not become ready at {address} within {READINESS_TIMEOUT:?}");
        }
        thread::sleep(READINESS_POLL_INTERVAL);
    }
}

fn stop_child(child: &mut Child) -> Result<ExitStatus> {
    match child
        .try_wait()
        .context("checking backend process status")?
    {
        Some(status) => Ok(status),
        None => {
            child.kill().context("killing backend process")?;
            child.wait().context("reaping backend process")
        }
    }
}

fn display_status(status: ExitStatus) -> String {
    status
        .code()
        .map(|code| format!("exit status {code}"))
        .unwrap_or_else(|| "terminated by signal".to_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    fn executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn port() -> u16 {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    }

    fn compile_server(directory: &Path) -> PathBuf {
        let source = directory.join("server.rs");
        let executable = directory.join("server");
        fs::write(
            &source,
            r#"use std::{env, net::TcpListener, thread, time::Duration};
fn main() {
    let address = env::var("SYCAMORE_SERVER_ADDR").unwrap();
    let public_dir = env::var("SYCAMORE_PUBLIC_DIR").unwrap();
    let manifest = env::var("SYCAMORE_ASSET_MANIFEST").unwrap();
    assert!(std::path::Path::new(&public_dir).is_dir());
    assert!(std::path::Path::new(&manifest).is_file());
    assert_eq!(std::path::Path::new(&manifest).parent(), std::path::Path::new(&public_dir).parent());
    let listener = TcpListener::bind(address).unwrap();
    for _ in listener.incoming() { thread::sleep(Duration::from_millis(1)); }
}
"#,
        )
        .unwrap();
        let output = Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&executable)
            .output()
            .expect("rustc must be available to run backend supervisor tests");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        executable
    }

    #[test]
    fn snapshot_copies_hidden_assets_and_executable_permissions() {
        let source = tempfile::tempdir().unwrap();
        let exe = source.path().join("backend");
        executable(&exe, "#!/bin/sh\nexit 0\n");
        let output = source.path().join("dist");
        let public = output.join("public");
        fs::create_dir_all(public.join(".hidden/nested")).unwrap();
        fs::write(public.join(".hidden/nested/file"), "asset").unwrap();
        fs::write(output.join("sycamore-assets.json"), "manifest").unwrap();

        let generation = snapshot(&exe, &output).unwrap();
        assert_eq!(
            fs::read(generation.public_dir.join(".hidden/nested/file")).unwrap(),
            b"asset"
        );
        assert_eq!(fs::read(&generation.manifest_path).unwrap(), b"manifest");
        assert_eq!(
            generation.manifest_path.parent(),
            generation.public_dir.parent()
        );
        assert_eq!(
            fs::metadata(generation.executable)
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0o111
        );
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_rejects_directory_symlink_cycles() {
        use std::os::unix::fs::symlink;
        let source = tempfile::tempdir().unwrap();
        let exe = source.path().join("backend");
        executable(&exe, "#!/bin/sh\nexit 0\n");
        let output = source.path().join("dist");
        let public = output.join("public");
        fs::create_dir_all(&public).unwrap();
        fs::write(output.join("sycamore-assets.json"), "manifest").unwrap();
        symlink(&public, public.join("loop")).unwrap();
        let error = snapshot(&exe, &output).unwrap_err();
        assert!(format!("{error:#}").contains("cycle"));
    }

    #[test]
    fn public_snapshot_rejects_out_of_tree_symlinks() {
        use std::os::unix::fs::symlink;
        let source = tempfile::tempdir().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let public = source.path().join("public");
        fs::create_dir(&public).unwrap();
        fs::write(source.path().join("secret"), "not public").unwrap();
        symlink(source.path().join("secret"), public.join("leak")).unwrap();
        let error = copy_public_directory(&public, destination.path()).unwrap_err();
        assert!(error.to_string().contains("escapes output directory"));
        assert!(!destination.path().join("leak").exists());
    }

    #[test]
    fn restart_rolls_back_after_new_executable_exits_early() {
        let source = tempfile::tempdir().unwrap();
        let server = compile_server(source.path());
        let broken = source.path().join("broken");
        executable(&broken, "#!/bin/sh\nexit 9\n");
        let output = source.path().join("dist");
        let public = output.join("public");
        fs::create_dir_all(&public).unwrap();
        fs::write(output.join("sycamore-assets.json"), "{}").unwrap();
        fs::write(public.join("index.html"), "before").unwrap();
        let backend_port = port();
        let mut failed_port = port();
        while failed_port == backend_port {
            failed_port = port();
        }

        let mut supervisor = BackendSupervisor::new();
        supervisor.restart(&server, &output, backend_port).unwrap();
        assert!(TcpStream::connect(("127.0.0.1", backend_port)).is_ok());
        let old_snapshot = supervisor
            .running
            .as_ref()
            .unwrap()
            .generation
            .public_dir
            .join("index.html");
        fs::write(public.join("index.html"), "after").unwrap();
        assert_eq!(fs::read_to_string(&old_snapshot).unwrap(), "before");

        let error = supervisor
            .restart(&broken, &output, failed_port)
            .unwrap_err();
        assert!(format!("{error:#}").contains("exited before becoming ready"));
        assert_eq!(
            supervisor.running.as_ref().unwrap().backend_port,
            backend_port
        );
        assert!(TcpStream::connect(("127.0.0.1", backend_port)).is_ok());
        assert!(TcpStream::connect(("127.0.0.1", failed_port)).is_err());
        let child = &mut supervisor.running.as_mut().unwrap().child;
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            supervisor
                .check()
                .unwrap_err()
                .to_string()
                .contains("exited unexpectedly")
        );
        supervisor.check().unwrap(); // A runtime exit is reported only once.
        supervisor.stop().unwrap();
        assert!(supervisor.running.is_none());
    }

    #[test]
    fn first_start_refuses_an_occupied_port() {
        let source = tempfile::tempdir().unwrap();
        let server = compile_server(source.path());
        let output = source.path().join("dist");
        fs::create_dir_all(output.join("public")).unwrap();
        fs::write(output.join("sycamore-assets.json"), "{}").unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let occupied_port = listener.local_addr().unwrap().port();

        let mut supervisor = BackendSupervisor::new();
        let error = supervisor
            .restart(&server, &output, occupied_port)
            .unwrap_err();
        assert!(format!("{error:#}").contains("already occupied"));
        assert!(supervisor.running.is_none());
        drop(listener);
    }
}
