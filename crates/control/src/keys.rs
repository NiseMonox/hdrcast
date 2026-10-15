//! Key names as mpv reports them, and the input section that makes mpv report every key to us.

/// Modifier bits carried with every event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: u8 = 1;
    pub const CTRL: u8 = 2;
    pub const ALT: u8 = 4;
    pub const META: u8 = 8;
    pub const ALL: [(u8, &'static str); 4] = [(Self::SHIFT, "Shift"), (Self::CTRL, "Ctrl"), (Self::ALT, "Alt"), (Self::META, "Meta")];

    pub fn has(self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

macro_rules! named_keys {
    ($($variant:ident = $name:literal),* $(,)?) => {
        /// Keys that are not a character. The order is the wire number; append, never reorder.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(u8)]
        pub enum NamedKey { $($variant),* }

        impl NamedKey {
            pub const ALL: &'static [NamedKey] = &[$(NamedKey::$variant),*];

            /// The name mpv uses for it.
            pub fn mpv_name(self) -> &'static str {
                match self { $(NamedKey::$variant => $name),* }
            }

            pub fn from_mpv_name(name: &str) -> Option<Self> {
                match name { $($name => Some(NamedKey::$variant),)* _ => None }
            }
        }
    };
}

named_keys! {
    Enter = "ENTER", Tab = "TAB", Backspace = "BS", Delete = "DEL", Insert = "INS", Home = "HOME", End = "END",
    PageUp = "PGUP", PageDown = "PGDWN", Escape = "ESC", Left = "LEFT", Right = "RIGHT", Up = "UP", Down = "DOWN",
    F1 = "F1", F2 = "F2", F3 = "F3", F4 = "F4", F5 = "F5", F6 = "F6", F7 = "F7", F8 = "F8", F9 = "F9", F10 = "F10",
    F11 = "F11", F12 = "F12",
    Kp0 = "KP0", Kp1 = "KP1", Kp2 = "KP2", Kp3 = "KP3", Kp4 = "KP4", Kp5 = "KP5", Kp6 = "KP6", Kp7 = "KP7",
    Kp8 = "KP8", Kp9 = "KP9", KpDecimal = "KP_DEC", KpEnter = "KP_ENTER", KpAdd = "KP_ADD",
    KpSubtract = "KP_SUBTRACT", KpMultiply = "KP_MULTIPLY", KpDivide = "KP_DIVIDE",
    Print = "PRINT", Menu = "MENU",
}

/// A key: a character as typed (shift already applied: `a`, `A`, `!`), or a named key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Named(NamedKey),
}

/// Named keys sit above the last Unicode scalar on the wire.
const NAMED_BASE: u32 = 0x0011_0000;

impl Key {
    pub fn code(self) -> u32 {
        match self {
            Key::Char(c) => c as u32,
            Key::Named(n) => NAMED_BASE + n as u32,
        }
    }

    pub fn from_code(code: u32) -> Option<Self> {
        if code >= NAMED_BASE {
            NamedKey::ALL.get((code - NAMED_BASE) as usize).copied().map(Key::Named)
        } else {
            char::from_u32(code).filter(|c| !c.is_control()).map(Key::Char)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Button {
    Left = 0,
    Middle = 1,
    Right = 2,
    Back = 3,
    Forward = 4,
}

impl Button {
    pub const ALL: [(Button, &'static str); 5] = [
        (Button::Left, "MBTN_LEFT"),
        (Button::Middle, "MBTN_MID"),
        (Button::Right, "MBTN_RIGHT"),
        (Button::Back, "MBTN_BACK"),
        (Button::Forward, "MBTN_FORWARD"),
    ];

    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.iter().map(|(b, _)| *b).find(|b| *b as u32 == code)
    }
}

/// What a key name from mpv stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Button(Button),
    /// One wheel notch: axis 0 = vertical, 1 = horizontal; positive = up / right.
    Wheel { axis: u8, direction: i8 },
}

const WHEELS: [(&str, u8, i8); 4] =
    [("WHEEL_UP", 0, 1), ("WHEEL_DOWN", 0, -1), ("WHEEL_RIGHT", 1, 1), ("WHEEL_LEFT", 1, -1)];

/// Parses a key name the way mpv prints it in a `key-binding` message: `Ctrl+Shift+ENTER`, `Alt+a`, `MBTN_LEFT`,
/// `Ctrl++`. `None` for names this module never binds (media keys, double-click pseudo keys).
pub fn parse_mpv_key(name: &str) -> Option<(Mods, Input)> {
    let mut mods = 0u8;
    let mut rest = name;
    'prefixes: loop {
        for (bit, word) in Mods::ALL {
            if let Some(tail) = rest.strip_prefix(word).and_then(|t| t.strip_prefix('+')) {
                if !tail.is_empty() {
                    mods |= bit;
                    rest = tail;
                    continue 'prefixes;
                }
            }
        }
        break;
    }
    let input = if let Some((b, _)) = Button::ALL.iter().find(|(_, n)| *n == rest) {
        Input::Button(*b)
    } else if let Some((_, axis, direction)) = WHEELS.iter().find(|(n, _, _)| *n == rest) {
        Input::Wheel { axis: *axis, direction: *direction }
    } else if let Some(n) = NamedKey::from_mpv_name(rest) {
        Input::Key(Key::Named(n))
    } else if rest == "SPACE" {
        Input::Key(Key::Char(' '))
    } else if rest == "SHARP" {
        Input::Key(Key::Char('#'))
    } else {
        let mut chars = rest.chars();
        let c = chars.next()?;
        if chars.next().is_some() || c.is_control() {
            return None;
        }
        Input::Key(Key::Char(c))
    };
    Some((Mods(mods), input))
}

/// Name of the mpv input section and of the binding its lines invoke.
pub const SECTION: &str = "hdrcast-control";
pub const BINDING: &str = "hdrcast-input";
/// The one combination kept for the viewer's own player: toggles full screen locally.
pub const LOCAL_FULLSCREEN: &str = "Ctrl+Alt+ENTER";

/// The input.conf text for `define-section`: every key, button and wheel direction, with every modifier
/// combination, bound to `script-binding <client>/hdrcast-input`. mpv then sends a `key-binding` client message
/// (with the up/down state and the key name) to that client, which is our IPC connection, instead of acting on
/// the key itself. mpv matches modifiers exactly, hence one line per combination.
pub fn mpv_input_section(client_name: &str) -> String {
    let mut names: Vec<String> = Vec::new();
    for c in 0x21u8..=0x7e {
        names.push(if c == b'#' { "SHARP".to_string() } else { (c as char).to_string() });
    }
    names.push("SPACE".to_string());
    names.extend(NamedKey::ALL.iter().map(|k| k.mpv_name().to_string()));
    names.extend(Button::ALL.iter().map(|(_, n)| n.to_string()));
    names.extend(WHEELS.iter().map(|(n, _, _)| n.to_string()));

    let mut out = String::with_capacity(names.len() * 16 * 48);
    for name in &names {
        for combo in 0u8..16 {
            let mut key = String::new();
            for (bit, word) in Mods::ALL {
                if combo & bit != 0 {
                    key.push_str(word);
                    key.push('+');
                }
            }
            key.push_str(name);
            if key == LOCAL_FULLSCREEN {
                out.push_str(&key);
                out.push_str(" cycle fullscreen\n");
                continue;
            }
            out.push_str(&key);
            out.push_str(" script-binding ");
            out.push_str(client_name);
            out.push('/');
            out.push_str(BINDING);
            out.push('\n');
        }
    }
    // Characters without a key of their own (IME output, dead-key results) arrive as text.
    out.push_str("any_unicode script-binding ");
    out.push_str(client_name);
    out.push('/');
    out.push_str(BINDING);
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_parse_like_mpv_prints_them() {
        assert_eq!(parse_mpv_key("a"), Some((Mods(0), Input::Key(Key::Char('a')))));
        assert_eq!(parse_mpv_key("Ctrl+c"), Some((Mods(Mods::CTRL), Input::Key(Key::Char('c')))));
        assert_eq!(
            parse_mpv_key("Shift+Ctrl+Alt+Meta+ENTER"),
            Some((Mods(15), Input::Key(Key::Named(NamedKey::Enter))))
        );
        assert_eq!(parse_mpv_key("Ctrl++"), Some((Mods(Mods::CTRL), Input::Key(Key::Char('+')))));
        assert_eq!(parse_mpv_key("+"), Some((Mods(0), Input::Key(Key::Char('+')))));
        assert_eq!(parse_mpv_key("SPACE"), Some((Mods(0), Input::Key(Key::Char(' ')))));
        assert_eq!(parse_mpv_key("Alt+SHARP"), Some((Mods(Mods::ALT), Input::Key(Key::Char('#')))));
        assert_eq!(parse_mpv_key("Shift+MBTN_LEFT"), Some((Mods(Mods::SHIFT), Input::Button(Button::Left))));
        assert_eq!(parse_mpv_key("WHEEL_DOWN"), Some((Mods(0), Input::Wheel { axis: 0, direction: -1 })));
        assert_eq!(parse_mpv_key("中"), Some((Mods(0), Input::Key(Key::Char('中')))));
        assert_eq!(parse_mpv_key("MBTN_LEFT_DBL"), None);
        assert_eq!(parse_mpv_key("VOLUME_UP"), None);
        assert_eq!(parse_mpv_key(""), None);
    }

    #[test]
    fn key_codes_survive_the_round_trip() {
        for key in [Key::Char('a'), Key::Char('中'), Key::Char(' '), Key::Named(NamedKey::F12), Key::Named(NamedKey::Menu)] {
            assert_eq!(Key::from_code(key.code()), Some(key));
        }
        assert_eq!(Key::from_code(0x0011_0000 + 200), None);
        assert_eq!(Key::from_code(0xD800), None, "not a Unicode scalar");
        assert_eq!(Key::from_code(7), None, "control characters are not keys");
    }

    #[test]
    fn the_section_binds_every_combination_and_keeps_one_for_the_player() {
        let s = mpv_input_section("ipc_0");
        assert!(s.contains("\na script-binding ipc_0/hdrcast-input\n"));
        assert!(s.contains("\nShift+Ctrl+Alt+Meta+F12 script-binding ipc_0/hdrcast-input\n"));
        assert!(s.contains("\nCtrl+MBTN_LEFT script-binding ipc_0/hdrcast-input\n"));
        assert!(s.contains("\nCtrl+Alt+ENTER cycle fullscreen\n"));
        assert!(!s.contains("\nCtrl+Alt+ENTER script-binding"));
        assert!(s.contains("\nSHARP script-binding"), "a bare # would start a comment");
        assert!(!s.lines().any(|l| l.starts_with('#')));
        // every line parses back to something we forward (or is the local one)
        for line in s.lines() {
            let key = line.split(' ').next().unwrap();
            assert!(key == "any_unicode" || parse_mpv_key(key).is_some(), "{key}");
        }
    }
}
