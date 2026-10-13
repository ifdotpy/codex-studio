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
        let after = self.file.metadata().map_err(|_| changed())?;
        let named = fs::metadata(&self.path).map_err(|_| changed())?;
        let actual: [u8; 32] = digest.finalize().into();
        // Appends before verification are fine; changes during verification retry.
        if actual != self.digest
            || before.len() != after.len()
            || before.modified().map_err(|_| changed())?
                != after.modified().map_err(|_| changed())?
            || filesystem_identity(&after) != filesystem_identity(&named)
        {
            return Err(changed());
        }
        Ok(())
    }
}
