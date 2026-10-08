//! The global shortcut that shows the window, as recorded on the settings page and stored as
//! text like `Ctrl+Alt+Space`.

use global_hotkey::hotkey::HotKey;
use gpui_kit::Keystroke;

/// Keys that are only modifiers, pressed while the rest of the shortcut is still to come
const MODIFIER_KEYS: [&str; 6] = ["shift", "control", "ctrl", "alt", "platform", "win"];

/// The shortcut for a pressed key, `Ok(None)` for a modifier alone. Shortcuts need Ctrl, Alt or
/// Win, so typing elsewhere is not taken over.
pub fn from_keystroke(keystroke: &Keystroke) -> Result<Option<String>, &'static str> {
    let key = keystroke.key.to_ascii_lowercase();
    if MODIFIER_KEYS.contains(&key.as_str()) {
        return Ok(None);
    }
    let m = &keystroke.modifiers;
    if !(m.control || m.alt || m.platform) {
        return Err("A shortcut needs Ctrl, Alt or Win together with a key");
    }
    let mut text = String::new();
    for (on, name) in [
        (m.control, "Ctrl"),
        (m.alt, "Alt"),
        (m.shift, "Shift"),
        (m.platform, "Win"),
    ] {
        if on {
            text.push_str(name);
            text.push('+');
        }
    }
    text.push_str(&key_name(&key));
    match hotkey(&text) {
        Some(_) => Ok(Some(text)),
        None => Err("This key can not be part of a shortcut"),
    }
}

/// The shortcut for registering it, `None` if the text is none.
pub fn hotkey(text: &str) -> Option<HotKey> {
    let text = text
        .split('+')
        .map(|part| if part == "Win" { "super" } else { part })
        .collect::<Vec<_>>()
        .join("+");
    text.parse().ok()
}

/// How a key shows in a shortcut: `A`, `Space`, `F5`, `PageUp`.
fn key_name(key: &str) -> String {
    match key {
        "pageup" => "PageUp".into(),
        "pagedown" => "PageDown".into(),
        "printscreen" => "PrintScreen".into(),
        _ => {
            let mut chars = key.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keystroke(text: &str) -> Keystroke {
        Keystroke::parse(text).expect("Valid keystroke")
    }

    #[test]
    fn recorded() {
        let shortcut = |text: &str| from_keystroke(&keystroke(text));
        assert_eq!(
            shortcut("ctrl-alt-space"),
            Ok(Some("Ctrl+Alt+Space".into()))
        );
        assert_eq!(shortcut("win-shift-f"), Ok(Some("Shift+Win+F".into())));
        assert_eq!(shortcut("alt-f5"), Ok(Some("Alt+F5".into())));
        assert_eq!(shortcut("ctrl-pageup"), Ok(Some("Ctrl+PageUp".into())));
        assert!(shortcut("shift-a").is_err());
        assert!(shortcut("a").is_err());
        for text in ["Ctrl+Alt+Space", "Shift+Win+F", "Alt+F5", "Ctrl+PageUp"] {
            assert!(hotkey(text).is_some(), "{}", text);
        }
    }
}
