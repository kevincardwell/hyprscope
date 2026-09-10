//! The tiny query language used by `hyprscope match` and the TUI match bar.
//!
//! `class:^kitty$ title:"btop" float:true tag:terminal` — space separated
//! `prop:value` pairs, values optionally double-quoted. A bare word is a class
//! regex. Every pair uses the same engine Hyprland would.

use anyhow::{bail, Result};

use super::model::{engine_for, Matcher};

/// Parse an expression into matchers. Errors on unknown props.
pub fn parse(expr: &str) -> Result<Vec<Matcher>> {
    let mut out = Vec::new();
    for tok in tokenize(expr) {
        let (prop, value) = match tok.split_once(':') {
            Some((p, v)) if engine_for(p).is_some() => (p.to_string(), v.to_string()),
            Some((p, _)) if !p.is_empty() && !p.contains(|c: char| !c.is_ascii_alphanumeric() && c != '_') => {
                bail!(
                    "unknown match property '{p}' (known: {})",
                    super::model::MATCH_PROPS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                )
            }
            _ => ("class".to_string(), tok.clone()),
        };
        let m = Matcher::new(&prop, &value);
        if let Some(e) = m.regex_error() {
            bail!("{prop}: bad regex `{value}`: {e}");
        }
        out.push(m);
    }
    Ok(out)
}

fn tokenize(s: &str) -> Vec<String> {
    let mut toks = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_quote = !in_quote,
            '\\' if in_quote => {
                if let Some(n) = chars.next() {
                    cur.push(n);
                }
            }
            c if c.is_whitespace() && !in_quote => {
                if !cur.is_empty() {
                    toks.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        toks.push(cur);
    }
    toks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_word_is_class() {
        let m = parse("kitty").unwrap();
        assert_eq!(m[0].prop, "class");
        assert_eq!(m[0].raw, "kitty");
    }

    #[test]
    fn quoted_values_keep_spaces() {
        let m = parse(r#"title:"Friends List" float:true"#).unwrap();
        assert_eq!(m[0].raw, "Friends List");
        assert_eq!(m[1].boolean, Some(true));
    }

    #[test]
    fn unknown_prop_errors() {
        assert!(parse("klass:foo").is_err());
    }
}
