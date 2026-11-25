//! W3C UI Events key chords: the grammar the browser presses a key by, and the agent checks a key copied from the
//! task against before pressing it.

/// The named key values of W3C UI Events (https://www.w3.org/TR/uievents-key/) a chord can name: the whitespace,
/// navigation, editing, lock and function keys a field takes. The names come from the standard, not from the pages
/// seen. A named key outside this set, such as Copy or PrintScreen, is refused by name rather than guessed at.
pub const NAMED_KEYS: &[&str] = &[
    "Enter",
    "Escape",
    "Backspace",
    "Tab",
    "Delete",
    "End",
    "Home",
    "Insert",
    "PageDown",
    "PageUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "ArrowUp",
    "CapsLock",
    "Clear",
    "ContextMenu",
    "NumLock",
    "Pause",
    "ScrollLock",
    "Select",
    "AltGraph",
    "Dead",
    "Unidentified",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "F11",
    "F12",
    "F13",
    "F14",
    "F15",
    "F16",
    "F17",
    "F18",
    "F19",
    "F20",
    "F21",
    "F22",
    "F23",
    "F24",
];

/// A key chord as the browser presses it: the CDP modifier bitmask and one W3C UI Events key value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: i64,
    pub key: String,
}

/// Why words are no key chord.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChordError {
    RepeatedModifier,
    NotAKey(String),
}

impl std::fmt::Display for ChordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RepeatedModifier => f.write_str("a key chord repeats a modifier"),
            Self::NotAKey(rest) => write!(
                f,
                "{rest:?} is not a key fastbrowse presses: a single character, or a W3C UI Events named key of a field, \
                 after any of Alt, Ctrl, Meta and Shift joined by +"
            ),
        }
    }
}

impl std::error::Error for ChordError {}

/// Parse the modifier and key values published by W3C UI Events, which browsers expose as DOM keyboard events. A chord
/// is any modifiers, each followed by `+`, then one key, so the key itself may be `+`. The browser presses only what
/// this accepts, and the agent checks a key it copied from the task by the same grammar before pressing it.
pub fn chord(value: &str) -> Result<Chord, ChordError> {
    let mut modifiers = 0;
    let mut rest = value.trim();
    while let Some((head, tail)) = rest.split_once('+')
        && !tail.trim().is_empty()
        && let Some(bit) = modifier(head.trim())
    {
        if modifiers & bit != 0 {
            return Err(ChordError::RepeatedModifier);
        }
        modifiers |= bit;
        rest = tail.trim();
    }
    let key = standard_key(rest).ok_or_else(|| ChordError::NotAKey(rest.to_string()))?;
    Ok(Chord { modifiers, key: key.to_string() })
}

/// The CDP modifier bit of a W3C UI Events modifier key, in any case.
fn modifier(value: &str) -> Option<i64> {
    match value.to_ascii_lowercase().as_str() {
        "alt" => Some(1),
        "ctrl" | "control" => Some(2),
        "meta" | "cmd" | "command" => Some(4),
        "shift" => Some(8),
        _ => None,
    }
}

/// The W3C UI Events named key `value` spells in any case, or a single character as given.
fn standard_key(value: &str) -> Option<&str> {
    if value.eq_ignore_ascii_case("space") {
        return Some(" ");
    }
    if let Some(named) = NAMED_KEYS.iter().find(|key| key.eq_ignore_ascii_case(value)) {
        return Some(named);
    }
    (value.chars().count() == 1).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_that_name_no_key_are_no_chord() {
        assert_eq!(chord("document."), Err(ChordError::NotAKey("document.".into())));
        assert_eq!(chord("Ctrl+Ctrl+a"), Err(ChordError::RepeatedModifier));
        assert_eq!(chord("Ctrl+B"), Ok(Chord { modifiers: 2, key: "B".into() }));
    }
}
