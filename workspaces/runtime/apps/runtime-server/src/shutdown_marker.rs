use std::{fs, io, path::Path};

const MARKER: &str = "runtime-clean-shutdown";
const TEMPORARY: &str = "runtime-clean-shutdown.tmp";

pub(crate) fn begin(root: &Path) -> io::Result<bool> {
    match fs::remove_file(root.join(MARKER)) {
        Ok(()) => {
            fs::File::open(root)?.sync_all()?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

pub(crate) fn finish(root: &Path) -> io::Result<()> {
    let temporary = root.join(TEMPORARY);
    let file = fs::File::create(&temporary)?;
    file.sync_all()?;
    fs::rename(temporary, root.join(MARKER))?;
    fs::File::open(root)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_marker_is_consumed_before_the_next_run() {
        let root = tempfile::tempdir().unwrap();
        assert!(begin(root.path()).unwrap(), "missing marker is unclean");
        finish(root.path()).unwrap();
        assert!(!begin(root.path()).unwrap(), "orderly stop is clean");
        assert!(
            begin(root.path()).unwrap(),
            "crash after consuming marker is unclean"
        );
    }
}
