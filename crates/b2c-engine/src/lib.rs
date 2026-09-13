//! b2cFastCopy engine.
//!
//! A bulk copier built around one idea: RAM is an elastic buffer between a fast
//! side and a slow side, and the scheduler's job is to keep whichever device is
//! currently the bottleneck completely busy.
//!
//! See the crate's README for the full design and the reasoning behind it.

#![cfg(windows)]

pub mod arena;
pub mod device;
pub mod engine;
pub mod error;
pub mod fmt;
pub mod progress;
pub mod rings;
pub mod scanner;
pub mod scheduler;
pub mod stats;
pub mod win;

pub use engine::{Copier, Options, Outcome};
pub use error::{Error, Result};
