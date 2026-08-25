use pdf_oxide::PdfDocument;
use pdf_oxide::object::Object;

const MAX_PDF_INPUT_BYTES: usize = 6 * 1024 * 1024;
const MAX_PDF_PAGES: usize = 200;
const MAX_PDF_OBJECTS: usize = 20_000;
const MAX_PDF_STRUCTURE_DEPTH: usize = 64;
const MAX_PDF_WORK_UNITS: usize = 2_000_000;
const MAX_EXTRACTED_TEXT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PdfExtraction {
    pub text: String,
    pub title: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, thiserror::Error)]
#[error("PDF extraction failed")]
pub struct PdfExtractionError;

/// Synchronous extraction interface. Callers always invoke it in a bounded
/// blocking worker and contain panics from third-party parsers.
pub trait PdfExtractor: Send + Sync {
    fn extract(&self, bytes: &[u8]) -> Result<PdfExtraction, PdfExtractionError>;
}

/// Pure-Rust native extractor selected for production.
#[derive(Clone, Copy, Debug, Default)]
pub struct NativePdfExtractor;

impl PdfExtractor for NativePdfExtractor {
    fn extract(&self, bytes: &[u8]) -> Result<PdfExtraction, PdfExtractionError> {
        preflight_pdf(bytes)?;
        let document = PdfDocument::from_bytes(bytes.to_vec()).map_err(|_| PdfExtractionError)?;
        let object_count = document.all_object_ids().len();
        if object_count > MAX_PDF_OBJECTS {
            return Err(PdfExtractionError);
        }
        let page_count = document.page_count().map_err(|_| PdfExtractionError)?;
        if page_count > MAX_PDF_PAGES
            || page_count.saturating_mul(object_count.max(1)) > MAX_PDF_WORK_UNITS
        {
            return Err(PdfExtractionError);
        }
        let title = pdf_title(&document);
        let mut text = String::new();
        let mut truncated = false;
        for page in 0..page_count {
            let page_text = document
                .extract_text(page)
                .map_err(|_| PdfExtractionError)?;
            if page > 0 {
                text.push('\u{000c}');
            }
            if text.len().saturating_add(page_text.len()) > MAX_EXTRACTED_TEXT_BYTES {
                let remaining = MAX_EXTRACTED_TEXT_BYTES.saturating_sub(text.len());
                text.push_str(crate::fetch::utf8_prefix(&page_text, remaining));
                truncated = true;
                break;
            }
            text.push_str(&page_text);
        }
        Ok(PdfExtraction {
            text,
            title,
            truncated,
        })
    }
}

fn preflight_pdf(bytes: &[u8]) -> Result<(), PdfExtractionError> {
    if bytes.len() > MAX_PDF_INPUT_BYTES || !bytes.starts_with(b"%PDF-") {
        return Err(PdfExtractionError);
    }
    let mut index = 0;
    let mut depth = 0_usize;
    let mut objects = 0_usize;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                index = bytes[index..]
                    .iter()
                    .position(|byte| matches!(byte, b'\r' | b'\n'))
                    .map_or(bytes.len(), |offset| index + offset + 1);
            }
            b'(' => skip_pdf_string(bytes, &mut index),
            b'<' if bytes.get(index + 1) != Some(&b'<') => skip_hex_string(bytes, &mut index),
            b'<' if bytes.get(index + 1) == Some(&b'<') => {
                depth = depth.saturating_add(1);
                index += 2;
            }
            b'>' if bytes.get(index + 1) == Some(&b'>') => {
                depth = depth.saturating_sub(1);
                index += 2;
            }
            b'[' => {
                depth = depth.saturating_add(1);
                index += 1;
            }
            b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            _ if token_at(bytes, index, b"stream") => {
                index += b"stream".len();
                let Some(end) = find_bytes(&bytes[index..], b"endstream") else {
                    return Err(PdfExtractionError);
                };
                index += end + b"endstream".len();
            }
            _ if token_at(bytes, index, b"obj") => {
                objects = objects.saturating_add(1);
                if objects > MAX_PDF_OBJECTS {
                    return Err(PdfExtractionError);
                }
                index += b"obj".len();
            }
            _ => index += 1,
        }
        if depth > MAX_PDF_STRUCTURE_DEPTH {
            return Err(PdfExtractionError);
        }
    }
    Ok(())
}

fn skip_pdf_string(bytes: &[u8], index: &mut usize) {
    let mut depth = 1_usize;
    *index += 1;
    while *index < bytes.len() && depth > 0 {
        match bytes[*index] {
            b'\\' => *index = (*index + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                *index += 1;
            }
            b')' => {
                depth -= 1;
                *index += 1;
            }
            _ => *index += 1,
        }
    }
}

fn skip_hex_string(bytes: &[u8], index: &mut usize) {
    *index += 1;
    while *index < bytes.len() && bytes[*index] != b'>' {
        *index += 1;
    }
    *index = (*index + 1).min(bytes.len());
}

fn token_at(bytes: &[u8], index: usize, token: &[u8]) -> bool {
    bytes.get(index..index + token.len()) == Some(token)
        && (index == 0 || is_pdf_delimiter(bytes[index - 1]))
        && bytes
            .get(index + token.len())
            .is_none_or(|byte| is_pdf_delimiter(*byte))
}

fn is_pdf_delimiter(byte: u8) -> bool {
    byte.is_ascii_whitespace()
        || matches!(
            byte,
            b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
        )
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn pdf_title(document: &PdfDocument) -> Option<String> {
    let info = document.trailer().as_dict()?.get("Info")?;
    let info = match info {
        Object::Reference(reference) => document.load_object(*reference).ok()?,
        object => object.clone(),
    };
    let bytes = info.as_dict()?.get("Title")?.as_string()?;
    let decoded = pdf_oxide::optional_content::decode_pdf_text_string(bytes);
    let normalized = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.chars().take(200).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_rejects_excessive_object_count() {
        let mut bytes = b"%PDF-1.7\n".to_vec();
        for object in 0..=MAX_PDF_OBJECTS {
            bytes.extend_from_slice(format!("{object} 0 obj\nnull\nendobj\n").as_bytes());
        }
        assert!(preflight_pdf(&bytes).is_err());
    }

    #[test]
    fn preflight_rejects_excessive_structure_depth() {
        let mut bytes = b"%PDF-1.7\n1 0 obj\n".to_vec();
        bytes.extend(std::iter::repeat_n(b'[', MAX_PDF_STRUCTURE_DEPTH + 1));
        bytes.extend(std::iter::repeat_n(b']', MAX_PDF_STRUCTURE_DEPTH + 1));
        bytes.extend_from_slice(b"\nendobj\n");
        assert!(preflight_pdf(&bytes).is_err());
    }
}
