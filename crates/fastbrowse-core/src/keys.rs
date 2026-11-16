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
    fn no_role_offers_tab_or_a_key_twice() {
        for role in ["slider", "spinbutton", "option", "tab", "radio", "menuitem", "gridcell", "treeitem"] {
            let keys = role_keys(role);
            assert!(!keys.contains(&"Tab"), "{role}");
            let unique: std::collections::HashSet<_> = keys.iter().collect();
            assert_eq!(unique.len(), keys.len(), "{role}");
        }
    }
}
