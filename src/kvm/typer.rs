//! Typing text into the server as keystrokes ("paste").
//!
//! The console only carries key presses, so text is turned into the key
//! strokes a US keyboard would need for it. The server's keyboard layout
//! decides what actually appears, as with a real keyboard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use super::keymap::usage;
use super::proto;

/// One key stroke: HID usage and whether Shift is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub usage: u16,
    pub shift: bool,
}

/// The key stroke for `c` on a US keyboard, or `None` if it has no key.
pub fn stroke(c: char) -> Option<Stroke> {
    let plain = |usage| Some(Stroke { usage, shift: false });
    let shifted = |usage| Some(Stroke { usage, shift: true });
    match c {
        'a'..='z' => plain(usage::letter(c)),
        'A'..='Z' => shifted(usage::letter(c)),
        '1'..='9' => plain(0x1e + (c as u16 - '1' as u16)),
        '0' => plain(0x27),
        '\n' => plain(usage::ENTER),
        '\t' => plain(usage::TAB),
        ' ' => plain(usage::SPACE),
        '!' => shifted(0x1e),
        '@' => shifted(0x1f),
        '#' => shifted(0x20),
        '$' => shifted(0x21),
        '%' => shifted(0x22),
        '^' => shifted(0x23),
        '&' => shifted(0x24),
        '*' => shifted(0x25),
        '(' => shifted(0x26),
        ')' => shifted(0x27),
        '-' => plain(0x2d),
        '_' => shifted(0x2d),
        '=' => plain(0x2e),
        '+' => shifted(0x2e),
        '[' => plain(0x2f),
        '{' => shifted(0x2f),
        ']' => plain(0x30),
        '}' => shifted(0x30),
        '\\' => plain(0x31),
        '|' => shifted(0x31),
        ';' => plain(0x33),
        ':' => shifted(0x33),
        '\'' => plain(0x34),
        '"' => shifted(0x34),
        '`' => plain(0x35),
        '~' => shifted(0x35),
        ',' => plain(0x36),
        '<' => shifted(0x36),
        '.' => plain(0x37),
        '>' => shifted(0x37),
        '/' => plain(0x38),
        '?' => shifted(0x38),
        _ => None,
    }
}

/// Clean up pasted text: Windows line endings become one Enter, and
/// typographic characters that word processors insert become their ASCII
/// look-alikes.
pub fn normalize(text: &str) -> String {
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    text.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{2032}' => '\'',
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{2033}' => '"',
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => '-',
            '\u{a0}' | '\u{2002}' | '\u{2003}' | '\u{2009}' | '\u{202f}' => ' ',
            c => c,
        })
        .collect()
}

/// Characters in `text` that cannot be typed, without duplicates, in order.
pub fn untypeable(text: &str) -> Vec<char> {
    let mut out = Vec::new();
    for c in normalize(text).chars() {
        if stroke(c).is_none() && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// How fast to type. Slow speeds help BIOS screens and boot loaders that
/// drop keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    Fast,
    Normal,
    Slow,
}

impl Speed {
    pub const ALL: [Speed; 3] = [Speed::Fast, Speed::Normal, Speed::Slow];

    pub fn label(self) -> &'static str {
        match self {
            Speed::Fast => "Fast",
            Speed::Normal => "Normal",
            Speed::Slow => "Slow (BIOS, boot loaders)",
        }
    }

    /// Pause after every key press and release.
    fn delay(self) -> Duration {
        Duration::from_millis(match self {
            Speed::Fast => 8,
            Speed::Normal => 20,
            Speed::Slow => 60,
        })
    }
}

/// A running typing job.
pub struct TypeJob {
    /// Characters to type (after normalising, skipping untypeable ones).
    pub total: usize,
    done: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
}

impl TypeJob {
    /// Start typing `text` in the background. Messages go to `tx` (the
    /// console's command queue).
    ///
    /// Caps Lock is not compensated for: the LED state the iDRAC reports
    /// does not reliably match what the server's OS applies.
    pub fn start(tx: Sender<Vec<u8>>, text: &str, speed: Speed, notify: Arc<dyn Fn() + Send + Sync>) -> TypeJob {
        let strokes: Vec<Stroke> = normalize(text).chars().filter_map(stroke).collect();
        let job = TypeJob {
            total: strokes.len(),
            done: Arc::new(AtomicUsize::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
        };
        let (done, cancel, finished) = (job.done.clone(), job.cancel.clone(), job.finished.clone());
        let delay = speed.delay();
        thread::spawn(move || {
            let send = |m: Vec<u8>| tx.send(m).is_ok();
            for (i, s) in strokes.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                let ok = (!s.shift || send(proto::key(usage::LSHIFT, true)))
                    && send(proto::key(s.usage, true))
                    && {
                        thread::sleep(delay);
                        send(proto::key(s.usage, false))
                    }
                    && (!s.shift || send(proto::key(usage::LSHIFT, false)));
                if !ok {
                    break;
                }
                thread::sleep(delay);
                done.store(i + 1, Ordering::Relaxed);
                if i % 8 == 0 {
                    notify();
                }
            }
            finished.store(true, Ordering::Relaxed);
            notify();
        });
        job
    }

    pub fn done(&self) -> usize {
        self.done.load(Ordering::Relaxed)
    }

    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for TypeJob {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_us_layout() {
        assert_eq!(stroke('a'), Some(Stroke { usage: 0x04, shift: false }));
        assert_eq!(stroke('Z'), Some(Stroke { usage: 0x1d, shift: true }));
        assert_eq!(stroke('1'), Some(Stroke { usage: 0x1e, shift: false }));
        assert_eq!(stroke('0'), Some(Stroke { usage: 0x27, shift: false }));
        assert_eq!(stroke(')'), Some(Stroke { usage: 0x27, shift: true }));
        assert_eq!(stroke('?'), Some(Stroke { usage: 0x38, shift: true }));
        assert_eq!(stroke('\n'), Some(Stroke { usage: usage::ENTER, shift: false }));
        assert_eq!(stroke('é'), None);
        // Every printable ASCII character can be typed.
        assert!((' '..='~').all(|c| stroke(c).is_some()));
    }

    #[test]
    fn normalizes_pasted_text() {
        assert_eq!(normalize("a\r\nb\rc"), "a\nb\nc");
        assert_eq!(normalize("\u{201c}hi\u{201d} \u{2013} it\u{2019}s"), "\"hi\" - it's");
        assert_eq!(untypeable("héllo wörld é"), vec!['é', 'ö']);
    }

    #[test]
    fn types_with_shift() {
        let (tx, rx) = std::sync::mpsc::channel();
        let job = TypeJob::start(tx, "Ab", Speed::Fast, Arc::new(|| {}));
        while !job.finished() {
            thread::sleep(Duration::from_millis(5));
        }
        let msgs: Vec<Vec<u8>> = rx.try_iter().collect();
        let expect = [
            proto::key(usage::LSHIFT, true),
            proto::key(0x04, true),
            proto::key(0x04, false),
            proto::key(usage::LSHIFT, false),
            proto::key(0x05, true),
            proto::key(0x05, false),
        ];
        assert_eq!(msgs, expect);
        assert_eq!(job.done(), 2);
    }
}
