import { useEffect, useState } from "react";

import { useAuth } from "./auth";

export function useApi<T>(path: string | null) {
  const { request } = useAuth();
  const [query, setQuery] = useState<{
    path: string | null;
    data: T | null;
    error: string | null;
    loading: boolean;
  }>({ path, data: null, error: null, loading: true });

  useEffect(() => {
    if (path === null) {
      setQuery({ path: null, data: null, error: null, loading: false });
      return;
    }
    const controller = new AbortController();
    let active = true;
    setQuery({ path, data: null, error: null, loading: true });
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
          setQuery({
            path,
            data: null,
            error: reason.message || "Request failed",
            loading: false,
          });
        }
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [path, request]);

  return query.path === path
    ? query
    : { path, data: null, error: null, loading: path !== null };
}
