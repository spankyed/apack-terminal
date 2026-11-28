//! The repository's own tasks, as a library so each can be driven without the binary.

pub mod bench;
pub mod cfg_hygiene;
pub mod fuzz_smoke;
pub mod generate;
pub mod leftovers;
pub mod mutants;
pub mod notices;
pub mod pre_push;
pub mod safety;
pub mod versions;
pub mod vt_prebuilt;
pub mod windows_check;
pub mod wire_codegen;
