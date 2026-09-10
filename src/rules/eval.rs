//! Evaluate a rule set against one window the way Hyprland does:
//! every rule in order, all predicates must hold, tag effects feed later
//! rules, and for each effect the last matching rule wins.

use std::collections::{BTreeMap, BTreeSet};

use super::effects::EffectKind;
use super::model::{Engine, Matcher, Rule, RuleSet};
use super::workspace::{self, WsFacts};
use crate::hypr::Client;

/// Which snapshot of the window the predicates see.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// When the window opened: class and title are the initial ones. This is
    /// what static effects (float, workspace, …) were decided on.
    Open,
    /// Right now: current class and title. This is what dynamic effects
    /// (opacity, tag, border_color, …) are re-evaluated on.
    Now,
}

/// Everything the matcher needs to know about a window.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub address: String,
    pub class: String,
    pub title: String,
    pub initial_class: String,
    pub initial_title: String,
    pub floating: bool,
    pub xwayland: bool,
    pub fullscreen: bool,
    pub fs_internal: i64,
    pub fs_client: i64,
    pub pinned: bool,
    pub focused: bool,
    pub grouped: bool,
    /// Not exposed by hyprctl; None means "unknown, assume false".
    pub modal: Option<bool>,
    pub content: String,
    pub xdg_tag: Option<String>,
    pub workspace: WsFacts,
    /// Tags as the compositor reports them (dynamic ones end in `*`).
    pub compositor_tags: Vec<String>,
}

impl Facts {
    pub fn from_client(c: &Client, focused: Option<&str>, workspaces: &[crate::hypr::Workspace], monitors: &[crate::hypr::Monitor]) -> Self {
        let ws = workspaces.iter().find(|w| w.id == c.workspace.id);
        let monitor_name = ws
            .map(|w| w.monitor.clone())
            .or_else(|| monitors.iter().find(|m| m.id == c.monitor).map(|m| m.name.clone()))
            .unwrap_or_default();
        Facts {
            address: c.address.clone(),
            class: c.class.clone(),
            title: c.title.clone(),
            initial_class: c.initial_class.clone(),
            initial_title: c.initial_title.clone(),
            floating: c.floating,
            xwayland: c.xwayland,
            fullscreen: c.is_fullscreen(),
            fs_internal: c.fullscreen as i64,
            fs_client: c.fullscreen_client as i64,
            pinned: c.pinned,
            focused: focused == Some(c.address.as_str()),
            grouped: c.is_grouped(),
            modal: None,
            content: c.content_type.clone(),
            xdg_tag: if c.xdg_tag.is_empty() { None } else { Some(c.xdg_tag.clone()) },
            workspace: WsFacts {
                id: c.workspace.id,
                name: c.workspace.name.clone(),
                monitor: monitor_name,
                windows: ws.map(|w| w.windows).unwrap_or(0),
                has_fullscreen: ws.map(|w| w.hasfullscreen).unwrap_or(false),
            },
            compositor_tags: c.tags.clone(),
        }
    }

    /// Static tags are the ones without the dynamic `*` suffix.
    pub fn static_tags(&self) -> BTreeSet<String> {
        self.compositor_tags.iter().filter(|t| !t.ends_with('*')).cloned().collect()
    }

    pub fn compositor_dynamic_tags(&self) -> BTreeSet<String> {
        self.compositor_tags.iter().filter(|t| t.ends_with('*')).cloned().collect()
    }
}

/// Outcome of one predicate.
#[derive(Clone, Debug)]
pub struct PredicateResult {
    pub prop: String,
    pub expected: String,
    pub actual: String,
    /// None when hyprscope cannot decide (bad regex, unsupported selector).
    pub ok: Option<bool>,
    pub note: Option<String>,
}

/// Outcome of one effect within a matched rule.
#[derive(Clone, Debug)]
pub struct EffectResult {
    pub key: String,
    pub value: String,
    pub kind: EffectKind,
    /// True when this rule is the last matching rule setting the key.
    pub winner: bool,
    /// Index of the rule that overrides this one, when not the winner.
    pub overridden_by: Option<usize>,
    /// Static effect on a rule that no longer matched at open time.
    pub inert: bool,
}

/// One rule's verdict for one window.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub rule_idx: usize,
    pub matched_open: bool,
    pub matched_now: bool,
    pub predicates_now: Vec<PredicateResult>,
    pub predicates_open: Vec<PredicateResult>,
    pub effects: Vec<EffectResult>,
}

impl Verdict {
    /// Whether the rule contributes anything to the window at all.
    pub fn is_active(&self) -> bool {
        self.matched_open || self.matched_now
    }
    pub fn wins_any(&self) -> bool {
        self.effects.iter().any(|e| e.winner)
    }
}

/// The full picture for one window.
#[derive(Clone, Debug, Default)]
pub struct Report {
    pub verdicts: Vec<Verdict>,
    /// Effective value of each effect key and the rule that set it.
    pub effective: BTreeMap<String, (String, usize)>,
    /// Dynamic tags hyprscope computed.
    pub computed_tags: BTreeSet<String>,
    /// Dynamic tags the compositor reports; differs from computed_tags when
    /// hyprscope's model and Hyprland disagree.
    pub compositor_tags: BTreeSet<String>,
}

impl Report {
    pub fn tags_agree(&self) -> bool {
        self.computed_tags == self.compositor_tags
    }
    pub fn verdict(&self, idx: usize) -> Option<&Verdict> {
        self.verdicts.iter().find(|v| v.rule_idx == idx)
    }
}

/// Evaluate all rules for one window.
pub fn evaluate(rules: &RuleSet, facts: &Facts) -> Report {
    let (now_matches, now_tags, now_preds) = run_phase(&rules.rules, facts, Phase::Now);
    let (open_matches, _open_tags, open_preds) = run_phase(&rules.rules, facts, Phase::Open);

    // Effective values: static from the open pass, dynamic from the now pass.
    let mut effective: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for rule in &rules.rules {
        for e in &rule.effects {
            if e.key == "tag" {
                continue;
            }
            let applies = match e.kind {
                EffectKind::Static => open_matches[rule.idx],
                EffectKind::Dynamic => now_matches[rule.idx],
                EffectKind::Unknown => false,
            };
            if applies {
                effective.insert(e.key.clone(), (e.hypr_string(), rule.idx));
            }
        }
    }

    let mut verdicts = Vec::with_capacity(rules.rules.len());
    for rule in &rules.rules {
        let effects = rule
            .effects
            .iter()
            .map(|e| {
                let (applies, inert) = match e.kind {
                    EffectKind::Static => (open_matches[rule.idx], !open_matches[rule.idx] && now_matches[rule.idx]),
                    EffectKind::Dynamic => (now_matches[rule.idx], false),
                    EffectKind::Unknown => (false, false),
                };
                let winner = e.key == "tag" && applies || effective.get(&e.key).is_some_and(|(_, idx)| *idx == rule.idx);
                let overridden_by = if applies && !winner && e.key != "tag" {
                    effective.get(&e.key).map(|(_, i)| *i)
                } else {
                    None
                };
                EffectResult {
                    key: e.key.clone(),
                    value: e.value.display(),
                    kind: e.kind,
                    winner,
                    overridden_by,
                    inert,
                }
            })
            .collect();
        verdicts.push(Verdict {
            rule_idx: rule.idx,
            matched_open: open_matches[rule.idx],
            matched_now: now_matches[rule.idx],
            predicates_now: now_preds[rule.idx].clone(),
            predicates_open: open_preds[rule.idx].clone(),
            effects,
        });
    }

    Report {
        verdicts,
        effective,
        computed_tags: now_tags,
        compositor_tags: facts.compositor_dynamic_tags(),
    }
}

/// Run the ordered rule pass with tag propagation until the tag set is stable.
fn run_phase(rules: &[Rule], facts: &Facts, phase: Phase) -> (Vec<bool>, BTreeSet<String>, Vec<Vec<PredicateResult>>) {
    let static_tags = facts.static_tags();
    let mut dynamic: BTreeSet<String> = BTreeSet::new();
    let mut matched = vec![false; rules.len()];
    let mut preds: Vec<Vec<PredicateResult>> = vec![Vec::new(); rules.len()];

    for _round in 0..8 {
        let before = dynamic.clone();
        for rule in rules {
            let tags: BTreeSet<String> = static_tags.iter().chain(dynamic.iter()).cloned().collect();
            let (ok, results) = matches(rule, facts, phase, &tags);
            matched[rule.idx] = ok;
            preds[rule.idx] = results;
            if ok {
                if let Some(t) = rule.tag_effect() {
                    apply_tag(&mut dynamic, t);
                }
            }
        }
        if before == dynamic {
            break;
        }
    }
    (matched, dynamic, preds)
}

/// Hyprland's CTagKeeper::applyTag with dynamic=true.
pub fn apply_tag(tags: &mut BTreeSet<String>, spec: &str) {
    let mut real = spec.to_string();
    if !real.ends_with('*') {
        real.push('*');
    }
    if let Some(rest) = real.strip_prefix('-') {
        tags.remove(rest);
    } else if let Some(rest) = real.strip_prefix('+') {
        tags.insert(rest.to_string());
    } else if tags.contains(&real) {
        tags.remove(&real);
    } else {
        tags.insert(real);
    }
}

/// CTagKeeper::isTagged, non-strict: `foo` matches `foo` and `foo*`.
pub fn is_tagged(tags: &BTreeSet<String>, m: &Matcher) -> bool {
    let name = m.tag_name();
    let hit = tags.contains(name) || tags.contains(&format!("{name}*"));
    hit != m.negative
}

fn matches(rule: &Rule, facts: &Facts, phase: Phase, tags: &BTreeSet<String>) -> (bool, Vec<PredicateResult>) {
    if !rule.can_match() {
        return (false, Vec::new());
    }
    let mut all = true;
    let mut out = Vec::with_capacity(rule.matchers.len());
    for m in &rule.matchers {
        let r = predicate(m, facts, phase, tags);
        if r.ok != Some(true) {
            all = false;
        }
        out.push(r);
    }
    (all, out)
}

fn predicate(m: &Matcher, facts: &Facts, phase: Phase, tags: &BTreeSet<String>) -> PredicateResult {
    let mut note = None;
    let (actual, ok): (String, Option<bool>) = match (m.prop.as_str(), m.engine) {
        ("class", _) => {
            let v = if phase == Phase::Open { &facts.initial_class } else { &facts.class };
            (v.clone(), m.match_str(v))
        }
        ("title", _) => {
            let v = if phase == Phase::Open { &facts.initial_title } else { &facts.title };
            (v.clone(), m.match_str(v))
        }
        ("initial_class", _) => (facts.initial_class.clone(), m.match_str(&facts.initial_class)),
        ("initial_title", _) => (facts.initial_title.clone(), m.match_str(&facts.initial_title)),
        ("content", _) => {
            let num = content_number(&facts.content).to_string();
            let hit = m.match_str(&num).unwrap_or(false) || m.match_str(&facts.content).unwrap_or(false);
            (facts.content.clone(), if m.regex_error().is_some() { None } else { Some(hit) })
        }
        ("xdg_tag", _) => match &facts.xdg_tag {
            Some(t) => (t.clone(), m.match_str(t)),
            None => ("(none)".into(), Some(false)),
        },
        ("float", _) => (facts.floating.to_string(), Some(facts.floating == m.boolean.unwrap_or(false))),
        ("xwayland", _) => (facts.xwayland.to_string(), Some(facts.xwayland == m.boolean.unwrap_or(false))),
        ("fullscreen", _) => (facts.fullscreen.to_string(), Some(facts.fullscreen == m.boolean.unwrap_or(false))),
        ("pin", _) => (facts.pinned.to_string(), Some(facts.pinned == m.boolean.unwrap_or(false))),
        ("focus", _) => (facts.focused.to_string(), Some(facts.focused == m.boolean.unwrap_or(false))),
        ("group", _) => (facts.grouped.to_string(), Some(facts.grouped == m.boolean.unwrap_or(false))),
        ("modal", _) => {
            let v = facts.modal.unwrap_or(false);
            if facts.modal.is_none() {
                note = Some("hyprctl does not expose modal; assumed false".into());
            }
            (v.to_string(), Some(v == m.boolean.unwrap_or(false)))
        }
        ("fullscreen_state_internal", _) => (facts.fs_internal.to_string(), Some(m.int == Some(facts.fs_internal))),
        ("fullscreen_state_client", _) => (facts.fs_client.to_string(), Some(m.int == Some(facts.fs_client))),
        ("workspace", _) => {
            let r = workspace::matches(&m.raw, &facts.workspace);
            if r.is_none() {
                note = Some("selector form not modelled by hyprscope".into());
            }
            (format!("{} ({})", facts.workspace.name, facts.workspace.id), r)
        }
        ("tag", _) => {
            let mut shown: Vec<&str> = tags.iter().map(|s| s.as_str()).collect();
            if shown.is_empty() {
                shown.push("(no tags)");
            }
            (shown.join(" "), Some(is_tagged(tags, m)))
        }
        (_, None) => ("?".into(), None),
        (_, Some(Engine::Regex)) => ("?".into(), None),
        _ => ("?".into(), None),
    };
    if let Some(e) = m.regex_error() {
        note = Some(format!("regex error: {e}"));
    }
    PredicateResult {
        prop: m.prop.clone(),
        expected: m.summary_value(),
        actual,
        ok,
        note,
    }
}

fn content_number(name: &str) -> i64 {
    match name {
        "none" => 0,
        "photo" => 1,
        "video" => 2,
        "game" => 3,
        _ => 0,
    }
}

/// Evaluate an ad-hoc match expression against a window (Now phase, compositor tags).
pub fn expr_matches(matchers: &[Matcher], facts: &Facts) -> bool {
    let tags: BTreeSet<String> = facts.compositor_tags.iter().cloned().collect();
    matchers.iter().all(|m| predicate(m, facts, Phase::Now, &tags).ok == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_apply_semantics() {
        let mut t = BTreeSet::new();
        apply_tag(&mut t, "+term");
        assert!(t.contains("term*"));
        apply_tag(&mut t, "term");
        assert!(!t.contains("term*"));
        apply_tag(&mut t, "term");
        assert!(t.contains("term*"));
        apply_tag(&mut t, "-term");
        assert!(t.is_empty());
    }

    #[test]
    fn tag_match_nonstrict() {
        let mut t = BTreeSet::new();
        t.insert("code*".to_string());
        assert!(is_tagged(&t, &Matcher::new("tag", "code")));
        assert!(is_tagged(&t, &Matcher::new("tag", "code*")));
        assert!(!is_tagged(&t, &Matcher::new("tag", "negative:code")));
        t.clear();
        t.insert("code".to_string());
        assert!(is_tagged(&t, &Matcher::new("tag", "code")));
        assert!(!is_tagged(&t, &Matcher::new("tag", "code*")));
    }

    #[test]
    fn regex_is_full_match() {
        let m = Matcher::new("class", "fire");
        assert_eq!(m.match_str("fire"), Some(true));
        assert_eq!(m.match_str("firefox"), Some(false));
        let n = Matcher::new("class", "negative:firefox");
        assert_eq!(n.match_str("firefox"), Some(false));
        assert_eq!(n.match_str("chromium"), Some(true));
        let e = Matcher::new("class", "");
        assert_eq!(e.match_str(""), Some(true));
        assert_eq!(e.match_str("x"), Some(false));
    }
}
