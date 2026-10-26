//! Pin private directories before mutations. Never resolve a destination through a link.
use rustix::fs::{AtFlags, Mode, OFlags, mkdirat, open, openat, renameat, unlinkat};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    os::fd::OwnedFd,
    path::{Component, Path},
};

pub(super) fn flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}
pub(super) struct Directory(pub OwnedFd);
impl Directory {
    pub fn open(path: &Path) -> std::io::Result<Self> {
        let path = if cfg!(target_os = "macos") {
            ["/var", "/tmp"]
                .iter()
                .find_map(|prefix| {
                    path.strip_prefix(prefix).ok().map(|tail| {
                        Path::new("/private")
                            .join(prefix.trim_start_matches('/'))
                            .join(tail)
                    })
                })
                .unwrap_or_else(|| path.to_owned())
        } else {
            path.to_owned()
        };
        if !path.is_absolute() {
            return Err(std::io::Error::other("Private directory must be absolute"));
        }
        let root = Self(open("/", flags(), Mode::empty())?);
        root.child(
            path.strip_prefix("/").map_err(std::io::Error::other)?,
            false,
        )
    }
    pub fn child(&self, relative: &Path, create: bool) -> std::io::Result<Self> {
        let mut fd = rustix::io::dup(&self.0)?;
        for part in relative.components() {
            let Component::Normal(name) = part else {
                return Err(std::io::Error::other(
                    "Private path contains a parent traversal",
                ));
            };
            if create {
                match mkdirat(&fd, name, Mode::from_raw_mode(0o700)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            fd = openat(&fd, name, flags(), Mode::empty())?;
        }
        Ok(Self(fd))
    }
    pub fn parent(&self, relative: &Path, create: bool) -> std::io::Result<(Self, OsString)> {
        let name = relative
            .file_name()
            .ok_or_else(|| std::io::Error::other("Missing private entry name"))?;
        // Validate the final component as well as its parent.
        if !matches!(
            relative.components().next_back(),
            Some(Component::Normal(_))
        ) {
            return Err(std::io::Error::other("Invalid private entry name"));
        }
        Ok((
            self.child(relative.parent().unwrap_or(Path::new("")), create)?,
            name.to_owned(),
        ))
    }
    pub fn file(&self, relative: &Path, flags: OFlags, mode: Mode) -> std::io::Result<File> {
        let (parent, name) = self.parent(relative, false)?;
        Ok(File::from(openat(
            &parent.0,
            name,
            flags | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        )?))
    }
    pub fn rename(&self, from: &Path, to: &Self, target: &Path) -> std::io::Result<()> {
        let (source, name) = self.parent(from, false)?;
        let (destination, target) = to.parent(target, false)?;
        renameat(&source.0, name, &destination.0, target)?;
        Ok(())
    }
    pub fn replace_file(&self, path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
        use std::io::Write;
        let (parent, name) = self.parent(path, false)?;
        let temp = OsString::from(format!(".studio-import-{}", uuid::Uuid::new_v4()));
        let mut file = File::from(openat(
            &parent.0,
            &temp,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode((mode & 0o7777) as _),
        )?);
        let result = (|| {
            file.write_all(bytes)?;
            renameat(&parent.0, &temp, &parent.0, &name)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = unlinkat(&parent.0, &temp, AtFlags::empty());
        }
        result
    }
    pub fn mode(&self) -> std::io::Result<u32> {
        Ok(u32::from(rustix::fs::fstat(&self.0)?.st_mode & 0o7777))
    }
    pub fn set_mode(&self, mode: u32) -> std::io::Result<()> {
        rustix::fs::fchmod(&self.0, Mode::from_raw_mode((mode & 0o7777) as _))?;
        Ok(())
    }
    pub fn exists(&self, name: &OsStr) -> std::io::Result<bool> {
        match rustix::fs::statat(&self.0, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review2_pinned_destination_survives_path_replacement() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let destination = root.path().join("destination");
        let saved = root.path().join("saved");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&destination).unwrap();
        std::fs::create_dir(&saved).unwrap();
        std::fs::write(source.join(".git"), "source").unwrap();
        std::fs::write(destination.join(".git"), "private").unwrap();
        let pinned = Directory::open(&destination).unwrap();
        let saved = Directory::open(&saved).unwrap();
        std::fs::rename(&destination, root.path().join("old-destination")).unwrap();
        std::os::unix::fs::symlink(&source, &destination).unwrap();
        pinned
            .rename(Path::new(".git"), &saved, Path::new("git"))
            .unwrap();
        pinned
            .replace_file(Path::new("code"), b"private", 0o600)
            .unwrap();
        assert_eq!(std::fs::read(source.join(".git")).unwrap(), b"source");
        assert!(!source.join("code").exists());
        assert_eq!(
            std::fs::read(root.path().join("old-destination/code")).unwrap(),
            b"private"
        );
    }
}
