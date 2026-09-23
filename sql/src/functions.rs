//! The explicit SQL function registry — item 23: "do NOT invent
//! arbitrary SQL functions... unknown functions must fail during
//! binding... never execute arbitrary Rust/plugin code from SQL." A
//! small, deliberately minimal v1 set of pure, deterministic, scalar
//! functions — no aggregates (aggregation binding is out of this
//! increment's scope, `crate::convert`'s `GROUP BY` rejection), no
//! side effects, no function ever resolves to something other than a
//! fixed Rust match arm here (never a plugin/dynamic dispatch surface).

use rubixdb::relational::RelationalType;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgType {
    Text,
    AnyNumeric,
}

impl ArgType {
    pub fn accepts(self, ty: RelationalType) -> bool {
        match self {
            ArgType::Text => ty == RelationalType::Text,
            ArgType::AnyNumeric => matches!(
                ty,
                RelationalType::Integer
                    | RelationalType::Bigint
                    | RelationalType::Real
                    | RelationalType::Double
                    | RelationalType::Decimal { .. }
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnType {
    Fixed(RelationalType),
    /// The exact resolved type of argument `N` (preserves e.g. a
    /// `DECIMAL`'s own scale, which a `Fixed` entry could not).
    SameAsArg(usize),
}

/// One function's full contract — name, arity, per-position argument
/// type, return type, and `NULL` behavior, exactly the fields item 23
/// requires be documented. `determinism`/`authorization implications`
/// are trivially "always deterministic, no authorization implications
/// beyond the caller's own table-level grants on its arguments' source
/// columns" for every entry in this v1 registry (no function reads
/// catalog/session state), stated once here rather than as a redundant
/// per-entry field.
#[derive(Debug, Clone, Copy)]
pub struct FunctionDef {
    pub name: &'static str,
    pub arg_types: &'static [ArgType],
    pub return_type: ReturnType,
    /// `true`: any `NULL` argument makes the result `NULL` (skipping the
    /// function body entirely) — true for every entry in this registry;
    /// kept as an explicit field (not assumed) so a future function with
    /// different `NULL` behavior doesn't silently inherit the wrong
    /// default.
    pub null_propagates: bool,
}

pub static REGISTRY: &[FunctionDef] = &[
    FunctionDef {
        name: "length",
        arg_types: &[ArgType::Text],
        return_type: ReturnType::Fixed(RelationalType::Integer),
        null_propagates: true,
    },
    FunctionDef {
        name: "upper",
        arg_types: &[ArgType::Text],
        return_type: ReturnType::Fixed(RelationalType::Text),
        null_propagates: true,
    },
    FunctionDef {
        name: "lower",
        arg_types: &[ArgType::Text],
        return_type: ReturnType::Fixed(RelationalType::Text),
        null_propagates: true,
    },
    FunctionDef {
        name: "abs",
        arg_types: &[ArgType::AnyNumeric],
        return_type: ReturnType::SameAsArg(0),
        null_propagates: true,
    },
];

/// Case-insensitive-per-D14-identifier-rules lookup — function names are
/// ordinary identifiers, already case-folded to lowercase by `crate::
/// convert` unless quoted (a quoted function name will simply never
/// match anything here, which is correct: this registry's entries are
/// all-lowercase canonical names).
pub fn lookup(name: &str) -> Option<&'static FunctionDef> {
    REGISTRY.iter().find(|f| f.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_names_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for f in REGISTRY {
            assert_eq!(f.name, f.name.to_lowercase(), "{} must be lowercase", f.name);
            assert!(seen.insert(f.name), "duplicate function name {}", f.name);
        }
    }

    #[test]
    fn lookup_is_case_sensitive_to_the_canonical_lowercase_form() {
        assert!(lookup("length").is_some());
        assert!(lookup("LENGTH").is_none());
        assert!(lookup("not_a_real_function").is_none());
    }
}
