mod config;
mod hypr;
mod lint;
mod out;
mod rules;
mod snippet;
mod tui;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

use hypr::{Client, Hypr};
use rules::model::Effect;
use rules::{evaluate, Facts, Report, RuleSet};

/// See why every Hyprland window landed where it did, and which rule did it.
#[derive(Parser, Debug)]
#[command(name = "hyprscope", version, about, long_about = None)]
struct Cli {
    /// Path to hyprland.lua (default: $XDG_CONFIG_HOME/hypr/hyprland.lua)
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Live TUI: windows on the left, rule verdicts on the right (default)
    Tui {
        /// Start in advanced mode (every rule, predicates, phases, overrides)
        #[arg(long)]
        advanced: bool,
    },
    /// Explain which rules apply to a window and why
    Why(WhyArgs),
    /// List the windows a match expression selects
    Match {
        /// e.g. `class:^kitty$ float:true` or just `kitty`
        expr: String,
        #[arg(long)]
        json: bool,
    },
    /// List every window rule the config registers, with source locations
    Rules {
        #[arg(long)]
        json: bool,
    },
    /// Check the config for mistakes, plus live checks against open windows
    Lint {
        #[arg(long)]
        json: bool,
        /// Skip checks that need a running compositor
        #[arg(long)]
        no_live: bool,
        /// List every rule that matches no open window (otherwise summarised)
        #[arg(long)]
        unused: bool,
    },
    /// Stream compositor events and explain each new window as it opens
    Watch {
        #[arg(long)]
        json: bool,
    },
    /// Print a ready-to-paste rule for a window (focused by default)
    Snippet {
        /// Window address (0x…) or a class regex/substring
        target: Option<String>,
        /// Effect to set, repeatable: --set float=true --set workspace=3
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Also match the (initial) title
        #[arg(long)]
        title: bool,
        /// Copy to the clipboard with wl-copy as well as printing
        #[arg(long)]
        copy: bool,
    },
}

#[derive(Args, Debug)]
struct WhyArgs {
    /// Window address (0x…) or a class regex; defaults to the focused window
    target: Option<String>,
    /// Show every rule, not only the ones that match
    #[arg(long, short)]
    all: bool,
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Let `hyprscope … | head` end quietly instead of panicking on EPIPE.
    unsafe {
        extern "C" {
            fn signal(sig: i32, handler: usize) -> usize;
        }
        signal(13, 0);
    }
    let cli = Cli::parse();
    match cli.cmd.unwrap_or(Cmd::Tui { advanced: false }) {
        Cmd::Tui { advanced } => tui::run(cli.config, if advanced { tui::Mode::Advanced } else { tui::Mode::Easy }).await,
        Cmd::Why(a) => cmd_why(cli.config, a).await,
        Cmd::Match { expr, json } => cmd_match(cli.config, &expr, json).await,
        Cmd::Rules { json } => cmd_rules(cli.config, json).await,
        Cmd::Lint { json, no_live, unused } => cmd_lint(cli.config, json, no_live, unused).await,
        Cmd::Watch { json } => cmd_watch(cli.config, json).await,
        Cmd::Snippet { target, set, title, copy } => cmd_snippet(cli.config, target, set, title, copy).await,
    }
}

/// Load the config, preferring the compositor's version string for hl.version().
pub async fn load_rules(config: Option<PathBuf>, hypr: Option<&Hypr>) -> Result<RuleSet> {
    let path = match config {
        Some(p) => p,
        None => config::default_config_path()?,
    };
    let version = match hypr {
        Some(h) => h.version().await.ok(),
        None => None,
    };
    let cap = config::load(&path, version.as_deref())?;
    Ok(RuleSet::from_raw(&cap.window_rules, cap.config_path, cap.errors))
}

/// Everything needed to evaluate windows: clients, workspaces, monitors, focus.
pub struct Snapshot {
    pub clients: Vec<Client>,
    pub workspaces: Vec<hypr::Workspace>,
    pub monitors: Vec<hypr::Monitor>,
    pub focused: Option<String>,
}

impl Snapshot {
    pub async fn take(h: &Hypr) -> Result<Self> {
        let (clients, workspaces, monitors, focused) = tokio::try_join!(h.clients(), h.workspaces(), h.monitors(), h.active_window())?;
        let clients = clients.into_iter().filter(|c| c.mapped).collect();
        Ok(Snapshot {
            clients,
            workspaces,
            monitors,
            focused,
        })
    }

    pub fn facts(&self, c: &Client) -> Facts {
        Facts::from_client(c, self.focused.as_deref(), &self.workspaces, &self.monitors)
    }
}

/// Resolve a `why`/`snippet` target: focused window, address, or class.
fn resolve_targets<'a>(snap: &'a Snapshot, target: &Option<String>) -> Result<Vec<&'a Client>> {
    let targets: Vec<&Client> = match target {
        None => {
            let f = snap.focused.clone().ok_or_else(|| anyhow!("no focused window; pass an address or class"))?;
            snap.clients.iter().filter(|c| c.address == f).collect()
        }
        Some(t) if t.starts_with("0x") => {
            let full = normalize_addr(t);
            snap.clients.iter().filter(|c| c.address == full).collect()
        }
        Some(t) => {
            // Full-match regex first (Hyprland semantics), then a forgiving
            // case-insensitive substring search over class and title.
            let re = regex::Regex::new(&format!("^(?:{t})$")).with_context(|| format!("bad class regex {t}"))?;
            let strict: Vec<&Client> = snap.clients.iter().filter(|c| re.is_match(&c.class) || re.is_match(&c.initial_class)).collect();
            if strict.is_empty() {
                let needle = t.to_lowercase();
                snap.clients
                    .iter()
                    .filter(|c| c.class.to_lowercase().contains(&needle) || c.title.to_lowercase().contains(&needle))
                    .collect()
            } else {
                strict
            }
        }
    };
    if targets.is_empty() {
        bail!("no window matches {:?}", target.clone().unwrap_or_else(|| "focused".into()));
    }
    Ok(targets)
}

async fn cmd_why(config: Option<PathBuf>, a: WhyArgs) -> Result<()> {
    let h = Hypr::from_env()?;
    let rules = load_rules(config, Some(&h)).await?;
    let snap = Snapshot::take(&h).await?;
    let targets = resolve_targets(&snap, &a.target)?;

    if a.json {
        let mut all = Vec::new();
        for c in targets {
            let facts = snap.facts(c);
            let rep = evaluate(&rules, &facts);
            all.push(report_json(&rules, c, &rep, a.all));
        }
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(());
    }

    for (i, c) in targets.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let facts = snap.facts(c);
        let rep = evaluate(&rules, &facts);
        print_why(&rules, c, &facts, &rep, a.all);
    }
    Ok(())
}

fn normalize_addr(s: &str) -> String {
    // hyprctl prints the short hex form; accept either.
    let hex = s.trim_start_matches("0x").trim_start_matches('0');
    format!("0x{hex}")
}

fn print_why(rules: &RuleSet, c: &Client, facts: &Facts, rep: &Report, all: bool) {
    use out::*;
    let flags = window_flags(c);
    println!(
        "{} {} {}  {}  ws {}{}",
        bold(&c.address),
        bold(&c.class),
        dim(&format!("\"{}\"", c.title)),
        if flags.is_empty() { String::new() } else { dim(&format!("[{flags}]")) },
        c.workspace.name,
        if c.initial_class != c.class || c.initial_title != c.title {
            dim(&format!("  (opened as {} \"{}\")", c.initial_class, c.initial_title))
        } else {
            String::new()
        }
    );
    let tags: Vec<&str> = c.tags.iter().map(|s| s.as_str()).collect();
    print!("  tags: {}", if tags.is_empty() { dim("(none)") } else { cyan(&tags.join(" ")) });
    if rep.tags_agree() {
        println!("  {}", green("✓ model agrees with Hyprland"));
    } else {
        println!(
            "  {}",
            yellow(&format!("model computed [{}]", rep.computed_tags.iter().cloned().collect::<Vec<_>>().join(" ")))
        );
    }

    println!();
    println!("  {}", bold("effective"));
    if rep.effective.is_empty() {
        println!("    {}", dim("nothing: no rule matches this window"));
    }
    for (key, (val, idx)) in &rep.effective {
        let r = &rules.rules[*idx];
        println!("    {:<20} {:<24} {}", key, val, dim(&format!("{} {}", r.display_name(), r.loc.short())));
    }

    println!();
    println!(
        "  {}   {}",
        bold("rules"),
        dim("O = matched at open (static effects), N = matches now (dynamic effects), ★ = wins")
    );
    let mut shown = 0;
    for v in &rep.verdicts {
        let r = &rules.rules[v.rule_idx];
        if !v.is_active() && !all {
            continue;
        }
        shown += 1;
        let o = if v.matched_open { green("O") } else { dim("·") };
        let n = if v.matched_now { green("N") } else { dim("·") };
        let name = if v.is_active() { bold(&r.display_name()) } else { dim(&r.display_name()) };
        println!("  {o}{n} {name:<10} {}  {}", r.loc.short(), dim(&r.match_summary()));
        if v.is_active() {
            for e in &v.effects {
                let kind = match e.kind {
                    rules::effects::EffectKind::Static => dim("static "),
                    rules::effects::EffectKind::Dynamic => dim("dynamic"),
                    rules::effects::EffectKind::Unknown => red("unknown"),
                };
                let status = if e.inert {
                    yellow("inert: static, but rule only matches now")
                } else if e.winner {
                    yellow("★")
                } else if let Some(by) = e.overridden_by {
                    dim(&format!("overridden by {} {}", rules.rules[by].display_name(), rules.rules[by].loc.short()))
                } else {
                    String::new()
                };
                println!("        {kind} {:<18} {:<24} {status}", e.key, e.value);
            }
        } else {
            let preds: Vec<String> = v
                .predicates_now
                .iter()
                .filter(|p| p.ok != Some(true))
                .map(|p| format!("{} {} {}", tick(p.ok), p.prop, dim(&format!("actual {}", short(&p.actual)))))
                .collect();
            if !preds.is_empty() {
                println!("        {}", preds.join("  "));
            }
        }
    }
    if shown == 0 {
        println!("    {}", dim("no rule matches; run with --all to see every rule"));
    }
    let _ = facts;
}

fn short(s: &str) -> String {
    if s.chars().count() > 40 {
        format!("{}…", s.chars().take(39).collect::<String>())
    } else {
        s.to_string()
    }
}

pub fn window_flags(c: &Client) -> String {
    let mut f = String::new();
    if c.floating {
        f.push_str("float ");
    }
    if c.xwayland {
        f.push_str("xwayland ");
    }
    if c.pinned {
        f.push_str("pinned ");
    }
    if c.is_fullscreen() {
        f.push_str("fullscreen ");
    }
    if c.is_grouped() {
        f.push_str("grouped ");
    }
    f.trim_end().to_string()
}

fn report_json(rules: &RuleSet, c: &Client, rep: &Report, all: bool) -> serde_json::Value {
    use serde_json::json;
    let verdicts: Vec<_> = rep
        .verdicts
        .iter()
        .filter(|v| all || v.is_active())
        .map(|v| {
            let r = &rules.rules[v.rule_idx];
            json!({
                "rule": r.idx + 1,
                "name": r.name,
                "location": r.loc.short(),
                "match": r.matchers.iter().map(|m| json!({"prop": m.prop, "value": m.raw})).collect::<Vec<_>>(),
                "matched_open": v.matched_open,
                "matched_now": v.matched_now,
                "predicates": v.predicates_now.iter().map(|p| json!({"prop": p.prop, "expected": p.expected, "actual": p.actual, "ok": p.ok, "note": p.note})).collect::<Vec<_>>(),
                "effects": v.effects.iter().map(|e| json!({"key": e.key, "value": e.value, "kind": format!("{:?}", e.kind).to_lowercase(), "winner": e.winner, "overridden_by": e.overridden_by.map(|i| i + 1), "inert": e.inert})).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "address": c.address,
        "class": c.class,
        "title": c.title,
        "initial_class": c.initial_class,
        "initial_title": c.initial_title,
        "workspace": c.workspace.name,
        "tags": c.tags,
        "computed_tags": rep.computed_tags,
        "tags_agree": rep.tags_agree(),
        "effective": rep.effective.iter().map(|(k, (v, i))| json!({"key": k, "value": v, "rule": i + 1, "location": rules.rules[*i].loc.short()})).collect::<Vec<_>>(),
        "rules": verdicts,
    })
}

async fn cmd_match(config: Option<PathBuf>, expr: &str, json: bool) -> Result<()> {
    let _ = config;
    let h = Hypr::from_env()?;
    let matchers = rules::expr::parse(expr)?;
    let snap = Snapshot::take(&h).await?;
    let hits: Vec<&Client> = snap.clients.iter().filter(|c| rules::eval::expr_matches(&matchers, &snap.facts(c))).collect();
    if json {
        let v: Vec<_> = hits.iter().map(|c| serde_json::json!({"address": c.address, "class": c.class, "title": c.title, "workspace": c.workspace.name, "floating": c.floating, "tags": c.tags})).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    if hits.is_empty() {
        println!(
            "{}",
            out::dim(&format!("no open window matches `{expr}` ({} windows checked)", snap.clients.len()))
        );
        return Ok(());
    }
    for c in &hits {
        println!(
            "{} {:<22} {:<40} ws {:<4} {}",
            out::cyan(&c.address),
            c.class,
            short(&c.title),
            c.workspace.name,
            out::dim(&window_flags(c))
        );
    }
    println!("{}", out::dim(&format!("{} of {} windows", hits.len(), snap.clients.len())));
    Ok(())
}

async fn cmd_rules(config: Option<PathBuf>, json: bool) -> Result<()> {
    let h = Hypr::from_env().ok();
    let rules = load_rules(config, h.as_ref()).await?;
    if json {
        let v: Vec<_> = rules
            .rules
            .iter()
            .map(|r| {
                serde_json::json!({
                    "rule": r.idx + 1, "name": r.name, "enabled": r.enabled, "location": r.loc.short(),
                    "via": r.via.iter().map(|l| l.short()).collect::<Vec<_>>(),
                    "match": r.matchers.iter().map(|m| serde_json::json!({"prop": m.prop, "value": m.raw})).collect::<Vec<_>>(),
                    "effects": r.effects.iter().map(|e| serde_json::json!({"key": e.key, "value": e.value.to_hypr_string(), "kind": format!("{:?}", e.kind).to_lowercase()})).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    println!("{} {}", out::dim("config"), rules.config_path.display());
    for e in &rules.load_errors {
        println!("{} {e}", out::red("load error:"));
    }
    let mut last_file = String::new();
    for r in &rules.rules {
        let file = r.loc.file.display().to_string();
        if file != last_file {
            println!(
                "\n{}",
                out::bold(&r.loc.short().rsplit_once(':').map(|(f, _)| f.to_string()).unwrap_or(file.clone()))
            );
            last_file = file;
        }
        let statics = r.effects.iter().filter(|e| e.kind == rules::effects::EffectKind::Static).count();
        let kind = if statics > 0 && statics < r.effects.len() {
            "S+D"
        } else if statics > 0 {
            "S  "
        } else {
            "D  "
        };
        println!(
            "  {:>4}  {:<5} {}  {}  →  {}",
            out::dim(&format!(":{}", r.loc.line)),
            out::dim(kind),
            out::bold(&r.display_name()),
            r.match_summary(),
            out::cyan(&r.effect_summary())
        );
        for p in &r.problems {
            println!("        {} {p}", out::red("!"));
        }
    }
    println!(
        "\n{}",
        out::dim(&format!(
            "{} rules  ·  S = static effects (decided at open)  D = dynamic effects (live)",
            rules.rules.len()
        ))
    );
    Ok(())
}

async fn cmd_lint(config: Option<PathBuf>, json: bool, no_live: bool, unused: bool) -> Result<()> {
    let h = if no_live { None } else { Hypr::from_env().ok() };
    let rules = load_rules(config, h.as_ref()).await?;
    let mut findings = lint::lint_static(&rules);
    let mut live_note = None;
    if let Some(h) = &h {
        match Snapshot::take(h).await {
            Ok(snap) => {
                let reports: Vec<(Facts, Report)> = snap
                    .clients
                    .iter()
                    .map(|c| {
                        let f = snap.facts(c);
                        let r = evaluate(&rules, &f);
                        (f, r)
                    })
                    .collect();
                findings.extend(lint::lint_live(&rules, &reports));
                if let Ok(v) = h.version().await {
                    findings.extend(lint::lint_advisories(&rules, &v));
                }
                if let Ok(errs) = h.config_errors().await {
                    for e in errs {
                        findings.push(lint::Finding {
                            severity: lint::Severity::Error,
                            rule_idx: None,
                            location: "hyprctl configerrors".into(),
                            message: e,
                            hint: None,
                        });
                    }
                }
                live_note = Some(snap.clients.len());
            }
            Err(e) => {
                live_note = {
                    eprintln!("{} live checks skipped: {e}", out::yellow("note:"));
                    None
                }
            }
        }
    }
    findings.sort_by_key(|f| (f.severity, f.rule_idx));
    let unused_count = findings.iter().filter(|f| f.message.starts_with("matches none of")).count();
    if !unused && !json {
        findings.retain(|f| !f.message.starts_with("matches none of"));
    }

    if json {
        let v: Vec<_> = findings.iter().map(|f| serde_json::json!({"severity": f.severity.label(), "rule": f.rule_idx.map(|i| i + 1), "location": f.location, "message": f.message, "hint": f.hint})).collect();
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        for f in &findings {
            let sev = match f.severity {
                lint::Severity::Error => out::red("error"),
                lint::Severity::Warn => out::yellow("warn "),
                lint::Severity::Info => out::dim("info "),
            };
            let rule = f.rule_idx.map(|i| format!(" {}", rules.rules[i].display_name())).unwrap_or_default();
            println!("{sev} {}{}  {}", out::bold(&f.location), out::dim(&rule), f.message);
            if let Some(h) = &f.hint {
                println!("      {}", out::dim(&format!("↳ {h}")));
            }
        }
        let errors = findings.iter().filter(|f| f.severity == lint::Severity::Error).count();
        let warns = findings.iter().filter(|f| f.severity == lint::Severity::Warn).count();
        let infos = findings.len() - errors - warns;
        if unused_count > 0 && !unused {
            println!(
                "{}",
                out::dim(&format!("{unused_count} rule(s) match no open window; pass --unused to list them"))
            );
        }
        println!(
            "{}",
            out::dim(&format!(
                "{} rules checked{}: {errors} error(s), {warns} warning(s), {infos} note(s)",
                rules.rules.len(),
                live_note.map(|n| format!(" against {n} open windows")).unwrap_or_default()
            ))
        );
    }
    if findings.iter().any(|f| f.severity == lint::Severity::Error) {
        std::process::exit(1);
    }
    Ok(())
}

async fn cmd_watch(config: Option<PathBuf>, json: bool) -> Result<()> {
    let h = Hypr::from_env()?;
    let mut rules = load_rules(config.clone(), Some(&h)).await?;
    let mut rx = h.subscribe();
    // Last printed verdict per window, so title churn only prints when the
    // outcome actually changes.
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    eprintln!("{}", out::dim("watching Hyprland events; ctrl-c to stop"));
    while let Some(ev) = rx.recv().await {
        match ev.name.as_str() {
            "openwindow" | "windowtitlev2" | "movewindowv2" | "changefloatingmode" | "fullscreen" | "pin" => {
                let addr = ev.data.split(',').next().unwrap_or("").to_string();
                let addr = if addr.starts_with("0x") { addr } else { format!("0x{addr}") };
                // Give the compositor a moment to finish applying rules.
                tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                let snap = Snapshot::take(&h).await?;
                let Some(c) = snap.clients.iter().find(|c| c.address == addr) else { continue };
                let facts = snap.facts(c);
                let rep = evaluate(&rules, &facts);
                let fingerprint = format!(
                    "{:?}|{:?}",
                    rep.effective,
                    rep.verdicts
                        .iter()
                        .filter(|v| v.is_active())
                        .map(|v| (v.rule_idx, v.matched_open, v.matched_now))
                        .collect::<Vec<_>>()
                );
                let changed = seen.insert(addr.clone(), fingerprint.clone()) != Some(fingerprint);
                if ev.name == "closewindow" {
                    seen.remove(&addr);
                }
                if !changed && ev.name != "openwindow" {
                    if !json {
                        println!(
                            "{} {}  {}",
                            out::magenta(&format!("▶ {}", ev.name)),
                            out::dim(&ev.data),
                            out::dim("(no change in verdicts)")
                        );
                    }
                    continue;
                }
                if json {
                    let mut v = report_json(&rules, c, &rep, false);
                    v["event"] = serde_json::Value::String(ev.name.clone());
                    println!("{}", serde_json::to_string(&v)?);
                } else {
                    println!("{} {}", out::magenta(&format!("▶ {}", ev.name)), out::dim(&ev.data));
                    print_why(&rules, c, &facts, &rep, false);
                    println!();
                }
            }
            "configreloaded" => match load_rules(config.clone(), Some(&h)).await {
                Ok(r) => {
                    rules = r;
                    if !json {
                        println!("{}", out::magenta(&format!("▶ configreloaded: {} rules", rules.rules.len())));
                    }
                }
                Err(e) => eprintln!("{} reload failed: {e}", out::red("error")),
            },
            _ => {}
        }
    }
    Ok(())
}

async fn cmd_snippet(config: Option<PathBuf>, target: Option<String>, set: Vec<String>, title: bool, copy: bool) -> Result<()> {
    let h = Hypr::from_env()?;
    let rules = load_rules(config, Some(&h)).await?;
    let snap = Snapshot::take(&h).await?;
    let targets = resolve_targets(&snap, &target)?;
    let mut effects = Vec::new();
    for s in &set {
        effects.push(snippet::parse_effect(s).ok_or_else(|| anyhow!("--set expects key=value, got {s}"))?);
    }
    let dialect = snippet::Dialect::detect(&rules);
    let mut all = String::new();
    for c in targets {
        let facts = snap.facts(c);
        all.push_str(&snippet::generate(c, &rules, &facts, dialect, &effects, title));
    }
    print!("{all}");
    if copy {
        if snippet::to_clipboard(&all) {
            eprintln!("{}", out::dim("copied to clipboard"));
        } else {
            eprintln!("{}", out::yellow("wl-copy not available; printed only"));
        }
    }
    Ok(())
}

#[allow(dead_code)]
fn effect_kind_label(e: &Effect) -> &'static str {
    match e.kind {
        rules::effects::EffectKind::Static => "static",
        rules::effects::EffectKind::Dynamic => "dynamic",
        rules::effects::EffectKind::Unknown => "unknown",
    }
}
