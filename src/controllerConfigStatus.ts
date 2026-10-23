import type { ConfigGetDto, LogLine } from "./types";

/** Log history is evidence, not a replacement for the controller's current recovery flag. */
export function recoveryErrors(config: ConfigGetDto): LogLine[] {
  if (!config.recovery_mode) return [];
  return (config.recovery_logs ?? [])
    .filter((line) => ["E", "ERROR", "F", "FATAL"].includes(line.level.toUpperCase()))
    .slice(-8)
    .reverse();
}
