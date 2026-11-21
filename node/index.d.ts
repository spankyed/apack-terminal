/**
 * Every message of the broker's protocol, **generated** from `src/protocol.rs` by ts-rs and shipped here, so
 * a consumer reads the types without building Rust. `cargo xtask generate` re-derives it;
 * `cargo xtask check-generated` fails when it has fallen behind.
 */
export type * from './protocol.js';

/** Node's `${process.platform}-${process.arch}` for this process, which is how the platform packages are named */
export declare const PLATFORM: string;

/** What the broker's binary is called on this platform: `apack-terminal.exe` on Windows, else `apack-terminal` */
export declare const BINARY_NAME: string;

/** The package holding this platform's broker, `@apackdev/terminal-<PLATFORM>`, whether or not one is built for it */
export declare const PACKAGE_NAME: string;

/** The apack-terminal commit whose release this package was packed for; `undefined` when packed from a checkout */
export declare const commit: string | undefined;

/**
 * The broker binary's path, or `undefined` when it is not installed: no package for this platform is installed,
 * or not every file that package lists is there.
 */
export declare function brokerBinary(): string | undefined;

/** Where the binary is in this platform's installed package, whether or not the file is; `undefined` with no package */
export declare function brokerBinaryPath(): string | undefined;

/** The directory holding this platform's files (the binary and what ships beside it); `undefined` with no package */
export declare function brokerDirectory(): string | undefined;
