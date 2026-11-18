import { useCallback, useEffect, useReducer, useRef, useState } from "react";

import { getDeviceUsage } from "./backend";
import type { DeviceUsage, UsageRange } from "./types";

const REFRESH_INTERVAL = 60_000;

type UsageResult = {
  range: UsageRange;
  usage: DeviceUsage;
};

type UsageRequestState =
  | { status: "idle"; result: UsageResult | null; error: string | null }
  | { status: "loading"; result: UsageResult | null; error: string | null }
  | { status: "ready"; result: UsageResult; error: null }
  | { status: "failed"; result: UsageResult | null; error: string };

type UsageRequestAction =
  | { type: "started" }
  | { type: "succeeded"; result: UsageResult }
  | { type: "failed"; error: string }
  | { type: "invalidated" };

function usageRequestReducer(
  state: UsageRequestState,
  action: UsageRequestAction,
): UsageRequestState {
  switch (action.type) {
    case "started":
      return { ...state, status: "loading" };
    case "succeeded":
      return { status: "ready", result: action.result, error: null };
    case "failed":
      return { status: "failed", result: state.result, error: action.error };
    case "invalidated":
      return { ...state, status: "idle" };
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** This device's usage for `range`, kept current while `active`. */
export function useDeviceUsage(active: boolean) {
  const [range, setRange] = useState<UsageRange>("7d");
  const [requestState, dispatch] = useReducer(usageRequestReducer, {
    status: "idle",
    result: null,
    error: null,
  });
  // Only the newest request may update state.
  const latest = useRef(0);

  const load = useCallback(async (nextRange: UsageRange) => {
    const request = ++latest.current;
    dispatch({ type: "started" });
    try {
      const next = await getDeviceUsage(
        nextRange,
        -new Date().getTimezoneOffset(),
      );
      if (request === latest.current) {
        dispatch({
          type: "succeeded",
          result: { range: nextRange, usage: next },
        });
      }
    } catch (reason: unknown) {
      if (request === latest.current) {
        dispatch({ type: "failed", error: errorMessage(reason) });
      }
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
      dispatch({ type: "invalidated" });
    };
  }, [active, range, load]);

  const usage =
    requestState.result?.range === range ? requestState.result.usage : null;

  return {
    error: requestState.error,
    loading: requestState.status === "loading",
    range,
    reload: () => load(range),
    setRange,
    usage,
  };
}
