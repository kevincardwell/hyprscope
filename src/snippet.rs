//! Generate a ready-to-paste rule for a window.

/// Escape for RE2: only the metacharacters, so `brave-origin` stays readable.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        if ".^$|()[]{}*+?\\".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

use crate::hypr::Client;
use crate::rules::{evaluate, Facts, RuleSet};

/// Which config dialect to emit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    /// Plain `hl.window_rule({...})`.
    Hyprland,
    /// Omarchy's `o.window(match, effects)` helper.
    Omarchy,
}

impl Dialect {
    /// Omarchy is in play when any rule was written through its helper.
    pub fn detect(rules: &RuleSet) -> Self {
        let omarchy = rules.rules.iter().any(|r| {
            r.via
                .iter()
                .any(|v| v.file_name() == "helpers.lua" && v.file.to_string_lossy().contains("omarchy"))
        });
        if omarchy {
            Dialect::Omarchy
        } else {
            Dialect::Hyprland
        }
    }
}

/// Build the snippet. `effects` are `key = value` pairs already in Lua form.
pub fn generate(c: &Client, rules: &RuleSet, facts: &Facts, dialect: Dialect, effects: &[(String, String)], by_title: bool) -> String {
    let class = if c.initial_class.is_empty() { &c.class } else { &c.initial_class };
    let mut matches = vec![("class".to_string(), format!("^{}$", escape(class)))];
    if by_title || class.is_empty() {
        let title = if c.initial_title.is_empty() { &c.title } else { &c.initial_title };
        matches.push(("title".to_string(), format!("^{}$", escape(title))));
    }
    let effects: Vec<(String, String)> = if effects.is_empty() {
        vec![("float".into(), "true".into())]
    } else {
        effects.to_vec()
    };

    let mut out = String::new();
    out.push_str(&format!("-- {} \"{}\"", c.class, c.title));
    if c.initial_class != c.class || c.initial_title != c.title {
        out.push_str(&format!("  (opened as {} \"{}\": static effects see these)", c.initial_class, c.initial_title));
    }
    out.push('\n');

    let rep = evaluate(rules, facts);
    let touching: Vec<String> = rep
        .verdicts
        .iter()
        .filter(|v| v.is_active())
        .filter_map(|v| {
            let r = &rules.rules[v.rule_idx];
            let keys: Vec<&str> = r
                .effects
                .iter()
                .map(|e| e.key.as_str())
                .filter(|k| effects.iter().any(|(ek, _)| ek == k))
                .collect();
            if keys.is_empty() {
                None
            } else {
                Some(format!("{} ({})", r.loc.compact(), keys.join(", ")))
            }
        })
        .collect();
    if !touching.is_empty() {
        out.push_str(&format!(
            "-- also sets these effects for this window: {}; a rule added later wins\n",
            touching.join(", ")
        ));
    }
    if by_title {
        out.push_str("-- title matching: static effects (float, workspace, size…) only see the initial title\n");
    }

    let lua_match: Vec<String> = matches.iter().map(|(k, v)| format!("{k} = {}", lua_str(v))).collect();
    let lua_effects: Vec<String> = effects.iter().map(|(k, v)| format!("{k} = {v}")).collect();
    match dialect {
        Dialect::Omarchy => {
            if matches.len() == 1 {
                out.push_str(&format!("o.window({}, {{ {} }})\n", lua_str(&matches[0].1), lua_effects.join(", ")));
            } else {
                out.push_str(&format!("o.window({{ {} }}, {{ {} }})\n", lua_match.join(", "), lua_effects.join(", ")));
            }
        }
        Dialect::Hyprland => {
            out.push_str(&format!(
                "hl.window_rule({{ match = {{ {} }}, {} }})\n",
                lua_match.join(", "),
                lua_effects.join(", ")
            ));
        }
    }
    out
}

/// Quote for Lua, escaping backslashes and quotes.
pub fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Parse `key=value` into Lua form: bare true/false/numbers pass through,
/// everything else becomes a quoted string.
pub fn parse_effect(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once('=')?;
    let k = k.trim().to_string();
    let v = v.trim();
    let lua = if v == "true" || v == "false" || v.parse::<f64>().is_ok() {
        v.to_string()
    } else {
        lua_str(v)
    };
    Some((k, lua))
}

/// Copy to the Wayland clipboard if wl-copy exists.
pub fn to_clipboard(text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("wl-copy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}
