//! Descriptor-relative opens keep a replaced directory or manifest from becoming
//! a symlink traversal. A bounded read still cannot interrupt a stalled mount.

use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

pub(super) struct Directory {
    pub path: PathBuf,
    dir: cap_std::fs::Dir,
}

impl Directory {
    pub fn open(path: &Path) -> io::Result<Self> {
        let absolute = std::path::absolute(path)?;
        if absolute.to_str().is_none() {
            return Err(io::Error::other("directory path must be UTF-8"));
        }
        if absolute.components().count() > 128 {
            return Err(io::Error::other("directory exceeds 128 path components"));
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open("/")?;
        let mut directory = Self::from_file("/".into(), file);
        for part in absolute.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => directory = directory.child(name)?,
                _ => {
                    return Err(io::Error::other(
                        "directory must not contain parent traversal",
                    ));
                }
            }
        }
        Ok(directory)
    }

    fn from_file(path: PathBuf, file: File) -> Self {
        Self {
            path,
            dir: cap_std::fs::Dir::from_std_file(file),
        }
    }

    pub fn child(&self, name: &OsStr) -> io::Result<Self> {
        let file = self.open_entry(name, libc::O_DIRECTORY)?;
        Ok(Self::from_file(self.path.join(name), file))
    }

    pub fn manifest(&self) -> io::Result<File> {
        let file = self.open_entry(OsStr::new("SKILL.md"), 0)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("SKILL.md must be a regular file"));
        }
        Ok(file)
    }

    fn open_entry(&self, name: &OsStr, flags: i32) -> io::Result<File> {
        let name = CString::new(name.as_bytes())?;
        // SAFETY: the descriptor is live and the name is NUL-terminated. All
        // callers pass one component; O_NOFOLLOW applies at each path step.
        let fd = unsafe {
            libc::openat(
                self.dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK | flags,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openat returned a fresh owned descriptor.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    pub fn entries(&self) -> io::Result<cap_std::fs::ReadDir> {
        self.dir.entries()
    }

    pub fn git_root(&self) -> io::Result<bool> {
        match self.dir.symlink_metadata(".git") {
            Ok(meta) => Ok(meta.is_dir() || meta.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}
