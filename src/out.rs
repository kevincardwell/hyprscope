//! Minimal ANSI styling for the plain CLI subcommands.

use std::io::IsTerminal;

pub fn color() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn wrap(code: &str, s: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn green(s: &str) -> String {
    wrap("32", s)
}
pub fn red(s: &str) -> String {
    wrap("31", s)
}
pub fn yellow(s: &str) -> String {
    wrap("33", s)
}
pub fn cyan(s: &str) -> String {
    wrap("36", s)
}
pub fn dim(s: &str) -> String {
    wrap("2", s)
}
pub fn bold(s: &str) -> String {
    wrap("1", s)
}
pub fn magenta(s: &str) -> String {
    wrap("35", s)
}

pub fn tick(ok: Option<bool>) -> String {
    match ok {
        Some(true) => green("✓"),
        Some(false) => red("✗"),
        None => yellow("?"),
    }
}
