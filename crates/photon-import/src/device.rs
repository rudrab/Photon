//! What kind of storage a folder lives on, to choose how many files an
//! import reads at once.
//!
//! A hard disk, a memory card and many USB drives serve one sequential
//! stream far faster than several: measured on a USB SSD, reading the same
//! 160 photos took 8 s with one thread (210 MB/s) and 35 s with four
//! (49 MB/s). Internal SSDs and network shares gain from parallel reads.

use std::path::{Path, PathBuf};

/// Read streams for internal SSDs and anything not recognised.
pub const PARALLEL_IO_THREADS: usize = 4;

/// How the storage under a path is attached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// A spinning disk, a USB device or a memory card reader: one stream.
    Sequential,
    /// Internal flash, network shares, or unknown: several streams.
    Parallel,
}

impl Storage {
    pub fn io_threads(self) -> usize {
        match self {
            Storage::Sequential => 1,
            Storage::Parallel => PARALLEL_IO_THREADS,
        }
    }
}

/// The storage under `path` (Linux; anything else is [`Storage::Parallel`]).
pub fn storage_of(path: &Path) -> Storage {
    #[cfg(target_os = "linux")]
    {
        if let Some(storage) = linux::storage_of(path) {
            return storage;
        }
    }
    let _ = path;
    Storage::Parallel
}

/// Read streams for importing `files`: judged by the first file.
pub fn suggested_io_threads(files: &[PathBuf]) -> usize {
    files.first().map_or(PARALLEL_IO_THREADS, |f| storage_of(f).io_threads())
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Storage;
    use std::path::Path;

    pub fn storage_of(path: &Path) -> Option<Storage> {
        let path = path.canonicalize().ok()?;
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let source = mount_source(&mountinfo, &path)?;
        classify(&std::fs::canonicalize(source).ok()?)
    }

    /// The device node behind the mount that contains `path`, e.g. `/dev/sda1`.
    /// None when that mount isn't a block device (tmpfs, network share, FUSE).
    pub(super) fn mount_source(mountinfo: &str, path: &Path) -> Option<String> {
        let mut best: Option<(usize, String)> = None;
        for line in mountinfo.lines() {
            // id parent major:minor root mountpoint options [optional...] - fstype source super-options
            let Some((before, after)) = line.split_once(" - ") else { continue };
            let Some(mountpoint) = before.split(' ').nth(4).map(unescape) else { continue };
            let Some(source) = after.split(' ').nth(1) else { continue };
            if !path.starts_with(&mountpoint) {
                continue;
            }
            // The deepest mount wins (later lines win ties: a mount on top of another).
            let depth = Path::new(&mountpoint).components().count();
            if best.as_ref().map_or(true, |(d, _)| depth >= *d) {
                best = Some((depth, source.to_string()));
            }
        }
        best.map(|(_, source)| source).filter(|source| source.starts_with("/dev/"))
    }

    /// mountinfo escapes spaces and a few others as octal (`\040`).
    fn unescape(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let digits: String = chars.by_ref().take(3).collect();
                if let Some(ch) = u8::from_str_radix(&digits, 8).ok().map(char::from) {
                    out.push(ch);
                    continue;
                }
                out.push('\\');
                out.push_str(&digits);
            } else {
                out.push(c);
            }
        }
        out
    }

    /// From the device's place in sysfs: USB and memory cards, and spinning disks.
    fn classify(device: &Path) -> Option<Storage> {
        let name = device.file_name()?.to_str()?;
        let sys = std::fs::canonicalize(format!("/sys/class/block/{name}")).ok()?;
        let sys = sys.to_string_lossy();
        if sys.contains("/usb") || sys.contains("/mmc") {
            return Some(Storage::Sequential);
        }
        // A partition has no queue/ of its own: its disk's, one level up.
        let rotational = ["", "/.."].iter().find_map(|up| {
            std::fs::read_to_string(format!("/sys/class/block/{name}{up}/queue/rotational")).ok()
        })?;
        Some(if rotational.trim() == "1" { Storage::Sequential } else { Storage::Parallel })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        const MOUNTINFO: &str = "\
36 1 0:30 /root / rw,relatime shared:1 - btrfs /dev/nvme0n1p8 rw,ssd
40 36 0:32 / /home rw,relatime shared:2 - btrfs /dev/nvme0n1p8 rw,ssd,subvol=/home
91 36 8:1 / /run/media/rudra/My\\040Drive rw,nosuid shared:3 - ext4 /dev/sda1 rw,errors=remount-ro
92 36 0:50 / /run/user/1000 rw shared:4 - tmpfs tmpfs rw
93 40 0:60 / /home/rudra/net rw shared:5 - nfs4 server:/share rw
";

        #[test]
        fn finds_the_device_of_the_deepest_mount() {
            let src = |p: &str| mount_source(MOUNTINFO, Path::new(p));
            assert_eq!(src("/home/rudra/Pictures/2024/a.jpg").as_deref(), Some("/dev/nvme0n1p8"));
            assert_eq!(src("/run/media/rudra/My Drive/Pictures/a.jpg").as_deref(), Some("/dev/sda1"));
            assert_eq!(src("/etc/hosts").as_deref(), Some("/dev/nvme0n1p8"));
            // tmpfs and network mounts have no device node: unknown, not the disk underneath.
            assert_eq!(src("/run/user/1000/x"), None);
            assert_eq!(src("/home/rudra/net/a.jpg"), None);
        }

        #[test]
        fn unescapes_octal() {
            assert_eq!(unescape("/a\\040b\\134c"), "/a b\\c");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_picks_the_stream_count() {
        assert_eq!(Storage::Sequential.io_threads(), 1);
        assert_eq!(Storage::Parallel.io_threads(), PARALLEL_IO_THREADS);
        assert_eq!(suggested_io_threads(&[]), PARALLEL_IO_THREADS);
    }

    #[test]
    fn an_ordinary_path_gets_an_answer() {
        // Whatever this machine's disk is, asking must not fail or hang.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.jpg");
        std::fs::write(&file, b"x").unwrap();
        let n = suggested_io_threads(&[file]);
        assert!(n == 1 || n == PARALLEL_IO_THREADS);
    }
}
