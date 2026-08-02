//! Paste helpers for the interactive editor.
//!
//! - **Text / paths:** bracketed-paste content is scanned for file paths and
//!   wrapped as `@[...]` so attachments load correctly.
//! - **Images (clipboard):** screenshot / image data is saved under the temp
//!   dir as PNG. The editor shows a short atom `[Image N]` (not the long path);
//!   on submit the atom is expanded to `@[/tmp/furcode-paste/….png]`. Backspace
//!   deletes the whole atom at once.
//!
//! On macOS, **Cmd+V is handled by the terminal** (bracketed paste), not as a
//! Super+V key event. The editor intercepts `BracketedPasteStart` and, when the
//! clipboard holds an image (and no text), inserts the image atom and drains
//! the paste body so empty/garbage paste does not pollute the draft.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Visible atom inserted into the prompt for a clipboard image (`[Image 1]`).
pub const IMAGE_ATOM_PREFIX: &str = "[Image ";

/// Process-wide registry mapping image atom ids to saved PNG paths.
struct ImagePasteRegistry {
    next_id: u32,
    paths: HashMap<u32, PathBuf>,
}

impl ImagePasteRegistry {
    fn new() -> Self {
        Self {
            next_id: 1,
            paths: HashMap::new(),
        }
    }
}

fn image_registry() -> &'static Mutex<ImagePasteRegistry> {
    static REG: OnceLock<Mutex<ImagePasteRegistry>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(ImagePasteRegistry::new()))
}

/// Formats a display atom for image id `n` (e.g. `[Image 1]`).
pub fn image_atom_label(n: u32) -> String {
    format!("[Image {n}]")
}

/// Registers `path` and returns the short display atom (e.g. `[Image 1]`).
pub fn register_image_atom(path: PathBuf) -> String {
    let mut reg = image_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let id = reg.next_id;
    reg.next_id = reg.next_id.saturating_add(1);
    reg.paths.insert(id, path);
    image_atom_label(id)
}

/// Expands every `[Image N]` atom in `text` to `@[/absolute/path.png]`.
///
/// Unknown ids are left unchanged. Used when the user submits a prompt so the
/// attachment pipeline still sees real paths.
pub fn expand_image_atoms(text: &str) -> String {
    let reg = image_registry()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    expand_image_atoms_with(text, |id| reg.paths.get(&id).cloned())
}

/// Testable expand helper: `lookup` returns the path for an image id.
fn expand_image_atoms_with(text: &str, mut lookup: impl FnMut(u32) -> Option<PathBuf>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(IMAGE_ATOM_PREFIX) {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        if let Some((atom_len, id)) = parse_image_atom_at(after) {
            if let Some(path) = lookup(id) {
                out.push_str(&format!("@[{}]", path.display()));
            } else {
                out.push_str(&after[..atom_len]);
            }
            rest = &after[atom_len..];
        } else {
            // Not a real atom — copy the prefix char and continue.
            out.push('[');
            rest = &rest[start + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// If `s` starts with `[Image N]`, returns `(byte_len, id)`.
fn parse_image_atom_at(s: &str) -> Option<(usize, u32)> {
    let rest = s.strip_prefix(IMAGE_ATOM_PREFIX)?;
    let digits_end = rest
        .char_indices()
        .take_while(|(_, c)| c.is_ascii_digit())
        .last()
        .map(|(i, c)| i + c.len_utf8())?;
    if digits_end == 0 {
        return None;
    }
    let id: u32 = rest[..digits_end].parse().ok()?;
    let after_digits = &rest[digits_end..];
    if !after_digits.starts_with(']') {
        return None;
    }
    let atom_len = IMAGE_ATOM_PREFIX.len() + digits_end + 1;
    Some((atom_len, id))
}

/// All `[Image N]` ranges in `line` as half-open byte spans `(start, end)`.
pub fn image_atom_ranges(line: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if let Some(rel) = line[i..].find(IMAGE_ATOM_PREFIX) {
            let start = i + rel;
            if let Some((atom_len, _)) = parse_image_atom_at(&line[start..]) {
                ranges.push((start, start + atom_len));
                i = start + atom_len;
                continue;
            }
            i = start + 1;
        } else {
            break;
        }
    }
    ranges
}

/// Byte range of the image atom that Backspace at `pos` should delete.
///
/// Includes a single trailing space after the atom when the cursor sits on or
/// after that space (so one Backspace after paste removes `[Image N] `).
pub fn image_atom_for_backspace(line: &str, pos: usize) -> Option<(usize, usize)> {
    if pos == 0 {
        return None;
    }
    for (start, end) in image_atom_ranges(line) {
        let mut del_end = end;
        if line.as_bytes().get(end) == Some(&b' ') {
            del_end = end + 1;
        }
        // Cursor inside atom, just after `]`, or just after trailing space.
        if pos > start && pos <= del_end {
            return Some((start, del_end));
        }
    }
    None
}

/// Byte range of the image atom that Delete (forward) at `pos` should remove.
pub fn image_atom_for_delete(line: &str, pos: usize) -> Option<(usize, usize)> {
    for (start, end) in image_atom_ranges(line) {
        let mut del_end = end;
        if line.as_bytes().get(end) == Some(&b' ') {
            del_end = end + 1;
        }
        if pos >= start && pos < del_end {
            return Some((start, del_end));
        }
    }
    None
}


/// Transforms pasted text by wrapping bare file paths in `@[...]` syntax.
///
/// Called when a bracketed-paste event is received. The pasted content is
/// normalised (CRLF to LF) and then checked for file paths. If the entire
/// paste (after stripping whitespace/quotes) is a single existing absolute
/// path it gets wrapped directly -- this handles paths with spaces. Otherwise
/// the text is scanned token-by-token for quoted or unquoted absolute paths.
///
/// Already-wrapped `@[...]` references and non-existent paths are left
/// untouched.
pub fn wrap_pasted_text(pasted: &str) -> String {
    let normalised = pasted.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = normalised.trim();

    // If the whole paste is empty, just return normalised form
    if trimmed.is_empty() {
        return normalised;
    }

    // Fast path: the entire paste is a single path (possibly quoted).
    // This is the most common drag-and-drop case and correctly handles
    // paths that contain spaces or backslash-escaped spaces.
    let unquoted = strip_surrounding_quotes(trimmed);
    if let Some(resolved) = resolve_file_path(unquoted) {
        // Reconstruct with the same leading/trailing whitespace the
        // original normalised string had.
        let trim_start_len = normalised.trim_start().len();
        let trim_end_len = normalised.trim_end().len();
        let leading = normalised
            .get(..normalised.len() - trim_start_len)
            .unwrap_or("");
        let trailing = normalised.get(trim_end_len..).unwrap_or("");
        return format!("{leading}@[{resolved}]{trailing}");
    }

    // Scan token by token, wrapping any absolute paths that exist on disk
    wrap_tokens(&normalised)
}

/// Strips surrounding single or double quotes that some terminals add
/// when dragging files with spaces in their names.
fn strip_surrounding_quotes(s: &str) -> &str {
    if s.len() < 2 {
        return s;
    }
    if (s.starts_with('\'') && s.ends_with('\'')) || (s.starts_with('"') && s.ends_with('"')) {
        s.get(1..s.len().saturating_sub(1)).unwrap_or(s)
    } else {
        s
    }
}

/// Removes backslash escapes from a string (e.g. `\ ` becomes ` `).
///
/// Many terminals (Ghostty, iTerm2, etc.) backslash-escape spaces when
/// drag-and-dropping file paths, producing strings like
/// `/path/my\ folder/file.txt`. This helper un-escapes them so the path
/// can be resolved against the filesystem.
///
/// Returns `None` if no backslash escapes were found (i.e. the input is
/// already clean), allowing callers to skip redundant `is_file()` checks.
fn unescape_backslashes(s: &str) -> Option<String> {
    if !s.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            // Take the next char literally, or keep the backslash if at end
            if let Some(next) = chars.next() {
                out.push(next);
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// Checks whether `candidate` resolves to an existing absolute file or
/// directory path.
///
/// Tries the raw string first, then falls back to un-escaping backslashes
/// (for terminals that send `/path/my\ file.txt`). Returns the resolved
/// clean path on success, or `None` if no match was found.
fn resolve_file_path(candidate: &str) -> Option<String> {
    let path = Path::new(candidate);
    if path.is_absolute() && path.exists() {
        return Some(candidate.to_string());
    }
    // Try un-escaping backslashes (e.g. Ghostty sends `/path/my\ file.txt`)
    if let Some(unescaped) = unescape_backslashes(candidate) {
        let path = Path::new(&unescaped);
        if path.is_absolute() && path.exists() {
            return Some(unescaped);
        }
    }
    None
}

/// Finds the end of a token in `input`, treating `\<char>` as an escaped
/// character that is part of the token (not a boundary).
///
/// Returns the byte offset of the first unescaped whitespace character, or
/// the length of `input` if no unescaped whitespace is found.
fn find_token_end(input: &str) -> usize {
    let mut escaped = false;
    for (i, c) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c.is_whitespace() {
            return i;
        }
    }
    input.len()
}

/// Walks through `input` token-by-token and wraps absolute file paths.
///
/// Handles both unquoted tokens (split on whitespace) and quoted strings
/// (single or double quotes) so that paths containing spaces are kept
/// together as a single token.
fn wrap_tokens(input: &str) -> String {
    let mut result = String::with_capacity(input.len() + 32);
    let mut remaining = input;

    while !remaining.is_empty() {
        // Preserve leading whitespace
        let ws_end = remaining
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(remaining.len());
        result.push_str(remaining.get(..ws_end).unwrap_or(""));
        remaining = remaining.get(ws_end..).unwrap_or("");

        if remaining.is_empty() {
            break;
        }

        // Skip already-wrapped @[...] references
        if remaining.starts_with("@[")
            && let Some(close) = remaining.find(']')
        {
            result.push_str(remaining.get(..=close).unwrap_or(""));
            remaining = remaining.get(close.saturating_add(1)..).unwrap_or("");
            continue;
        }

        // If the token starts with a quote, consume everything up to the
        // matching closing quote so that paths with spaces stay together.
        let first_char = remaining.as_bytes().first().copied().unwrap_or(0);
        if first_char == b'\'' || first_char == b'"' {
            let quote = first_char as char;
            if let Some(close) = remaining.get(1..).and_then(|s| s.find(quote)) {
                let token_end = close.saturating_add(2); // include both quotes
                let token = remaining.get(..token_end).unwrap_or("");
                let clean = strip_surrounding_quotes(token);
                if let Some(resolved) = resolve_file_path(clean) {
                    result.push_str(&format!("@[{}]", resolved));
                } else {
                    result.push_str(token);
                }
                remaining = remaining.get(token_end..).unwrap_or("");
                continue;
            }
        }

        // Extract the next token, treating backslash-escaped whitespace
        // (e.g. `\ `) as part of the token.  This handles terminals like
        // Ghostty that send `/path/my\ file.txt` for drag-and-drop.
        let token_end = find_token_end(remaining);
        let token = remaining.get(..token_end).unwrap_or("");

        if let Some(resolved) = resolve_file_path(token) {
            result.push_str(&format!("@[{}]", resolved));
        } else {
            result.push_str(token);
        }

        remaining = remaining.get(token_end..).unwrap_or("");
    }

    result
}

/// Directory for clipboard image dumps (created on demand).
pub fn clipboard_image_dir() -> PathBuf {
    std::env::temp_dir().join("furcode-paste")
}

/// Lightweight clipboard probe — no multi‑MB image decode.
///
/// Used by the TUI to toast “Image ready · Ctrl+V to paste” when a screenshot
/// lands on the pasteboard. Terminals often swallow Cmd+V for image-only
/// clipboards (no `Event::Paste`), so detection must not wait for a paste event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipboardProbe {
    /// Platform change counter / generation (0 when unknown).
    pub change_token: u64,
    pub has_image: bool,
    pub has_text: bool,
}

impl ClipboardProbe {
    /// True when an image is available and text is not competing for paste.
    pub fn image_ready_to_paste(self) -> bool {
        self.has_image && !self.has_text
    }
}

/// How aggressively to take an image over text on the clipboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImagePasteMode {
    /// Skip image when non-empty text is also present (safe for normal paste).
    PreferText,
    /// Always take the image when present (empty bracketed paste / explicit key).
    PreferImage,
}

/// Probe whether the system clipboard currently holds image / text data.
///
/// On macOS this only inspects pasteboard *types* + `changeCount` (cheap).
/// Elsewhere it may open the clipboard briefly; avoid calling on every frame.
#[cfg(not(target_os = "android"))]
pub fn probe_clipboard() -> ClipboardProbe {
    #[cfg(target_os = "macos")]
    {
        return probe_clipboard_macos();
    }
    #[cfg(not(target_os = "macos"))]
    {
        probe_clipboard_arboard()
    }
}

#[cfg(target_os = "android")]
pub fn probe_clipboard() -> ClipboardProbe {
    ClipboardProbe::default()
}

/// macOS: `NSPasteboard` type list — no TIFF decode.
#[cfg(target_os = "macos")]
fn probe_clipboard_macos() -> ClipboardProbe {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::ClassType;
    use objc2_app_kit::NSPasteboard;

    let pasteboard: Option<Retained<NSPasteboard>> =
        unsafe { msg_send![NSPasteboard::class(), generalPasteboard] };
    let Some(pb) = pasteboard else {
        return ClipboardProbe::default();
    };

    let change: isize = unsafe { msg_send![&*pb, changeCount] };
    let change_token = u64::try_from(change).unwrap_or(0);

    let mut has_image = false;
    let mut has_text = false;

    if let Some(types) = pb.types() {
        for ty in types.iter() {
            let name = ty.to_string();
            let lower = name.to_ascii_lowercase();
            if is_image_pasteboard_type(&lower) {
                has_image = true;
            }
            // Plain string only — HTML/RTF alone must not block image paste
            // (Chrome often puts public.html next to a PNG).
            if is_plain_text_pasteboard_type(&lower) {
                has_text = true;
            }
        }
    }

    // Confirm plain text is actually non-empty (type may be advertised empty).
    if has_text {
        if let Ok(mut cb) = arboard::Clipboard::new() {
            match cb.get_text() {
                Ok(t) if !t.trim().is_empty() => {}
                _ => has_text = false,
            }
        }
    }

    ClipboardProbe {
        change_token,
        has_image,
        has_text,
    }
}

#[cfg(target_os = "macos")]
fn is_image_pasteboard_type(lower: &str) -> bool {
    lower.contains("tiff")
        || lower.contains("png")
        || lower.contains("jpeg")
        || lower.contains("jpg")
        || lower.contains("gif")
        || lower.contains("bmp")
        || lower.contains("jp2")
        || lower == "public.image"
        || (lower.contains("image")
            && !lower.contains("html")
            && !lower.contains("icon")
            && !lower.contains("dynamic"))
}

#[cfg(target_os = "macos")]
fn is_plain_text_pasteboard_type(lower: &str) -> bool {
    lower == "public.utf8-plain-text"
        || lower == "public.utf16-plain-text"
        || lower == "public.utf16-external-plain-text"
        || lower == "nsstringpboardtype"
        || lower.ends_with("plain-text")
}

/// Fallback probe via arboard (may decode image — used off the hot path).
#[cfg(all(not(target_os = "android"), not(target_os = "macos")))]
fn probe_clipboard_arboard() -> ClipboardProbe {
    let mut probe = ClipboardProbe::default();
    let Ok(mut cb) = arboard::Clipboard::new() else {
        return probe;
    };
    match cb.get_text() {
        Ok(t) if !t.trim().is_empty() => probe.has_text = true,
        _ => {}
    }
    match cb.get_image() {
        Ok(_) => probe.has_image = true,
        Err(_) => {}
    }
    // No changeCount — bump token when content shape changes.
    probe.change_token = u64::from(probe.has_image) | (u64::from(probe.has_text) << 1);
    probe
}

/// Tries to read an image from the system clipboard and persist it as a PNG.
///
/// With [`ImagePasteMode::PreferText`] (default), non-empty text wins so normal
/// Cmd+V of a copied string is not stolen by a leftover screenshot.
/// With [`ImagePasteMode::PreferImage`], image wins when present (empty paste
/// body / explicit image paste key).
///
/// On success returns a short display atom (`[Image N]`) registered for later
/// expansion to `@[path]` at submit time.
///
/// # Returns
///
/// - `Ok(Some("[Image N]"))` on success (path stored in the image registry).
/// - `Ok(None)` when the clipboard has no image (or prefers text); callers may
///   fall back to normal text paste.
/// - `Err(...)` when clipboard access or PNG serialization fails (not for
///   missing image content).
#[cfg(not(target_os = "android"))]
pub fn try_paste_clipboard_image() -> anyhow::Result<Option<String>> {
    try_paste_clipboard_image_with(ImagePasteMode::PreferText)
}

/// See [`try_paste_clipboard_image`]; `mode` controls text-vs-image priority.
#[cfg(not(target_os = "android"))]
pub fn try_paste_clipboard_image_with(mode: ImagePasteMode) -> anyhow::Result<Option<String>> {
    // macOS fast path: copy already-compressed PNG/JPEG off the pasteboard.
    // arboard's get_image() decodes TIFF → full RGBA then we re-encode PNG —
    // for a Retina screenshot that is tens of MB and multi-second.
    #[cfg(target_os = "macos")]
    {
        if mode == ImagePasteMode::PreferText {
            let probe = probe_clipboard_macos();
            if probe.has_text {
                return Ok(None);
            }
        }
        if let Some(path) = save_macos_clipboard_image_fast()? {
            return Ok(Some(register_image_atom(path)));
        }
        // No compressed bytes — fall through to arboard RGBA path.
    }

    let mut clipboard = arboard::Clipboard::new()?;

    if mode == ImagePasteMode::PreferText {
        match clipboard.get_text() {
            Ok(text) if !text.trim().is_empty() => return Ok(None),
            Ok(_) | Err(arboard::Error::ContentNotAvailable) => {}
            Err(_) => {
                // Other text-read errors: still attempt image.
            }
        }
    }

    let img = match clipboard.get_image() {
        Ok(img) => img,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let path = save_rgba_as_png(img.width, img.height, img.bytes.as_ref())?;
    Ok(Some(register_image_atom(path)))
}

/// Dump pasteboard PNG/JPEG bytes straight to disk (no decode/re-encode).
///
/// Returns `Ok(None)` when those types are absent so callers can fall back.
#[cfg(target_os = "macos")]
fn save_macos_clipboard_image_fast() -> anyhow::Result<Option<PathBuf>> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::ClassType;
    use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG};
    use objc2_foundation::ns_string;

    let pasteboard: Option<Retained<NSPasteboard>> =
        unsafe { msg_send![NSPasteboard::class(), generalPasteboard] };
    let Some(pb) = pasteboard else {
        return Ok(None);
    };

    // Prefer PNG (screenshots + most browsers). Then JPEG / other compressed.
    // `NSPasteboardTypePNG` is an extern static — read inside unsafe.
    let type_png = unsafe { NSPasteboardTypePNG };
    let candidates: [(&objc2_foundation::NSString, &str); 6] = [
        (type_png, "png"),
        (ns_string!("public.png"), "png"),
        (ns_string!("public.jpeg"), "jpg"),
        (ns_string!("public.jpg"), "jpg"),
        (ns_string!("public.jpeg-2000"), "jp2"),
        (ns_string!("public.gif"), "gif"),
    ];

    for (ty, ext) in candidates {
        if let Some(data) = pb.dataForType(ty) {
            let bytes = data.to_vec();
            if bytes.is_empty() {
                continue;
            }
            let path = unique_clipboard_path(ext)?;
            std::fs::write(&path, &bytes)?;
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// Unique path under [`clipboard_image_dir`] with the given extension.
fn unique_clipboard_path(ext: &str) -> anyhow::Result<PathBuf> {
    let dir = clipboard_image_dir();
    std::fs::create_dir_all(&dir)?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    Ok(dir.join(format!("clipboard-{nanos}.{ext}")))
}

/// Android / no-clipboard builds: image paste is unsupported.
#[cfg(target_os = "android")]
pub fn try_paste_clipboard_image() -> anyhow::Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "android")]
pub fn try_paste_clipboard_image_with(_mode: ImagePasteMode) -> anyhow::Result<Option<String>> {
    Ok(None)
}

/// Writes RGBA8 pixels to a unique PNG under [`clipboard_image_dir`].
///
/// Slow path used when the OS only exposes uncompressed pixels (or macOS
/// compressed-type dump failed). Prefer platform-native dumps first.
#[cfg(not(target_os = "android"))]
fn save_rgba_as_png(width: usize, height: usize, rgba: &[u8]) -> anyhow::Result<PathBuf> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};

    let w = u32::try_from(width).map_err(|_| anyhow::anyhow!("image width too large"))?;
    let h = u32::try_from(height).map_err(|_| anyhow::anyhow!("image height too large"))?;
    let expected = width.saturating_mul(height).saturating_mul(4);
    if rgba.len() < expected {
        anyhow::bail!(
            "clipboard image buffer too small: got {} bytes, need {expected}",
            rgba.len()
        );
    }

    let path = unique_clipboard_path("png")?;
    // Fastest PNG settings — default compression is multi-second on retina shots.
    let file = std::fs::File::create(&path)?;
    let enc = PngEncoder::new_with_quality(file, CompressionType::Fast, FilterType::NoFilter);
    enc.write_image(
        &rgba[..expected],
        w,
        h,
        image::ExtendedColorType::Rgba8,
    )
    .map_err(|e| anyhow::anyhow!("png encode: {e}"))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn test_wrap_pasted_text_cjk_no_paths() {
        let fixture = "公";
        let actual = wrap_pasted_text(fixture);
        let expected = "公";
        assert_eq!(actual, expected);
    }

    #[cfg(not(target_os = "android"))]
    #[test]
    fn test_save_rgba_as_png_roundtrip() {
        // 2×1: red + green pixels
        let rgba = [255, 0, 0, 255, 0, 255, 0, 255];
        let path = save_rgba_as_png(2, 1, &rgba).expect("save png");
        assert!(path.exists());
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_image_atom_label() {
        assert_eq!(image_atom_label(1), "[Image 1]");
        assert_eq!(image_atom_label(12), "[Image 12]");
    }

    #[test]
    fn test_expand_image_atoms_with_lookup() {
        let fixture = "see [Image 1] and [Image 2] please";
        let actual = expand_image_atoms_with(fixture, |id| match id {
            1 => Some(PathBuf::from("/tmp/a.png")),
            2 => Some(PathBuf::from("/tmp/b.png")),
            _ => None,
        });
        let expected = "see @[/tmp/a.png] and @[/tmp/b.png] please";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_expand_unknown_atom_left_intact() {
        let fixture = "see [Image 99]";
        let actual = expand_image_atoms_with(fixture, |_| None);
        assert_eq!(actual, fixture);
    }

    #[test]
    fn test_image_atom_for_backspace_at_end_with_space() {
        let fixture = "hi [Image 1] ";
        let pos = fixture.len();
        let actual = image_atom_for_backspace(fixture, pos);
        let expected = Some((3, fixture.len()));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_image_atom_for_backspace_inside_token() {
        let fixture = "[Image 1]";
        // cursor after "Im"
        let pos = "[Im".len();
        let actual = image_atom_for_backspace(fixture, pos);
        let expected = Some((0, fixture.len()));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_image_atom_for_backspace_not_on_token() {
        let fixture = "hello";
        let actual = image_atom_for_backspace(fixture, fixture.len());
        assert_eq!(actual, None);
    }

    #[test]
    fn test_image_atom_for_delete_at_start() {
        let fixture = "[Image 1] x";
        let actual = image_atom_for_delete(fixture, 0);
        let expected = Some((0, "[Image 1] ".len()));
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_register_and_expand_roundtrip() {
        let path = PathBuf::from("/tmp/furcode-paste/test-atom.png");
        let atom = register_image_atom(path.clone());
        assert!(atom.starts_with("[Image "));
        assert!(atom.ends_with(']'));
        let expanded = expand_image_atoms(&format!("look {atom}"));
        assert_eq!(expanded, format!("look @[{}]", path.display()));
    }

    #[test]
    fn test_wrap_pasted_text_no_paths() {
        let fixture = "hello world";
        let actual = wrap_pasted_text(fixture);
        let expected = "hello world";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_already_wrapped() {
        let fixture = "check @[/usr/bin/env]";
        let actual = wrap_pasted_text(fixture);
        let expected = "check @[/usr/bin/env]";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_existing_file() {
        // /usr/bin/env exists on macOS/Linux
        let fixture = "look at /usr/bin/env please";
        let actual = wrap_pasted_text(fixture);
        let expected = "look at @[/usr/bin/env] please";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_nonexistent_path_untouched() {
        let fixture = "look at /nonexistent/path/file.rs please";
        let actual = wrap_pasted_text(fixture);
        let expected = "look at /nonexistent/path/file.rs please";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_bare_path_only() {
        // Just a bare path (typical drag-and-drop result)
        // /usr/bin/env is a real file, so it should be wrapped
        let fixture = "/usr/bin/env";
        let actual = wrap_pasted_text(fixture);
        let expected = "@[/usr/bin/env]";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_bare_path_nonexistent() {
        let fixture = "/nonexistent/path/file.rs";
        let actual = wrap_pasted_text(fixture);
        let expected = "/nonexistent/path/file.rs";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_with_text_before() {
        let fixture = "analyze /usr/bin/env";
        let actual = wrap_pasted_text(fixture);
        let expected = "analyze @[/usr/bin/env]";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_preserves_whitespace() {
        let fixture = "hello  world";
        let actual = wrap_pasted_text(fixture);
        let expected = "hello  world";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_mixed_existing_and_nonexistent() {
        let fixture = "check /usr/bin/env and /nonexistent/foo.rs";
        let actual = wrap_pasted_text(fixture);
        let expected = "check @[/usr/bin/env] and /nonexistent/foo.rs";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_crlf_normalised() {
        let fixture = "/usr/bin/env\r\n";
        let actual = wrap_pasted_text(fixture);
        let expected = "@[/usr/bin/env]\n";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_single_quoted_path() {
        let fixture = "'/usr/bin/env'";
        let actual = wrap_pasted_text(fixture);
        let expected = "@[/usr/bin/env]";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_strip_surrounding_quotes_single() {
        let actual = strip_surrounding_quotes("'/some/path'");
        let expected = "/some/path";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_strip_surrounding_quotes_double() {
        let actual = strip_surrounding_quotes("\"/some/path\"");
        let expected = "/some/path";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_strip_surrounding_quotes_none() {
        let actual = strip_surrounding_quotes("/some/path");
        let expected = "/some/path";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_strip_surrounding_quotes_single_char() {
        let actual = strip_surrounding_quotes("'");
        let expected = "'";
        assert_eq!(actual, expected);
    }

    // -- Tests for paths with spaces -----------------------------------------

    /// Helper that creates a temp directory containing a file at the given
    /// relative path (which may include spaces) and returns the absolute path
    /// to that file along with the `TempDir` guard to keep it alive.
    fn create_file_with_spaces(relative: &str) -> (String, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join(relative);
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file_path, "test").unwrap();
        (file_path.to_string_lossy().into_owned(), dir)
    }

    #[test]
    fn test_wrap_pasted_text_bare_path_with_spaces() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let actual = wrap_pasted_text(&path);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_single_quoted_path_with_spaces() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("'{path}'");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_double_quoted_path_with_spaces() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("\"{path}\"");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_path_with_spaces_in_directory() {
        let (path, _dir) = create_file_with_spaces("my folder/file.txt");
        let actual = wrap_pasted_text(&path);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_path_with_spaces_trailing_newline() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("{path}\n");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("@[{path}]\n");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_path_with_spaces_crlf() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("{path}\r\n");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("@[{path}]\n");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_quoted_path_with_spaces_in_sentence() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("check '{path}' please");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("check @[{path}] please");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_nonexistent_path_with_spaces() {
        let fixture = "/nonexistent/path with spaces/file.txt";
        let actual = wrap_pasted_text(fixture);
        let expected = "/nonexistent/path with spaces/file.txt";
        assert_eq!(actual, expected);
    }

    // -- Tests for backslash-escaped paths -----------------------------------

    #[test]
    fn test_wrap_pasted_text_backslash_escaped_spaces() {
        // Terminals like Ghostty send /path/my\ file.txt for drag-and-drop
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let escaped = path.replace(' ', "\\ ");
        let actual = wrap_pasted_text(&escaped);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_backslash_escaped_spaces_in_directory() {
        let (path, _dir) = create_file_with_spaces("my folder/file.txt");
        let escaped = path.replace(' ', "\\ ");
        let actual = wrap_pasted_text(&escaped);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_backslash_escaped_nonexistent() {
        let fixture = "/nonexistent/my\\ folder/file.txt";
        let actual = wrap_pasted_text(fixture);
        let expected = "/nonexistent/my\\ folder/file.txt";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_backslash_escaped_in_sentence() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let escaped = path.replace(' ', "\\ ");
        let fixture = format!("check {escaped} please");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("check @[{path}] please");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unescape_backslashes_spaces() {
        let actual = unescape_backslashes("/path/my\\ file.txt");
        let expected = Some("/path/my file.txt".to_string());
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_unescape_backslashes_no_escapes() {
        let actual = unescape_backslashes("/path/file.txt");
        assert_eq!(actual, None);
    }

    #[test]
    fn test_unescape_backslashes_trailing_backslash() {
        let actual = unescape_backslashes("/path/file\\");
        let expected = Some("/path/file\\".to_string());
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_find_token_end_backslash_escaped_space() {
        let actual = find_token_end("/path/my\\ file.txt please");
        let expected = "/path/my\\ file.txt".len();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_resolve_file_path_plain() {
        let actual = resolve_file_path("/usr/bin/env");
        assert_eq!(actual, Some("/usr/bin/env".to_string()));
    }

    #[test]
    fn test_resolve_file_path_escaped() {
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let escaped = path.replace(' ', "\\ ");
        let actual = resolve_file_path(&escaped);
        assert_eq!(actual, Some(path));
    }

    #[test]
    fn test_resolve_file_path_nonexistent() {
        let actual = resolve_file_path("/nonexistent/file.txt");
        assert_eq!(actual, None);
    }

    #[test]
    fn test_resolve_file_path_directory() {
        // /tmp is a real directory on macOS/Linux
        let actual = resolve_file_path("/tmp");
        assert_eq!(actual, Some("/tmp".to_string()));
    }

    #[test]
    fn test_wrap_pasted_text_directory_path() {
        // /tmp is a real directory, so it should be wrapped
        let fixture = "/tmp";
        let actual = wrap_pasted_text(fixture);
        let expected = "@[/tmp]";
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_directory_in_sentence() {
        let fixture = "look at /tmp please";
        let actual = wrap_pasted_text(fixture);
        let expected = "look at @[/tmp] please";
        assert_eq!(actual, expected);
    }

    // -- Tests for VSCode-style drag-and-drop (path sent via sendText) -------

    #[test]
    fn test_wrap_pasted_text_vscode_quoted_path() {
        // VSCode's preparePathForShell may single-quote paths with spaces
        let (path, _dir) = create_file_with_spaces("my file.txt");
        let fixture = format!("'{path}'");
        let actual = wrap_pasted_text(&fixture);
        let expected = format!("@[{path}]");
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_wrap_pasted_text_vscode_plain_path() {
        // VSCode sends a plain path for files without spaces
        let fixture = "/usr/bin/env";
        let actual = wrap_pasted_text(fixture);
        let expected = "@[/usr/bin/env]";
        assert_eq!(actual, expected);
    }

    // Verifies that Cyrillic text with multi-byte UTF-8 characters doesn't panic
    // when pasting. The original bug was caused by unsafe string slicing at byte
    // boundaries inside multi-byte UTF-8 characters.
    #[test]
    fn test_wrap_pasted_text_cyrillic_no_crash() {
        let fixture = "Проверь ПОЛНОСТЬЮ этот проект на соответствие КАЖДОГО пункта функционала исходному тексту задачи";
        // This should NOT panic - the fix uses .get() instead of direct slicing
        let actual = wrap_pasted_text(fixture);
        eprintln!("DEBUG: actual output = {:?}", actual);
        // The text should be preserved (it contains no absolute paths)
        // The important thing is this doesn't panic with "byte index is not a char
        // boundary"
        assert!(!actual.is_empty());
        assert!(actual.starts_with("Проверь"));
    }

    #[test]
    fn test_wrap_pasted_text_cyrillic_with_mixed_paths() {
        // Mix of Cyrillic text and paths that should be wrapped
        let fixture = "Проверь /usr/bin/env и /tmp пожалуйста";
        let actual = wrap_pasted_text(fixture);
        let expected = "Проверь @[/usr/bin/env] и @[/tmp] пожалуйста";
        assert_eq!(actual, expected);
    }
}
