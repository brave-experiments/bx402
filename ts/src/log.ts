/** Severity levels, ordered from least to most verbose. */
const LEVELS = ["error", "warn", "info", "debug", "trace"] as const;

export type Level = (typeof LEVELS)[number];

/** Level used when `LOG_LEVEL` is unset or is not one of the level names. */
const DEFAULT_LEVEL: Level = "info";

let threshold = LEVELS.indexOf(DEFAULT_LEVEL);

/**
 * Set the log level from `LOG_LEVEL`. An unrecognised value falls back to the
 * default rather than failing startup, so a typo cannot take the service down.
 */
export function initLogging(value = process.env.LOG_LEVEL): void {
  const level = LEVELS.find((name) => name === value?.trim().toLowerCase());
  threshold = LEVELS.indexOf(level ?? DEFAULT_LEVEL);
}

/**
 * Write one line to stderr, so logs never mix into a response body and stay
 * separate from anything the process prints on stdout.
 */
function emit(level: Level, message: string): void {
  if (LEVELS.indexOf(level) > threshold) {
    return;
  }
  const stamp = new Date().toISOString();
  process.stderr.write(`${stamp} ${level.toUpperCase().padStart(5)} ${message}\n`);
}

export const log = {
  error: (message: string) => emit("error", message),
  warn: (message: string) => emit("warn", message),
  info: (message: string) => emit("info", message),
  debug: (message: string) => emit("debug", message),
  trace: (message: string) => emit("trace", message),
};
