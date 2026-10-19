//! Finding text by what it means.
//!
//! A [`Model`] turns a piece of text into a list of numbers, its vector, so
//! that texts that mean much the same get vectors that point the same way. An
//! [`Index`] keeps the vectors of many texts and finds the ones closest to a
//! question's.
//!
//! The model the sidecar uses is potion-base-8M, made by Minish Lab and free
//! to use under the MIT licence: a table of 256 numbers for each of about
//! 30,000 pieces of English words, 30 MB in all. A text's vector is the
//! average of its pieces'. That takes well under a thousandth of a second for
//! a short text and needs no other program, at the price of missing what a
//! large model would catch from the order of the words.

mod index;
mod model;
pub mod tiny;

pub use index::{Found, Index, Missing, Prepared};
pub use model::{fetch, Model, Pinned, POTION, POTION_SOURCE};

// Runs the code examples in README.md as part of `cargo test`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeExamples;
