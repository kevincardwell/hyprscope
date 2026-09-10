//! Static and live checks over a rule set.

use std::collections::{BTreeMap, BTreeSet};

use crate::rules::effects::{all_effects, nearest, EffectKind, BOOL_EFFECTS, FLOAT_EFFECTS, INT_EFFECTS, STRING_EFFECTS, VEC2_EFFECTS};
use crate::rules::model::{Engine, RawValue, Rule, RuleSet, MATCH_PROPS};
use crate::rules::{Facts, Report};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warn,
    Info,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warn => "warn",
            Severity::Info => "info",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub rule_idx: Option<usize>,
    pub location: String,
    pub message: String,
    pub hint: Option<String>,
}

/// Checks that need only the config.
pub fn lint_static(rules: &RuleSet) -> Vec<Finding> {
    let mut out = Vec::new();

    for e in &rules.load_errors {
        out.push(Finding {
            severity: Severity::Error,
            rule_idx: None,
            location: rules.config_path.display().to_string(),
            message: format!("config did not finish loading: {e}"),
            hint: None,
        });
    }

    for rule in &rules.rules {
        let loc = rule.loc.short();
        let push = |out: &mut Vec<Finding>, sev, msg: String, hint: Option<String>| {
            out.push(Finding {
                severity: sev,
                rule_idx: Some(rule.idx),
                location: loc.clone(),
                message: msg,
                hint,
            });
        };

        for p in &rule.problems {
            push(&mut out, Severity::Error, p.clone(), None);
        }

        if !rule.enabled {
            push(&mut out, Severity::Info, "rule is disabled (enabled = false)".into(), None);
        }

        if rule.matchers.is_empty() {
            push(
                &mut out,
                Severity::Error,
                "no match table: Hyprland never applies a rule without predicates".into(),
                Some("add match = { class = \"…\" } or similar".into()),
            );
        }

        for m in &rule.matchers {
            if m.engine.is_none() {
                let hint = nearest(&m.prop, MATCH_PROPS.iter().map(|(n, _)| *n)).map(|n| format!("did you mean `{n}`?"));
                push(
                    &mut out,
                    Severity::Error,
                    format!("unknown match property `{}` (Hyprland reports a config error and ignores it)", m.prop),
                    hint,
                );
                continue;
            }
            if let Some(err) = m.regex_error() {
                push(
                    &mut out,
                    Severity::Error,
                    format!("`{}` regex `{}` does not compile: {err}", m.prop, m.raw),
                    Some("Hyprland uses RE2 syntax: no lookaround, no backreferences".into()),
                );
            }
            if m.engine == Some(Engine::Regex) && m.raw.contains(' ') && !m.raw.contains("\\ ") && m.prop == "class" {
                push(
                    &mut out,
                    Severity::Info,
                    format!("class regex `{}` contains a space; app ids rarely do", m.raw),
                    None,
                );
            }
            if m.engine == Some(Engine::Bool) && !matches!(m.raw.as_str(), "true" | "false" | "1" | "0") {
                push(
                    &mut out,
                    Severity::Warn,
                    format!(
                        "`{}` = \"{}\" is read with truthy(): prefixes of true/yes/on are true, anything else false",
                        m.prop, m.raw
                    ),
                    Some("use a Lua boolean".into()),
                );
            }
            if m.engine == Some(Engine::Int) && m.int.is_none() {
                push(&mut out, Severity::Error, format!("`{}` = \"{}\" is not an integer", m.prop, m.raw), None);
            }
        }

        if rule.effects.is_empty() {
            push(&mut out, Severity::Warn, "rule has a match table but no effects".into(), None);
        }

        for e in &rule.effects {
            if e.kind == EffectKind::Unknown {
                let hint = nearest(&e.key, all_effects()).map(|n| format!("did you mean `{n}`?"));
                push(
                    &mut out,
                    Severity::Error,
                    format!("unknown effect `{}` (Hyprland reports \"unknown field\" and ignores it)", e.key),
                    hint,
                );
                continue;
            }
            check_effect_type(&mut out, rule, e);
        }

        // Static effects and the title trap.
        if rule.has_static_effects() {
            if let Some(m) = rule.matcher("title") {
                let statics: Vec<&str> = rule.effects.iter().filter(|e| e.kind == EffectKind::Static).map(|e| e.key.as_str()).collect();
                push(
                    &mut out,
                    Severity::Info,
                    format!(
                        "static effect(s) {} are decided once at open, when `title` is still the initial title (`{}` must match that)",
                        statics.join(", "),
                        m.raw
                    ),
                    Some("match initial_title explicitly, or use hl.on(\"window.title\", …) with a dispatch".into()),
                );
            }
        }

        if rule.effects.iter().any(|e| e.key == "pin" && e.value == RawValue::Bool(true)) && !rule.effects.iter().any(|e| e.key == "float") {
            push(
                &mut out,
                Severity::Warn,
                "`pin` is ignored for tiled windows; this rule does not also float".into(),
                Some("add float = true".into()),
            );
        }
    }

    out.extend(check_tags(rules));
    out.extend(check_shadowed(rules));
    out.sort_by_key(|f| (f.severity, f.rule_idx));
    out
}

fn check_effect_type(out: &mut Vec<Finding>, rule: &Rule, e: &crate::rules::model::Effect) {
    let loc = rule.loc.short();
    let mut push = |sev, msg: String, hint: Option<String>| {
        out.push(Finding {
            severity: sev,
            rule_idx: Some(rule.idx),
            location: loc.clone(),
            message: msg,
            hint,
        })
    };
    let key = e.key.as_str();
    match &e.value {
        RawValue::Bool(_) if BOOL_EFFECTS.contains(&key) => {}
        RawValue::Bool(_) => push(Severity::Error, format!("`{key}` does not take a boolean"), None),
        RawValue::Int(_) if INT_EFFECTS.contains(&key) || FLOAT_EFFECTS.contains(&key) => {}
        RawValue::Float(_) if FLOAT_EFFECTS.contains(&key) => {}
        RawValue::Float(_) if INT_EFFECTS.contains(&key) => push(Severity::Warn, format!("`{key}` expects an integer"), None),
        RawValue::Int(_) | RawValue::Float(_) if BOOL_EFFECTS.contains(&key) => push(
            Severity::Warn,
            format!("`{key}` expects a boolean, got a number"),
            Some("use true/false".into()),
        ),
        RawValue::Str(s) => {
            if BOOL_EFFECTS.contains(&key) {
                push(
                    Severity::Warn,
                    format!("`{key}` expects a boolean, got the string \"{s}\""),
                    Some("use true/false".into()),
                );
            } else if VEC2_EFFECTS.contains(&key) {
                if s.split_whitespace().count() != 2 {
                    push(Severity::Error, format!("`{key}` = \"{s}\" must be two expressions, e.g. {{ 800, 600 }}"), None);
                }
            } else if INT_EFFECTS.contains(&key) && s.trim().parse::<i64>().is_err() {
                push(Severity::Error, format!("`{key}` = \"{s}\" is not an integer"), None);
            } else if key == "opacity" {
                if let Err(m) = check_opacity(s) {
                    push(Severity::Error, format!("opacity \"{s}\": {m}"), None);
                }
            } else if key == "idle_inhibit" && !matches!(s.as_str(), "none" | "always" | "focus" | "fullscreen") {
                push(
                    Severity::Error,
                    format!("idle_inhibit \"{s}\" is not one of none/always/focus/fullscreen"),
                    None,
                );
            } else if key == "content" && !matches!(s.as_str(), "none" | "photo" | "video" | "game") {
                push(Severity::Error, format!("content \"{s}\" is not one of none/photo/video/game"), None);
            } else if key == "tag" {
                let body = s.trim_start_matches(['+', '-']);
                if body.is_empty() {
                    push(Severity::Error, "tag effect has an empty name".into(), None);
                }
                if !s.starts_with('+') && !s.starts_with('-') {
                    push(
                        Severity::Info,
                        format!("tag \"{s}\" has no +/- prefix, so it toggles on every re-evaluation"),
                        Some(format!("use \"+{s}\" to set or \"-{s}\" to unset")),
                    );
                }
            } else if key == "workspace" {
                let head = s.split_whitespace().next().unwrap_or("");
                if head.is_empty() {
                    push(Severity::Error, "workspace effect is empty".into(), None);
                }
            } else if !STRING_EFFECTS.contains(&key) && !VEC2_EFFECTS.contains(&key) && key != "border_color" {
                push(Severity::Warn, format!("`{key}` given a string \"{s}\""), None);
            }
        }
        RawValue::Seq(items) => {
            if VEC2_EFFECTS.contains(&key) {
                if items.len() != 2 {
                    push(Severity::Error, format!("`{key}` needs exactly two values, got {}", items.len()), None);
                }
            } else if key != "border_color" {
                push(Severity::Error, format!("`{key}` does not take a table"), None);
            }
        }
        RawValue::Map(_) if key != "border_color" => push(Severity::Error, format!("`{key}` does not take a table"), None),
        _ => {}
    }
}

fn check_opacity(s: &str) -> Result<(), String> {
    let mut n = 0;
    for tok in s.split_whitespace() {
        if tok == "override" || tok == "opacity" {
            continue;
        }
        let v: f32 = tok.parse().map_err(|_| format!("`{tok}` is not a number"))?;
        if !(0.0..=1.0).contains(&v) {
            return Err(format!("{v} is outside 0.0..1.0"));
        }
        n += 1;
        if n > 3 {
            return Err("more than 3 alpha values".into());
        }
    }
    if n == 0 {
        return Err("no alpha value".into());
    }
    Ok(())
}

/// Tags that rules match on but nothing ever sets.
fn check_tags(rules: &RuleSet) -> Vec<Finding> {
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for r in &rules.rules {
        if let Some(t) = r.tag_effect() {
            produced.insert(t.trim_start_matches(['+', '-']).trim_end_matches('*').to_string());
        }
    }
    let mut out = Vec::new();
    for r in &rules.rules {
        for m in &r.matchers {
            if m.engine != Some(Engine::Tag) {
                continue;
            }
            let name = m.tag_name().trim_end_matches('*');
            if !produced.contains(name) {
                out.push(Finding {
                    severity: Severity::Warn,
                    rule_idx: Some(r.idx),
                    location: r.loc.short(),
                    message: format!("matches tag `{name}` but no rule in this config ever sets it"),
                    hint: Some("only a static tag from hl.dsp.window.tag could satisfy it".into()),
                });
            }
        }
    }
    out
}

/// A rule whose match table equals a later rule's, and whose every effect the
/// later rule also sets, can never win anything.
fn check_shadowed(rules: &RuleSet) -> Vec<Finding> {
    let mut out = Vec::new();
    let sig = |r: &Rule| -> Vec<(String, String)> { r.matchers.iter().map(|m| (m.prop.clone(), m.raw.clone())).collect() };
    for (i, a) in rules.rules.iter().enumerate() {
        if a.effects.is_empty() || a.effects.iter().any(|e| e.key == "tag") {
            continue;
        }
        let sa = sig(a);
        for b in rules.rules.iter().skip(i + 1) {
            if sig(b) != sa {
                continue;
            }
            let bkeys: BTreeSet<&str> = b.effects.iter().map(|e| e.key.as_str()).collect();
            if a.effects.iter().all(|e| bkeys.contains(e.key.as_str())) {
                out.push(Finding {
                    severity: Severity::Warn,
                    rule_idx: Some(a.idx),
                    location: a.loc.short(),
                    message: format!(
                        "shadowed: rule {} at {} has the same match and sets every effect this one sets, and last match wins",
                        b.display_name(),
                        b.loc.short()
                    ),
                    hint: None,
                });
                break;
            }
        }
    }
    out
}

/// Upstream bugs worth knowing about, keyed by the running Hyprland version.
pub fn lint_advisories(rules: &RuleSet, version: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let v = version.trim_start_matches('v');
    let is_056 = v.starts_with("0.56.");
    if is_056 {
        for r in &rules.rules {
            if r.matcher("xwayland").is_some() {
                out.push(Finding {
                    severity: Severity::Info,
                    rule_idx: Some(r.idx),
                    location: r.loc.short(),
                    message: format!("Hyprland {v}: `xwayland` matching was reported to match every window after the 0.56 upgrade (hyprwm/Hyprland#15762, traced to distro patches for one reporter)"),
                    hint: Some("verify with: hyprscope match 'xwayland:true'".into()),
                });
            }
        }
    }
    out
}

/// A class regex that matches no open window, while some open window has a
/// class that is not claimed by any specific rule and shares a long common
/// substring with a literal chunk of that regex: the app probably renamed
/// its class (Brave shipping as `brave-origin` against `[bB]rave-browser`).
fn near_misses(rules: &RuleSet, reports: &[(Facts, Report)]) -> Vec<Finding> {
    let catch_all = |raw: &str| matches!(raw, ".*" | ".+" | "^.*$" | "^.+$" | "");
    // Classes some non-catch-all rule already matches are "known" to the config.
    let known: BTreeSet<String> = reports
        .iter()
        .filter(|(f, rep)| {
            rep.verdicts.iter().any(|v| {
                let r = &rules.rules[v.rule_idx];
                r.matcher("class").is_some_and(|m| !catch_all(&m.raw)) && v.predicates_now.iter().any(|p| p.prop == "class" && p.ok == Some(true))
            }) || f.class.is_empty()
        })
        .map(|(f, _)| f.class.clone())
        .collect();

    let mut out = Vec::new();
    for r in &rules.rules {
        let Some(m) = r.matcher("class") else { continue };
        if m.negative || m.regex_error().is_some() || catch_all(&m.raw) {
            continue;
        }
        let matched_any = reports.iter().any(|(_, rep)| {
            rep.verdict(r.idx)
                .is_some_and(|v| v.predicates_now.iter().any(|p| p.prop == "class" && p.ok == Some(true)))
        });
        if matched_any {
            continue;
        }
        let literals = literal_chunks(&m.raw);
        let mut seen = BTreeSet::new();
        for (facts, _) in reports {
            if known.contains(&facts.class) || !seen.insert(facts.class.clone()) {
                continue;
            }
            let class = facts.class.to_lowercase();
            let best = literals.iter().map(|l| (lcs_len(l, &class), l)).max_by_key(|(n, _)| *n);
            if let Some((n, lit)) = best {
                if n >= 5 && n * 100 / class.len().max(1) >= 40 {
                    out.push(Finding {
                        severity: Severity::Warn,
                        rule_idx: Some(r.idx),
                        location: r.loc.short(),
                        message: format!(
                            "near miss: class regex `{}` matches no open window, but \"{}\" is close to \"{}\" and no other rule claims it; the app may have renamed its class",
                            m.raw, facts.class, lit
                        ),
                        hint: Some(format!("hyprscope snippet {}  prints a rule for it", facts.class)),
                    });
                }
            }
        }
    }
    out
}

/// Length of the longest common substring.
fn lcs_len(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev = vec![0usize; b.len() + 1];
    let mut best = 0;
    for i in 1..=a.len() {
        let mut cur = vec![0usize; b.len() + 1];
        for j in 1..=b.len() {
            if a[i - 1] == b[j - 1] {
                cur[j] = prev[j - 1] + 1;
                best = best.max(cur[j]);
            }
        }
        prev = cur;
    }
    best
}

/// Lower-cased runs of letters/digits/./-/_ of at least 4 chars from a regex,
/// ignoring anything inside character classes or groups with alternation.
fn literal_chunks(re: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut depth_bracket = 0;
    let mut prev_backslash = false;
    for ch in re.chars() {
        if prev_backslash {
            // Escaped literal like \. keeps the character.
            if ch == '.' || ch == '-' || ch == '_' {
                cur.push(ch);
            } else {
                flush(&mut cur, &mut chunks);
            }
            prev_backslash = false;
            continue;
        }
        match ch {
            '\\' => prev_backslash = true,
            '[' => {
                depth_bracket += 1;
                flush(&mut cur, &mut chunks);
            }
            ']' => depth_bracket = (depth_bracket - 1).max(0),
            _ if depth_bracket > 0 => {}
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => cur.push(c.to_ascii_lowercase()),
            _ => flush(&mut cur, &mut chunks),
        }
    }
    flush(&mut cur, &mut chunks);
    chunks
}

fn flush(cur: &mut String, chunks: &mut Vec<String>) {
    if cur.len() >= 4 {
        chunks.push(cur.clone());
    }
    cur.clear();
}

/// Checks that need the live window list.
pub fn lint_live(rules: &RuleSet, reports: &[(Facts, Report)]) -> Vec<Finding> {
    let mut out = Vec::new();
    if reports.is_empty() {
        return out;
    }
    out.extend(near_misses(rules, reports));
    let mut matched: BTreeMap<usize, usize> = BTreeMap::new();
    let mut won: BTreeMap<usize, usize> = BTreeMap::new();
    for (_, rep) in reports {
        for v in &rep.verdicts {
            if v.is_active() {
                *matched.entry(v.rule_idx).or_default() += 1;
                if v.wins_any() {
                    *won.entry(v.rule_idx).or_default() += 1;
                }
            }
        }
    }
    for r in &rules.rules {
        if !r.can_match() {
            continue;
        }
        let m = matched.get(&r.idx).copied().unwrap_or(0);
        let w = won.get(&r.idx).copied().unwrap_or(0);
        if m == 0 {
            out.push(Finding {
                severity: Severity::Info,
                rule_idx: Some(r.idx),
                location: r.loc.short(),
                message: format!("matches none of the {} open windows", reports.len()),
                hint: None,
            });
        } else if w == 0 && !r.effects.is_empty() {
            out.push(Finding {
                severity: Severity::Warn,
                rule_idx: Some(r.idx),
                location: r.loc.short(),
                message: format!("matches {m} window(s) but every effect is overridden by a later rule on all of them"),
                hint: None,
            });
        }
    }
    for (facts, rep) in reports {
        for v in &rep.verdicts {
            if v.matched_now && !v.matched_open {
                let inert: Vec<&str> = v.effects.iter().filter(|e| e.inert).map(|e| e.key.as_str()).collect();
                if !inert.is_empty() {
                    let r = &rules.rules[v.rule_idx];
                    out.push(Finding {
                        severity: Severity::Warn,
                        rule_idx: Some(r.idx),
                        location: r.loc.short(),
                        message: format!(
                            "{} \"{}\" matches this rule now but did not when it opened (as {} \"{}\"), so static {} never applied",
                            facts.class,
                            facts.title,
                            facts.initial_class,
                            facts.initial_title,
                            inert.join(", ")
                        ),
                        hint: Some("static effects only see initial_class/initial_title; react to title changes with hl.on(\"window.title\", …)".into()),
                    });
                }
            }
        }
        if !rep.tags_agree() {
            out.push(Finding {
                severity: Severity::Warn,
                rule_idx: None,
                location: facts.address.clone(),
                message: format!(
                    "{}: hyprscope computed tags [{}] but Hyprland reports [{}]",
                    facts.class,
                    rep.computed_tags.iter().cloned().collect::<Vec<_>>().join(" "),
                    rep.compositor_tags.iter().cloned().collect::<Vec<_>>().join(" ")
                ),
                hint: Some("a static tag from a dispatcher, a plugin, or a rule hyprscope does not model".into()),
            });
        }
    }
    out.sort_by_key(|f| (f.severity, f.rule_idx));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_chunks_skip_classes_and_metachars() {
        let l = literal_chunks("((google-)?[cC]hrom(e|ium)|[bB]rave-browser|Vivaldi-stable)");
        assert!(l.contains(&"google-".to_string()));
        assert!(l.contains(&"rave-browser".to_string()));
        assert!(l.contains(&"vivaldi-stable".to_string()));
        assert!(!l.iter().any(|c| c.contains('[')));
    }

    #[test]
    fn lcs_finds_shared_run() {
        assert_eq!(lcs_len("rave-browser", "brave-origin"), 5);
        assert_eq!(lcs_len("omarchy", "org.omarchy.agent"), 7);
    }
}
