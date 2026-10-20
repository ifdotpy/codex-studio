//! Revalidate bytes consumed from the same opened rollout before any effects.
use super::{HistoryError, filesystem_identity};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

pub(super) struct ReadProof {
    file: File,
    path: PathBuf,
    start: u64,
    length: u64,
    digest: [u8; 32],
}
impl ReadProof {
    pub(super) fn new(
        file: File,
        path: PathBuf,
        start: u64,
        length: u64,
        digest: [u8; 32],
    ) -> Self {
        Self {
            file,
            path,
            start,
            length,
            digest,
        }
    }
    pub(super) fn validate(&mut self) -> Result<(), HistoryError> {
        let changed = || HistoryError::ChangedDuringRead;
        let before = self.file.metadata().map_err(|_| changed())?;
        let named = fs::metadata(&self.path).map_err(|_| changed())?;
        if filesystem_identity(&before) != filesystem_identity(&named)
            || before.len() < self.start + self.length
        {
            return Err(changed());
        }
        self.file
            .seek(SeekFrom::Start(self.start))
            .map_err(|_| changed())?;
        let mut digest = Sha256::new();
        let mut remaining = self.length;
        let mut bytes = [0; 16 * 1024];
        while remaining > 0 {
            let count = remaining.min(bytes.len() as u64) as usize;
            self.file
                .read_exact(&mut bytes[..count])
                .map_err(|_| changed())?;
            digest.update(&bytes[..count]);
            remaining -= count as u64;
        }
        let named = fs::metadata(&self.path).map_err(|_| changed())?;
        let actual: [u8; 32] = digest.finalize().into();
        // The digest proves the consumed prefix; appends beyond it are not a change.
        if actual != self.digest || filesystem_identity(&before) != filesystem_identity(&named) {
            return Err(changed());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn proof(dir: &std::path::Path) -> (ReadProof, PathBuf) {
        let path = dir.join("rollout.jsonl");
        fs::write(&path, b"0123456789").expect("write");
        let digest = Sha256::digest(b"0123456789").into();
        let file = File::open(&path).expect("open");
        (ReadProof::new(file, path.clone(), 0, 10, digest), path)
    }

    #[test]
    fn append_after_the_read_range_is_not_a_change() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut proof, path) = proof(dir.path());
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("append");
        file.write_all(b"more").expect("write");
        proof.validate().expect("append is not a change");
    }

    #[test]
    fn rewritten_prefix_is_a_change() {
        let dir = tempfile::tempdir().expect("dir");
        let (mut proof, path) = proof(dir.path());
        fs::write(&path, b"0123456780").expect("rewrite");
        assert!(matches!(
            proof.validate(),
            Err(HistoryError::ChangedDuringRead)
        ));
    }
}
