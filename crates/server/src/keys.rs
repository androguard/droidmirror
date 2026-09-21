//! Android `KeyEvent` keycodes → Linux `KEY_*` codes, plus ASCII → (key, shift).

pub fn android_to_linux(keycode: u32) -> Option<u16> {
    Some(match keycode {
        3 => 172,   // HOME → KEY_HOMEPAGE
        4 => 158,   // BACK
        19 => 103,  // DPAD_UP
        20 => 108,  // DPAD_DOWN
        21 => 105,  // DPAD_LEFT
        22 => 106,  // DPAD_RIGHT
        24 => 115,  // VOLUME_UP
        25 => 114,  // VOLUME_DOWN
        26 => 116,  // POWER
        29..=54 => (keycode - 29 + 30) as u16, // A-Z → KEY_A..
        7 => 11,    // 0
        8..=16 => (keycode - 8 + 2) as u16,    // 1-9 → KEY_1..
        61 => 15,   // TAB
        62 => 57,   // SPACE
        66 => 28,   // ENTER
        67 => 14,   // DEL (backspace)
        111 => 1,   // ESCAPE
        187 => 139, // APP_SWITCH → KEY_MENU
        _ => return None,
    })
}

/// Printable ASCII to a Linux key and whether Shift should be held.
pub fn ascii_to_linux(c: char) -> Option<(u16, bool)> {
    match c {
        'a'..='z' => Some((30 + (c as u16 - 'a' as u16), false)),
        'A'..='Z' => Some((30 + (c as u16 - 'A' as u16), true)),
        '1'..='9' => Some((2 + (c as u16 - '1' as u16), false)),
        '0' => Some((11, false)),
        ' ' => Some((57, false)),
        '\n' | '\r' => Some((28, false)),
        '\t' => Some((15, false)),
        '!' => Some((2, true)),
        '@' => Some((3, true)),
        '#' => Some((4, true)),
        '$' => Some((5, true)),
        '%' => Some((6, true)),
        '^' => Some((7, true)),
        '&' => Some((8, true)),
        '*' => Some((9, true)),
        '(' => Some((10, true)),
        ')' => Some((11, true)),
        '-' => Some((12, false)),
        '_' => Some((12, true)),
        '=' => Some((13, false)),
        '+' => Some((13, true)),
        '[' => Some((26, false)),
        '{' => Some((26, true)),
        ']' => Some((27, false)),
        '}' => Some((27, true)),
        '\\' => Some((43, false)),
        '|' => Some((43, true)),
        ';' => Some((39, false)),
        ':' => Some((39, true)),
        '\'' => Some((40, false)),
        '"' => Some((40, true)),
        ',' => Some((51, false)),
        '<' => Some((51, true)),
        '.' => Some((52, false)),
        '>' => Some((52, true)),
        '/' => Some((53, false)),
        '?' => Some((53, true)),
        '`' => Some((41, false)),
        '~' => Some((41, true)),
        _ => None,
    }
}

pub const KEY_LEFTSHIFT: u16 = 42;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_nav() {
        assert_eq!(android_to_linux(29), Some(30)); // A
        assert_eq!(android_to_linux(4), Some(158));
        assert_eq!(android_to_linux(26), Some(116));
        assert_eq!(ascii_to_linux('A'), Some((30, true)));
        assert_eq!(ascii_to_linux('!'), Some((2, true)));
        assert_eq!(ascii_to_linux('€'), None);
    }
}
