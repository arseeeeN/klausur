//! Key names for `page.press`, in the form of the DOM's `KeyboardEvent.key`: `"Enter"`, `"a"`,
//! or a combination such as `"Shift+Tab"` or `"Control+a"`.

#[derive(Debug)]
pub struct Key {
    pub key: String,
    pub code: String,
    pub key_code: u32,
    /// What the key types; keys without text (arrows, Escape) send a raw key-down.
    pub text: Option<String>,
    /// CDP's bit mask: Alt 1, Control 2, Meta 4, Shift 8.
    pub modifiers: u32,
}

/// Named keys: `(key, code, Windows virtual key code, text)`.
const NAMED: &[(&str, &str, u32, Option<&str>)] = &[
    ("Enter", "Enter", 13, Some("\r")),
    ("Tab", "Tab", 9, None),
    ("Backspace", "Backspace", 8, None),
    ("Delete", "Delete", 46, None),
    ("Escape", "Escape", 27, None),
    ("ArrowUp", "ArrowUp", 38, None),
    ("ArrowDown", "ArrowDown", 40, None),
    ("ArrowLeft", "ArrowLeft", 37, None),
    ("ArrowRight", "ArrowRight", 39, None),
    ("Home", "Home", 36, None),
    ("End", "End", 35, None),
    ("PageUp", "PageUp", 33, None),
    ("PageDown", "PageDown", 34, None),
    ("Space", "Space", 32, Some(" ")),
];

const MODIFIERS: &[(&str, u32)] = &[("Alt", 1), ("Control", 2), ("Meta", 4), ("Shift", 8)];

pub fn parse(combo: &str) -> Result<Key, String> {
    let mut parts: Vec<&str> = combo.split('+').collect();
    // "Control++" means Control and the plus key.
    if combo.ends_with("++") {
        parts.truncate(parts.len() - 2);
        parts.push("+");
    }
    let name = parts
        .pop()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| format!("`{combo}` names no key"))?;
    let mut modifiers = 0;
    for m in parts {
        let bit = MODIFIERS
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(m))
            .map(|(_, b)| *b)
            .ok_or_else(|| {
                format!("`{m}` isn't a modifier key; use Alt, Control, Meta or Shift")
            })?;
        modifiers |= bit;
    }
    // A shortcut such as Control+a shouldn't also type the letter.
    let typing = modifiers & !8 == 0;
    if let Some((key, code, key_code, text)) =
        NAMED.iter().find(|(k, ..)| k.eq_ignore_ascii_case(name))
    {
        return Ok(Key {
            key: if *key == "Space" {
                " ".into()
            } else {
                key.to_string()
            },
            code: code.to_string(),
            key_code: *key_code,
            text: text.filter(|_| typing).map(str::to_string),
            modifiers,
        });
    }
    let mut chars = name.chars();
    let (Some(c), None) = (chars.next(), chars.next()) else {
        let names: Vec<&str> = NAMED.iter().map(|(k, ..)| *k).collect();
        return Err(format!(
            "`{name}` isn't a key; use a single character or one of {}",
            names.join(", ")
        ));
    };
    let shifted = modifiers & 8 != 0;
    let (code, key_code) = match c.to_ascii_uppercase() {
        u @ 'A'..='Z' => (format!("Key{u}"), u as u32),
        d @ '0'..='9' => (format!("Digit{d}"), d as u32),
        _ => (String::new(), 0),
    };
    let typed = if shifted {
        c.to_uppercase().to_string()
    } else {
        c.to_string()
    };
    Ok(Key {
        key: typed.clone(),
        code,
        key_code,
        text: typing.then_some(typed),
        modifiers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_combinations() {
        let enter = parse("Enter").unwrap();
        assert_eq!((enter.key_code, enter.text.as_deref()), (13, Some("\r")));
        let tab = parse("Shift+Tab").unwrap();
        assert_eq!((tab.key.as_str(), tab.modifiers), ("Tab", 8));
        let select_all = parse("Control+a").unwrap();
        assert_eq!(
            (
                select_all.code.as_str(),
                select_all.modifiers,
                select_all.text
            ),
            ("KeyA", 2, None)
        );
        assert_eq!(parse("Shift+a").unwrap().text.as_deref(), Some("A"));
        assert_eq!(parse("Control++").unwrap().key, "+");
        assert!(parse("Hyper+a").unwrap_err().contains("isn't a modifier"));
        assert!(parse("Enterr").unwrap_err().contains("isn't a key"));
    }
}
