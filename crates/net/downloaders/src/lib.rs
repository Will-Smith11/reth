#![warn(missing_docs, unreachable_pub)]
#![deny(unused_must_use, rust_2018_idioms)]
#![doc(test(
    no_crate_inject,
    attr(deny(warnings, rust_2018_idioms), allow(dead_code, unused_variables))
))]

//! Implements the downloader algorithms.

/// The collection of algorithms for downloading block bodies.
pub mod bodies;

/// The collection of alhgorithms for downloading block headers.
pub mod headers;

#[cfg(test)]
mod test_utils;
