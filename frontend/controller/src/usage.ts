import type { UsageDimension } from "@agentdesktop/ui";

/** Exact-match filters of the usage page, by dimension. */
export type UsageFilters = Partial<Record<UsageDimension, string>>;

/** Breakdown dimensions, in the order the usage page offers them. */
export const usageDimensions: ReadonlyArray<{
  value: UsageDimension;
  label: string;
}> = [
  { value: "user", label: "Users" },
  { value: "device", label: "Devices" },
  { value: "tool", label: "Tools" },
  { value: "model", label: "Models" },
  { value: "provider", label: "Providers" },
];
