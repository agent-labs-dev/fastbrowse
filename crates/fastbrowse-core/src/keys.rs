//! The keys a focused control's ARIA role supports, as the WAI-ARIA Authoring Practices Guide's keyboard interaction
//! sections list them (https://www.w3.org/WAI/ARIA/apg/patterns/). Jev picks one of these for a Press, so a slider is
//! stepped, a tab list or menu walked and a grid crossed by the keys the role defines, not by a key Jev would have to
//! write. A key the task gives as a chord is still copied from the task.
//!
//! Each name is a W3C UI Events key value, after any modifiers joined by `+`, as the browser's Press parses it.
//!
//! Roles left out on purpose:
//! - button, link, checkbox and switch: their keys (Enter, Space) only activate, which a click on them already does.
//! - textbox and searchbox: the APG defines no pattern for them; a key the task gives is still pressed in them.
//! - Tab, which every pattern names, moves focus out of the widget: that is Blur. Printable characters (type-ahead)
//!   would be text Jev writes.

/// The APG's keyboard section for `role`, keys marked optional included: the page may not handle one, and a key that
/// changes nothing counts as no progress like any other step.
pub fn role_keys(role: &str) -> &'static [&'static str] {
    match role {
        // https://www.w3.org/WAI/ARIA/apg/patterns/slider/
        "slider" => &["ArrowRight", "ArrowUp", "ArrowLeft", "ArrowDown", "Home", "End", "PageUp", "PageDown"],
        // https://www.w3.org/WAI/ARIA/apg/patterns/spinbutton/
        "spinbutton" => &["ArrowUp", "ArrowDown", "Home", "End", "PageUp", "PageDown"],
        // https://www.w3.org/WAI/ARIA/apg/patterns/listbox/ (the recommended multiple selection model)
        "option" | "listbox" => &[
            "ArrowDown",
            "ArrowUp",
            "Home",
            "End",
            "Space",
            "Shift+ArrowDown",
            "Shift+ArrowUp",
            "Shift+Space",
            "Ctrl+Shift+Home",
            "Ctrl+Shift+End",
            "Ctrl+A",
        ],
        // https://www.w3.org/WAI/ARIA/apg/patterns/tabs/ (Up and Down for a vertical tab list)
        "tab" | "tablist" => {
            &["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Space", "Enter", "Home", "End", "Shift+F10", "Delete"]
        }
        // https://www.w3.org/WAI/ARIA/apg/patterns/radio/
        "radio" => &["Space", "ArrowRight", "ArrowDown", "ArrowLeft", "ArrowUp"],
        // https://www.w3.org/WAI/ARIA/apg/patterns/menubar/
        "menuitem" | "menuitemradio" | "menuitemcheckbox" | "menu" | "menubar" => {
            &["Enter", "Space", "ArrowDown", "ArrowUp", "ArrowRight", "ArrowLeft", "Home", "End", "Escape"]
        }
        // https://www.w3.org/WAI/ARIA/apg/patterns/grid/ (data grids, with the keys a grid that selects cells uses)
        "gridcell" | "grid" | "treegrid" => &[
            "ArrowRight",
            "ArrowLeft",
            "ArrowDown",
            "ArrowUp",
            "PageDown",
            "PageUp",
            "Home",
            "End",
            "Ctrl+Home",
            "Ctrl+End",
            "Ctrl+Space",
            "Shift+Space",
            "Ctrl+A",
            "Shift+ArrowRight",
            "Shift+ArrowLeft",
            "Shift+ArrowDown",
            "Shift+ArrowUp",
        ],
        // https://www.w3.org/WAI/ARIA/apg/patterns/treeview/
        "treeitem" | "tree" => &[
            "ArrowRight",
            "ArrowLeft",
            "ArrowDown",
            "ArrowUp",
            "Home",
            "End",
            "Enter",
            "*",
            "Space",
            "Shift+ArrowDown",
            "Shift+ArrowUp",
            "Shift+Space",
            "Ctrl+Shift+Home",
            "Ctrl+Shift+End",
            "Ctrl+A",
        ],
        // https://www.w3.org/WAI/ARIA/apg/patterns/combobox/ (opening and navigating its popup)
        "combobox" => &["ArrowDown", "ArrowUp", "Alt+ArrowDown", "Alt+ArrowUp", "Enter", "Escape", "Home", "End"],
        _ => &[],
    }
}

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

/// The keys a Press offers on any control, whatever its role: the W3C UI Events navigation and editing keys
/// (https://www.w3.org/TR/uievents-key/) that move a caret, a selection or a picked-up item, and one chord that
/// registers a programmatic edit with an editor. Widgets such as a sortable list pick an item up with Space, move
/// it with the arrows and drop it with Space, and declare no role that lists those keys. Tab is Blur, and a
/// printable character is text.
pub const STANDARD_KEYS: &[&str] = &[
    "ArrowUp",
    "ArrowDown",
    "ArrowLeft",
    "ArrowRight",
    "Home",
    "End",
    "PageUp",
    "PageDown",
    "Space",
    "Enter",
    "Escape",
    "Backspace",
    "Delete",
    "Ctrl+Home",
    "Ctrl+End",
];

/// What a Press on `role` may send: the keys its role defines, then the standard keys it does not repeat.
pub fn press_keys(role: &str) -> Vec<&'static str> {
    let mut keys = role_keys(role).to_vec();
    let own = keys.clone();
    keys.extend(STANDARD_KEYS.iter().filter(|key| !own.contains(key)));
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activation_only_and_text_roles_have_no_keys() {
        for role in ["button", "link", "checkbox", "switch", "textbox", "searchbox", "generic"] {
            assert!(role_keys(role).is_empty(), "{role}");
        }
        assert!(role_keys("slider").contains(&"ArrowRight"));
    }

    #[test]
    fn every_role_key_is_a_chord() {
        for role in ["slider", "spinbutton", "option", "tab", "radio", "menuitem", "gridcell", "treeitem", "combobox"] {
            for key in role_keys(role) {
                assert!(chord(key).is_ok(), "{role}: {key}");
            }
        }
    }

    #[test]
    fn words_that_name_no_key_are_no_chord() {
        assert_eq!(chord("document."), Err(ChordError::NotAKey("document.".into())));
        assert_eq!(chord("Ctrl+Ctrl+a"), Err(ChordError::RepeatedModifier));
        assert_eq!(chord("Ctrl+B"), Ok(Chord { modifiers: 2, key: "B".into() }));
    }

    #[test]
    fn every_role_presses_the_standard_keys_once() {
        for role in ["slider", "option", "button", "textbox", "generic"] {
            let keys = press_keys(role);
            assert!(keys.contains(&"Space") && keys.contains(&"Ctrl+End"), "{role}");
            let unique: std::collections::HashSet<_> = keys.iter().collect();
            assert_eq!(unique.len(), keys.len(), "{role}");
        }
        assert_eq!(&press_keys("slider")[..2], &["ArrowRight", "ArrowUp"]);
        // The browser presses only what the chord grammar parses, so a standard key it refused would end the run.
        for key in STANDARD_KEYS {
            assert!(chord(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn no_role_offers_tab_or_a_key_twice() {
        for role in ["slider", "spinbutton", "option", "tab", "radio", "menuitem", "gridcell", "treeitem"] {
            let keys = role_keys(role);
            assert!(!keys.contains(&"Tab"), "{role}");
            let unique: std::collections::HashSet<_> = keys.iter().collect();
            assert_eq!(unique.len(), keys.len(), "{role}");
        }
    }
}
