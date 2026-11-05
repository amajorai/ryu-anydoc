//! AnyDoc conversion and the stable Ryu result/error projection.

use anydoc::{ConvertError, Format};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::limits::Limits;

pub const BACKEND: &str = "anydoc";
pub const ANYDOC_LIBRARY_VERSION: &str = "0.2.4";
pub const SUPPORTED_EXTENSIONS: &[&str] = &[
    ".csv", ".doc", ".docm", ".docx", ".epub", ".odp", ".ods", ".odt", ".pdf", ".pot", ".pps",
    ".ppsm", ".ppsx", ".ppt", ".pptm", ".pptx", ".rtf", ".xls", ".xlsb", ".xlsm", ".xlsx",
];

#[derive(Clone, Debug, Serialize)]
pub struct ExtractionResult {
    pub backend: &'static str,
    pub backend_version: &'static str,
    pub markdown: String,
    pub warnings: Vec<String>,
    pub truncated: bool,
    pub source_sha256: String,
    pub metadata: ExtractionMetadata,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExtractionMetadata {
    pub filename: String,
    pub format: String,
    pub input_bytes: usize,
}

#[derive(Debug)]
pub struct ConversionFailure {
    pub code: String,
    pub message: String,
    pub details: Value,
}

impl ConversionFailure {
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Value::Null,
        }
    }

    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

impl std::fmt::Display for ConversionFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConversionFailure {}

pub fn parse_format(raw: &str) -> Result<Format, ConversionFailure> {
    let normalized = raw.trim().trim_start_matches('.');
    let format = (normalized.eq_ignore_ascii_case("excel"))
        .then_some(Format::Excel)
        .or_else(|| Format::from_extension(normalized));
    format.ok_or_else(|| {
        ConversionFailure::new(
            "invalid_format",
            format!("unsupported format `{raw}`; use one of the advertised extensions"),
        )
    })
}

pub fn format_from_filename(filename: &str) -> Option<Format> {
    let basename = filename.replace('\\', "/");
    let extension = basename.rsplit('/').next()?.rsplit_once('.')?.1;
    Format::from_extension(extension)
}

pub fn format_name(format: Format) -> &'static str {
    match format {
        Format::Doc => "doc",
        Format::Docx => "docx",
        Format::Odt => "odt",
        Format::Pdf => "pdf",
        Format::Ppt => "ppt",
        Format::Pptx => "pptx",
        Format::Rtf => "rtf",
        Format::Epub => "epub",
        Format::Excel => "excel",
        Format::Ods => "ods",
        Format::Odp => "odp",
        Format::Csv => "csv",
    }
}

pub fn convert_bytes(
    bytes: &[u8],
    filename: &str,
    requested_format: Option<&str>,
    limits: &Limits,
) -> Result<ExtractionResult, ConversionFailure> {
    if bytes.is_empty() {
        return Err(ConversionFailure::new(
            "empty_input",
            "document input is empty",
        ));
    }
    if bytes.len() > limits.max_input_bytes {
        return Err(ConversionFailure::new(
            "input_too_large",
            format!(
                "document input is {} bytes, over the {}-byte limit",
                bytes.len(),
                limits.max_input_bytes
            ),
        ));
    }

    let format = match requested_format {
        Some(raw) => parse_format(raw)?,
        None => Format::from_bytes(bytes)
            .or_else(|| format_from_filename(filename))
            .ok_or_else(|| {
                ConversionFailure::new(
					"unsupported_format",
					"AnyDoc could not identify this document; provide a supported filename or format",
				)
            })?,
    };
    let markdown = anydoc::to_markdown_bytes(bytes, format).map_err(map_anydoc_error)?;
    if markdown.trim().is_empty() {
        return Err(ConversionFailure::new(
            "empty_document",
            "AnyDoc produced no document text",
        ));
    }

    let (markdown, truncated) = truncate(markdown, limits.max_output_bytes);
    let mut warnings = Vec::new();
    if truncated {
        warnings.push(format!(
            "output was truncated at {} bytes",
            limits.max_output_bytes
        ));
    }

    Ok(ExtractionResult {
        backend: BACKEND,
        backend_version: ANYDOC_LIBRARY_VERSION,
        markdown,
        warnings,
        truncated,
        source_sha256: sha256_hex(bytes),
        metadata: ExtractionMetadata {
            filename: filename.to_owned(),
            format: format_name(format).to_owned(),
            input_bytes: bytes.len(),
        },
    })
}

fn map_anydoc_error(error: ConvertError) -> ConversionFailure {
    match error {
        ConvertError::NeedsOcr { pages, page_count } => ConversionFailure::new(
            "needs_ocr",
            "document contains scanned or image-only PDF pages; AnyDoc does not perform OCR",
        )
        .with_details(json!({ "pages": pages, "pageCount": page_count })),
        ConvertError::Unsupported(_) => ConversionFailure::new(
            "unsupported_format",
            "AnyDoc does not support this document format",
        ),
        ConvertError::Malformed { .. } => {
            ConversionFailure::new("malformed_document", "document structure is not usable")
        }
        ConvertError::Encrypted => ConversionFailure::new(
            "encrypted_document",
            "document is encrypted or password-protected",
        ),
        ConvertError::ResourceLimit { limit, .. } => ConversionFailure::new(
            "resource_limit",
            format!("AnyDoc safety limit exceeded: {limit}"),
        ),
        ConvertError::MissingPart { .. } => {
            ConversionFailure::new("missing_part", "document is missing a required part")
        }
        ConvertError::Io(_) => {
            ConversionFailure::new("input_rejected", "document could not be read")
        }
        _ => ConversionFailure::new(
            "conversion_failed",
            "AnyDoc could not extract this document",
        ),
    }
}

fn truncate(mut markdown: String, max_bytes: usize) -> (String, bool) {
    if markdown.len() <= max_bytes {
        return (markdown, false);
    }
    let mut end = max_bytes;
    while end > 0 && !markdown.is_char_boundary(end) {
        end -= 1;
    }
    markdown.truncate(end);
    (markdown, true)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::{convert_bytes, format_from_filename, format_name, parse_format};
    use crate::limits::Limits;
    use anydoc::Format;

    #[test]
    fn format_dispatch_uses_content_then_filename_for_signatureless_csv() {
        assert_eq!(format_from_filename("report.DOCM"), Some(Format::Docx));
        assert_eq!(format_name(Format::Excel), "excel");
        assert_eq!(parse_format(".csv").unwrap(), Format::Csv);
        assert_eq!(parse_format("excel").unwrap(), Format::Excel);
        let output = convert_bytes(
            b"name,value\nRyu,1\n",
            "report.csv",
            None,
            &Limits::default(),
        )
        .expect("CSV should convert");
        assert_eq!(output.metadata.format, "csv");
        assert!(output.markdown.contains("name"));
    }

    #[test]
    fn unsupported_and_empty_inputs_are_reported() {
        let limits = Limits::default();
        assert_eq!(
            convert_bytes(b"", "empty.docx", None, &limits)
                .expect_err("empty input must fail")
                .code,
            "empty_input"
        );
        assert_eq!(
            convert_bytes(b"not a document", "unknown.bin", None, &limits)
                .expect_err("unknown input must fail")
                .code,
            "unsupported_format"
        );
    }
}
