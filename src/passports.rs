//! Private pet-passport scans and PDFs, stored with the existing household-scoped FileStore.
//! No OCR, public URLs, or independent model calls.

use crate::storage::{FileKey, FileStore};
use anyhow::{Context, bail};
use rand::{Rng, distr::Alphanumeric};

pub const MAX_PASSPORT_BYTES: usize = 15 * 1024 * 1024;

/// Identify supported file types from their magic bytes, never from a filename
/// or client-supplied Content-Type.
pub fn detect_type(bytes: &[u8]) -> anyhow::Result<(&'static str, &'static str)> {
    if bytes.is_empty() || bytes.len() > MAX_PASSPORT_BYTES {
        bail!("Passport must be between 1 byte and 15 MB");
    }
    if bytes.starts_with(b"%PDF-") {
        Ok(("application/pdf", "pdf"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Ok(("image/png", "png"))
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Ok(("image/webp", "webp"))
    } else {
        bail!("Use a PDF, JPEG, PNG or WebP passport scan");
    }
}

pub fn safe_filename(filename: &str, extension: &str) -> String {
    let final_segment = filename.rsplit(['/', '\\']).next().unwrap_or_default();
    let clean: String = final_segment
        .chars()
        .filter(|ch| !ch.is_control())
        .take(120)
        .collect();
    let clean = clean.trim();
    if clean.is_empty() {
        format!("pet-passport.{extension}")
    } else {
        clean.to_owned()
    }
}

pub async fn store(
    files: &FileStore,
    household_id: i64,
    pet_id: i64,
    bytes: &[u8],
) -> anyhow::Result<(String, &'static str, &'static str)> {
    let (mime, extension) = detect_type(bytes)?;
    let token: String = rand::rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let relative = format!("passports/{pet_id}/{token}.{extension}");
    files
        .put(
            &FileKey::Report {
                household_id,
                relative: &relative,
            },
            bytes.to_vec(),
        )
        .await
        .context("failed to store private passport")?;
    Ok((relative, mime, extension))
}

pub async fn remove(files: &FileStore, household_id: i64, relative: &str) {
    files
        .delete(&FileKey::Report {
            household_id,
            relative,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_bytes_and_preserves_simple_filename() {
        assert_eq!(
            detect_type(b"%PDF-1.4\n%%EOF").unwrap(),
            ("application/pdf", "pdf")
        );
        assert_eq!(
            detect_type(b"\x89PNG\r\n\x1a\nrest").unwrap(),
            ("image/png", "png")
        );
        assert!(detect_type(b"<script>not a passport</script>").is_err());
        assert_eq!(
            safe_filename("../scanned passport.pdf", "pdf"),
            "scanned passport.pdf"
        );
        assert_eq!(safe_filename("", "pdf"), "pet-passport.pdf");
    }
}
