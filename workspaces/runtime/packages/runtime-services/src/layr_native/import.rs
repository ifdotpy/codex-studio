//! Native archive admission must finish before writing any imported entry.
use super::NativeLayrError;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
};

pub(super) fn unpack(archive: &Path, destination: &Path) -> Result<(), NativeLayrError> {
    unpack_with_case_policy(archive, destination, case_sensitive(destination)?)
}

pub(super) fn validate(archive: &Path, destination: &Path) -> Result<(), NativeLayrError> {
    validate_case_policy(
        archive,
        case_sensitive(destination)?,
        cfg!(target_os = "macos"),
    )
}

#[cfg(target_os = "macos")]
fn case_sensitive(destination: &Path) -> Result<bool, NativeLayrError> {
    let probe = tempfile::Builder::new()
        .prefix(".studio-Case-A-")
        .tempfile_in(destination)?;
    let lowercase = probe.path().with_file_name(
        probe
            .path()
            .file_name()
            .expect("temporary file has a basename")
            .to_string_lossy()
            .to_lowercase(),
    );
    Ok(!lowercase.exists())
}

#[cfg(not(target_os = "macos"))]
fn case_sensitive(_destination: &Path) -> Result<bool, NativeLayrError> {
    Ok(true)
}

fn case_key(path: &str) -> String {
    let normalizer = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd();
    // Match Layr 0.2.0 APFS fsutil::case_key, including compatibility case variants.
    let folded: String = normalizer
        .normalize(path)
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'ς' => 'σ',
            'ſ' => 's',
            'ϐ' => 'β',
            'ϑ' => 'θ',
            'ϕ' => 'φ',
            'ϖ' => 'π',
            'ϰ' => 'κ',
            'ϱ' => 'ρ',
            'ϵ' => 'ε',
            c => c,
        })
        .collect();
    normalizer.normalize(&folded).into_owned()
}

fn validate_case_policy(
    archive: &Path,
    case_sensitive: bool,
    unicode_normalizing: bool,
) -> Result<(), NativeLayrError> {
    if case_sensitive && !unicode_normalizing {
        return Ok(());
    }
    let mut seen: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for entry in tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?)).entries()? {
        let entry = entry?;
        let path = entry.path()?;
        let path = path
            .to_str()
            .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 archive path".into()))?;
        let mut prefix = String::new();
        for component in path.split('/').filter(|part| !part.is_empty()) {
            if !prefix.is_empty() {
                prefix.push('/')
            }
            prefix.push_str(component);
            let key = if case_sensitive {
                icu_normalizer::DecomposingNormalizerBorrowed::new_nfd()
                    .normalize(&prefix)
                    .into_owned()
            } else {
                case_key(&prefix)
            };
            seen.entry(key).or_default().insert(prefix.clone());
        }
    }
    let collisions: Vec<String> = seen
        .into_values()
        .filter(|paths| paths.len() > 1)
        .map(|paths| paths.into_iter().collect::<Vec<_>>().join(", "))
        .collect();
    if !collisions.is_empty() {
        return Err(NativeLayrError::Refused(format!(
            "Archive paths differ only in case or Unicode form and cannot coexist on this volume:\n{}",
            collisions.join("\n")
        )));
    }
    Ok(())
}

fn unpack_with_case_policy(
    archive: &Path,
    destination: &Path,
    case_sensitive: bool,
) -> Result<(), NativeLayrError> {
    validate_case_policy(archive, case_sensitive, cfg!(target_os = "macos"))?;
    tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?)).unpack(destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_import_always_rejects_unicode_aliases_on_apfs() {
        let archive = tempfile::NamedTempFile::new().expect("fixture operation succeeds");
        let gzip = flate2::write::GzEncoder::new(
            File::create(archive.path()).expect("fixture operation succeeds"),
            flate2::Compression::default(),
        );
        let mut builder = tar::Builder::new(gzip);
        for name in ["café", "cafe\u{301}"] {
            let mut header = tar::Header::new_gnu();
            header.set_size(0);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, name, &b""[..])
                .expect("fixture operation succeeds");
        }
        builder
            .into_inner()
            .expect("fixture operation succeeds")
            .finish()
            .expect("fixture operation succeeds");
        assert!(validate_case_policy(archive.path(), true, true).is_err());
        assert!(validate_case_policy(archive.path(), true, false).is_ok());
    }

    #[test]
    fn native_layr_import_refuses_case_and_unicode_collisions_before_any_write() {
        for names in [["Src/one", "src/two"], ["café", "cafe\u{301}"]] {
            let archive = tempfile::NamedTempFile::new().expect("fixture operation succeeds");
            let gzip = flate2::write::GzEncoder::new(
                File::create(archive.path()).expect("fixture operation succeeds"),
                flate2::Compression::default(),
            );
            let mut builder = tar::Builder::new(gzip);
            for name in names {
                let mut header = tar::Header::new_gnu();
                header.set_size(4);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, name, &b"data"[..])
                    .expect("fixture operation succeeds");
            }
            builder
                .into_inner()
                .expect("fixture operation succeeds")
                .finish()
                .expect("fixture operation succeeds");
            let destination = tempfile::tempdir().expect("fixture operation succeeds");
            let error = unpack_with_case_policy(archive.path(), destination.path(), false)
                .expect_err("operation must fail")
                .to_string();
            assert!(
                error.contains(
                    names[0]
                        .split('/')
                        .next()
                        .expect("fixture operation succeeds")
                )
            );
            assert!(
                error.contains(
                    names[1]
                        .split('/')
                        .next()
                        .expect("fixture operation succeeds")
                )
            );
            assert_eq!(
                std::fs::read_dir(destination.path())
                    .expect("fixture operation succeeds")
                    .count(),
                0
            );
        }
    }
}
