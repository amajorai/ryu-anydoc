//! Path and filename validation for the local sidecar input form.

use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct InputError {
    pub code: &'static str,
    pub message: String,
}

impl InputError {
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for InputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for InputError {}

/// Resolve the path and then check containment under the configured roots.
///
/// The order is security-critical: checking the un-resolved path first lets a
/// symlink inside an allowed directory point at an unrelated file.
pub fn resolve_input(
    raw: &str,
    roots: &[PathBuf],
    max_input_bytes: usize,
) -> Result<PathBuf, InputError> {
    let requested = PathBuf::from(raw.trim());
    if !requested.is_absolute() {
        return Err(InputError::new(
            "input_rejected",
            "path input must be absolute",
        ));
    }
    let resolved = requested.canonicalize().map_err(|_| {
        InputError::new("input_rejected", "path input does not name a readable file")
    })?;
    if !roots.iter().any(|root| resolved.starts_with(root)) {
        return Err(InputError::new(
            "input_rejected",
            "path input is outside the configured AnyDoc roots",
        ));
    }
    let metadata = std::fs::metadata(&resolved)
        .map_err(|_| InputError::new("input_rejected", "path input metadata could not be read"))?;
    if !metadata.is_file() {
        return Err(InputError::new(
            "input_rejected",
            "path input is not a regular file",
        ));
    }
    if metadata.len() > max_input_bytes as u64 {
        return Err(InputError::new(
            "input_too_large",
            format!(
                "path input is {} bytes, over the {}-byte limit",
                metadata.len(),
                max_input_bytes
            ),
        ));
    }
    Ok(resolved)
}

/// Roots are explicit when `RYU_ANYDOC_ROOTS` is set. Otherwise Core's
/// profile-aware `RYU_DIR` is the only fallback. An explicitly empty allow-list
/// remains empty and therefore rejects every path input.
pub fn roots_from_env() -> Vec<PathBuf> {
    if let Ok(raw) = std::env::var("RYU_ANYDOC_ROOTS") {
        return std::env::split_paths(&raw)
            .filter_map(|path| path.canonicalize().ok())
            .collect();
    }
    std::env::var_os("RYU_DIR")
        .map(PathBuf::from)
        .and_then(|path| path.canonicalize().ok())
        .into_iter()
        .collect()
}

/// Keep only the basename used for display and format dispatch.
///
/// The caller may send a path-like display name, but it never becomes a write
/// path. This preserves Windows names on a Unix node while making traversal
/// components irrelevant to the service.
pub fn safe_basename(raw: &str) -> Result<String, InputError> {
    let normalized = raw.trim().replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default().trim();
    if basename.is_empty() {
        return Err(InputError::new("invalid_filename", "filename is required"));
    }
    if basename == "." || basename == ".." || basename.contains('\0') {
        return Err(InputError::new("invalid_filename", "filename is invalid"));
    }
    if basename.len() > 255 || basename.chars().any(char::is_control) {
        return Err(InputError::new("invalid_filename", "filename is invalid"));
    }
    Ok(basename.to_owned())
}

/// Validate a content address without using it to construct a filesystem path.
pub fn validate_sha256(raw: &str) -> Result<String, InputError> {
    let value = raw.trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(InputError::new(
            "input_rejected",
            "blob_sha256 must be 64 hexadecimal characters",
        ));
    }
    if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(InputError::new(
            "input_rejected",
            "blob_sha256 must use lowercase hexadecimal characters",
        ));
    }
    Ok(value.to_owned())
}

pub fn filename_from_path(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::safe_basename;

    #[test]
    fn basename_drops_path_components_without_becoming_a_write_path() {
        assert_eq!(safe_basename("../../report.docx").unwrap(), "report.docx");
        assert_eq!(
            safe_basename(r"C:\\docs\\report.docx").unwrap(),
            "report.docx"
        );
    }

    #[test]
    fn basename_rejects_empty_and_control_names() {
        assert!(safe_basename("/").is_err());
        assert!(safe_basename("report\n.docx").is_err());
    }
}
