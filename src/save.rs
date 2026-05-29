//! Saving the annotated screenshot: to disk and to the Wayland clipboard.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context as _};
use cairo::ImageSurface;

fn screenshot_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join("Pictures").join("Screenshots")
}

pub fn save_to_disk(surface: &ImageSurface) -> anyhow::Result<PathBuf> {
    let dir = screenshot_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("Screenshot-{stamp}.png"));

    let mut file = fs::File::create(&path).with_context(|| format!("creating {}", path.display()))?;
    surface
        .write_to_png(&mut file)
        .context("encoding PNG")?;
    Ok(path)
}

pub fn copy_to_clipboard(surface: &ImageSurface) -> anyhow::Result<()> {
    let mut png: Vec<u8> = Vec::new();
    surface.write_to_png(&mut png).context("encoding PNG")?;

    let mut child = Command::new("wl-copy")
        .args(["--type", "image/png"])
        .stdin(Stdio::piped())
        .spawn()
        .context("failed to run `wl-copy` (is wl-clipboard installed?)")?;

    child
        .stdin
        .take()
        .context("no stdin for wl-copy")?
        .write_all(&png)
        .context("writing to wl-copy")?;

    let status = child.wait().context("waiting for wl-copy")?;
    if !status.success() {
        bail!("wl-copy exited with {status}");
    }
    Ok(())
}
