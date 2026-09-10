//! The effect vocabulary, mirrored from Hyprland's WindowRuleEffectContainer.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    /// Evaluated once when the window opens, against initial class and title.
    Static,
    /// Re-evaluated whenever a matched property changes.
    Dynamic,
    /// Hyprland would reject the rule with "unknown field".
    Unknown,
}

pub const STATIC_EFFECTS: &[&str] = &[
    "float",
    "tile",
    "fullscreen",
    "maximize",
    "fullscreen_state",
    "move",
    "size",
    "center",
    "pseudo",
    "monitor",
    "workspace",
    "no_initial_focus",
    "pin",
    "group",
    "suppress_event",
    "content",
    "no_close_for",
    "scrolling_width",
];

pub const DYNAMIC_EFFECTS: &[&str] = &[
    "rounding",
    "rounding_power",
    "persistent_size",
    "animation",
    "border_color",
    "idle_inhibit",
    "opacity",
    "tag",
    "max_size",
    "min_size",
    "border_size",
    "allows_input",
    "dim_around",
    "decorate",
    "focus_on_activate",
    "keep_aspect_ratio",
    "nearest_neighbor",
    "no_anim",
    "no_blur",
    "no_dim",
    "no_focus",
    "no_follow_mouse",
    "no_max_size",
    "no_shadow",
    "no_glow",
    "no_wobble",
    "no_shortcuts_inhibit",
    "opaque",
    "force_rgbx",
    "sync_fullscreen",
    "immediate",
    "xray",
    "render_unfocused",
    "no_screen_share",
    "no_vrr",
    "no_auto_hdr",
    "tonemap",
    "scroll_mouse",
    "scroll_touchpad",
    "stay_focused",
    "confine_pointer",
    "no_xdg_drags",
];

/// Effects whose Lua value must be a boolean.
pub const BOOL_EFFECTS: &[&str] = &[
    "float",
    "tile",
    "fullscreen",
    "maximize",
    "center",
    "pseudo",
    "no_initial_focus",
    "pin",
    "persistent_size",
    "allows_input",
    "dim_around",
    "decorate",
    "focus_on_activate",
    "keep_aspect_ratio",
    "nearest_neighbor",
    "no_anim",
    "no_blur",
    "no_dim",
    "no_focus",
    "no_follow_mouse",
    "no_max_size",
    "no_shadow",
    "no_glow",
    "no_wobble",
    "no_shortcuts_inhibit",
    "opaque",
    "force_rgbx",
    "sync_fullscreen",
    "immediate",
    "xray",
    "render_unfocused",
    "no_screen_share",
    "no_vrr",
    "no_auto_hdr",
    "stay_focused",
    "confine_pointer",
    "no_xdg_drags",
];

pub const INT_EFFECTS: &[&str] = &["no_close_for", "rounding", "border_size"];
pub const FLOAT_EFFECTS: &[&str] = &["scrolling_width", "rounding_power", "scroll_mouse", "scroll_touchpad"];
pub const VEC2_EFFECTS: &[&str] = &["move", "size", "max_size", "min_size"];
pub const STRING_EFFECTS: &[&str] = &[
    "fullscreen_state",
    "monitor",
    "workspace",
    "group",
    "suppress_event",
    "content",
    "animation",
    "idle_inhibit",
    "opacity",
    "tag",
    "tonemap",
];

pub fn effect_kind(key: &str) -> EffectKind {
    if STATIC_EFFECTS.contains(&key) {
        EffectKind::Static
    } else if DYNAMIC_EFFECTS.contains(&key) {
        EffectKind::Dynamic
    } else {
        EffectKind::Unknown
    }
}

pub fn all_effects() -> impl Iterator<Item = &'static str> {
    STATIC_EFFECTS.iter().chain(DYNAMIC_EFFECTS.iter()).copied()
}

/// Levenshtein-based nearest known name, for typo hints.
pub fn nearest<'a>(name: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let mut best: Option<(usize, &str)> = None;
    for c in candidates {
        let d = levenshtein(name, c);
        if d <= 3 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, c));
        }
    }
    best.map(|(_, c)| c)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}
