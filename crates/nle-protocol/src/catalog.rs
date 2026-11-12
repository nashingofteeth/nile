//! The wire-catalog macro.
//!
//! Rust has no reflection over enum variants, so a `macro_rules!` template is
//! the single-source tool: the same token list becomes the enum, its serde
//! attributes, `method()`, and the public name table. Add a method in one
//! place and the compiler plus the catalog tests force every dependent piece
//! to follow.
//!
//! It is internal: `wire_catalog!` is not exported, and only [`crate::Call`]
//! and [`crate::Event`] use it.

/// One `method()` match pattern. A unit variant and a struct variant need
/// different patterns, and a macro cannot expand to a whole match arm, but it
/// can expand to a pattern.
macro_rules! wire_pattern {
    ($name:ident, $variant:ident,) => {
        $name::$variant
    };
    ($name:ident, $variant:ident, { $($field:tt)* }) => {
        $name::$variant { .. }
    };
}

/// Generate a JSON-RPC method catalog.
///
/// ```text
/// wire_catalog! {
///     pub enum Call, methods: METHODS {
///         ProjectShow = "project.show";
///         Split { track: usize, at: f64 } = "split";
///     }
/// }
/// ```
///
/// Expands to the enum (`tag = "method"`, `content = "params"`), a
/// `method()` match, and `METHODS: &[&str]`. Fields are kept as raw tokens so
/// field-level serde attributes (for example `with = "…"`) pass through
/// unchanged.
macro_rules! wire_catalog {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident, methods: $methods:ident {
            $(
                $(#[$variant_meta:meta])*
                $variant:ident $({ $($field:tt)* })?
                    = $wire:literal;
            )*
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
        #[serde(tag = "method", content = "params", deny_unknown_fields)]
        $vis enum $name {
            $(
                $(#[$variant_meta])*
                #[serde(rename = $wire)]
                $variant $({ $($field)* })?
            ,)*
        }

        impl $name {
            /// The JSON-RPC method name.
            $vis fn method(&self) -> &'static str {
                match self {
                    $( wire_pattern!($name, $variant, $({ $($field)* })?) => $wire, )*
                }
            }
        }

        /// Every method name, generated from the same list as the enum.
        $vis const $methods: &'static [&'static str] = &[$($wire),*];
    };
}
