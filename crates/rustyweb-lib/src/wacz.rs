use std::path::Path;

use anyhow::{Context, Result};

/// Iterate over the paths of WARC archives embedded inside a WACZ file.
///
/// WACZ is a ZIP archive; WARC files live under the `archive/` prefix.
/// The paths returned are the ZIP entry names (e.g. `archive/data.warc.gz`);
/// callers that need absolute paths should resolve them relative to the
/// WACZ file itself — but since the WARC data is *inside* the ZIP, callers
/// will extract each WARC to a temp file first (see `extract_warc_from_wacz`).
pub fn iter_warc_paths(wacz_path: &Path) -> Result<impl Iterator<Item = Result<String>>> {
    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .with_context(|| format!("reading ZIP in {}", wacz_path.display()))?;

    let mut names: Vec<String> = Vec::new();
    for i in 0..zip.len() {
        let entry = zip.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("archive/") && (name.ends_with(".warc.gz") || name.ends_with(".warc")) {
            names.push(name);
        }
    }

    Ok(names.into_iter().map(Ok))
}

/// Extract a single named WARC entry from a WACZ ZIP into a temp file and
/// return the path.  The caller owns the `NamedTempFile` and must keep it
/// alive as long as the WARC data is needed.
pub fn extract_warc_from_wacz(
    wacz_path: &Path,
    entry_name: &str,
) -> Result<tempfile::NamedTempFile> {
    use std::io::copy;

    let file = std::fs::File::open(wacz_path)
        .with_context(|| format!("opening WACZ {}", wacz_path.display()))?;
    let mut zip = zip::ZipArchive::new(file)?;
    let mut entry = zip
        .by_name(entry_name)
        .with_context(|| format!("entry {} not found in {}", entry_name, wacz_path.display()))?;

    let suffix = if entry_name.ends_with(".warc.gz") { ".warc.gz" } else { ".warc" };
    let mut tmp = tempfile::Builder::new().suffix(suffix).tempfile()?;
    copy(&mut entry, &mut tmp)?;

    Ok(tmp)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(FIXTURES).join(name)
    }

    #[test]
    fn list_warc_paths_in_wacz() {
        let paths: Vec<_> = iter_warc_paths(&fixture("simple.wacz"))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        assert!(!paths.is_empty(), "should find at least one WARC entry");
        assert!(
            paths.iter().any(|p| p.contains("archive/")),
            "entries should be under archive/: {paths:?}"
        );
    }

    #[test]
    fn extract_warc_from_wacz_succeeds() {
        let paths: Vec<_> = iter_warc_paths(&fixture("simple.wacz"))
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();

        let first = &paths[0];
        let tmp = extract_warc_from_wacz(&fixture("simple.wacz"), first).unwrap();
        assert!(tmp.path().exists());
        assert!(tmp.path().metadata().unwrap().len() > 0);
    }
}
