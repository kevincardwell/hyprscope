//! Runs the user's Hyprland Lua config inside a sandbox and captures every
//! `hl.window_rule` call with the source location that made it.

use anyhow::{bail, Context, Result};
use mlua::{Lua, Table, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::rules::model::{Location, RawRule, RawValue};

const PRELUDE: &str = include_str!("prelude.lua");

/// mlua's error is not Send + Sync, so it cannot flow through anyhow directly.
trait LuaCtx<T> {
    fn lua(self) -> Result<T>;
}

impl<T> LuaCtx<T> for std::result::Result<T, mlua::Error> {
    fn lua(self) -> Result<T> {
        self.map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// Everything the sandbox learned from one config run.
#[derive(Debug, Default)]
pub struct Capture {
    pub config_path: PathBuf,
    pub window_rules: Vec<RawRule>,
    pub layer_rules: usize,
    pub workspace_rules: usize,
    /// Errors raised while running the config (Lua errors, bad arguments).
    pub errors: Vec<String>,
}

/// Resolve the config file the compositor would load.
pub fn default_config_path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| home().join(".config"));
    let lua = base.join("hypr").join("hyprland.lua");
    if lua.exists() {
        return Ok(lua);
    }
    let conf = base.join("hypr").join("hyprland.conf");
    if conf.exists() {
        bail!(
            "found {} but hyprscope reads the Lua config format (Hyprland 0.53+); \
             see https://wiki.hypr.land/Configuring/ for the migration",
            conf.display()
        );
    }
    bail!("no hyprland.lua under {}", base.join("hypr").display())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Run the config at `path` and capture rule registrations.
pub fn load(path: &Path, hyprland_version: Option<&str>) -> Result<Capture> {
    let lua = unsafe { Lua::unsafe_new() };
    let globals = lua.globals();

    // package.path mirrors what a config expects: its own directory first.
    let dir = path.parent().unwrap_or(Path::new("."));
    let extra = format!(
        "{d}/?.lua;{d}/?/init.lua;{c}/?.lua;{c}/?/init.lua;",
        d = dir.display(),
        c = dir.parent().unwrap_or(dir).display()
    );
    let package: Table = globals.get("package").lua()?;
    let old: String = package.get("path").lua()?;
    package.set("path", format!("{extra}{old}")).lua()?;

    if let Some(v) = hyprland_version {
        globals.set("__hs_hyprland_version", v).lua()?;
    }

    lua.load(PRELUDE)
        .set_name("hyprscope-prelude")
        .exec()
        .lua()
        .context("loading sandbox prelude")?;

    let mut errors = Vec::new();
    let chunk = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    if let Err(e) = lua.load(&chunk).set_name(format!("@{}", path.display())).exec() {
        errors.push(format!("{}", e));
    }

    let capture: Table = globals.get("__hs_capture").lua()?;
    let mut out = Capture {
        config_path: path.to_path_buf(),
        errors,
        ..Default::default()
    };

    let win: Table = capture.get("window_rules").lua()?;
    for (i, entry) in win.sequence_values::<Table>().enumerate() {
        let entry = entry.lua()?;
        let rule: Table = entry.get("rule").lua()?;
        let stack: Table = entry.get("stack").lua()?;
        out.window_rules.push(raw_rule(i, &rule, &stack)?);
    }
    out.layer_rules = capture.get::<Table>("layer_rules").lua()?.len().lua()? as usize;
    out.workspace_rules = capture.get::<Table>("workspace_rules").lua()?.len().lua()? as usize;
    let errs: Table = capture.get("errors").lua()?;
    for e in errs.sequence_values::<Table>() {
        let e = e.lua()?;
        let msg: String = e.get("message").unwrap_or_default();
        out.errors.push(msg);
    }
    Ok(out)
}

fn raw_rule(idx: usize, rule: &Table, stack: &Table) -> Result<RawRule> {
    let mut fields = BTreeMap::new();
    let mut matches = BTreeMap::new();
    let mut name = None;
    let mut enabled = true;
    let mut bad = Vec::new();

    for pair in rule.pairs::<Value, Value>() {
        let (k, v) = pair.lua()?;
        let Value::String(k) = k else { continue };
        let key = k.to_str().lua()?.to_string();
        match key.as_str() {
            "name" => {
                if let Value::String(s) = &v {
                    name = Some(s.to_str().lua()?.to_string());
                }
            }
            "enabled" => {
                if let Value::Boolean(b) = v {
                    enabled = b;
                }
            }
            "match" => {
                if let Value::Table(t) = &v {
                    for p in t.pairs::<Value, Value>() {
                        let (mk, mv) = p.lua()?;
                        let Value::String(mk) = mk else { continue };
                        let mk = mk.to_str().lua()?.to_string();
                        match raw_value(&mv) {
                            Some(rv @ (RawValue::Bool(_) | RawValue::Int(_) | RawValue::Str(_))) => {
                                matches.insert(mk, rv);
                            }
                            Some(RawValue::Float(f)) => {
                                // Hyprland truncates numbers to integers here.
                                matches.insert(mk, RawValue::Int(f as i64));
                            }
                            _ => bad.push(format!("match value for '{mk}' must be string, bool, or number")),
                        }
                    }
                } else {
                    bad.push("match must be a table".into());
                }
            }
            _ => match raw_value(&v) {
                Some(rv) => {
                    fields.insert(key, rv);
                }
                None => bad.push(format!("field '{key}' has an unsupported value type")),
            },
        }
    }

    let mut frames = Vec::new();
    for f in stack.sequence_values::<Table>() {
        let f = f.lua()?;
        let file: String = f.get("file").unwrap_or_default();
        let line: u32 = f.get("line").unwrap_or(0);
        let func: Option<String> = f.get("name").ok();
        frames.push(Location {
            file: PathBuf::from(file),
            line,
            func,
        });
    }

    Ok(RawRule {
        idx,
        name,
        enabled,
        matches,
        fields,
        frames,
        problems: bad,
    })
}

fn raw_value(v: &Value) -> Option<RawValue> {
    Some(match v {
        Value::Boolean(b) => RawValue::Bool(*b),
        Value::Integer(i) => RawValue::Int(*i),
        Value::Number(n) => RawValue::Float(*n),
        Value::String(s) => RawValue::Str(s.to_str().ok()?.to_string()),
        Value::Table(t) => {
            // Either a sequence ({a, b}) or a map ({colors = {...}, angle = n}).
            let mut seq = Vec::new();
            let mut map = BTreeMap::new();
            for pair in t.pairs::<Value, Value>() {
                let (k, val) = pair.ok()?;
                let inner = raw_value(&val)?;
                match k {
                    Value::Integer(_) => seq.push(inner),
                    Value::String(s) => {
                        map.insert(s.to_str().ok()?.to_string(), inner);
                    }
                    _ => {}
                }
            }
            if map.is_empty() {
                RawValue::Seq(seq)
            } else {
                RawValue::Map(map)
            }
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hyprscope-test-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("hyprland.lua");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn captures_rules_with_locations_through_wrappers() {
        let p = fixture(
            "wrap",
            r#"
local function my_window(class, rules)
  rules.match = { class = class }
  hl.window_rule(rules)          -- line 4
end
hl.window_rule({ match = { class = "kitty", float = true }, opacity = "0.9", no_blur = true })  -- line 6
my_window("firefox", { tag = "+browser" })  -- line 7
hl.bind("SUPER + Q", hl.dsp.window.close())
hl.on("hyprland.start", function() hl.exec_cmd("rm -rf /") end)
hl.config({ general = { gaps_in = 3 } })
hl.window_rule({ name = "n", enabled = false, match = { title = "x" }, bogus_effect = 1 })
"#,
        );
        let cap = load(&p, Some("0.56.2")).unwrap();
        assert!(cap.errors.is_empty(), "{:?}", cap.errors);
        assert_eq!(cap.window_rules.len(), 3);
        let r0 = &cap.window_rules[0];
        assert_eq!(r0.frames[0].line, 6);
        assert_eq!(r0.matches.get("float"), Some(&RawValue::Bool(true)));
        assert_eq!(r0.fields.get("opacity"), Some(&RawValue::Str("0.9".into())));
        let r1 = &cap.window_rules[1];
        // Innermost frame is the wrapper (line 4), the next one is the call site (line 7).
        assert_eq!(r1.frames[0].line, 4);
        assert_eq!(r1.frames[1].line, 7);
        let r2 = &cap.window_rules[2];
        assert_eq!(r2.name.as_deref(), Some("n"));
        assert!(!r2.enabled);
        assert!(r2.fields.contains_key("bogus_effect"));
    }

    #[test]
    fn lua_errors_are_reported_not_fatal() {
        let p = fixture("err", "hl.window_rule({ match = { class = 'a' }, float = true })\nerror('boom')\n");
        let cap = load(&p, None).unwrap();
        assert_eq!(cap.window_rules.len(), 1);
        assert!(cap.errors.iter().any(|e| e.contains("boom")));
    }
}
