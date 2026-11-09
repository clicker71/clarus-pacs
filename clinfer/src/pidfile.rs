// Copyright (C) 2026 Daniil Solgalov <clicker71@github>. License: LGPLv3.

//! A PID file, so a second sidecar cannot silently share one config.
//!
//! Ported from the server's own implementation, which is the reference for
//! this repository: the same stale-pid rule, the same "only a live foreign
//! pid is fatal" rule, and the same refusal to make the location
//! configurable. A PID file is an internal artefact.
//!
//! The failure semantics matter more than the file. A PID file that cannot
//! be written degrades observability and must NOT stop the service; a PID
//! file naming a live process is the one fatal case, because two sidecars
//! claiming the same workitems is a data-integrity problem rather than an
//! inconvenience.

use std::io::Write;
use std::path::{Path, PathBuf};

/// The PID file for a configuration file: `<config>.pid`, beside it.
///
/// Deriving it from the config path rather than from a port is deliberate:
/// two sidecars on one host are separated by their configurations, and a
/// config-adjacent file is writable by whoever can run the process, which
/// `/var/run` is not without packaging.
#[must_use]
pub fn path_for(config_path: &Path) -> PathBuf {
    let mut name = config_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "clinfer.conf".to_string());
    name.push_str(".pid");
    config_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(name)
}

/// Whether a process with this pid is running.
///
/// Standard library only: no `unsafe`, no extra crate. A false answer is
/// safe (the file is treated as stale and overwritten), and a false
/// negative would need the pid to have been recycled onto an unrelated
/// process, which the overwrite warning then covers.
#[must_use]
pub fn is_process_alive(pid: u32) -> bool {
    is_process_alive_impl(pid)
}

#[cfg(windows)]
fn is_process_alive_impl(pid: u32) -> bool {
    match std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()),
        Err(_) => false,
    }
}

#[cfg(unix)]
fn is_process_alive_impl(pid: u32) -> bool {
    // `pid_t` is i32. 0 names the kernel and anything above i32::MAX wraps
    // to a negative value, which kill(1) treats as a signal to a process
    // GROUP (-1 = every process the caller may signal) - neither is a
    // process, so a pid file holding such a value is stale by definition.
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(any(windows, unix)))]
fn is_process_alive_impl(_pid: u32) -> bool {
    false
}

/// Claim the PID file at `path`.
///
/// Returns `Ok(true)` when the file now names this process, `Ok(false)`
/// when it could not be written for a non-fatal reason, and `Err` when a
/// LIVE foreign process owns it - the caller's cue to exit rather than run
/// a second claimant.
///
/// A stale pid (dead, unparseable or unreadable) is overwritten: a crash
/// leaves a pid file behind by definition, and refusing to start after one
/// would make the file a liability.
pub fn claim(path: &Path, pid: u32) -> Result<bool, String> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            let _ = writeln!(file, "{pid}");
            Ok(true)
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let previous = std::fs::read_to_string(path).unwrap_or_default();
            let previous = previous.trim().to_string();
            let stale = previous
                .parse::<u32>()
                .map(|old| !is_process_alive(old))
                .unwrap_or(true);
            if !stale {
                return Err(format!(
                    "PID file {} exists and pid {} is alive: another clinfer is \
                     running with this configuration. Remove {} if that is wrong.",
                    path.display(),
                    previous,
                    path.display()
                ));
            }
            // Remove-then-create is not atomic, but this runs once before
            // serving: a race here means two processes started in the same
            // instant, and `create_new` still makes the loser fail.
            let _ = std::fs::remove_file(path);
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{pid}");
                    Ok(true)
                }
                Err(err) => {
                    eprintln!(
                        "clinfer: WARNING: cannot rewrite the stale PID file {}: {err}",
                        path.display()
                    );
                    Ok(false)
                }
            }
        }
        Err(err) => {
            eprintln!(
                "clinfer: WARNING: cannot create the PID file {}: {err}",
                path.display()
            );
            Ok(false)
        }
    }
}

/// Write this process's PID beside its configuration.
///
/// Returns the path in use, or an empty path when the process runs without
/// one. Only a live foreign pid is fatal.
///
/// # Errors
///
/// Returns the message describing the live foreign owner.
pub fn acquire(config_path: &Path) -> Result<PathBuf, String> {
    let path = path_for(config_path);
    match claim(&path, std::process::id())? {
        true => Ok(path),
        false => Ok(PathBuf::new()),
    }
}

/// Remove the PID file, ignoring failures.
///
/// Called on the clean exit path only. A hard kill leaves the file behind,
/// which is the case the stale-pid rule exists for.
pub fn release(path: &Path) {
    if !path.as_os_str().is_empty() {
        let _ = std::fs::remove_file(path);
    }
}

/// RAII ownership of a claimed PID file: removed on `Drop`.
///
/// `main` holds this so an EARLY error path - a failed bind, a failed
/// origin - releases the file, while a hard kill still leaves it behind,
/// which is the stale-pid rule's case. Note that `process::exit` does NOT
/// run destructors, so the forced-exit paths still release explicitly.
pub struct PidfileGuard(PathBuf);

impl PidfileGuard {
    /// Claim the PID file beside `config_path`; a live foreign owner is
    /// still the one fatal case.
    pub fn new(config_path: &Path) -> Result<Self, String> {
        acquire(config_path).map(Self)
    }

    /// The claimed path, for the explicit releases on `process::exit`
    /// paths (which skip destructors).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PidfileGuard {
    fn drop(&mut self) {
        release(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory that cleans itself up.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "clinfer-pid-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("scratch");
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("clinfer.conf.pid")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_file_sits_beside_the_configuration() {
        let path = path_for(Path::new("/etc/clinfer/clinfer.conf"));
        assert_eq!(path, Path::new("/etc/clinfer/clinfer.conf.pid"));
    }

    #[test]
    fn a_guard_releases_the_pid_file_on_drop() {
        let scratch = Scratch::new();
        let guard = PidfileGuard::new(&scratch.0.join("clinfer.conf")).expect("claim");
        assert!(scratch.file().exists(), "the claim must create the file");
        drop(guard);
        assert!(!scratch.file().exists(), "the drop must remove the file");
    }

    #[test]
    fn a_config_with_no_directory_still_gets_a_name() {
        // The default configuration is `./clinfer.conf`, whose parent is
        // the empty path; `Path::new("").join(..)` yields the bare name,
        // which resolves against the working directory - the same place
        // the relative config path itself resolves.
        let path = path_for(Path::new("clinfer.conf"));
        assert_eq!(path, Path::new("clinfer.conf.pid"));

        // And the explicit relative spelling lands in the same place.
        assert_eq!(
            path_for(Path::new("./clinfer.conf")),
            Path::new("./clinfer.conf.pid")
        );
    }

    #[test]
    fn a_free_path_is_claimed_and_names_this_process() {
        let scratch = Scratch::new();
        let path = scratch.file();
        assert!(claim(&path, 4321).expect("claimed"));
        let written = std::fs::read_to_string(&path).expect("read");
        assert_eq!(written.trim(), "4321");
    }

    #[test]
    fn a_stale_pid_is_overwritten_rather_than_refused() {
        // A crash leaves the file behind by definition. Refusing to start
        // after one would make the PID file a liability rather than a
        // guard.
        let scratch = Scratch::new();
        let path = scratch.file();
        // A pid that cannot be alive: the maximum is well past any real
        // process on both platforms.
        std::fs::write(&path, "4294967295\n").expect("write a stale pid");
        assert!(claim(&path, 4321).expect("the stale file is taken over"));
        let written = std::fs::read_to_string(&path).expect("read");
        assert_eq!(written.trim(), "4321");
    }

    #[test]
    fn an_unparsable_pid_is_treated_as_stale() {
        // A truncated write or a hand-edited file: there is no pid to
        // check, and refusing to start would need an operator for a file
        // that carries no information.
        let scratch = Scratch::new();
        let path = scratch.file();
        std::fs::write(&path, "not a pid\n").expect("write");
        assert!(claim(&path, 4321).expect("taken over"));
        assert_eq!(std::fs::read_to_string(&path).expect("read").trim(), "4321");
    }

    #[test]
    fn a_live_pid_is_refused_and_the_message_names_both_processes() {
        // The one fatal case. Two sidecars sharing a configuration would
        // both claim the same workitems.
        let scratch = Scratch::new();
        let path = scratch.file();
        let live = std::process::id();
        std::fs::write(&path, format!("{live}\n")).expect("write");
        let err = claim(&path, live.wrapping_add(1)).expect_err("a live owner is refused");
        assert!(err.contains(&live.to_string()), "names the owner: {err}");
        assert!(err.contains("clinfer"), "names the process: {err}");
        // And the file was left alone rather than clobbered.
        assert_eq!(
            std::fs::read_to_string(&path).expect("read").trim(),
            live.to_string()
        );
    }

    #[test]
    fn our_own_live_pid_is_still_a_live_pid() {
        // The check must not special-case the current process: a second
        // start of the SAME binary is exactly the collision to catch, and
        // it presents as a live pid that happens to be ours.
        let scratch = Scratch::new();
        let path = scratch.file();
        let mine = std::process::id();
        std::fs::write(&path, format!("{mine}\n")).expect("write");
        assert!(claim(&path, mine).is_err(), "a live pid is refused");
    }

    #[test]
    fn acquiring_then_releasing_leaves_nothing_behind() {
        let scratch = Scratch::new();
        let config = scratch.0.join("clinfer.conf");
        std::fs::write(&config, "server: {}\n").expect("write config");
        let path = acquire(&config).expect("acquired");
        assert!(path.exists(), "the pid file is there while we hold it");
        assert_eq!(path, path_for(&config));
        release(&path);
        assert!(!path.exists(), "and gone after a clean exit");
    }

    #[test]
    fn releasing_an_empty_path_is_not_an_error() {
        // The "no writable location" case returns an empty path, and the
        // shutdown path must cope with it rather than trying to remove the
        // current directory.
        release(Path::new(""));
    }

    #[test]
    fn the_current_process_is_alive() {
        assert!(is_process_alive(std::process::id()));
    }

    #[test]
    fn an_impossible_pid_is_not_alive() {
        assert!(!is_process_alive(4_294_967_295));
    }
}
