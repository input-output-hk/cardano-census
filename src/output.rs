use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

/// Write `text` to `path`, or to stdout when `path` is "-".
/// Files are written beside their destination and renamed into place, so a
/// reader never sees a partial file.
pub fn write(path: &Path, text: &str) -> Result<()> {
    if path.as_os_str() == "-" {
        std::io::stdout().write_all(text.as_bytes())?;
        return Ok(());
    }

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = Path::new(&tmp);

    std::fs::write(tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}
