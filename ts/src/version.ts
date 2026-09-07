import { createRequire } from "node:module";

/**
 * The running version, read from the package manifest so the banner and the
 * build metric cannot disagree. The manifest sits two levels up from `ts/src`
 * and from the compiled `ts/dist` alike, so the same path works in both.
 */
export const VERSION = (createRequire(import.meta.url)("../../package.json") as { version: string })
  .version;
