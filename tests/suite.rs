//! The broker's suite, as one program.
//!
//! **Each file beside this one is a module of it rather than a test program of its own.** Cargo builds
//! every file directly under `tests/` as a separate binary, and each of those links the whole library:
//! thirty links for one changed line, which a mutation run pays once a mutant. The cases are unchanged
//! and are still run a process each (`.config/nextest.toml`); what is one is the linking.
//!
//! A file that drives something only POSIX has is compiled only there, which is said here rather than
//! in the file: an attribute on a whole file can only be written in the root of a program.

mod broker;
#[cfg(unix)]
mod candidates;
#[cfg(unix)]
mod channel;
mod coalescing;
mod command_rows;
mod commands;
mod conformance;
mod core_features;
mod deadlines;
mod effects;
mod feed;
mod frames;
mod holds;
mod housekeeping;
mod images;
#[cfg(unix)]
mod keeping;
mod markers;
mod parser;
#[cfg(unix)]
mod proxy;
mod reaper;
mod recording;
mod screen;
mod security;
#[cfg(unix)]
mod signals;
#[cfg(unix)]
mod view_broker;
mod view_frames;
mod view_input;
mod view_model;
mod view_reads;
mod view_sink;
mod view_vectors;
mod view_wire;
mod vouched;
mod vt;
mod writer;
