//! Running inside a Flatpak.
//!
//! Photon ships on Linux as a Flatpak, but it is a front end to the user's own
//! tools and folders, so two things behave as in a native build:
//!
//! - **Folders.** The catalogue, thumbnails, models and preferences stay in
//!   `~/.local/share/photon`, `~/.cache/photon` and `~/.config/photon`, not in
//!   `~/.var/app/…`, so the Flatpak and a source build share one library.
//!   [`use_host_folders`] points the XDG variables back at the host's before
//!   anything reads them (the manifest grants `--filesystem=host`).
//! - **Programs.** darktable-cli, exiftool, the video thumbnailers and the
//!   user's editors live on the host. [`command`] runs them through
//!   `flatpak-spawn --host` (the manifest grants `org.freedesktop.Flatpak`).
//!   A program the host doesn't have fails to spawn with `NotFound`, exactly
//!   as natively, so callers keep their "isn't installed" handling.
//!
//! Outside a Flatpak every function here is a no-op or the plain equivalent.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};

/// Whether Photon runs inside a Flatpak sandbox.
pub fn in_flatpak() -> bool {
    static IN: OnceLock<bool> = OnceLock::new();
    *IN.get_or_init(|| Path::new("/.flatpak-info").exists())
}

/// Inside a Flatpak, use the host's XDG data, cache, config and state
/// folders (`HOST_XDG_*` when the host set them, else the XDG defaults
/// under `$HOME`), and a temporary folder host programs can see (the
/// sandbox's `/tmp` is private). Must run first in `main`, before GLib or
/// any thread reads the environment.
pub fn use_host_folders() {
    if !in_flatpak() {
        return;
    }
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else { return };
    for (var, default) in [
        ("XDG_DATA_HOME", ".local/share"),
        ("XDG_CACHE_HOME", ".cache"),
        ("XDG_CONFIG_HOME", ".config"),
        ("XDG_STATE_HOME", ".local/state"),
    ] {
        let host = std::env::var_os(format!("HOST_{var}"))
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default));
        std::env::set_var(var, host);
    }
    let cache = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache"));
    let tmp = cache.join("photon").join("tmp");
    match std::fs::create_dir_all(&tmp) {
        Ok(()) => std::env::set_var("TMPDIR", &tmp),
        Err(e) => eprintln!("photon: can't create {}: {e}; host programs can't see temporary files", tmp.display()),
    }
}

/// Where `program` is: on the host inside a Flatpak, else on `PATH` (or
/// `program` itself when it is a path). `None` if it isn't installed.
pub fn find_program(program: &OsStr) -> Option<PathBuf> {
    if !in_flatpak() {
        let path = Path::new(program);
        if path.components().count() > 1 {
            return path.is_file().then(|| path.to_path_buf());
        }
        return std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(program)).find(|p| p.is_file());
    }
    static FOUND: OnceLock<Mutex<HashMap<OsString, Option<PathBuf>>>> = OnceLock::new();
    let cache = FOUND.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(program) {
        return found.clone();
    }
    // `command -v` on the host, through its shell, so its PATH applies.
    let found = Command::new("flatpak-spawn")
        .args(["--host", "sh", "-c", "command -v -- \"$1\"", "sh"])
        .arg(program)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(program.to_os_string(), found.clone());
    found
}

/// A command running `program` where the user's programs are: on the host
/// inside a Flatpak (through `flatpak-spawn --host`), else directly. When
/// the host doesn't have `program`, spawning fails with `NotFound`.
pub fn command(program: impl AsRef<OsStr>) -> Command {
    let program = program.as_ref();
    if !in_flatpak() {
        return Command::new(program);
    }
    match find_program(program) {
        Some(path) => {
            let mut cmd = Command::new("flatpak-spawn");
            cmd.arg("--host").arg(path);
            cmd
        }
        // A path that can't exist: spawn() then reports NotFound, as natively.
        None => Command::new(Path::new("/nonexistent/photon-host-program").join(program)),
    }
}

/// Stop a child started with [`command`]. Inside a Flatpak the child is
/// `flatpak-spawn`, which forwards SIGTERM to the host program but can't
/// forward SIGKILL, so it gets SIGTERM first (and a moment to pass it on).
pub fn terminate(child: &mut Child) {
    if in_flatpak() {
        if let Ok(pid) = i32::try_from(child.id()) {
            // SAFETY: sending a signal to our own child's pid has no memory effects.
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
            for _ in 0..20 {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }
    }
    let _ = child.kill();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_are_found_on_path_and_missing_ones_fail_as_not_found() {
        // The test runs outside a Flatpak: plain PATH lookup and spawning.
        assert!(!in_flatpak() || std::env::var_os("FLATPAK_ID").is_some());
        if in_flatpak() {
            return;
        }
        assert!(find_program(OsStr::new("sh")).is_some());
        assert_eq!(find_program(OsStr::new("photon-no-such-program")), None);
        let err = command("photon-no-such-program").spawn().unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        let status = command("sh").args(["-c", "exit 3"]).status().unwrap();
        assert_eq!(status.code(), Some(3));
    }
}
