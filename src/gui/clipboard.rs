//! System clipboard access.
//!
//! One clipboard handle lives for the whole run: on X11 and Wayland the
//! copied data is served by that handle, and dropping it would empty the
//! clipboard again.

use std::sync::{Mutex, OnceLock};

use anyhow::{Result, anyhow};

fn clipboard() -> &'static Mutex<Option<arboard::Clipboard>> {
    static C: OnceLock<Mutex<Option<arboard::Clipboard>>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!("no clipboard: {e}");
                None
            }
        })
    })
}

/// The clipboard's text, if it holds any.
pub fn text() -> Option<String> {
    let mut c = clipboard().lock().ok()?;
    c.as_mut()?.get_text().ok()
}

/// Put an image on the clipboard. `rgb` holds 0x00RRGGBB pixels.
pub fn copy_image(width: usize, height: usize, rgb: &[u32]) -> Result<()> {
    let bytes: Vec<u8> = rgb.iter().flat_map(|&p| [(p >> 16) as u8, (p >> 8) as u8, p as u8, 0xff]).collect();
    let mut c = clipboard().lock().map_err(|_| anyhow!("clipboard unavailable"))?;
    let c = c.as_mut().ok_or_else(|| anyhow!("no clipboard is available"))?;
    c.set_image(arboard::ImageData { width, height, bytes: bytes.into() })?;
    Ok(())
}
