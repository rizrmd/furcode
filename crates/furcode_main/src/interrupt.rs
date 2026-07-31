//! Keyboard handling while an agent turn is running.
//!
//! Uses **cbreak on `/dev/tty`** (not full raw mode) so stdout keeps OPOST/ONLCR
//! and concurrent tool output does not staircase.
//!
//! While busy the user can:
//! - Press **Esc** → interrupt the turn
//! - Type a line and press **Enter** → queue as pending send (sent after the turn)
//!
//! ## Typeahead draft (display owned by the live feed)
//!
//! Keystrokes only update a shared text buffer. The **live activity feed** is
//! the sole painter of the `› ` line (last row of its redraw block, always with
//! a trailing newline). That keeps `clear_lines` correct so Thinking updates
//! redraw in place instead of stacking.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Shared typeahead queue filled while the agent is busy.
pub type PendingQueue = Arc<Mutex<VecDeque<String>>>;

/// Shared typeahead buffer (display is owned by [`crate::live_feed`]).
struct TypeaheadState {
    /// Listener is running — live feed should paint `› `.
    active: bool,
    /// Current draft text (not including the chevron).
    text: String,
}

fn typeahead_state() -> &'static Mutex<TypeaheadState> {
    static STATE: OnceLock<Mutex<TypeaheadState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(TypeaheadState {
        active: false,
        text: String::new(),
    }))
}

/// Whether a busy typeahead session is active (live feed should show `› `).
pub fn typeahead_is_active() -> bool {
    typeahead_state()
        .lock()
        .map(|s| s.active)
        .unwrap_or(false)
}

/// Snapshot of the typeahead text when active (`None` if inactive).
pub fn typeahead_snapshot() -> Option<String> {
    let s = typeahead_state().lock().ok()?;
    if s.active {
        Some(s.text.clone())
    } else {
        None
    }
}

/// Formats the typeahead display line (`› ` / `› text`).
pub fn format_typeahead_line(text: &str) -> String {
    let clean = text.trim_end_matches('\n');
    if clean.is_empty() {
        "\x1b[2m›\x1b[0m ".to_string()
    } else {
        format!("\x1b[2m›\x1b[0m {clean}")
    }
}

/// Starts a typeahead session (buffer only — live feed paints on next redraw).
pub fn typeahead_begin() {
    if let Ok(mut s) = typeahead_state().lock() {
        s.active = true;
        s.text.clear();
    }
}

/// Ends the session and clears the draft buffer.
pub fn typeahead_end() {
    if let Ok(mut s) = typeahead_state().lock() {
        s.active = false;
        s.text.clear();
    }
}

/// Updates draft text. Display updates on the next live-feed tick/redraw.
pub fn typeahead_set_text(text: impl Into<String>) {
    if let Ok(mut s) = typeahead_state().lock() {
        s.text = text.into();
    }
}

/// Handle returned by [`spawn_busy_listener`].
pub struct BusyListener {
    /// Set to `true` to stop the listener (also set on Escape).
    pub cancel: Arc<AtomicBool>,
    /// Set to `true` when the user presses Esc (interrupt the turn).
    pub escaped: Arc<AtomicBool>,
    /// Lines submitted with Enter during the busy turn.
    pub pending: PendingQueue,
    join: tokio::task::JoinHandle<String>,
}

impl BusyListener {
    /// Stops the listener and returns any unfinished draft text (not submitted).
    pub async fn stop(self) -> String {
        self.cancel.store(true, Ordering::Relaxed);
        let draft = self.join.await.unwrap_or_default();
        typeahead_end();
        draft
    }
}

/// Spawns a background TTY reader for Esc + typeahead while the agent is busy.
///
/// On Enter, non-empty lines are pushed to [`BusyListener::pending`] and echoed
/// on the TTY as `Queued: …`. Incomplete draft text is returned from
/// [`BusyListener::stop`].
pub fn spawn_busy_listener() -> BusyListener {
    let cancel = Arc::new(AtomicBool::new(false));
    let escaped = Arc::new(AtomicBool::new(false));
    let pending: PendingQueue = Arc::new(Mutex::new(VecDeque::new()));

    typeahead_begin();

    let cancel_flag = cancel.clone();
    let escaped_flag = escaped.clone();
    let pending_q = pending.clone();

    let join = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            unix::listen_busy_blocking(&cancel_flag, &escaped_flag, pending_q)
        }
        #[cfg(not(unix))]
        {
            non_unix::listen_busy_blocking(&cancel_flag, &escaped_flag, pending_q)
        }
    });

    BusyListener { cancel, escaped, pending, join }
}

/// Resolves when `escaped` becomes true or `cancel` is set.
pub async fn wait_until_flag(flag: Arc<AtomicBool>, cancel: Arc<AtomicBool>) {
    while !flag.load(Ordering::Relaxed) && !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
}

/// Resolves when the user presses Esc (or when `cancel` is set).
pub async fn wait_for_escape(cancel: Arc<AtomicBool>) {
    let cancel_flag = cancel.clone();
    let _ = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            unix::wait_for_escape_blocking(&cancel_flag);
        }
        #[cfg(not(unix))]
        {
            non_unix::wait_for_escape_blocking(&cancel_flag);
        }
    })
    .await;
}

#[cfg(unix)]
mod unix {
    use std::collections::VecDeque;
    use std::fs::OpenOptions;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use libc::{ECHO, ICANON, TCSANOW, VMIN, VTIME, tcgetattr, tcsetattr, termios};

    struct TermiosGuard {
        fd: i32,
        original: termios,
    }

    impl TermiosGuard {
        fn enter_cbreak(fd: i32) -> Option<Self> {
            unsafe {
                let mut original: termios = std::mem::zeroed();
                if tcgetattr(fd, &mut original) != 0 {
                    return None;
                }
                let mut modified = original;
                modified.c_lflag &= !(ICANON | ECHO);
                modified.c_cc[VMIN] = 0;
                modified.c_cc[VTIME] = 1;
                if tcsetattr(fd, TCSANOW, &modified) != 0 {
                    return None;
                }
                Some(Self { fd, original })
            }
        }
    }

    impl Drop for TermiosGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = tcsetattr(self.fd, TCSANOW, &self.original);
            }
        }
    }

    fn open_tty() -> Option<std::fs::File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/tty")
            .ok()
    }

    fn clear_nonblock(fd: i32) {
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
    }

    pub(super) fn wait_for_escape_blocking(cancel: &AtomicBool) {
        let Some(tty) = open_tty() else {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        };
        let fd = tty.as_raw_fd();
        let Some(_guard) = TermiosGuard::enter_cbreak(fd) else {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return;
        };
        clear_nonblock(fd);
        let mut file = tty;
        let mut buf = [0u8; 32];
        while !cancel.load(Ordering::Relaxed) {
            let n = match file.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            if is_lone_escape(&buf[..n], &mut file) {
                return;
            }
        }
    }

    pub(super) fn listen_busy_blocking(
        cancel: &AtomicBool,
        escaped: &AtomicBool,
        pending: Arc<Mutex<VecDeque<String>>>,
    ) -> String {
        let Some(tty) = open_tty() else {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return String::new();
        };
        let fd = tty.as_raw_fd();
        let Some(_guard) = TermiosGuard::enter_cbreak(fd) else {
            while !cancel.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
            return String::new();
        };
        clear_nonblock(fd);

        let mut file = tty;
        let mut buf = [0u8; 128];
        let mut draft = String::new();
        let mut utf8_acc: Vec<u8> = Vec::new();
        // Sticky bar was armed by spawn_busy_listener; just keep the buffer.
        super::typeahead_set_text("");

        while !cancel.load(Ordering::Relaxed) {
            let n = match file.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => n,
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };

            let mut i = 0;
            while i < n {
                let b = buf[i];

                // ESC → CSI / OSC / SS3 / lone Esc (user interrupt)
                if b == 0x1b {
                    i += 1;
                    let next = if i < n {
                        Some(buf[i])
                    } else {
                        // Peek for sequence vs user Esc
                        std::thread::sleep(Duration::from_millis(30));
                        let mut more = [0u8; 1];
                        match file.read(&mut more) {
                            Ok(1) => Some(more[0]),
                            _ => None,
                        }
                    };
                    match next {
                        None => {
                            // Sticky bar torn down by BusyListener::stop.
                            escaped.store(true, Ordering::Relaxed);
                            cancel.store(true, Ordering::Relaxed);
                            return sanitize_draft(&draft);
                        }
                        Some(b']') => {
                            // OSC: ESC ] … BEL or ESC ] … ESC \
                            if i < n && buf[i] == b']' {
                                i += 1;
                            }
                            i = skip_osc_in_buf(&buf, i, n);
                            if i >= n {
                                skip_osc_from_reader(&mut file);
                            }
                            utf8_acc.clear();
                            continue;
                        }
                        Some(b'[') => {
                            if i < n && buf[i] == b'[' {
                                i += 1;
                            }
                            // Parse CSI params so we can detect bracketed paste
                            // (ESC [ 200 ~ / ESC [ 201 ~). Cmd+V image paste on
                            // macOS arrives this way — try clipboard image on start.
                            let (new_i, csi_final, csi_params) =
                                take_csi_in_buf(&buf, i, n, &mut file);
                            i = new_i;
                            if csi_final == Some(b'~') && csi_params == "200" {
                                // Bracketed paste start: prefer clipboard image.
                                if let Ok(Some(attachment)) =
                                    crate::paste::try_paste_clipboard_image()
                                {
                                    // Drain until paste end so empty/garbage body
                                    // does not enter the draft.
                                    i = drain_bracketed_paste(&buf, i, n, &mut file);
                                    draft.push_str(&attachment);
                                    draft.push(' ');
                                    paint_typeahead_line(&draft);
                                    utf8_acc.clear();
                                    continue;
                                }
                                // No image — leave paste body for normal draft.
                            }
                            // 201~ (paste end) and other CSI: already consumed.
                            utf8_acc.clear();
                            continue;
                        }
                        Some(b'O') => {
                            // SS3: ESC O X — discard one more byte
                            if i < n && buf[i] == b'O' {
                                i += 1;
                            }
                            if i < n {
                                i += 1;
                            } else {
                                let mut one = [0u8; 1];
                                let _ = file.read(&mut one);
                            }
                            utf8_acc.clear();
                            continue;
                        }
                        Some(_) => {
                            // Other ESC-prefix sequence: ignore the following byte
                            if i < n {
                                i += 1;
                            }
                            utf8_acc.clear();
                            continue;
                        }
                    }
                }

                if b == b'\n' || b == b'\r' {
                    let line = sanitize_draft(&draft);
                    draft.clear();
                    super::typeahead_set_text("");
                    if !line.is_empty() {
                        let preview: String = line.chars().take(72).collect();
                        let suffix = if line.chars().count() > 72 { "…" } else { "" };
                        // Queued notice scrolls in the content region; sticky
                        // prompt stays on the last row.
                        let _ = writeln!(
                            file,
                            "\x1b[2m◆ Queued\x1b[0m \x1b[2m{preview}{suffix}\x1b[0m"
                        );
                        let _ = file.flush();
                        if let Ok(mut q) = pending.lock() {
                            q.push_back(line);
                        }
                    }
                    paint_typeahead_line("");
                    i += 1;
                    utf8_acc.clear();
                    continue;
                }

                if b == 0x7f || b == 0x08 {
                    // Atomic delete of `[Image N]` (+ trailing space) like the editor.
                    let pos = draft.len();
                    if let Some((start, end)) =
                        crate::paste::image_atom_for_backspace(&draft, pos)
                    {
                        draft.replace_range(start..end, "");
                        paint_typeahead_line(&draft);
                    } else if draft.pop().is_some() {
                        paint_typeahead_line(&draft);
                    }
                    i += 1;
                    utf8_acc.clear();
                    continue;
                }

                if b == 0x03 {
                    escaped.store(true, Ordering::Relaxed);
                    cancel.store(true, Ordering::Relaxed);
                    return sanitize_draft(&draft);
                }

                // Printable / UTF-8 — skip ASCII control
                if b >= 0x20 || b >= 0x80 {
                    utf8_acc.push(b);
                    match std::str::from_utf8(&utf8_acc) {
                        Ok(s) => {
                            draft.push_str(s);
                            utf8_acc.clear();
                            // Drop terminal-reply garbage that slipped past ESC parsing
                            // (e.g. body of OSC 10 without the leading ESC).
                            if looks_like_terminal_reply(&draft) {
                                draft.clear();
                                paint_typeahead_line("");
                            } else {
                                paint_typeahead_line(&draft);
                            }
                        }
                        Err(e) if e.error_len().is_none() => {}
                        Err(_) => utf8_acc.clear(),
                    }
                    i += 1;
                    continue;
                }

                i += 1;
            }
        }

        sanitize_draft(&draft)
    }

    /// Updates the shared draft and rewrites the inline `› ` row.
    fn paint_typeahead_line(draft: &str) {
        let clean = sanitize_draft(draft);
        super::typeahead_set_text(clean);
    }

    /// Strip OSC/CSI residue and known terminal query replies from typeahead.
    fn sanitize_draft(s: &str) -> String {
        let mut out = s.to_string();
        // ESC-prefixed sequences still in the string
        while let Some(pos) = out.find('\u{1b}') {
            let rest = &out[pos..];
            let end = if rest.starts_with("\u{1b}]") {
                // OSC … BEL or ST
                rest.find(['\u{7}', '\u{9c}'])
                    .map(|i| pos + i + 1)
                    .or_else(|| {
                        rest.find("\u{1b}\\").map(|i| pos + i + 2)
                    })
                    .unwrap_or(out.len())
            } else if rest.starts_with("\u{1b}[") {
                rest.char_indices()
                    .skip(2)
                    .find(|(_, c)| matches!(c, '\u{40}'..='\u{7e}'))
                    .map(|(i, c)| pos + i + c.len_utf8())
                    .unwrap_or(out.len())
            } else {
                pos + 1
            };
            out.replace_range(pos..end.min(out.len()), "");
        }
        // Bare OSC 10 / DA bodies when ESC was already consumed
        // e.g. "10;rgb:ffff/ffff/ffff" or "?62;22;52c"
        let stripped = strip_bare_terminal_replies(&out);
        stripped.trim().to_string()
    }

    fn looks_like_terminal_reply(s: &str) -> bool {
        let t = s.trim();
        if t.is_empty() {
            return false;
        }
        // OSC color query body
        if t.contains("rgb:") || t.starts_with("10;") || t.starts_with("11;") {
            return true;
        }
        // Device attributes: ?62;22;52c
        if t.starts_with('?') && t.ends_with('c') && t.chars().all(|c| c.is_ascii_digit() || matches!(c, '?' | ';' | 'c')) {
            return true;
        }
        // Mostly digits/semicolons with a trailing c (partial DA)
        if t.contains(';') && t.chars().filter(|c| c.is_ascii_alphabetic()).count() <= 1 {
            let alpha: String = t.chars().filter(|c| c.is_ascii_alphabetic()).collect();
            if alpha == "c" || alpha == "rgb" || alpha.is_empty() {
                return t.chars().any(|c| c.is_ascii_digit());
            }
        }
        false
    }

    fn strip_bare_terminal_replies(s: &str) -> String {
        let mut out = s.to_string();
        // Remove "N;rgb:xxxx/xxxx/xxxx" chunks
        while let Some(start) = out.find("rgb:") {
            // extend left over digits and ';'
            let mut left = start;
            while left > 0 {
                let ch = out[..left].chars().last().unwrap();
                if ch.is_ascii_digit() || ch == ';' {
                    left -= ch.len_utf8();
                } else {
                    break;
                }
            }
            // extend right over hex and '/'
            let mut right = start + 4;
            while right < out.len() {
                let ch = out[right..].chars().next().unwrap();
                if ch.is_ascii_hexdigit() || ch == '/' {
                    right += ch.len_utf8();
                } else {
                    break;
                }
            }
            out.replace_range(left..right, "");
        }
        // Remove "?digits;…c"
        if let Some(start) = out.find('?') {
            if let Some(rel) = out[start..].find('c') {
                let end = start + rel + 1;
                let slice = &out[start..end];
                if slice.chars().all(|c| c.is_ascii_digit() || matches!(c, '?' | ';' | 'c')) {
                    out.replace_range(start..end, "");
                }
            }
        }
        out
    }

    /// Consume a CSI sequence, returning `(index, final_byte, params)`.
    ///
    /// `params` is the parameter string before the final byte (e.g. `"200"` for
    /// bracketed-paste start). Reads from `file` if the sequence spans the buffer.
    fn take_csi_in_buf(
        buf: &[u8],
        mut i: usize,
        n: usize,
        file: &mut impl Read,
    ) -> (usize, Option<u8>, String) {
        let mut params = String::new();
        while i < n {
            let c = buf[i];
            i += 1;
            if (0x40..=0x7e).contains(&c) {
                return (i, Some(c), params);
            }
            if c.is_ascii_digit() || c == b';' || c == b'?' {
                params.push(c as char);
            }
        }
        // Remainder may still be in the TTY stream.
        let mut b = [0u8; 1];
        for _ in 0..64 {
            match file.read(&mut b) {
                Ok(1) if (0x40..=0x7e).contains(&b[0]) => {
                    return (i, Some(b[0]), params);
                }
                Ok(1) => {
                    if b[0].is_ascii_digit() || b[0] == b';' || b[0] == b'?' {
                        params.push(b[0] as char);
                    }
                }
                _ => break,
            }
        }
        (i, None, params)
    }

    /// Discard input until bracketed-paste end (`ESC [ 201 ~`) or a short timeout.
    fn drain_bracketed_paste(
        buf: &[u8],
        mut i: usize,
        n: usize,
        file: &mut impl Read,
    ) -> usize {
        // Fast path: end already in the current buffer.
        while i < n {
            if buf[i] == 0x1b {
                i += 1;
                if i < n && buf[i] == b'[' {
                    i += 1;
                    let (new_i, final_b, params) = take_csi_in_buf(buf, i, n, file);
                    i = new_i;
                    if final_b == Some(b'~') && params == "201" {
                        return i;
                    }
                    continue;
                }
            } else {
                i += 1;
            }
        }
        // Slow path: read from TTY until paste end.
        let mut acc: Vec<u8> = Vec::new();
        let mut b = [0u8; 64];
        for _ in 0..512 {
            match file.read(&mut b) {
                Ok(0) => break,
                Ok(m) => {
                    acc.extend_from_slice(&b[..m]);
                    // Search for ESC [ 201 ~
                    if let Some(pos) = find_bracketed_paste_end(&acc) {
                        // Anything after paste-end stays unread only if still in
                        // acc — we can't push back easily; drop trailing for safety.
                        let _ = pos;
                        return n;
                    }
                    if acc.len() > 4096 {
                        break;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        n
    }

    fn find_bracketed_paste_end(data: &[u8]) -> Option<usize> {
        // ESC [ 201 ~
        data.windows(6).position(|w| w == b"\x1b[201~").map(|p| p + 6)
    }

    /// Skip OSC body already in buffer; returns new index.
    fn skip_osc_in_buf(buf: &[u8], mut i: usize, n: usize) -> usize {
        while i < n {
            let c = buf[i];
            i += 1;
            if c == 0x07 {
                // BEL terminator
                break;
            }
            if c == 0x1b {
                // ST = ESC \
                if i < n && buf[i] == b'\\' {
                    i += 1;
                }
                break;
            }
        }
        i
    }

    fn skip_osc_from_reader(file: &mut impl Read) {
        let mut b = [0u8; 1];
        for _ in 0..256 {
            match file.read(&mut b) {
                Ok(1) if b[0] == 0x07 => break,
                Ok(1) if b[0] == 0x1b => {
                    let mut n = [0u8; 1];
                    let _ = file.read(&mut n);
                    break;
                }
                Ok(1) => continue,
                _ => break,
            }
        }
    }

    fn is_lone_escape(first: &[u8], file: &mut impl Read) -> bool {
        if first.is_empty() || first[0] != 0x1b {
            return false;
        }
        if first.len() == 1 {
            std::thread::sleep(Duration::from_millis(30));
            let mut more = [0u8; 64];
            match file.read(&mut more) {
                Ok(0) | Err(_) => return true,
                Ok(n) => {
                    // Discard the sequence that followed Esc
                    let _ = &more[..n];
                    return false;
                }
            }
        }
        false
    }

    #[cfg(test)]
    mod sanitize_tests {
        use super::*;
        use pretty_assertions::assert_eq;

        #[test]
        fn test_sanitize_strips_rgb_and_da() {
            let fixture = "10;rgb:ffff/ffff/ffff?62;22;52c";
            let actual = sanitize_draft(fixture);
            assert_eq!(actual, "");
        }

        #[test]
        fn test_sanitize_keeps_user_text() {
            let fixture = "hello world";
            let actual = sanitize_draft(fixture);
            assert_eq!(actual, "hello world");
        }

        #[test]
        fn test_sanitize_strips_osc_escape() {
            let fixture = "\u{1b}]10;rgb:ffff/ffff/ffff\u{7}keep";
            let actual = sanitize_draft(fixture);
            assert_eq!(actual, "keep");
        }

        #[test]
        fn test_looks_like_terminal_reply() {
            assert!(looks_like_terminal_reply("10;rgb:ffff/ffff/ffff"));
            assert!(looks_like_terminal_reply("?62;22;52c"));
            assert!(!looks_like_terminal_reply("explain this image"));
        }
    }
}

#[cfg(not(unix))]
mod non_unix {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{disable_raw_mode, enable_raw_mode};

    pub(super) fn wait_for_escape_blocking(cancel: &AtomicBool) {
        let _ = enable_raw_mode();
        while !cancel.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if let Ok(Event::Key(key)) = crossterm::event::read() {
                        if key.code == KeyCode::Esc
                            && !matches!(key.kind, KeyEventKind::Release)
                        {
                            let _ = disable_raw_mode();
                            return;
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = disable_raw_mode();
    }

    pub(super) fn listen_busy_blocking(
        cancel: &AtomicBool,
        escaped: &AtomicBool,
        pending: Arc<Mutex<VecDeque<String>>>,
    ) -> String {
        let _ = enable_raw_mode();
        let mut draft = String::new();
        while !cancel.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    if let Ok(Event::Key(key)) = crossterm::event::read() {
                        if matches!(key.kind, KeyEventKind::Release) {
                            continue;
                        }
                        match key.code {
                            KeyCode::Esc => {
                                escaped.store(true, Ordering::Relaxed);
                                cancel.store(true, Ordering::Relaxed);
                                break;
                            }
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                escaped.store(true, Ordering::Relaxed);
                                cancel.store(true, Ordering::Relaxed);
                                break;
                            }
                            KeyCode::Enter => {
                                let line = draft.trim().to_string();
                                draft.clear();
                                if !line.is_empty() {
                                    if let Ok(mut q) = pending.lock() {
                                        q.push_back(line);
                                    }
                                }
                            }
                            KeyCode::Backspace => {
                                let _ = draft.pop();
                            }
                            KeyCode::Char(c) => {
                                draft.push(c);
                            }
                            _ => {}
                        }
                    }
                }
                Ok(false) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = disable_raw_mode();
        draft
    }
}
