//! Physical key -> USB HID usage id (what the iDRAC6 expects on the wire).
//!
//! Using physical keys means the remote OS applies its own keyboard layout,
//! exactly like a real USB keyboard plugged into the server.

// Complete protocol definitions are kept even where the client does not
// use them yet; they document the wire format.
#![allow(dead_code)]

use winit::keyboard::KeyCode as K;

/// HID usage ids used by macros.
pub mod usage {
    pub const A: u16 = 0x04;
    pub const ENTER: u16 = 0x28;
    pub const ESC: u16 = 0x29;
    pub const BACKSPACE: u16 = 0x2a;
    pub const TAB: u16 = 0x2b;
    pub const SPACE: u16 = 0x2c;
    pub const MINUS: u16 = 0x2d;
    pub const F1: u16 = 0x3a;
    pub const PRINT_SCREEN: u16 = 0x46;
    pub const SCROLL_LOCK: u16 = 0x47;
    pub const PAUSE: u16 = 0x48;
    pub const DELETE: u16 = 0x4c;
    pub const NUM_LOCK: u16 = 0x53;
    pub const CAPS_LOCK: u16 = 0x39;
    pub const LCTRL: u16 = 0xe0;
    pub const LSHIFT: u16 = 0xe1;
    pub const LALT: u16 = 0xe2;
    pub const LGUI: u16 = 0xe3;
    pub const RCTRL: u16 = 0xe4;
    pub const RSHIFT: u16 = 0xe5;
    pub const RALT: u16 = 0xe6;
    pub const RGUI: u16 = 0xe7;
    /// Alt+PrintScreen is SysRq on PC keyboards.
    pub const SYSRQ: u16 = PRINT_SCREEN;

    /// F1..F12
    pub fn f(n: u16) -> u16 {
        F1 + (n - 1)
    }
    /// A..Z
    pub fn letter(c: char) -> u16 {
        A + (c.to_ascii_lowercase() as u16 - 'a' as u16)
    }
}

pub fn hid_usage(k: K) -> Option<u16> {
    Some(match k {
        K::KeyA => 0x04,
        K::KeyB => 0x05,
        K::KeyC => 0x06,
        K::KeyD => 0x07,
        K::KeyE => 0x08,
        K::KeyF => 0x09,
        K::KeyG => 0x0a,
        K::KeyH => 0x0b,
        K::KeyI => 0x0c,
        K::KeyJ => 0x0d,
        K::KeyK => 0x0e,
        K::KeyL => 0x0f,
        K::KeyM => 0x10,
        K::KeyN => 0x11,
        K::KeyO => 0x12,
        K::KeyP => 0x13,
        K::KeyQ => 0x14,
        K::KeyR => 0x15,
        K::KeyS => 0x16,
        K::KeyT => 0x17,
        K::KeyU => 0x18,
        K::KeyV => 0x19,
        K::KeyW => 0x1a,
        K::KeyX => 0x1b,
        K::KeyY => 0x1c,
        K::KeyZ => 0x1d,
        K::Digit1 => 0x1e,
        K::Digit2 => 0x1f,
        K::Digit3 => 0x20,
        K::Digit4 => 0x21,
        K::Digit5 => 0x22,
        K::Digit6 => 0x23,
        K::Digit7 => 0x24,
        K::Digit8 => 0x25,
        K::Digit9 => 0x26,
        K::Digit0 => 0x27,
        K::Enter => 0x28,
        K::Escape => 0x29,
        K::Backspace => 0x2a,
        K::Tab => 0x2b,
        K::Space => 0x2c,
        K::Minus => 0x2d,
        K::Equal => 0x2e,
        K::BracketLeft => 0x2f,
        K::BracketRight => 0x30,
        K::Backslash => 0x31,
        K::Semicolon => 0x33,
        K::Quote => 0x34,
        K::Backquote => 0x35,
        K::Comma => 0x36,
        K::Period => 0x37,
        K::Slash => 0x38,
        K::CapsLock => 0x39,
        K::F1 => 0x3a,
        K::F2 => 0x3b,
        K::F3 => 0x3c,
        K::F4 => 0x3d,
        K::F5 => 0x3e,
        K::F6 => 0x3f,
        K::F7 => 0x40,
        K::F8 => 0x41,
        K::F9 => 0x42,
        K::F10 => 0x43,
        K::F11 => 0x44,
        K::F12 => 0x45,
        K::PrintScreen => 0x46,
        K::ScrollLock => 0x47,
        K::Pause => 0x48,
        K::Insert => 0x49,
        K::Home => 0x4a,
        K::PageUp => 0x4b,
        K::Delete => 0x4c,
        K::End => 0x4d,
        K::PageDown => 0x4e,
        K::ArrowRight => 0x4f,
        K::ArrowLeft => 0x50,
        K::ArrowDown => 0x51,
        K::ArrowUp => 0x52,
        K::NumLock => 0x53,
        K::NumpadDivide => 0x54,
        K::NumpadMultiply => 0x55,
        K::NumpadSubtract => 0x56,
        K::NumpadAdd => 0x57,
        K::NumpadEnter => 0x58,
        K::Numpad1 => 0x59,
        K::Numpad2 => 0x5a,
        K::Numpad3 => 0x5b,
        K::Numpad4 => 0x5c,
        K::Numpad5 => 0x5d,
        K::Numpad6 => 0x5e,
        K::Numpad7 => 0x5f,
        K::Numpad8 => 0x60,
        K::Numpad9 => 0x61,
        K::Numpad0 => 0x62,
        K::NumpadDecimal => 0x63,
        K::IntlBackslash => 0x64,
        K::ContextMenu => 0x65,
        K::NumpadEqual => 0x67,
        K::F13 => 0x68,
        K::F14 => 0x69,
        K::F15 => 0x6a,
        K::F16 => 0x6b,
        K::F17 => 0x6c,
        K::F18 => 0x6d,
        K::F19 => 0x6e,
        K::F20 => 0x6f,
        K::F21 => 0x70,
        K::F22 => 0x71,
        K::F23 => 0x72,
        K::F24 => 0x73,
        K::NumpadComma => 0x85,
        K::IntlRo => 0x87,
        K::KanaMode => 0x88,
        K::IntlYen => 0x89,
        K::Convert => 0x8a,
        K::NonConvert => 0x8b,
        K::Lang1 => 0x90,
        K::Lang2 => 0x91,
        K::ControlLeft => 0xe0,
        K::ShiftLeft => 0xe1,
        K::AltLeft => 0xe2,
        K::SuperLeft => 0xe3,
        K::ControlRight => 0xe4,
        K::ShiftRight => 0xe5,
        K::AltRight => 0xe6,
        K::SuperRight => 0xe7,
        _ => return None,
    })
}

/// A named key combination for the Macros menu.
pub struct Macro {
    pub label: &'static str,
    pub keys: Vec<u16>,
}

/// The macros offered by the original iDRAC6 viewer.
pub fn macros() -> Vec<Macro> {
    use usage::*;
    let mut v = vec![
        Macro { label: "Ctrl-Alt-Del", keys: vec![LCTRL, LALT, DELETE] },
        Macro { label: "Alt-Tab", keys: vec![LALT, TAB] },
        Macro { label: "Alt-Esc", keys: vec![LALT, ESC] },
        Macro { label: "Ctrl-Esc", keys: vec![LCTRL, ESC] },
        Macro { label: "Alt-Space", keys: vec![LALT, SPACE] },
        Macro { label: "Alt-Enter", keys: vec![LALT, ENTER] },
        Macro { label: "Alt-Hyphen", keys: vec![LALT, MINUS] },
        Macro { label: "Alt-F4", keys: vec![LALT, f(4)] },
        Macro { label: "PrtScrn", keys: vec![PRINT_SCREEN] },
        Macro { label: "Alt-PrtScrn", keys: vec![LALT, PRINT_SCREEN] },
        Macro { label: "F1", keys: vec![f(1)] },
        Macro { label: "Pause", keys: vec![PAUSE] },
        Macro { label: "Windows key", keys: vec![LGUI] },
        Macro { label: "Ctrl-Alt-Backspace", keys: vec![LCTRL, LALT, BACKSPACE] },
        Macro { label: "Alt-LShift-RShift-Esc", keys: vec![LALT, LSHIFT, RSHIFT, ESC] },
    ];
    for n in 1..=12 {
        v.push(Macro { label: leak(format!("Ctrl-Alt-F{n}")), keys: vec![LCTRL, LALT, f(n)] });
    }
    v
}

/// Linux magic SysRq combinations (Alt+SysRq+<key>).
pub fn sysrq_macros() -> Vec<Macro> {
    use usage::*;
    [
        ('b', "reBoot"),
        ('e', "tErminate all"),
        ('i', "kIll all"),
        ('o', "power Off"),
        ('r', "un-Raw keyboard"),
        ('s', "Sync disks"),
        ('u', "Unmount (remount ro)"),
        ('h', "Help"),
    ]
    .into_iter()
    .map(|(c, what)| Macro { label: leak(format!("Alt-SysRq-{} ({what})", c.to_ascii_uppercase())), keys: vec![LALT, SYSRQ, letter(c)] })
    .collect()
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}
