/**
 * Every message of the broker's protocol, **generated** from `src/protocol.rs` by ts-rs and shipped here, so
 * a consumer reads the types without building Rust. `cargo xtask generate` re-derives it;
 * `cargo xtask check-generated` fails when it has fallen behind.
 */
export type * from './protocol.js';
