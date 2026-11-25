import { useEffect, useState } from "react";

import { useAuth } from "./auth";

export function useApi<T>(path: string | null, refresh = 0) {
  const { request } = useAuth();
  const [query, setQuery] = useState<{
    path: string | null;
    data: T | null;
    error: string | null;
    loading: boolean;
  }>({ path, data: null, error: null, loading: true });

  // biome-ignore lint/correctness/useExhaustiveDependencies: refresh triggers a refetch of the same path.
  useEffect(() => {
    if (path === null) {
      setQuery({ path: null, data: null, error: null, loading: false });
      return;
    }
    const controller = new AbortController();
    let active = true;
    setQuery((current) =>
      current.path === path && current.data !== null
        ? { ...current, error: null }
        : { path, data: null, error: null, loading: true },
    );
    request(path, { signal: controller.signal })
      .then(async (response) => {
        return response.json() as Promise<T>;
      })
      .then((response) => {
        if (active) {
          setQuery({ path, data: response, error: null, loading: false });
        }
      })
      .catch((reason: Error) => {
        if (active && reason.name !== "AbortError") {
          setQuery((current) => ({
            path,
            data: current.path === path ? current.data : null,
            error: reason.message || "Request failed",
            loading: false,
          }));
        }
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [path, refresh, request]);

  return query.path === path
    ? query
    : { path, data: null, error: null, loading: path !== null };
}
