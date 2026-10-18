import { useCallback, useEffect, useRef, useState } from "react";

import { getDeviceUsage } from "./backend";
import type { DeviceUsage, UsageRange } from "./types";

const REFRESH_INTERVAL = 60_000;

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** This device's usage for `range`, kept current while `active`. */
export function useDeviceUsage(active: boolean) {
  const [range, setRange] = useState<UsageRange>("7d");
  const [result, setResult] = useState<{
    range: UsageRange;
    usage: DeviceUsage;
  } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  // Only the newest request may update state.
  const latest = useRef(0);

  const load = useCallback(async (nextRange: UsageRange) => {
    const request = ++latest.current;
    setLoading(true);
    try {
      const next = await getDeviceUsage(
        nextRange,
        -new Date().getTimezoneOffset(),
      );
      if (request === latest.current) {
        setResult({ range: nextRange, usage: next });
        setError(null);
      }
    } catch (reason: unknown) {
      if (request === latest.current) setError(errorMessage(reason));
    } finally {
      if (request === latest.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    if (!active) return;
    load(range);
    const interval = window.setInterval(() => {
      if (!document.hidden) load(range);
    }, REFRESH_INTERVAL);
    return () => {
      window.clearInterval(interval);
      latest.current += 1;
      setLoading(false);
    };
  }, [active, range, load]);

  const usage = result?.range === range ? result.usage : null;

  return {
    error,
    loading,
    range,
    reload: () => load(range),
    setRange,
    usage,
  };
}
