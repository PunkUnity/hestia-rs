//! Slint key events → GameStream virtual-key codes.
//!
//! Slint reports keys as text: printable characters as themselves, named
//! keys as control characters or private-use code points (the `Key`
//! namespace, following the NSEvent function-key convention). Printable
//! characters carry their US-layout key and whether that character needs
//! Shift for toolkits that report the character without the modifier.
//!
//! Characters are mapped by their US-layout key; anything without a key
//! (accented letters, IME output) is sent as text on key press. That is the
//! fallback for on-screen keyboards: physical keyboards go by position
//! ([`physical`]), so any layout works the way the host's layout says.

use hestia_core::stream::input::vk;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mapped {
    Key { code: u16, implied_shift: bool },
    Text(String),
}

/// The virtual-key code for a physical key, by its Linux input event code
/// (`KEY_*` in `linux/input-event-codes.h`; what evdev, Wayland and X11
/// minus 8, and Android's scan codes report). GameStream virtual keys name
/// positions on a US keyboard, so the key keeps its place whatever this
/// device's layout is and the host's layout gives it meaning, as on a
/// keyboard plugged into the host: dead keys, AltGr and the extra keys of
/// ISO and ABNT2 keyboards included.
///
/// Media, volume and power keys aren't mapped: they stay with this device.
pub fn physical(code: u32) -> Option<u16> {
    let letter = |c: u8| c as u16;
    Some(match code {
        1 => vk::ESCAPE,
        // KEY_1..KEY_9, KEY_0
        2..=10 => b'1' as u16 + (code as u16 - 2),
        11 => b'0' as u16,
        12 => vk::OEM_MINUS,
        13 => vk::OEM_PLUS,
        14 => vk::BACK,
        15 => vk::TAB,
        16 => letter(b'Q'),
        17 => letter(b'W'),
        18 => letter(b'E'),
        19 => letter(b'R'),
        20 => letter(b'T'),
        21 => letter(b'Y'),
        22 => letter(b'U'),
        23 => letter(b'I'),
        24 => letter(b'O'),
        25 => letter(b'P'),
        26 => vk::OEM_4,
        27 => vk::OEM_6,
        28 => vk::RETURN,
        29 => vk::LCONTROL,
        30 => letter(b'A'),
        31 => letter(b'S'),
        32 => letter(b'D'),
        33 => letter(b'F'),
        34 => letter(b'G'),
        35 => letter(b'H'),
        36 => letter(b'J'),
        37 => letter(b'K'),
        38 => letter(b'L'),
        39 => vk::OEM_1,
        40 => vk::OEM_7,
        41 => vk::OEM_3,
        42 => vk::LSHIFT,
        43 => vk::OEM_5,
        44 => letter(b'Z'),
        45 => letter(b'X'),
        46 => letter(b'C'),
        47 => letter(b'V'),
        48 => letter(b'B'),
        49 => letter(b'N'),
        50 => letter(b'M'),
        51 => vk::OEM_COMMA,
        52 => vk::OEM_PERIOD,
        53 => vk::OEM_2,
        54 => vk::RSHIFT,
        55 => vk::MULTIPLY,
        56 => vk::LMENU,
        57 => vk::SPACE,
        58 => vk::CAPITAL,
        // KEY_F1..KEY_F10
        59..=68 => vk::F1 + (code as u16 - 59),
        69 => vk::NUMLOCK,
        70 => vk::SCROLL,
        71 => vk::NUMPAD0 + 7,
        72 => vk::NUMPAD0 + 8,
        73 => vk::NUMPAD0 + 9,
        74 => vk::SUBTRACT,
        75 => vk::NUMPAD0 + 4,
        76 => vk::NUMPAD0 + 5,
        77 => vk::NUMPAD0 + 6,
        78 => vk::ADD,
        79 => vk::NUMPAD0 + 1,
        80 => vk::NUMPAD0 + 2,
        81 => vk::NUMPAD0 + 3,
        82 => vk::NUMPAD0,
        83 => vk::DECIMAL,
        86 => vk::OEM_102,
        87 => vk::F1 + 10,
        88 => vk::F1 + 11,
        // KEY_RO: ABNT2's /? and the Japanese ろ key.
        89 => vk::ABNT_C1,
        // KEY_KPENTER: hosts don't tell the two Enters apart.
        96 => vk::RETURN,
        97 => vk::RCONTROL,
        98 => vk::DIVIDE,
        99 => vk::SNAPSHOT,
        100 => vk::RMENU,
        102 => vk::HOME,
        103 => vk::UP,
        104 => vk::PRIOR,
        105 => vk::LEFT,
        106 => vk::RIGHT,
        107 => vk::END,
        108 => vk::DOWN,
        109 => vk::NEXT,
        110 => vk::INSERT,
        111 => vk::DELETE,
        119 => vk::PAUSE,
        121 => vk::SEPARATOR,
        125 => vk::LWIN,
        126 => vk::RWIN,
        127 => vk::APPS,
        // KEY_F13..KEY_F24
        183..=194 => vk::F1 + 12 + (code as u16 - 183),
        _ => return None,
    })
}

fn special(c: char) -> Option<u16> {
    Some(match c {
        '\u{0008}' => vk::BACK,
        '\u{0009}' | '\u{0019}' => vk::TAB, // Tab, Backtab (Shift+Tab)
        '\u{000a}' | '\u{000d}' => vk::RETURN,
        '\u{001b}' => vk::ESCAPE,
        '\u{007f}' => vk::DELETE,
        '\u{0010}' => vk::LSHIFT,
        '\u{0011}' => vk::LCONTROL,
        '\u{0012}' => vk::LMENU,
        '\u{0013}' => vk::RMENU, // AltGr
        '\u{0014}' => vk::CAPITAL,
        '\u{0015}' => vk::RSHIFT,
        '\u{0016}' => vk::RCONTROL,
        '\u{0017}' => vk::LWIN,
        '\u{0018}' => vk::RWIN,
        '\u{F700}' => vk::UP,
        '\u{F701}' => vk::DOWN,
        '\u{F702}' => vk::LEFT,
        '\u{F703}' => vk::RIGHT,
        // F1–F24
        c @ '\u{F704}'..='\u{F71B}' => vk::F1 + (c as u16 - 0xF704),
        '\u{F727}' => vk::INSERT,
        '\u{F729}' => vk::HOME,
        '\u{F72B}' => vk::END,
        '\u{F72C}' => vk::PRIOR,
        '\u{F72D}' => vk::NEXT,
        '\u{F72E}' => vk::SNAPSHOT,
        '\u{F72F}' => 0x91, // VK_SCROLL
        '\u{F730}' => vk::PAUSE,
        '\u{F735}' => vk::APPS, // Menu
        _ => return None,
    })
}

/// Maps the `text` of a Slint key event.
pub fn map(text: &str) -> Option<Mapped> {
    let mut chars = text.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        // Composed input (dead keys, IME).
        return Some(Mapped::Text(text.to_owned()));
    }
    if let Some(code) = special(first) {
        return Some(Mapped::Key {
            code,
            implied_shift: false,
        });
    }
    if let Some((code, implied_shift)) = vk::from_char(first) {
        return Some(Mapped::Key { code, implied_shift });
    }
    if first.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&first) {
        return None;
    }
    Some(Mapped::Text(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: u16, implied_shift: bool) -> Option<Mapped> {
        Some(Mapped::Key { code, implied_shift })
    }

    #[test]
    fn maps_named_keys() {
        assert_eq!(map("\u{F702}"), key(vk::LEFT, false));
        assert_eq!(map("\u{F704}"), key(vk::F1, false));
        assert_eq!(map("\u{F70F}"), key(vk::F1 + 11, false));
        assert_eq!(map("\n"), key(vk::RETURN, false));
        assert_eq!(map("\u{1b}"), key(vk::ESCAPE, false));
        assert_eq!(map("\u{10}"), key(vk::LSHIFT, false));
    }

    #[test]
    fn maps_characters_to_keys_with_implied_shift() {
        assert_eq!(map("a"), key(b'A' as u16, false));
        assert_eq!(map("A"), key(b'A' as u16, true));
        assert_eq!(map("!"), key(b'1' as u16, true));
        assert_eq!(map(" "), key(vk::SPACE, false));
        assert_eq!(map("?"), key(vk::OEM_2, true));
    }

    #[test]
    fn physical_keys_go_by_position() {
        // KEY_Q is Q's place, whatever an AZERTY layout prints on it.
        assert_eq!(physical(16), Some(b'Q' as u16));
        assert_eq!(physical(30), Some(b'A' as u16));
        assert_eq!(physical(2), Some(b'1' as u16));
        assert_eq!(physical(10), Some(b'9' as u16));
        assert_eq!(physical(11), Some(b'0' as u16));
        // ABNT2: Ç sits where US has ;, the accent keys where US has [ and '.
        assert_eq!(physical(39), Some(vk::OEM_1));
        assert_eq!(physical(26), Some(vk::OEM_4));
        assert_eq!(physical(40), Some(vk::OEM_7));
        assert_eq!(physical(89), Some(vk::ABNT_C1));
        assert_eq!(physical(86), Some(vk::OEM_102));
        assert_eq!(physical(59), Some(vk::F1));
        assert_eq!(physical(68), Some(vk::F1 + 9));
        assert_eq!(physical(87), Some(vk::F1 + 10));
        assert_eq!(physical(88), Some(vk::F1 + 11));
        assert_eq!(physical(183), Some(vk::F1 + 12));
        assert_eq!(physical(194), Some(vk::F1 + 23));
        assert_eq!(physical(82), Some(vk::NUMPAD0));
        assert_eq!(physical(73), Some(vk::NUMPAD0 + 9));
        assert_eq!(physical(100), Some(vk::RMENU), "AltGr");
        assert_eq!(physical(125), Some(vk::LWIN));
        // Volume, the Android back key, power: this device's.
        for code in [0, 113, 114, 115, 116, 158, 164] {
            assert_eq!(physical(code), None, "{code}");
        }
    }

    #[test]
    fn every_physical_key_is_distinct_except_enter() {
        let mut seen = std::collections::HashMap::new();
        for code in 0..256 {
            if let Some(key) = physical(code)
                && let Some(other) = seen.insert(key, code)
            {
                assert_eq!((other, code), (28, 96), "{key:#x} twice");
            }
        }
    }

    #[test]
    fn falls_back_to_text() {
        assert_eq!(map("ç"), Some(Mapped::Text("ç".into())));
        assert_eq!(map("é"), Some(Mapped::Text("é".into())));
        assert_eq!(map("ab"), Some(Mapped::Text("ab".into())));
        assert_eq!(map(""), None);
        assert_eq!(map("\u{F7FF}"), None);
    }
}
