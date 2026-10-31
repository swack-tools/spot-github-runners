use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const MAX_TRAVERSAL_DEPTH: usize = 64;
pub const MAX_TRAVERSAL_ENTRIES: usize = 100_000;

#[derive(Debug, Error)]
pub enum CleanTargetError {
    #[error("target is not a directory")]
    NotADirectory,

    #[error("Source cleanup depth bound")]
    DepthBoundExceeded,

    #[error("Source cleanup entry bound")]
    EntryBoundExceeded,

    #[error("Foreign filesystem inside target")]
    ForeignFilesystem,

    #[error("safe directory removal is unavailable: {0}")]
    Unavailable(String),

    #[error(transparent)]
    Io(#[from] io::Error),
}

#[derive(Debug, PartialEq, Eq)]
pub enum CleanOutcome {
    Disabled,
    SkippedOtherRepository,
    SkippedUnexpectedWorkspace,
    TargetAbsent {
        free_bytes: u64,
    },
    TargetRemoved {
        expected_path: PathBuf,
        free_before: u64,
        free_after: u64,
    },
}

/// Computes available free bytes on the filesystem hosting `path`,
/// matching Python's `shutil.disk_usage(path).free`.
pub fn get_free_bytes<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    let c_path = CString::new(path.as_ref().as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let res = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    if res != 0 {
        return Err(io::Error::last_os_error());
    }

    let frsize = if stat.f_frsize != 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Ok(stat.f_bavail as u64 * frsize as u64)
}

/// RAII wrapper around `*mut libc::DIR` that automatically calls `closedir`.
struct DirStream(*mut libc::DIR);

impl DirStream {
    fn open_from_fd(fd: RawFd) -> io::Result<Self> {
        let dup_fd = unsafe { libc::dup(fd) };
        if dup_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let dir = unsafe { libc::fdopendir(dup_fd) };
        if dir.is_null() {
            let err = io::Error::last_os_error();
            unsafe {
                libc::close(dup_fd);
            }
            return Err(err);
        }
        Ok(Self(dir))
    }
}

impl Drop for DirStream {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
}

/// Helper to open a directory descriptor relative to an optional parent fd,
/// enforcing `O_RDONLY | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC`.
pub fn open_dir_nofollow(parent_fd: Option<RawFd>, name: &CStr) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let raw_fd = match parent_fd {
        Some(dir_fd) => unsafe { libc::openat(dir_fd, name.as_ptr(), flags) },
        None => unsafe { libc::open(name.as_ptr(), flags) },
    };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

/// Recursively traverses and empties `dir_fd` descriptor-relatively without following symlinks.
pub fn empty_directory_fd(
    dir_fd: RawFd,
    expected_dev: libc::dev_t,
    depth: usize,
    count: &mut usize,
) -> Result<(), CleanTargetError> {
    if depth > MAX_TRAVERSAL_DEPTH {
        return Err(CleanTargetError::DepthBoundExceeded);
    }

    let stream = DirStream::open_from_fd(dir_fd)?;
    loop {
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            break;
        }

        let entry_name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let bytes = entry_name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }

        *count += 1;
        if *count > MAX_TRAVERSAL_ENTRIES {
            return Err(CleanTargetError::EntryBoundExceeded);
        }

        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let stat_res = unsafe {
            libc::fstatat(
                dir_fd,
                entry_name.as_ptr(),
                &mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_res != 0 {
            return Err(CleanTargetError::Io(io::Error::last_os_error()));
        }

        if st.st_dev != expected_dev {
            return Err(CleanTargetError::ForeignFilesystem);
        }

        if (st.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            let child_fd = open_dir_nofollow(Some(dir_fd), entry_name)?;
            empty_directory_fd(child_fd.as_raw_fd(), expected_dev, depth + 1, count)?;
            drop(child_fd);

            let res = unsafe { libc::unlinkat(dir_fd, entry_name.as_ptr(), libc::AT_REMOVEDIR) };
            if res != 0 {
                return Err(CleanTargetError::Io(io::Error::last_os_error()));
            }
        } else {
            // Unlinks regular files, sockets, FIFOs, and SYMLINKS.
            // Using flag 0 unlinks the symlink itself without following it!
            let res = unsafe { libc::unlinkat(dir_fd, entry_name.as_ptr(), 0) };
            if res != 0 {
                return Err(CleanTargetError::Io(io::Error::last_os_error()));
            }
        }
    }

    Ok(())
}

/// Verifies that neither the path nor any of its ancestors is a symbolic link.
pub fn checked_path(path: &Path) -> Result<(), CleanTargetError> {
    let mut curr = Some(path);
    while let Some(p) = curr {
        #[cfg(target_os = "macos")]
        if p == Path::new("/var") || p == Path::new("/tmp") || p == Path::new("/etc") {
            curr = p.parent();
            continue;
        }
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            if meta.file_type().is_symlink() {
                return Err(CleanTargetError::Unavailable(format!(
                    "work_root ancestor is a symlink: {}",
                    p.display()
                )));
            }
        }
        curr = p.parent();
    }
    Ok(())
}

/// Pure-Rust implementation of OxiDex runner target cleanup with explicit environment.
pub fn clean_target_with_env(
    work_root: &Path,
    env: &HashMap<String, String>,
) -> Result<CleanOutcome, CleanTargetError> {
    checked_path(Path::new(work_root))?;

    // Cleanup runs only when at least one switch is set and every switch that is set says 1,
    // so EXAMPLE_RUST_PROJECT_CLEAN_TARGET=0 turns it off even though the image sets OXIDEX_CLEAN_TARGET=1.
    let switches: Vec<&str> = ["OXIDEX_CLEAN_TARGET", "EXAMPLE_RUST_PROJECT_CLEAN_TARGET"]
        .iter()
        .filter_map(|name| env.get(*name).map(String::as_str))
        .collect();
    if switches.is_empty() || switches.iter().any(|v| *v != "1") {
        return Ok(CleanOutcome::Disabled);
    }

    if env.get("GITHUB_REPOSITORY").map(String::as_str) != Some("swack-tools/oxidex") {
        return Ok(CleanOutcome::SkippedOtherRepository);
    }

    let expected = work_root.join("oxidex").join("oxidex");
    let expected_str = expected.to_string_lossy();
    if env.get("GITHUB_WORKSPACE").map(String::as_str) != Some(&expected_str) {
        return Ok(CleanOutcome::SkippedUnexpectedWorkspace);
    }

    let c_work_root = CString::new(work_root.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let c_oxidex = CStr::from_bytes_with_nul(b"oxidex\0").unwrap();
    let c_target = CStr::from_bytes_with_nul(b"target\0").unwrap();

    // 1. Open work_root refusing symlinks
    let root_fd = open_dir_nofollow(None, &c_work_root)?;

    // 2. Open work_root/oxidex refusing symlinks
    let repo_fd = open_dir_nofollow(Some(root_fd.as_raw_fd()), c_oxidex)?;

    // 3. Open work_root/oxidex/oxidex refusing symlinks
    let workspace_fd = open_dir_nofollow(Some(repo_fd.as_raw_fd()), c_oxidex)?;

    let before = get_free_bytes(work_root)?;

    // 4. Check target via fstatat without following symlinks
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let stat_res = unsafe {
        libc::fstatat(
            workspace_fd.as_raw_fd(),
            c_target.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };

    if stat_res != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return Ok(CleanOutcome::TargetAbsent { free_bytes: before });
        }
        return Err(CleanTargetError::Io(err));
    }

    // Must be a real directory. If it is a symlink, st.st_mode has S_IFLNK, not S_IFDIR.
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        return Err(CleanTargetError::NotADirectory);
    }

    let target_dev = st.st_dev;

    // 5. Open target descriptor refusing symlinks
    let target_fd = open_dir_nofollow(Some(workspace_fd.as_raw_fd()), c_target)?;

    // 6. Recursively empty target contents
    let mut entry_count = 0;
    empty_directory_fd(target_fd.as_raw_fd(), target_dev, 0, &mut entry_count)?;
    drop(target_fd);

    // 7. Remove the now-empty target directory itself
    let rm_res = unsafe {
        libc::unlinkat(
            workspace_fd.as_raw_fd(),
            c_target.as_ptr(),
            libc::AT_REMOVEDIR,
        )
    };
    if rm_res != 0 {
        return Err(CleanTargetError::Io(io::Error::last_os_error()));
    }

    // 8. Commit changes to workspace directory
    unsafe {
        libc::fsync(workspace_fd.as_raw_fd());
    }

    let after = get_free_bytes(work_root)?;
    Ok(CleanOutcome::TargetRemoved {
        expected_path: expected,
        free_before: before,
        free_after: after,
    })
}

/// Convenience entrypoint using current process environment.
pub fn clean_target(work_root: &Path) -> Result<CleanOutcome, CleanTargetError> {
    let env_map: HashMap<String, String> = std::env::vars().collect();
    clean_target_with_env(work_root, &env_map)
}

/// CLI runner matching exact Python stdout/stderr strings and exit codes.
pub fn run(args: &[String]) -> Result<i32, Box<dyn std::error::Error>> {
    let default_root = PathBuf::from("/work");
    let work_root = args.first().map(PathBuf::from).unwrap_or(default_root);

    let env_map: HashMap<String, String> = std::env::vars().collect();
    match clean_target_with_env(&work_root, &env_map) {
        Ok(CleanOutcome::Disabled) => {
            println!("OxiDex target cleanup disabled");
            Ok(0)
        }
        Ok(CleanOutcome::SkippedOtherRepository) => {
            println!("OxiDex target cleanup skipped: other repository");
            Ok(0)
        }
        Ok(CleanOutcome::SkippedUnexpectedWorkspace) => {
            println!("OxiDex target cleanup skipped: unexpected workspace");
            Ok(0)
        }
        Ok(CleanOutcome::TargetAbsent { free_bytes }) => {
            println!("OxiDex target absent; free bytes={free_bytes}");
            Ok(0)
        }
        Ok(CleanOutcome::TargetRemoved {
            expected_path,
            free_before,
            free_after,
        }) => {
            println!(
                "Removed {}/target; free bytes before={} after={}",
                expected_path.display(),
                free_before,
                free_after
            );
            Ok(0)
        }
        Err(err) => {
            eprintln!("OxiDex target cleanup failed: {err}");
            Ok(1)
        }
    }
}
