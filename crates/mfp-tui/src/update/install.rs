//! Downloading a release archive, verifying it, and swapping the binaries in place.
//!
//! Nothing on disk is touched until the whole archive is in memory and its SHA-256 matches
//! what `checksums.txt` declares, and each binary is then staged beside its destination and
//! moved onto it with a rename. A rename within a directory cannot half-succeed, so an
//! interrupted update leaves the working copy behind rather than a truncated one - which a
//! write-in-place would not, and which matters most for the binary doing the writing.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

/// Nothing this player ships comes close, and an archive far larger than a release is a
/// reason to stop rather than to fill memory with it.
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;

/// Fetches `url` whole. Release archives are a few megabytes and every byte is needed before
/// anything can be verified, so there is nothing to stream to.
pub fn download(http: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>> {
    let response = http
        .get(url)
        .send()
        .context("the download could not be started")?
        .error_for_status()
        .context("the download was refused")?;
    Ok(response
        .bytes()
        .context("the download was cut short")?
        .to_vec())
}

/// Checks `data` against the entry for `name` in a `sha256sum`-format listing.
///
/// A missing entry is a failure, not a skip: an unverified binary is exactly what this
/// guards against, and the release always publishes the file.
pub fn verify(data: &[u8], name: &str, checksums: &str) -> Result<()> {
    let expected = checksums
        .lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            let hash = fields.next()?;
            let file = fields.next()?.trim_start_matches('*');
            (file == name).then_some(hash)
        })
        .ok_or_else(|| anyhow!("checksums.txt has no entry for {name}"))?;

    let actual = format!("{:x}", Sha256::digest(data));
    if actual != expected {
        bail!("{name} does not match its published checksum");
    }
    Ok(())
}

/// Unpacks the wanted binaries from `archive` and moves each onto its destination.
///
/// The pair is staged in full first and only then renamed, so a failure to extract one does
/// not leave the other already replaced. The two renames themselves are separate calls and
/// a crash between them would leave one binary newer than the other, which the protocol's
/// forward tolerance is there to survive.
pub fn replace(archive: &[u8], destinations: &[(&str, PathBuf)]) -> Result<()> {
    let mut staged = Vec::with_capacity(destinations.len());

    let result = (|| -> Result<()> {
        for (name, destination) in destinations {
            let source = extract(archive, name)?;
            let path = stage(&source, destination)
                .with_context(|| format!("cannot write beside {}", destination.display()))?;
            staged.push((path, destination));
        }
        Ok(())
    })();

    if let Err(error) = result {
        for (path, _) in &staged {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }

    for (path, destination) in &staged {
        fs::rename(path, destination)
            .with_context(|| format!("cannot replace {}", destination.display()))?;
    }
    Ok(())
}

/// Reads one named binary out of the gzipped tar, ignoring everything else it carries.
///
/// Matched on the file name alone and never unpacked to a path the archive chose: an entry
/// is a name to look for here, not a destination to obey.
fn extract(archive: &[u8], name: &str) -> Result<Vec<u8>> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(archive));
    for entry in tar.entries().context("the archive could not be read")? {
        let mut entry = entry.context("the archive is damaged")?;
        let path = entry.path().context("the archive has an unreadable path")?;
        if path.file_name().is_none_or(|file| file != name) {
            continue;
        }
        if entry.size() > MAX_ARCHIVE_BYTES {
            bail!("{name} in the archive is implausibly large");
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .with_context(|| format!("cannot read {name} out of the archive"))?;
        return Ok(bytes);
    }
    Err(anyhow!("the archive does not contain {name}"))
}

/// Writes `bytes` executable beside `destination`, where a rename onto it cannot cross a
/// filesystem boundary.
fn stage(bytes: &[u8], destination: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let path = destination.with_extension("new");
    fs::write(&path, bytes)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, bytes) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder.append_data(&mut header, name, *bytes).unwrap();
        }
        let tar = builder.into_inner().unwrap();
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    /// A tarball carrying one entry under a name `tar::Builder` would refuse to write.
    ///
    /// The name goes straight into the header, because the point of the test it serves is
    /// what happens to an archive nothing well-behaved produced.
    fn tarball_with_raw_name(name: &[u8], bytes: &[u8]) -> Vec<u8> {
        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();

        let mut builder = tar::Builder::new(Vec::new());
        builder.append(&header, bytes).unwrap();
        let tar = builder.into_inner().unwrap();

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(&tar).unwrap();
        encoder.finish().unwrap()
    }

    fn sha256(data: &[u8]) -> String {
        format!("{:x}", Sha256::digest(data))
    }

    // == Verification ==

    #[test]
    fn a_matching_checksum_verifies() {
        let data = b"an archive";
        let listing = format!("{}  musicforprogramming-x.tar.gz\n", sha256(data));
        verify(data, "musicforprogramming-x.tar.gz", &listing).unwrap();
    }

    #[test]
    fn the_right_entry_is_picked_out_of_a_listing_of_every_target() {
        let data = b"an archive";
        let listing = format!(
            "0000  musicforprogramming-x86_64-apple-darwin.tar.gz\n\
             {}  musicforprogramming-aarch64-apple-darwin.tar.gz\n\
             1111  musicforprogramming-x86_64-unknown-linux-gnu.tar.gz\n",
            sha256(data)
        );
        verify(
            data,
            "musicforprogramming-aarch64-apple-darwin.tar.gz",
            &listing,
        )
        .unwrap();
    }

    #[test]
    fn a_binary_mode_star_before_the_filename_is_tolerated() {
        let data = b"an archive";
        let listing = format!("{} *musicforprogramming-x.tar.gz\n", sha256(data));
        verify(data, "musicforprogramming-x.tar.gz", &listing).unwrap();
    }

    #[test]
    fn a_mismatched_checksum_is_refused() {
        let listing = "0000  musicforprogramming-x.tar.gz\n";
        let error = verify(b"an archive", "musicforprogramming-x.tar.gz", listing).unwrap_err();
        assert!(error.to_string().contains("checksum"), "{error}");
    }

    #[test]
    fn a_missing_entry_is_refused_rather_than_skipped() {
        let data = b"an archive";
        let listing = format!("{}  something-else.tar.gz\n", sha256(data));
        let error = verify(data, "musicforprogramming-x.tar.gz", &listing).unwrap_err();
        assert!(error.to_string().contains("no entry"), "{error}");
    }

    // == Extraction ==

    #[test]
    fn a_named_binary_is_read_out_of_the_archive() {
        let archive = tarball(&[("mfp", b"client"), ("mfp-daemon", b"daemon")]);
        assert_eq!(extract(&archive, "mfp").unwrap(), b"client");
        assert_eq!(extract(&archive, "mfp-daemon").unwrap(), b"daemon");
    }

    #[test]
    fn an_entry_nested_under_a_directory_is_still_found_by_its_name() {
        let archive = tarball(&[("staging/mfp", b"client")]);
        assert_eq!(extract(&archive, "mfp").unwrap(), b"client");
    }

    /// An entry is a name to look for, never a destination to obey: extraction hands back
    /// bytes and the caller decides where they go, so a traversing name has nowhere to lead.
    #[test]
    fn a_traversing_entry_is_read_as_bytes_rather_than_followed() {
        let archive = tarball_with_raw_name(b"../../../../etc/mfp", b"hostile");
        assert_eq!(extract(&archive, "mfp").unwrap(), b"hostile");
        assert!(!Path::new("/etc/mfp").exists());
    }

    #[test]
    fn a_staged_binary_is_written_beside_its_destination_and_nowhere_else() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("mfp");
        let staged = stage(b"new client", &destination).unwrap();
        assert_eq!(staged, dir.path().join("mfp.new"));
    }

    #[test]
    fn a_missing_binary_is_an_error_rather_than_an_empty_install() {
        let archive = tarball(&[("mfp", b"client")]);
        let error = extract(&archive, "mfp-daemon").unwrap_err();
        assert!(error.to_string().contains("mfp-daemon"), "{error}");
    }

    #[test]
    fn a_damaged_archive_is_an_error_rather_than_a_panic() {
        assert!(extract(b"not a gzip stream at all", "mfp").is_err());
    }

    // == Replacement ==

    #[test]
    fn both_binaries_are_replaced_and_left_executable() {
        let dir = tempfile::tempdir().unwrap();
        let mfp = dir.path().join("mfp");
        let daemon = dir.path().join("mfp-daemon");
        fs::write(&mfp, b"old client").unwrap();
        fs::write(&daemon, b"old daemon").unwrap();

        let archive = tarball(&[("mfp", b"new client"), ("mfp-daemon", b"new daemon")]);
        replace(
            &archive,
            &[("mfp", mfp.clone()), ("mfp-daemon", daemon.clone())],
        )
        .unwrap();

        assert_eq!(fs::read(&mfp).unwrap(), b"new client");
        assert_eq!(fs::read(&daemon).unwrap(), b"new daemon");
        assert_eq!(
            fs::metadata(&mfp).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }

    #[test]
    fn nothing_is_replaced_when_one_of_the_pair_is_missing_from_the_archive() {
        let dir = tempfile::tempdir().unwrap();
        let mfp = dir.path().join("mfp");
        let daemon = dir.path().join("mfp-daemon");
        fs::write(&mfp, b"old client").unwrap();
        fs::write(&daemon, b"old daemon").unwrap();

        let archive = tarball(&[("mfp", b"new client")]);
        let error = replace(
            &archive,
            &[("mfp", mfp.clone()), ("mfp-daemon", daemon.clone())],
        )
        .unwrap_err();

        assert!(error.to_string().contains("mfp-daemon"), "{error}");
        assert_eq!(fs::read(&mfp).unwrap(), b"old client");
        assert_eq!(fs::read(&daemon).unwrap(), b"old daemon");
    }

    #[test]
    fn a_failed_update_leaves_no_staged_files_behind() {
        let dir = tempfile::tempdir().unwrap();
        let mfp = dir.path().join("mfp");
        fs::write(&mfp, b"old client").unwrap();

        let archive = tarball(&[("mfp-daemon", b"new daemon")]);
        assert!(replace(&archive, &[("mfp", mfp.clone())]).is_err());

        assert!(!mfp.with_extension("new").exists());
        assert_eq!(fs::read(&mfp).unwrap(), b"old client");
    }
}
