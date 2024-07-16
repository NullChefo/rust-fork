//! This crates defines the type inference engine.
//!
//! - **Type inference.** The type inference code can be found in the `infer` module;
//!   this code handles low-level equality and subtyping operations. The
//!   type check pass in the compiler is found in the `rustc_hir_analysis` crate.
//!
//! For more information about how rustc works, see the [rustc dev guide].
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/
//!
//! # Note
//!
//! This API is completely unstable and subject to change.

// tidy-alphabetical-start
#![allow(internal_features)]
#![allow(rustc::diagnostic_outside_of_impl)]
#![allow(rustc::untranslatable_diagnostic)]
#![doc(html_root_url = "https://doc.rust-lang.org/nightly/nightly-rustc/")]
#![doc(rust_logo)]
#![feature(box_patterns)]
#![feature(control_flow_enum)]
#![feature(extend_one)]
#![feature(if_let_guard)]
#![feature(iter_intersperse)]
#![feature(iterator_try_collect)]
#![feature(let_chains)]
#![feature(rustdoc_internals)]
#![feature(try_blocks)]
#![feature(yeet_expr)]
#![recursion_limit = "512"] // For rustdoc
// tidy-alphabetical-end

#[macro_use]
extern crate tracing;

pub mod error_reporting;
mod errors;
pub mod infer;
pub mod traits;

rustc_fluent_macro::fluent_messages! { "../messages.ftl" }
