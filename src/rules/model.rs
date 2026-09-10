use regex::Regex;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use super::effects::{effect_kind, EffectKind};

/// A source position captured from the Lua call stack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub file: PathBuf,
    pub line: u32,
    pub func: Option<String>,
}

impl Location {
    /// `~/.config/hypr/windows.lua:14` style rendering.
    pub fn short(&self) -> String {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut f = self.file.display().to_string();
        if !home.is_empty() && f.starts_with(&home) {
            f = format!("~{}", &f[home.len()..]);
        }
        format!("{f}:{}", self.line)
    }

    /// A short form for dense lists: `omarchy/apps/browser.lua:2`,
    /// `hypr/looknfeel.lua:73`, or the last two path components.
    pub fn compact(&self) -> String {
        let f = self.file.display().to_string();
        let home = std::env::var("HOME").unwrap_or_default();
        let omarchy = std::env::var("OMARCHY_PATH").unwrap_or_else(|_| "/usr/share/omarchy".into());
        let base = if let Some(rest) = f.strip_prefix(&format!("{omarchy}/default/hypr/")) {
            format!("omarchy/{rest}")
        } else if let Some(rest) = f.strip_prefix(&format!("{omarchy}/")) {
            format!("omarchy/{rest}")
        } else if !home.is_empty() && f.starts_with(&format!("{home}/.config/")) {
            f[home.len() + "/.config/".len()..].to_string()
        } else {
            let parts: Vec<&str> = f.rsplit('/').take(2).collect();
            parts.into_iter().rev().collect::<Vec<_>>().join("/")
        };
        format!("{base}:{}", self.line)
    }

    pub fn file_name(&self) -> String {
        self.file.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
    }
}

/// A Lua value as seen in a rule table, before Hyprland's string conversion.
#[derive(Clone, Debug, PartialEq)]
pub enum RawValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Seq(Vec<RawValue>),
    Map(BTreeMap<String, RawValue>),
}

impl RawValue {
    /// Hyprland's `ruleValueToString` plus the table forms used by vec2/gradients.
    pub fn to_hypr_string(&self) -> String {
        match self {
            RawValue::Bool(b) => {
                if *b {
                    "true".into()
                } else {
                    "false".into()
                }
            }
            RawValue::Int(i) => i.to_string(),
            RawValue::Float(f) => format!("{f}"),
            RawValue::Str(s) => s.clone(),
            RawValue::Seq(items) => items.iter().map(|i| i.to_hypr_string()).collect::<Vec<_>>().join(" "),
            RawValue::Map(m) => {
                let mut parts = Vec::new();
                if let Some(RawValue::Seq(colors)) = m.get("colors") {
                    parts.extend(colors.iter().map(|c| c.to_hypr_string()));
                }
                if let Some(a) = m.get("angle") {
                    parts.push(format!("{}deg", a.to_hypr_string()));
                }
                if parts.is_empty() {
                    m.iter().map(|(k, v)| format!("{k}={}", v.to_hypr_string())).collect::<Vec<_>>().join(" ")
                } else {
                    parts.join(" ")
                }
            }
        }
    }

    /// Lua-ish rendering for display.
    pub fn display(&self) -> String {
        match self {
            RawValue::Str(s) => format!("\"{s}\""),
            RawValue::Seq(items) => format!("{{ {} }}", items.iter().map(|i| i.display()).collect::<Vec<_>>().join(", ")),
            RawValue::Map(m) => format!(
                "{{ {} }}",
                m.iter().map(|(k, v)| format!("{k} = {}", v.display())).collect::<Vec<_>>().join(", ")
            ),
            other => other.to_hypr_string(),
        }
    }
}

/// Exactly what the config passed to `hl.window_rule`, plus where from.
#[derive(Clone, Debug)]
pub struct RawRule {
    pub idx: usize,
    pub name: Option<String>,
    pub enabled: bool,
    pub matches: BTreeMap<String, RawValue>,
    pub fields: BTreeMap<String, RawValue>,
    /// Call stack, innermost first. Frame 0 is the direct caller of hl.window_rule.
    pub frames: Vec<Location>,
    pub problems: Vec<String>,
}

/// Which matching engine a property uses (mirrors Hyprland's RULE_ENGINES table).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Regex,
    Bool,
    Int,
    Workspace,
    Tag,
}

pub const MATCH_PROPS: &[(&str, Engine)] = &[
    ("class", Engine::Regex),
    ("title", Engine::Regex),
    ("initial_class", Engine::Regex),
    ("initial_title", Engine::Regex),
    ("float", Engine::Bool),
    ("tag", Engine::Tag),
    ("xwayland", Engine::Bool),
    ("fullscreen", Engine::Bool),
    ("pin", Engine::Bool),
    ("focus", Engine::Bool),
    ("group", Engine::Bool),
    ("modal", Engine::Bool),
    ("fullscreen_state_internal", Engine::Int),
    ("fullscreen_state_client", Engine::Int),
    ("workspace", Engine::Workspace),
    ("content", Engine::Regex),
    ("xdg_tag", Engine::Regex),
];

pub fn engine_for(prop: &str) -> Option<Engine> {
    MATCH_PROPS.iter().find(|(n, _)| *n == prop).map(|(_, e)| *e)
}

/// Hyprland's `truthy()`: "1", or a case-insensitive prefix of true/yes/on.
pub fn truthy(s: &str) -> bool {
    if s == "1" {
        return true;
    }
    let l = s.to_ascii_lowercase();
    l.starts_with("true") || l.starts_with("yes") || l.starts_with("on")
}

/// One compiled predicate from a rule's `match` table.
#[derive(Clone, Debug)]
pub struct Matcher {
    pub prop: String,
    pub raw: String,
    pub engine: Option<Engine>,
    pub negative: bool,
    pub regex: Option<Result<Regex, String>>,
    pub boolean: Option<bool>,
    pub int: Option<i64>,
}

impl Matcher {
    pub fn new(prop: &str, raw: &str) -> Self {
        let engine = engine_for(prop);
        let mut m = Matcher {
            prop: prop.to_string(),
            raw: raw.to_string(),
            engine,
            negative: false,
            regex: None,
            boolean: None,
            int: None,
        };
        match engine {
            Some(Engine::Regex) => {
                let pat = if let Some(rest) = raw.strip_prefix("negative:") {
                    m.negative = true;
                    rest
                } else {
                    raw
                };
                m.regex = Some(compile_full(pat));
            }
            Some(Engine::Tag) => {
                if let Some(rest) = raw.strip_prefix("negative") {
                    // Hyprland checks for the prefix "negative" and skips 9 bytes.
                    m.negative = true;
                    m.raw = rest.get(1..).unwrap_or("").to_string();
                    m.raw = format!("negative:{}", m.raw);
                }
            }
            Some(Engine::Bool) => m.boolean = Some(truthy(raw)),
            Some(Engine::Int) => m.int = raw.trim().parse().ok(),
            Some(Engine::Workspace) | None => {}
        }
        m
    }

    /// The tag name a tag matcher looks for, without the negative prefix.
    pub fn tag_name(&self) -> &str {
        self.raw.strip_prefix("negative:").unwrap_or(&self.raw)
    }

    pub fn regex_error(&self) -> Option<&str> {
        match &self.regex {
            Some(Err(e)) => Some(e.as_str()),
            _ => None,
        }
    }

    /// RE2 FullMatch semantics, with `negative:` inversion.
    pub fn match_str(&self, s: &str) -> Option<bool> {
        match &self.regex {
            Some(Ok(re)) => Some(re.is_match(s) != self.negative),
            _ => None,
        }
    }
}

fn compile_full(pat: &str) -> Result<Regex, String> {
    Regex::new(&format!("^(?:{pat})$")).map_err(|e| trim_regex_error(&e.to_string()))
}

fn trim_regex_error(e: &str) -> String {
    // The regex crate's errors are multi-line; keep the first meaningful line.
    e.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("regex parse error"))
        .unwrap_or(e)
        .to_string()
}

/// A rule effect with its Hyprland-side classification.
#[derive(Clone, Debug)]
pub struct Effect {
    pub key: String,
    pub value: RawValue,
    pub kind: EffectKind,
}

impl Effect {
    pub fn hypr_string(&self) -> String {
        self.value.to_hypr_string()
    }
}

/// A fully interpreted window rule.
#[derive(Clone, Debug)]
pub struct Rule {
    pub idx: usize,
    pub name: Option<String>,
    pub enabled: bool,
    pub matchers: Vec<Matcher>,
    pub effects: Vec<Effect>,
    /// Where the rule was written, skipping wrapper helpers like Omarchy's o.window.
    pub loc: Location,
    /// Wrapper frames between the rule site and hl.window_rule, if any.
    pub via: Vec<Location>,
    pub problems: Vec<String>,
}

impl Rule {
    pub fn from_raw(raw: &RawRule) -> Self {
        let mut matchers: Vec<Matcher> = raw.matches.iter().map(|(k, v)| Matcher::new(k, &v.to_hypr_string())).collect();
        // Keep a stable, readable order: regex props first, then the rest.
        matchers.sort_by_key(|m| MATCH_PROPS.iter().position(|(n, _)| *n == m.prop).unwrap_or(usize::MAX));

        let effects = raw
            .fields
            .iter()
            .map(|(k, v)| Effect {
                key: k.clone(),
                value: v.clone(),
                kind: effect_kind(k),
            })
            .collect();

        let (loc, via) = pick_location(&raw.frames);
        Rule {
            idx: raw.idx,
            name: raw.name.clone(),
            enabled: raw.enabled,
            matchers,
            effects,
            loc,
            via,
            problems: raw.problems.clone(),
        }
    }

    /// Hyprland refuses to match a rule with no predicates or when disabled.
    pub fn can_match(&self) -> bool {
        self.enabled && !self.matchers.is_empty()
    }

    pub fn has_static_effects(&self) -> bool {
        self.effects.iter().any(|e| e.kind == EffectKind::Static)
    }

    pub fn tag_effect(&self) -> Option<&str> {
        self.effects.iter().find(|e| e.key == "tag").and_then(|e| match &e.value {
            RawValue::Str(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn matcher(&self, prop: &str) -> Option<&Matcher> {
        self.matchers.iter().find(|m| m.prop == prop)
    }

    /// `class ~ ^kitty$, float = true` style summary of the match table.
    pub fn match_summary(&self) -> String {
        if self.matchers.is_empty() {
            return "(no match table)".into();
        }
        self.matchers
            .iter()
            .map(|m| format!("{}{}", m.prop, m.summary_value()))
            .collect::<Vec<_>>()
            .join("  ")
    }

    pub fn effect_summary(&self) -> String {
        self.effects
            .iter()
            .map(|e| format!("{}={}", e.key, e.value.display()))
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn display_name(&self) -> String {
        match &self.name {
            Some(n) => format!("#{} \"{}\"", self.idx + 1, n),
            None => format!("#{}", self.idx + 1),
        }
    }
}

impl Matcher {
    pub fn summary_value(&self) -> String {
        match self.engine {
            Some(Engine::Regex) => format!("~{}", quote(&self.raw)),
            Some(Engine::Tag) => format!("={}", self.raw),
            Some(Engine::Workspace) => format!("={}", self.raw),
            Some(Engine::Bool) => format!("={}", self.boolean.unwrap_or(false)),
            Some(Engine::Int) => format!("={}", self.raw),
            None => format!("={} (unknown prop)", self.raw),
        }
    }
}

fn quote(s: &str) -> String {
    if s.contains(' ') || s.is_empty() {
        format!("\"{s}\"")
    } else {
        s.to_string()
    }
}

/// Choose the user-facing location for a rule: the innermost frame that is not
/// a known wrapper helper. Everything skipped is reported as `via`.
fn pick_location(frames: &[Location]) -> (Location, Vec<Location>) {
    let is_wrapper = |l: &Location| {
        let name = l.file_name();
        name == "helpers.lua" || l.func.as_deref() == Some("window")
    };
    let mut via = Vec::new();
    for f in frames {
        if is_wrapper(f) {
            via.push(f.clone());
            continue;
        }
        return (f.clone(), via);
    }
    let fallback = frames.first().cloned().unwrap_or(Location {
        file: PathBuf::from("<unknown>"),
        line: 0,
        func: None,
    });
    (fallback, Vec::new())
}

/// All rules from one config load, in registration order.
#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    pub config_path: PathBuf,
    pub load_errors: Vec<String>,
}

impl RuleSet {
    pub fn from_raw(raw: &[RawRule], config_path: PathBuf, load_errors: Vec<String>) -> Self {
        RuleSet {
            rules: raw.iter().map(Rule::from_raw).collect(),
            config_path,
            load_errors,
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}  →  {}", self.display_name(), self.match_summary(), self.effect_summary())
    }
}
