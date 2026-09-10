//! The live view: windows on the left, rule verdicts on the right.

use anyhow::Result;
use ratatui::crossterm::event::{self, Event as CEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

use crate::hypr::{Client, Event, Hypr};
use crate::rules::effects::EffectKind;
use crate::rules::model::Matcher;
use crate::rules::{evaluate, Facts, Report, RuleSet};
use crate::{load_rules, window_flags, Snapshot};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Windows,
    Rules,
}

/// Easy mode answers "what did this window get, and from where" in plain
/// rows. Advanced mode shows every rule with predicates, phases and overrides.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Easy,
    Advanced,
}

/// One row of the easy view: an effective value and the rule that set it.
struct EasyItem {
    key: String,
    value: String,
    rule_idx: usize,
    is_static: bool,
    /// Rules that also matched and set this key, but lost to `rule_idx`.
    beaten: Vec<usize>,
}

struct App {
    hypr: Hypr,
    config: Option<PathBuf>,
    rules: RuleSet,
    version: String,
    snap: Snapshot,
    facts: Vec<Facts>,
    reports: Vec<Report>,
    win_sel: usize,
    rule_sel: usize,
    pane: Pane,
    show_all: bool,
    expr_text: String,
    expr: Option<Vec<Matcher>>,
    expr_err: Option<String>,
    editing: bool,
    events: VecDeque<(Instant, String)>,
    status: Option<(Instant, String)>,
    help: bool,
    dirty: bool,
    last_refresh: Instant,
    mode: Mode,
    easy_sel: usize,
    /// Config files and their last seen modification time.
    watched: Vec<(PathBuf, Option<std::time::SystemTime>)>,
    /// Files edited on disk since Hyprland last reloaded its config.
    unapplied: Vec<String>,
}

enum Msg {
    Key(KeyEvent),
    Hypr(Event),
    Tick,
}

pub async fn run(config: Option<PathBuf>, mode: Mode) -> Result<()> {
    let hypr = Hypr::from_env()?;
    let version = hypr.version().await.unwrap_or_else(|_| "?".into());
    let rules = load_rules(config.clone(), Some(&hypr)).await?;
    let snap = Snapshot::take(&hypr).await?;
    let mut app = App {
        hypr: hypr.clone(),
        config,
        rules,
        version,
        snap,
        facts: Vec::new(),
        reports: Vec::new(),
        win_sel: 0,
        rule_sel: 0,
        pane: Pane::Windows,
        show_all: false,
        expr_text: String::new(),
        expr: None,
        expr_err: None,
        editing: false,
        events: VecDeque::new(),
        status: None,
        help: false,
        dirty: false,
        last_refresh: Instant::now(),
        mode,
        easy_sel: 0,
        watched: Vec::new(),
        unapplied: Vec::new(),
    };
    app.rebuild_watch_list();
    app.recompute();
    app.select_focused();

    let (tx, mut rx) = mpsc::channel::<Msg>(256);
    // Keyboard on a blocking thread.
    let ktx = tx.clone();
    std::thread::spawn(move || loop {
        if event::poll(Duration::from_millis(250)).unwrap_or(false) {
            if let Ok(CEvent::Key(k)) = event::read() {
                if ktx.blocking_send(Msg::Key(k)).is_err() {
                    break;
                }
            }
        } else if ktx.blocking_send(Msg::Tick).is_err() {
            break;
        }
    });
    let mut hrx = hypr.subscribe();
    let htx = tx.clone();
    tokio::spawn(async move {
        while let Some(ev) = hrx.recv().await {
            if htx.send(Msg::Hypr(ev)).await.is_err() {
                break;
            }
        }
    });

    let mut terminal = ratatui::init();
    let res = async {
        loop {
            terminal.draw(|f| draw(f, &mut app))?;
            let Some(msg) = rx.recv().await else { break };
            match msg {
                Msg::Key(k) => {
                    if app.handle_key(k).await? {
                        break;
                    }
                }
                Msg::Hypr(ev) => app.on_event(ev).await?,
                Msg::Tick => app.poll_files().await,
            }
            if app.dirty && app.last_refresh.elapsed() > Duration::from_millis(40) {
                app.refresh().await?;
            }
            // Drain anything else that queued up so redraws stay cheap.
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Key(k) => {
                        if app.handle_key(k).await? {
                            ratatui::restore();
                            return Ok(());
                        }
                    }
                    Msg::Hypr(ev) => app.on_event(ev).await?,
                    Msg::Tick => {}
                }
            }
            if app.dirty && app.last_refresh.elapsed() > Duration::from_millis(40) {
                app.refresh().await?;
            }
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    ratatui::restore();
    res
}

impl App {
    fn recompute(&mut self) {
        self.facts = self.snap.clients.iter().map(|c| self.snap.facts(c)).collect();
        self.reports = self.facts.iter().map(|f| evaluate(&self.rules, f)).collect();
        if self.win_sel >= self.snap.clients.len() {
            self.win_sel = self.snap.clients.len().saturating_sub(1);
        }
        self.clamp_rule_sel();
    }

    fn select_focused(&mut self) {
        if let Some(f) = &self.snap.focused {
            if let Some(i) = self.snap.clients.iter().position(|c| &c.address == f) {
                self.win_sel = i;
            }
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let keep = self.snap.clients.get(self.win_sel).map(|c| c.address.clone());
        self.snap = Snapshot::take(&self.hypr).await?;
        if let Some(a) = keep {
            if let Some(i) = self.snap.clients.iter().position(|c| c.address == a) {
                self.win_sel = i;
            }
        }
        self.recompute();
        self.dirty = false;
        self.last_refresh = Instant::now();
        Ok(())
    }

    async fn reload(&mut self) {
        match load_rules(self.config.clone(), Some(&self.hypr)).await {
            Ok(r) => {
                self.rules = r;
                self.set_status(format!("reloaded {} rules", self.rules.rules.len()));
            }
            Err(e) => self.set_status(format!("reload failed: {e}")),
        }
        self.recompute();
    }

    /// Every file a rule came from, plus the entry config.
    fn rebuild_watch_list(&mut self) {
        let mut files: Vec<PathBuf> = vec![self.rules.config_path.clone()];
        for r in &self.rules.rules {
            files.push(r.loc.file.clone());
            for v in &r.via {
                files.push(v.file.clone());
            }
        }
        files.sort();
        files.dedup();
        self.watched = files
            .into_iter()
            .map(|f| {
                let m = std::fs::metadata(&f).and_then(|m| m.modified()).ok();
                (f, m)
            })
            .collect();
    }

    /// Reload the rules when any watched file changes on disk, so verdicts
    /// track the editor before Hyprland has reloaded.
    async fn poll_files(&mut self) {
        let mut changed = Vec::new();
        for (f, seen) in &mut self.watched {
            let now = std::fs::metadata(&*f).and_then(|m| m.modified()).ok();
            if now != *seen {
                *seen = now;
                changed.push(f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
            }
        }
        if changed.is_empty() {
            return;
        }
        for c in changed {
            if !self.unapplied.contains(&c) {
                self.unapplied.push(c);
            }
        }
        self.reload().await;
        self.rebuild_watch_list();
    }

    fn set_status(&mut self, s: String) {
        self.status = Some((Instant::now(), s));
    }

    async fn on_event(&mut self, ev: Event) -> Result<()> {
        let interesting = matches!(
            ev.name.as_str(),
            "openwindow"
                | "closewindow"
                | "windowtitlev2"
                | "movewindowv2"
                | "changefloatingmode"
                | "fullscreen"
                | "pin"
                | "activewindowv2"
                | "togglegroup"
                | "moveintogroup"
                | "moveoutofgroup"
                | "urgent"
                | "configreloaded"
        );
        if !interesting {
            return Ok(());
        }
        let shown = match ev.name.as_str() {
            "activewindowv2" => None,
            _ => Some(format!("{} {}", ev.name, ev.data)),
        };
        if let Some(s) = shown {
            self.events.push_front((Instant::now(), s));
            self.events.truncate(200);
        }
        if ev.name == "configreloaded" {
            self.unapplied.clear();
            self.reload().await;
            self.rebuild_watch_list();
        }
        self.dirty = true;
        Ok(())
    }

    fn easy_items(&self) -> Vec<EasyItem> {
        let Some(rep) = self.reports.get(self.win_sel) else { return Vec::new() };
        let mut items = Vec::new();
        for (key, (value, idx)) in &rep.effective {
            let rule = &self.rules.rules[*idx];
            let is_static = rule.effects.iter().any(|e| &e.key == key && e.kind == EffectKind::Static);
            let beaten = rep
                .verdicts
                .iter()
                .filter(|v| v.rule_idx != *idx && v.effects.iter().any(|e| &e.key == key && e.overridden_by == Some(*idx)))
                .map(|v| v.rule_idx)
                .collect();
            items.push(EasyItem {
                key: key.clone(),
                value: value.clone(),
                rule_idx: *idx,
                is_static,
                beaten,
            });
        }
        // Tags accumulate rather than override, so list every rule that added one.
        for v in &rep.verdicts {
            if !v.matched_now {
                continue;
            }
            for e in &v.effects {
                if e.key == "tag" && e.winner {
                    items.push(EasyItem {
                        key: "tag".into(),
                        value: e.value.trim_matches('"').to_string(),
                        rule_idx: v.rule_idx,
                        is_static: false,
                        beaten: Vec::new(),
                    });
                }
            }
        }
        items
    }

    fn clamp_easy_sel(&mut self) {
        let n = self.easy_items().len();
        self.easy_sel = if n == 0 { 0 } else { self.easy_sel.min(n - 1) };
    }

    fn visible_rules(&self) -> Vec<usize> {
        let Some(rep) = self.reports.get(self.win_sel) else { return Vec::new() };
        rep.verdicts.iter().filter(|v| self.show_all || v.is_active()).map(|v| v.rule_idx).collect()
    }

    fn clamp_rule_sel(&mut self) {
        let n = self.visible_rules().len();
        if n == 0 {
            self.rule_sel = 0;
        } else if self.rule_sel >= n {
            self.rule_sel = n - 1;
        }
        self.clamp_easy_sel();
    }

    /// The rule the right pane currently points at, in either mode.
    fn selected_rule(&self) -> Option<usize> {
        match self.mode {
            Mode::Easy => self.easy_items().get(self.easy_sel).map(|i| i.rule_idx),
            Mode::Advanced => self.visible_rules().get(self.rule_sel).copied(),
        }
    }

    fn apply_expr(&mut self) {
        let t = self.expr_text.trim().to_string();
        if t.is_empty() {
            self.expr = None;
            self.expr_err = None;
            return;
        }
        match crate::rules::expr::parse(&t) {
            Ok(m) => {
                self.expr = Some(m);
                self.expr_err = None;
            }
            Err(e) => {
                self.expr = None;
                self.expr_err = Some(e.to_string());
            }
        }
    }

    fn expr_hits(&self) -> Vec<bool> {
        match &self.expr {
            Some(m) => self.facts.iter().map(|f| crate::rules::eval::expr_matches(m, f)).collect(),
            None => vec![false; self.facts.len()],
        }
    }

    /// Returns true to quit.
    async fn handle_key(&mut self, k: KeyEvent) -> Result<bool> {
        if self.help {
            self.help = false;
            return Ok(false);
        }
        if self.editing {
            match k.code {
                KeyCode::Esc => {
                    self.editing = false;
                    self.expr_text.clear();
                    self.apply_expr();
                }
                KeyCode::Enter => {
                    self.editing = false;
                    self.apply_expr();
                }
                KeyCode::Backspace => {
                    self.expr_text.pop();
                    self.apply_expr();
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.expr_text.clear();
                    self.apply_expr();
                }
                KeyCode::Char(c) => {
                    self.expr_text.push(c);
                    self.apply_expr();
                }
                _ => {}
            }
            return Ok(false);
        }
        match k.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Esc => {
                if self.expr.is_some() || !self.expr_text.is_empty() {
                    self.expr_text.clear();
                    self.apply_expr();
                } else {
                    return Ok(true);
                }
            }
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('/') => self.editing = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.pane = if self.pane == Pane::Windows { Pane::Rules } else { Pane::Windows };
            }
            KeyCode::Char('h') | KeyCode::Left => self.pane = Pane::Windows,
            KeyCode::Char('l') | KeyCode::Right => self.pane = Pane::Rules,
            KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::Char('g') | KeyCode::Home => self.move_sel(i64::MIN / 2),
            KeyCode::Char('G') | KeyCode::End => self.move_sel(i64::MAX / 2),
            KeyCode::Char('m') => {
                self.mode = if self.mode == Mode::Easy { Mode::Advanced } else { Mode::Easy };
                self.clamp_rule_sel();
            }
            KeyCode::Char('a') => {
                if self.mode == Mode::Easy {
                    self.mode = Mode::Advanced;
                }
                self.show_all = !self.show_all;
                self.clamp_rule_sel();
            }
            KeyCode::Char('r') => self.reload().await,
            KeyCode::Char('R') => match self.hypr.request("reload config-only").await {
                Ok(r) if r.trim() == "ok" => self.set_status("asked Hyprland to reload its config".into()),
                Ok(r) => self.set_status(format!("reload: {}", r.trim())),
                Err(e) => self.set_status(format!("reload failed: {e}")),
            },
            KeyCode::Char('y') => {
                if let Some(c) = self.snap.clients.get(self.win_sel) {
                    let facts = self.snap.facts(c);
                    let dialect = crate::snippet::Dialect::detect(&self.rules);
                    let text = crate::snippet::generate(c, &self.rules, &facts, dialect, &[], false);
                    let last = text.lines().last().unwrap_or("").to_string();
                    if crate::snippet::to_clipboard(&text) {
                        self.set_status(format!("copied: {last}"));
                    } else {
                        self.set_status(format!("wl-copy missing; snippet: {last}"));
                    }
                }
            }
            KeyCode::Char('f') => {
                if let Some(c) = self.snap.clients.get(self.win_sel) {
                    let addr = c.address.clone();
                    match self.hypr.focus(&addr).await {
                        Ok(()) => self.set_status(format!("focused {addr}")),
                        Err(e) => self.set_status(format!("focus failed: {e}")),
                    }
                }
            }
            KeyCode::Char('e') | KeyCode::Enter => self.open_editor().await,
            KeyCode::Char('n') => {
                // Jump to the next window matching the expression.
                let hits = self.expr_hits();
                if hits.iter().any(|h| *h) {
                    for step in 1..=hits.len() {
                        let i = (self.win_sel + step) % hits.len();
                        if hits[i] {
                            self.win_sel = i;
                            break;
                        }
                    }
                    self.clamp_rule_sel();
                }
            }
            _ => {}
        }
        Ok(false)
    }

    fn move_sel(&mut self, delta: i64) {
        match self.pane {
            Pane::Windows => {
                let n = self.snap.clients.len() as i64;
                if n > 0 {
                    self.win_sel = (self.win_sel as i64 + delta).clamp(0, n - 1) as usize;
                    self.clamp_rule_sel();
                }
            }
            Pane::Rules => match self.mode {
                Mode::Easy => {
                    let n = self.easy_items().len() as i64;
                    if n > 0 {
                        self.easy_sel = (self.easy_sel as i64 + delta).clamp(0, n - 1) as usize;
                    }
                }
                Mode::Advanced => {
                    let n = self.visible_rules().len() as i64;
                    if n > 0 {
                        self.rule_sel = (self.rule_sel as i64 + delta).clamp(0, n - 1) as usize;
                    }
                }
            },
        }
    }

    async fn open_editor(&mut self) {
        let Some(idx) = self.selected_rule() else { return };
        let rule = &self.rules.rules[idx];
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let file = rule.loc.file.clone();
        let line = rule.loc.line;
        ratatui::restore();
        let status = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(format!("{editor} +{line} \"$1\"",))
            .arg("hyprscope")
            .arg(&file)
            .status()
            .await;
        let _ = ratatui::init();
        match status {
            Ok(s) if s.success() => self.set_status(format!("edited {}", rule.loc.short())),
            Ok(s) => self.set_status(format!("editor exited with {s}")),
            Err(e) => self.set_status(format!("could not run {editor}: {e}")),
        }
        self.reload().await;
        self.dirty = true;
    }
}

// ---------------------------------------------------------------- rendering

const ACCENT: Color = Color::Cyan;
const OK: Color = Color::Green;
const BAD: Color = Color::Red;
const WIN: Color = Color::Yellow;
const DIM: Color = Color::DarkGray;

fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(8), Constraint::Length(4), Constraint::Length(1)])
        .split(area);

    draw_header(f, rows[0], app);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(rows[1]);
    draw_windows(f, cols[0], app);
    draw_rules(f, cols[1], app);
    draw_events(f, rows[2], app);
    draw_footer(f, rows[3], app);

    if app.help {
        draw_help(f, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let cfg = app.rules.config_path.display().to_string();
    let home = std::env::var("HOME").unwrap_or_default();
    let cfg = if !home.is_empty() && cfg.starts_with(&home) {
        format!("~{}", &cfg[home.len()..])
    } else {
        cfg
    };
    let mut spans = vec![
        Span::styled(" hyprscope ", Style::default().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" Hyprland "),
        Span::styled(app.version.clone(), Style::default().fg(ACCENT)),
        Span::raw(format!("  {} windows  {} rules  ", app.snap.clients.len(), app.rules.rules.len())),
        Span::styled(cfg, Style::default().fg(DIM)),
        Span::raw("  "),
        Span::styled(
            match app.mode {
                Mode::Easy => " easy ",
                Mode::Advanced => " advanced ",
            },
            Style::default().fg(Color::Black).bg(if app.mode == Mode::Easy { OK } else { WIN }),
        ),
    ];
    if !app.rules.load_errors.is_empty() {
        spans.push(Span::styled(
            format!("  ⚠ {} load error(s)", app.rules.load_errors.len()),
            Style::default().fg(BAD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_windows(f: &mut Frame, area: Rect, app: &mut App) {
    let hits = app.expr_hits();
    let focused = app.pane == Pane::Windows;
    let border = if focused { Style::default().fg(ACCENT) } else { Style::default().fg(DIM) };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(" windows ", Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let class_w = (inner.width as usize / 3).clamp(8, 22);
    let items: Vec<ListItem> = app
        .snap
        .clients
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let rep = &app.reports[i];
            let hit = hits.get(i).copied().unwrap_or(false);
            let marker = if app.expr.is_some() {
                if hit {
                    Span::styled("◆ ", Style::default().fg(ACCENT))
                } else {
                    Span::styled("  ", Style::default())
                }
            } else {
                Span::raw("  ")
            };
            let active = rep.verdicts.iter().filter(|v| v.is_active()).count();
            let flags = flag_chars(c);
            let is_focus = app.snap.focused.as_deref() == Some(c.address.as_str());
            let mut style = Style::default();
            if app.expr.is_some() && !hit {
                style = style.fg(DIM);
            }
            let title_w = inner.width as usize;
            let title_w = title_w.saturating_sub(class_w + 14);
            ListItem::new(Line::from(vec![
                marker,
                Span::styled(
                    cell(c.label(), class_w),
                    style.add_modifier(if is_focus { Modifier::BOLD } else { Modifier::empty() }),
                ),
                Span::raw(" "),
                Span::styled(cell(&c.title, title_w), style.fg(if app.expr.is_some() && !hit { DIM } else { Color::Gray })),
                Span::styled(format!(" {:>3}", c.workspace.name), style.fg(ACCENT)),
                Span::styled(format!(" {:<3}", flags), style.fg(WIN)),
                Span::styled(if app.mode == Mode::Advanced { format!("{:>2}", active) } else { "  ".into() }, style.fg(DIM)),
            ]))
        })
        .collect();

    let mut state = ListState::default().with_selected(Some(app.win_sel));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Rgb(40, 44, 52)).add_modifier(Modifier::BOLD))
        .highlight_symbol("▸");
    f.render_stateful_widget(list, inner, &mut state);
}

fn flag_chars(c: &Client) -> String {
    let mut s = String::new();
    if c.floating {
        s.push('F')
    }
    if c.xwayland {
        s.push('X')
    }
    if c.pinned {
        s.push('P')
    }
    if c.is_fullscreen() {
        s.push('▣')
    }
    s
}

/// Clip to `w` display columns and pad to exactly `w`, so wide characters do
/// not shift later columns. For table cells.
fn cell(s: &str, w: usize) -> String {
    let mut out = clip(s, w);
    let used = unicode_width::UnicodeWidthStr::width(out.as_str());
    for _ in used..w {
        out.push(' ');
    }
    out
}

/// Clip to at most `w` display columns, ending with an ellipsis when cut.
fn clip(s: &str, w: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if w == 0 {
        return String::new();
    }
    let total = s.width();
    let mut out = String::new();
    let mut used = 0;
    if total <= w {
        return s.to_string();
    }
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(1);
        if used + cw > w - 1 {
            break;
        }
        out.push(ch);
        used += cw;
    }
    out.push('…');
    out
}

fn draw_rules(f: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.pane == Pane::Rules;
    let border = if focused { Style::default().fg(ACCENT) } else { Style::default().fg(DIM) };
    let Some(c) = app.snap.clients.get(app.win_sel) else {
        f.render_widget(Block::default().borders(Borders::ALL).border_style(border).title(" rules "), area);
        return;
    };
    let rep = &app.reports[app.win_sel];
    if app.mode == Mode::Easy {
        draw_easy(f, area, app, border);
        return;
    }
    let title = format!(" rules for {} {} ", c.address, c.label());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Header: window facts + effective values.
    let mut head: Vec<Line> = Vec::new();
    let mut l1 = vec![Span::styled(
        format!("\"{}\"", clip(&c.title, inner.width as usize / 2)),
        Style::default().fg(Color::Gray),
    )];
    l1.push(Span::raw(format!("  ws {}  ", c.workspace.name)));
    let fl = window_flags(c);
    l1.push(Span::styled(if fl.is_empty() { "tiled".into() } else { fl }, Style::default().fg(WIN)));
    if c.initial_class != c.class || c.initial_title != c.title {
        l1.push(Span::styled(
            format!("  opened as {} \"{}\"", c.initial_class, clip(&c.initial_title, 30)),
            Style::default().fg(DIM),
        ));
    }
    head.push(Line::from(l1));
    let mut l2 = vec![Span::styled("tags ", Style::default().fg(DIM))];
    if c.tags.is_empty() {
        l2.push(Span::styled("none", Style::default().fg(DIM)));
    } else {
        l2.push(Span::styled(c.tags.join(" "), Style::default().fg(ACCENT)));
    }
    if rep.tags_agree() {
        l2.push(Span::styled("  ✓ model matches Hyprland", Style::default().fg(OK)));
    } else {
        l2.push(Span::styled(
            format!("  ⚠ model computed [{}]", rep.computed_tags.iter().cloned().collect::<Vec<_>>().join(" ")),
            Style::default().fg(WIN),
        ));
    }
    head.push(Line::from(l2));
    let mut l3 = vec![Span::styled("effective ", Style::default().fg(DIM))];
    if rep.effective.is_empty() {
        l3.push(Span::styled("nothing", Style::default().fg(DIM)));
    }
    for (k, (v, _)) in &rep.effective {
        l3.push(Span::styled(format!("{k}="), Style::default().fg(Color::Gray)));
        l3.push(Span::styled(format!("{v} "), Style::default().fg(WIN)));
    }
    head.push(Line::from(l3));

    let head_h = 3u16;
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(head_h), Constraint::Min(4), Constraint::Length(7)])
        .split(inner);
    f.render_widget(Paragraph::new(head).wrap(Wrap { trim: true }), sections[0]);

    // Rule list.
    let visible = app.visible_rules();
    let loc_w = (sections[1].width as usize * 3 / 10).clamp(16, 40);
    let items: Vec<ListItem> = visible
        .iter()
        .map(|&idx| {
            let r = &app.rules.rules[idx];
            let v = rep.verdict(idx).unwrap();
            let o = if v.matched_open {
                Span::styled("O", Style::default().fg(OK))
            } else {
                Span::styled("·", Style::default().fg(DIM))
            };
            let n = if v.matched_now {
                Span::styled("N", Style::default().fg(OK))
            } else {
                Span::styled("·", Style::default().fg(DIM))
            };
            let dimmed = !v.is_active();
            let base = if dimmed { Style::default().fg(DIM) } else { Style::default() };
            let star = if v.wins_any() {
                Span::styled("★", Style::default().fg(WIN))
            } else {
                Span::raw(" ")
            };
            let mut spans = vec![
                o,
                n,
                Span::raw(" "),
                star,
                Span::styled(format!(" {:>3} ", idx + 1), base.fg(if dimmed { DIM } else { ACCENT })),
                Span::styled(cell(&r.loc.compact(), loc_w), base),
                Span::raw(" "),
                Span::styled(r.match_summary(), base.fg(if dimmed { DIM } else { Color::Gray })),
                Span::raw("  "),
            ];
            for e in &v.effects {
                let style = if e.inert {
                    Style::default().fg(WIN).add_modifier(Modifier::DIM)
                } else if e.winner {
                    Style::default().fg(WIN)
                } else if dimmed {
                    Style::default().fg(DIM)
                } else {
                    Style::default().fg(DIM).add_modifier(Modifier::CROSSED_OUT)
                };
                spans.push(Span::styled(format!("{}={} ", e.key, e.value), style));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default().with_selected(if visible.is_empty() { None } else { Some(app.rule_sel) });
    let list = List::new(items).highlight_style(Style::default().bg(Color::Rgb(40, 44, 52)));
    let list_area = sections[1];
    if visible.is_empty() {
        let msg = if app.show_all {
            "no rules loaded"
        } else {
            "no rule matches this window · press a to show all rules"
        };
        f.render_widget(Paragraph::new(Span::styled(msg, Style::default().fg(DIM))), list_area);
    } else {
        f.render_stateful_widget(list, list_area, &mut state);
    }

    // Detail for the selected rule.
    draw_detail(f, sections[2], app, rep, visible.get(app.rule_sel).copied());
}

fn draw_easy(f: &mut Frame, area: Rect, app: &mut App, border: Style) {
    let c = &app.snap.clients[app.win_sel];
    let rep = &app.reports[app.win_sel];
    let title = format!(" {} ", c.label());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(title, Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let items = app.easy_items();
    let list_h = (items.len().max(1) as u16).min(inner.height.saturating_sub(12).max(3));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(list_h), Constraint::Length(2), Constraint::Min(6)])
        .split(inner);

    // Header: what the window is.
    let fl = window_flags(c);
    let mut head = vec![Line::from(vec![
        Span::styled(format!("\"{}\"", clip(&c.title, inner.width as usize / 2)), Style::default().fg(Color::Gray)),
        Span::raw(format!("  workspace {}  ", c.workspace.name)),
        Span::styled(if fl.is_empty() { "tiled".into() } else { fl }, Style::default().fg(WIN)),
    ])];
    let mut l2 = vec![Span::styled("tags ", Style::default().fg(DIM))];
    if c.tags.is_empty() {
        l2.push(Span::styled("none", Style::default().fg(DIM)));
    } else {
        l2.push(Span::styled(
            c.tags.iter().map(|t| t.trim_end_matches('*')).collect::<Vec<_>>().join(", "),
            Style::default().fg(ACCENT),
        ));
    }
    if !rep.tags_agree() {
        l2.push(Span::styled(
            "  ⚠ hyprscope expected different tags; press m for details",
            Style::default().fg(WIN),
        ));
    }
    head.push(Line::from(l2));
    head.push(Line::from(Span::styled("what this window got, and from where", Style::default().fg(DIM))));
    f.render_widget(Paragraph::new(head), sections[0]);

    // Rows.
    let key_w = 16usize;
    let val_w = (sections[1].width as usize / 5).clamp(10, 26);
    let rows: Vec<ListItem> = items
        .iter()
        .map(|it| {
            let r = &app.rules.rules[it.rule_idx];
            let mut spans = vec![
                Span::styled(cell(&it.key, key_w), Style::default().fg(Color::Gray)),
                Span::styled(cell(&it.value, val_w), Style::default().fg(WIN)),
                Span::styled(" ← ", Style::default().fg(DIM)),
                Span::styled(r.loc.compact(), Style::default().fg(ACCENT)),
            ];
            if it.is_static {
                spans.push(Span::styled("  decided at open", Style::default().fg(DIM)));
            }
            if !it.beaten.is_empty() {
                let names: Vec<String> = it.beaten.iter().map(|b| app.rules.rules[*b].loc.compact()).collect();
                spans.push(Span::styled(format!("  beat {}", names.join(", ")), Style::default().fg(DIM)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("no rule touches this window", Style::default().fg(DIM))),
            sections[1],
        );
    } else {
        let mut state = ListState::default().with_selected(Some(app.easy_sel));
        f.render_stateful_widget(
            List::new(rows).highlight_style(Style::default().bg(Color::Rgb(40, 44, 52))),
            sections[1],
            &mut state,
        );
    }

    // What else happened, in one dim line.
    let lost: Vec<String> = rep
        .verdicts
        .iter()
        .filter(|v| v.is_active() && !v.wins_any())
        .map(|v| app.rules.rules[v.rule_idx].loc.compact())
        .collect();
    let missed = rep.verdicts.iter().filter(|v| !v.is_active()).count();
    let mut summary = vec![Span::styled(format!("{missed} rules did not match this window"), Style::default().fg(DIM))];
    if !lost.is_empty() {
        summary.push(Span::styled(
            format!(" · matched but lost every effect: {}", lost.join(", ")),
            Style::default().fg(DIM),
        ));
    }
    summary.push(Span::styled(" · m shows them all", Style::default().fg(DIM)));
    f.render_widget(Paragraph::new(Line::from(summary)).wrap(Wrap { trim: true }), sections[2]);

    // Plain-language detail for the selected row.
    let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(DIM));
    let dinner = block.inner(sections[3]);
    f.render_widget(block, sections[3]);
    let Some(it) = items.get(app.easy_sel) else { return };
    let r = &app.rules.rules[it.rule_idx];
    let v = rep.verdict(it.rule_idx).unwrap();
    let because: Vec<String> = v
        .predicates_now
        .iter()
        .filter(|p| p.ok == Some(true))
        .map(|p| match p.prop.as_str() {
            "class" | "title" | "initial_class" | "initial_title" | "xdg_tag" | "content" => {
                format!("{} \"{}\" fits {}", p.prop, clip(&p.actual, 30), p.expected.trim_start_matches('~'))
            }
            "tag" => format!("it carries tag {}", p.expected.trim_start_matches('=')),
            "workspace" => format!("it is on workspace {}", p.expected.trim_start_matches('=')),
            _ => format!("{} is {}", p.prop, p.actual),
        })
        .collect();
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} = {}", it.key, it.value), Style::default().fg(WIN).add_modifier(Modifier::BOLD)),
        Span::raw(" comes from "),
        Span::styled(format!("{} {}", r.display_name(), r.loc.short()), Style::default().fg(ACCENT)),
        Span::raw("."),
    ])];
    lines.push(Line::from(format!(
        "It matched because {}.",
        if because.is_empty() {
            "every predicate held".to_string()
        } else {
            because.join(" and ")
        }
    )));
    if it.is_static {
        lines.push(Line::from(Span::styled(
            format!(
                "{} is a static effect: Hyprland decided it once, when the window opened as {} \"{}\".",
                it.key,
                c.initial_class,
                clip(&c.initial_title, 40)
            ),
            Style::default().fg(DIM),
        )));
    }
    if !it.beaten.is_empty() {
        let names: Vec<String> = it
            .beaten
            .iter()
            .map(|b| format!("{} {}", app.rules.rules[*b].display_name(), app.rules.rules[*b].loc.short()))
            .collect();
        lines.push(Line::from(format!(
            "It overrides {} which also matched and set {}. Later rules win.",
            names.join(", "),
            it.key
        )));
    }
    for via in &r.via {
        lines.push(Line::from(Span::styled(
            format!(
                "Written through {}{}.",
                via.func.as_deref().map(|n| format!("{n}() at ")).unwrap_or_default(),
                via.compact()
            ),
            Style::default().fg(DIM),
        )));
    }
    lines.push(Line::from(Span::styled(
        "e opens this rule in your editor · m shows every rule",
        Style::default().fg(DIM),
    )));
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), dinner);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App, rep: &Report, idx: Option<usize>) {
    let block = Block::default().borders(Borders::TOP).border_style(Style::default().fg(DIM));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let Some(idx) = idx else { return };
    let r = &app.rules.rules[idx];
    let v = rep.verdict(idx).unwrap();
    let mut lines: Vec<Line> = Vec::new();
    let mut l = vec![
        Span::styled(r.display_name(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::raw(r.loc.short()),
    ];
    for via in &r.via {
        l.push(Span::styled(
            format!("  via {}{}", via.func.as_deref().map(|n| format!("{n}() ")).unwrap_or_default(), via.compact()),
            Style::default().fg(DIM),
        ));
    }
    if !r.enabled {
        l.push(Span::styled("  disabled", Style::default().fg(BAD)));
    }
    lines.push(Line::from(l));

    // Predicates: show the "now" view, and note if open differs.
    let mut pl = vec![Span::styled("match  ", Style::default().fg(DIM))];
    for (p, po) in v.predicates_now.iter().zip(v.predicates_open.iter()) {
        let (glyph, color) = match p.ok {
            Some(true) => ("✓", OK),
            Some(false) => ("✗", BAD),
            None => ("?", WIN),
        };
        pl.push(Span::styled(format!("{glyph} {}{} ", p.prop, p.expected), Style::default().fg(color)));
        pl.push(Span::styled(format!("[{}]", clip(&p.actual, 28)), Style::default().fg(DIM)));
        if po.ok != p.ok && (p.prop == "class" || p.prop == "title") {
            pl.push(Span::styled(
                format!(" (at open: {} [{}])", if po.ok == Some(true) { "✓" } else { "✗" }, clip(&po.actual, 20)),
                Style::default().fg(WIN),
            ));
        }
        if let Some(n) = &p.note {
            pl.push(Span::styled(format!(" {n}"), Style::default().fg(WIN)));
        }
        pl.push(Span::raw("  "));
    }
    if v.predicates_now.is_empty() {
        pl.push(Span::styled("no predicates: Hyprland never applies this rule", Style::default().fg(BAD)));
    }
    lines.push(Line::from(pl));

    for e in &v.effects {
        let kind = match e.kind {
            EffectKind::Static => Span::styled("static  ", Style::default().fg(DIM)),
            EffectKind::Dynamic => Span::styled("dynamic ", Style::default().fg(DIM)),
            EffectKind::Unknown => Span::styled("unknown ", Style::default().fg(BAD)),
        };
        let status = if e.inert {
            Span::styled(
                "inert: static effect, but the rule only matches the current title/class, not the initial one",
                Style::default().fg(WIN),
            )
        } else if e.winner {
            Span::styled("★ wins", Style::default().fg(WIN))
        } else if let Some(by) = e.overridden_by {
            let b = &app.rules.rules[by];
            Span::styled(format!("overridden by {} {}", b.display_name(), b.loc.compact()), Style::default().fg(DIM))
        } else if !v.is_active() {
            Span::styled("not applied", Style::default().fg(DIM))
        } else {
            Span::raw("")
        };
        lines.push(Line::from(vec![
            Span::raw("       "),
            kind,
            Span::styled(format!("{:<18}", e.key), Style::default().fg(Color::Gray)),
            Span::styled(format!("{:<22} ", clip(&e.value, 22)), Style::default().fg(WIN)),
            status,
        ]));
    }
    for p in &r.problems {
        lines.push(Line::from(Span::styled(format!("       ! {p}"), Style::default().fg(BAD))));
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_events(f: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" events ", Style::default().fg(DIM)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let lines: Vec<Line> = app
        .events
        .iter()
        .take(inner.height as usize)
        .map(|(t, s)| {
            let age = t.elapsed().as_secs();
            let (name, rest) = s.split_once(' ').unwrap_or((s.as_str(), ""));
            Line::from(vec![
                Span::styled(format!("{:>4}s ", age), Style::default().fg(DIM)),
                Span::styled(format!("{name:<18}"), Style::default().fg(Color::Magenta)),
                Span::styled(clip(rest, inner.width.saturating_sub(26) as usize), Style::default().fg(Color::Gray)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hits = app.expr_hits().iter().filter(|h| **h).count();
    let mut spans = Vec::new();
    if app.editing {
        spans.push(Span::styled(" match › ", Style::default().fg(Color::Black).bg(ACCENT)));
        spans.push(Span::raw(format!(" {}▏", app.expr_text)));
        if let Some(e) = &app.expr_err {
            spans.push(Span::styled(format!("  {e}"), Style::default().fg(BAD)));
        } else if app.expr.is_some() {
            spans.push(Span::styled(format!("  {hits} window(s)"), Style::default().fg(ACCENT)));
            if hits == 0 {
                spans.push(Span::styled(
                    "  regexes must match the whole value, as in Hyprland: try .*foo.*",
                    Style::default().fg(DIM),
                ));
            }
        } else {
            spans.push(Span::styled(
                "  prop:value … e.g. class:^kitty$ float:true tag:terminal  (Enter keep · Esc clear)",
                Style::default().fg(DIM),
            ));
        }
    } else if let Some((t, s)) = &app.status {
        if t.elapsed() < Duration::from_secs(4) {
            spans.push(Span::styled(format!(" {s} "), Style::default().fg(Color::Black).bg(WIN)));
            spans.push(Span::raw("  "));
        }
        spans.extend(keys_hint(app, hits));
    } else {
        spans.extend(keys_hint(app, hits));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn keys_hint(app: &App, hits: usize) -> Vec<Span<'static>> {
    let mut v = Vec::new();
    if app.expr.is_some() {
        v.push(Span::styled(format!(" ◆ {} → {hits} window(s) ", app.expr_text), Style::default().fg(ACCENT)));
        v.push(Span::styled("n next · Esc clear · ", Style::default().fg(DIM)));
    }
    let hint = match app.mode {
        Mode::Easy => "j/k move · Tab pane · / match · m advanced · y copy rule · e edit · f focus · R apply · ? help · q quit".to_string(),
        Mode::Advanced => format!(
            "j/k move · Tab pane · / match · m easy · a {} · y copy rule · e edit · f focus · R apply · ? help · q quit",
            if app.show_all { "matching only" } else { "all rules" }
        ),
    };
    v.push(Span::styled(hint, Style::default().fg(DIM)));
    v
}

fn draw_help(f: &mut Frame, area: Rect) {
    let w = 70.min(area.width.saturating_sub(4));
    let h = 22.min(area.height.saturating_sub(2));
    let rect = Rect {
        x: (area.width - w) / 2,
        y: (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, rect);
    let text = vec![
        Line::from(Span::styled("hyprscope", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))),
        Line::from(""),
        Line::from("Left pane lists open windows. Right pane lists the rules from your"),
        Line::from("config, evaluated for the selected window exactly as Hyprland does:"),
        Line::from("all predicates must hold, rules run top to bottom, last match wins."),
        Line::from("Edits to config files are picked up as you save, before Hyprland"),
        Line::from("reloads, so you can check a rule and then apply it with R."),
        Line::from(""),
        Line::from(vec![
            Span::styled("O ", Style::default().fg(OK)),
            Span::raw("matched when the window opened (initial class/title) → static effects"),
        ]),
        Line::from(vec![
            Span::styled("N ", Style::default().fg(OK)),
            Span::raw("matches now (current class/title) → dynamic effects"),
        ]),
        Line::from(vec![
            Span::styled("★ ", Style::default().fg(WIN)),
            Span::raw("this rule sets at least one effective value"),
        ]),
        Line::from(vec![
            Span::styled("s̶t̶r̶i̶k̶e̶ ", Style::default().fg(DIM)),
            Span::raw("effect overridden by a later rule"),
        ]),
        Line::from(""),
        Line::from("m         switch between easy (what applied, from where) and advanced (every rule)"),
        Line::from("j/k ↑/↓   move          Tab h/l   switch pane      g/G  top/bottom"),
        Line::from("/         match expression: prop:value pairs, bare word = class"),
        Line::from("n         next window matching the expression"),
        Line::from("a         toggle all rules / only rules touching this window"),
        Line::from("e Enter   open the selected rule in $EDITOR at its line"),
        Line::from("f         focus the selected window in Hyprland"),
        Line::from("y         copy a ready-to-paste rule for the selected window (wl-copy)"),
        Line::from("R         ask Hyprland to reload its config (hyprctl reload config-only)"),
        Line::from("r         re-read the config; automatic when a watched file changes"),
        Line::from("q Esc     quit"),
        Line::from(""),
        Line::from(Span::styled("press any key to close", Style::default().fg(DIM))),
    ];
    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(ACCENT)).title(" help ");
    f.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: false }), rect);
}
