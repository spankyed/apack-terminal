//! What the linked core was built with, checked once so that a missing feature is a failed case and not a
//! request that answers `UNSUPPORTED` in front of a user.
//!
//! The library is a prebuilt archive (`cargo xtask vt-prebuilt`). A feature compiled out of it still links,
//! and each of its calls refuses at run time, so nothing but asking says what the archive holds.

use apack_terminal::vt::features;

/// **Every feature the broker is written against is in the build it links.** Rows for a view come from the
/// render state, text from the formatter, a word under the pointer from selection, find from search, keys
/// and the mouse from the input encoders, command output from grid introspection, a restored session from a
/// snapshot, and an inline image from Kitty graphics.
#[test]
fn the_linked_core_has_every_feature_the_broker_uses() {
    let built = features();

    assert!(built.render_state, "no render state: {built:?}");
    assert!(built.formatter, "no formatter: {built:?}");
    assert!(built.selection, "no selection: {built:?}");
    assert!(built.search, "no search: {built:?}");
    assert!(built.input_encode, "no input encoding: {built:?}");
    assert!(built.grid_introspection, "no grid introspection: {built:?}");
    assert!(built.snapshot, "no snapshots: {built:?}");
    assert!(built.kitty_graphics, "no Kitty graphics: {built:?}");
}
