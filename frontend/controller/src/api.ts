import { useEffect, useState } from "react";

export function useApi<T>(path: string) {
  const [query, setQuery] = useState<{
    path: string;
    data: T | null;
    error: string | null;
    loading: boolean;
  }>({ path, data: null, error: null, loading: true });

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setQuery({ path, data: null, error: null, loading: true });
    fetch(path, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
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
  }, [path]);

  return query.path === path
    ? query
    : { path, data: null, error: null, loading: true };
}
