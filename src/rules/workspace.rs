//! Hyprland's workspace selector grammar (CWorkspaceFilter), as far as it can
//! be decided from `hyprctl -j workspaces` data.

/// What we know about the workspace a window sits on.
#[derive(Clone, Debug, Default)]
pub struct WsFacts {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    pub windows: i64,
    pub has_fullscreen: bool,
}

impl WsFacts {
    fn is_special(&self) -> bool {
        self.id < 0 || self.name.starts_with("special")
    }
    fn is_named(&self) -> bool {
        self.name != self.id.to_string()
    }
}

/// Some(true/false) when decidable, None when the selector uses data hyprctl
/// does not expose (per-window count flags, fullscreen modes) or is invalid.
pub fn matches(selector: &str, ws: &WsFacts) -> Option<bool> {
    let f = selector.trim();
    if f.is_empty() {
        return Some(false);
    }
    if let Some(name) = f.strip_prefix("name:") {
        if name.is_empty() {
            return Some(false);
        }
        return Some(ws.name == name);
    }
    if f == "special" {
        return Some(ws.is_special());
    }
    if let Some(rest) = f.strip_prefix("special:") {
        if rest.is_empty() {
            return Some(false);
        }
        return Some(ws.name == f);
    }
    if f.chars().all(|c| c.is_ascii_digit()) {
        return match f.parse::<u32>() {
            Ok(0) | Err(_) => Some(false),
            Ok(n) => Some(ws.id == n as i64),
        };
    }
    if !f.contains('[') {
        return Some(ws.name == f);
    }

    let bytes: Vec<char> = f.chars().collect();
    let mut i = 0;
    let mut result = Some(true);
    while i < bytes.len() {
        if bytes[i].is_whitespace() {
            i += 1;
            continue;
        }
        let ty = bytes[i];
        if i + 1 >= bytes.len() || bytes[i + 1] != '[' {
            return Some(false);
        }
        let close = bytes[i + 2..].iter().position(|&c| c == ']')? + i + 2;
        let value: String = bytes[i + 2..close].iter().collect();
        let r = statement(ty, &value, ws)?;
        if let Some(r) = r {
            if !r {
                result = Some(false);
            }
        } else {
            result = None;
        }
        i = close + 1;
    }
    result
}

/// Outer None = invalid selector; inner None = undecidable.
fn statement(ty: char, value: &str, ws: &WsFacts) -> Option<Option<bool>> {
    if value.is_empty() {
        return None;
    }
    Some(match ty {
        'r' => {
            let (a, b) = range(value)?;
            Some(ws.id >= a && ws.id <= b)
        }
        's' => Some(ws.is_special() == boolean(value)?),
        'n' => {
            if let Some(p) = value.strip_prefix("s:") {
                if p.is_empty() {
                    return None;
                }
                Some(ws.name.starts_with(p))
            } else if let Some(s) = value.strip_prefix("e:") {
                if s.is_empty() {
                    return None;
                }
                Some(ws.name.ends_with(s))
            } else {
                Some(ws.is_named() == boolean(value)?)
            }
        }
        'm' => Some(ws.monitor == value),
        'w' => {
            let flags: String = value.chars().take_while(|c| matches!(c, 't' | 'f' | 'p' | 'g' | 'v')).collect();
            let rest = &value[flags.len()..];
            if rest.is_empty() {
                return None;
            }
            let (a, b) = if rest.contains('-') {
                range(rest)?
            } else {
                let n: i64 = rest.parse().ok()?;
                (n, n)
            };
            if flags.is_empty() {
                Some(ws.windows >= a && ws.windows <= b)
            } else {
                None
            }
        }
        'f' => {
            let state: i64 = value.parse().ok()?;
            if !(-1..=2).contains(&state) {
                return None;
            }
            if state == -1 {
                Some(!ws.has_fullscreen)
            } else if !ws.has_fullscreen {
                Some(false)
            } else {
                None
            }
        }
        _ => return None,
    })
}

fn range(v: &str) -> Option<(i64, i64)> {
    let (a, b) = v.split_once('-')?;
    let a: i64 = a.trim().parse().ok()?;
    let b: i64 = b.trim().parse().ok()?;
    if a > b {
        return None;
    }
    Some((a, b))
}

fn boolean(v: &str) -> Option<bool> {
    if v.starts_with("true") || v.starts_with("yes") || v.starts_with("on") {
        return Some(true);
    }
    if v.starts_with("false") || v.starts_with("no") || v.starts_with("off") {
        return Some(false);
    }
    v.parse::<i64>().ok().map(|n| n != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(id: i64, name: &str) -> WsFacts {
        WsFacts {
            id,
            name: name.into(),
            monitor: "DP-1".into(),
            windows: 2,
            has_fullscreen: false,
        }
    }

    #[test]
    fn forms() {
        assert_eq!(matches("3", &ws(3, "3")), Some(true));
        assert_eq!(matches("3", &ws(4, "4")), Some(false));
        assert_eq!(matches("name:code", &ws(5, "code")), Some(true));
        assert_eq!(matches("code", &ws(5, "code")), Some(true));
        assert_eq!(matches("special:term", &ws(-98, "special:term")), Some(true));
        assert_eq!(matches("special", &ws(-98, "special:term")), Some(true));
        assert_eq!(matches("r[1-4] s[false] n[false]", &ws(2, "2")), Some(true));
        assert_eq!(matches("r[1-4] s[false] n[false]", &ws(2, "dev")), Some(false));
        assert_eq!(matches("m[DP-1]", &ws(2, "2")), Some(true));
        assert_eq!(matches("n[s:dev]", &ws(2, "dev-code")), Some(true));
        assert_eq!(matches("w[2]", &ws(2, "2")), Some(true));
        assert_eq!(matches("w[tpgv1-3]", &ws(2, "2")), None);
        assert_eq!(matches("x[1]", &ws(2, "2")), None);
    }
}
