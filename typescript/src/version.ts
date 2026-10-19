/**
 * The version of this package. A sidecar of another release is not used.
 *
 * @module
 */

/** The version, written down once, here. `package.json` has to match it. */
export const VERSION = "0.1.0";

/** The version a connection compares the sidecar's with. Only tests change
 * it, to stand for a package of another release. */
export const release = { version: VERSION };
