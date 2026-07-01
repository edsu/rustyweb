//! PDF text extraction for indexing archived PDF documents.

/// Extract plain text from PDF bytes.
///
/// Returns `None` if the bytes are not a parseable PDF or yield no text.
/// `pdf-extract` (via `lopdf`) can *panic* on some malformed input rather than
/// return an error, so extraction runs inside `catch_unwind` to ensure one bad
/// PDF never aborts a whole index run.
pub fn extract_pdf_text(bytes: &[u8]) -> Option<String> {
    let attempt = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    match attempt {
        Ok(Ok(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        Ok(Err(_)) => None, // parse/extraction error
        Err(_) => None,     // panic inside pdf-extract/lopdf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_pdf_bytes_return_none() {
        assert!(extract_pdf_text(b"this is not a pdf").is_none());
    }

    #[test]
    fn empty_bytes_return_none() {
        assert!(extract_pdf_text(b"").is_none());
    }
}
