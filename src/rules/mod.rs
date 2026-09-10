//! Window-rule model and an evaluator that reproduces Hyprland's matching.

pub mod effects;
pub mod eval;
pub mod expr;
pub mod model;
pub mod workspace;

pub use eval::{evaluate, Facts, Report};
pub use model::RuleSet;
