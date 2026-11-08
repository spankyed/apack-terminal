/**
 * Every message of the broker's protocol, **generated** from `src/protocol.rs` by ts-rs and shipped here, so
 * a consumer reads the types without building Rust. `cargo xtask generate` re-derives it;
 * `cargo xtask check-generated` fails when it has fallen behind.
 */
export type * from './protocol.js';

/** What `cargo` names the broker on this platform: `apack-terminal`, or `apack-terminal.exe` on Windows */
export declare const BINARY_NAME: string;

/**
 * Where the binary would be, whether or not it is there.
 *
 * Separate from `brokerBinary` so a caller's refusal can name what it looked for: a function that answers
 * only when the file exists cannot be asked that.
 */
export declare function brokerBinaryPath(): string;

/**
 * The broker binary this package ships, or `undefined` when it holds none.
 *
 * A checkout that has not run `cargo xtask package` holds none, and that is an absence rather than an
 * error: in a development checkout the broker is the debug build in `target/`, which the consumer finds
 * for itself.
 */
export declare function brokerBinary(): string | undefined;
