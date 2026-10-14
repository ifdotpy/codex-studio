//! Atomic receipt replacement; temporary and durable files never contain codes or URLs.
use super::{LoginError, LoginSnapshot, ports::LoginReceipts};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;
pub const MAX_JOBS: usize = 4096;
pub struct FileReceipts {
    directory: PathBuf,
}
impl FileReceipts {
    pub fn new(directory: PathBuf) -> Result<Self, LoginError> {
        fs::create_dir_all(&directory).map_err(|_| LoginError::Persistence)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                .map_err(|_| LoginError::Persistence)?;
        }
        Ok(Self { directory })
    }
    pub fn directory(&self) -> &Path {
        &self.directory
    }
}
impl LoginReceipts for FileReceipts {
    fn load(&self) -> Result<Vec<LoginSnapshot>, LoginError> {
        let mut records = Vec::new();
        for entry in fs::read_dir(&self.directory).map_err(|_| LoginError::Persistence)? {
            let path = entry.map_err(|_| LoginError::Persistence)?.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            if records.len() >= MAX_JOBS {
                return Err(LoginError::Unavailable);
            }
            if !fs::symlink_metadata(&path)
                .map_err(|_| LoginError::Persistence)?
                .is_file()
            {
                return Err(LoginError::InvalidRecord("receipt-file".into()));
            }
            let mut bytes = Vec::new();
            File::open(&path)
                .map_err(|_| LoginError::Persistence)?
                .take(MAX_RECEIPT_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| LoginError::Persistence)?;
            if bytes.len() as u64 > MAX_RECEIPT_BYTES {
                return Err(LoginError::InvalidRecord("receipt-size".into()));
            }
            let record = LoginSnapshot::from_record(
                serde_json::from_slice(&bytes)
                    .map_err(|_| LoginError::InvalidRecord("receipt-json".into()))?,
            )?;
            if path.file_stem().and_then(|v| v.to_str()) != Some(record.request_id()) {
                return Err(LoginError::InvalidRecord("requestId".into()));
            }
            records.push(record);
        }
        Ok(records)
    }
    fn save(&self, record: &LoginSnapshot) -> Result<(), LoginError> {
        let bytes = serde_json::to_vec(record.record()).map_err(|_| LoginError::Persistence)?;
        if bytes.len() as u64 > MAX_RECEIPT_BYTES {
            return Err(LoginError::Persistence);
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)
            .map_err(|_| LoginError::Persistence)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| LoginError::Persistence)?;
        }
        temporary
            .write_all(&bytes)
            .and_then(|_| temporary.as_file().sync_all())
            .map_err(|_| LoginError::Persistence)?;
        temporary
            .persist(self.directory.join(format!("{}.json", record.request_id())))
            .map_err(|_| LoginError::Persistence)?;
        File::open(&self.directory)
            .and_then(|parent| parent.sync_all())
            .map_err(|_| LoginError::Persistence)
    }
}
