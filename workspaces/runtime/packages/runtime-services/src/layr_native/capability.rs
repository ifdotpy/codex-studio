//! Read-only host readiness for native workspace selection.
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NativeLayrCapability {
    pub ready: bool,
    pub reason: Option<String>,
}

impl NativeLayrCapability {
    fn from_result(result: Result<(), String>) -> Self {
        match result {
            Ok(()) => Self {
                ready: true,
                reason: None,
            },
            Err(reason) => Self {
                ready: false,
                reason: Some(reason),
            },
        }
    }
}

/// Linux needs a btrfs store and the root daemon for this exact Studio state.
/// This probe sends no daemon request and never creates directories or a store.
pub fn capability(state_dir: &Path) -> NativeLayrCapability {
    #[cfg(target_os = "macos")]
    {
        let _ = state_dir;
        NativeLayrCapability::from_result(Ok(()))
    }
    #[cfg(target_os = "linux")]
    {
        NativeLayrCapability::from_result(linux_readiness(state_dir))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = state_dir;
        NativeLayrCapability::from_result(Err("Native Layr requires macOS or Linux.".into()))
    }
}

#[cfg(target_os = "linux")]
fn linux_readiness(state_dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    // Linux statfs filesystem type identifier, BTRFS_SUPER_MAGIC.
    const BTRFS_FILESYSTEM_TYPE: u64 = 0x9123_683e;
    let store = super::store_path(state_dir);
    if !store.is_dir()
        || rustix::fs::statfs(&store)
            .map(|info| info.f_type as u64)
            .ok()
            != Some(BTRFS_FILESYSTEM_TYPE)
    {
        return Err(format!(
            "Native Layr requires a btrfs store at {}.",
            store.display()
        ));
    }
    let socket = super::socket_path(state_dir);
    let refusal = || format!("Start the native Layr root daemon at {}.", socket.display());
    let metadata = std::fs::symlink_metadata(&socket).map_err(|_| refusal())?;
    if !metadata.file_type().is_socket() || metadata.uid() != 0 {
        return Err(refusal());
    }
    let table = std::fs::read_to_string("/proc/net/unix").map_err(|_| refusal())?;
    if !has_listener(&table, &socket) {
        return Err(refusal());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn has_listener(table: &str, socket: &Path) -> bool {
    let Some(expected) = socket.to_str() else {
        return false;
    };
    table.lines().any(|line| {
        // Seven whitespace-separated columns precede the path. Preserve spaces in the path.
        let mut remainder = line.trim_start();
        let mut columns = Vec::with_capacity(7);
        for _ in 0..7 {
            let Some((column, tail)) = remainder.split_once(char::is_whitespace) else {
                return false;
            };
            columns.push(column);
            remainder = tail.trim_start();
        }
        columns[3] == "00010000" // SO_ACCEPTCON: this socket is listening.
            && columns[4] == "0001" // SOCK_STREAM.
            && columns[5] == "01" // Unconnected listener, not an accepted stream.
            && columns[6].parse::<u64>().is_ok_and(|inode| inode != 0)
            && remainder == expected
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_capability_listener_matches_the_exact_owned_socket_path() {
        let socket = Path::new("/studio state/layr-native/layr.sock");
        let row =
            "00000000: 00000002 00000000 00010000 0001 01 123 /studio state/layr-native/layr.sock";
        assert!(has_listener(row, socket));
        assert!(!has_listener(
            row,
            Path::new("/other/layr-native/layr.sock")
        ));
        assert!(!has_listener(&format!("{row}.other"), socket));
        assert!(!has_listener("", socket));
    }

    #[test]
    fn native_capability_refuses_bound_stale_or_non_stream_sockets() {
        let socket = Path::new("/studio/layr-native/layr.sock");
        let row = "00000000: 00000002 00000000 00010000 0001 01 123 /studio/layr-native/layr.sock";
        assert!(!has_listener(&row.replace("00010000", "00000000"), socket));
        assert!(!has_listener(&row.replace("0001 01", "0002 01"), socket));
        assert!(!has_listener(&row.replace("0001 01", "0001 03"), socket));
        assert!(!has_listener(&row.replace("01 123", "01 0"), socket));
    }

    #[test]
    fn native_capability_keeps_the_reason_only_for_unavailable_hosts() {
        assert_eq!(
            NativeLayrCapability::from_result(Ok(())),
            NativeLayrCapability {
                ready: true,
                reason: None
            }
        );
        assert_eq!(
            NativeLayrCapability::from_result(Err("btrfs is required".into())),
            NativeLayrCapability {
                ready: false,
                reason: Some("btrfs is required".into())
            }
        );
    }
}
