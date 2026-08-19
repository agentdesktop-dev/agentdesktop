export function friendlyOs(os: string) {
  const names: Record<string, string> = {
    linux: "Linux",
    macos: "macOS",
    darwin: "macOS",
    windows: "Windows",
  };
  return names[os.toLowerCase()] ?? (os || "Unknown");
}

export function formatTime(timestamp: number | null) {
  if (!timestamp) return "Never";
  const delta = Math.max(0, Math.round(Date.now() / 1000 - timestamp));
  if (delta < 60) return `${delta}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

export function formatTimeMilliseconds(timestamp: number) {
  return formatTime(Math.floor(timestamp / 1000));
}

export function formatDate(timestamp: number) {
  return new Intl.DateTimeFormat(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  }).format(new Date(timestamp * 1000));
}
