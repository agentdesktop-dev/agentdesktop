import { useEffect, useState } from "react";

export function useApi<T>(path: string) {
  const [data, setData] = useState<T | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  useEffect(() => {
    const controller = new AbortController();
    let active = true;
    setLoading(true);
    fetch(path, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) throw new Error(await response.text());
        return response.json() as Promise<T>;
      })
      .then((response) => {
        if (active) setData(response);
      })
      .catch((reason: Error) => {
        if (active && reason.name !== "AbortError") {
          setError(reason.message || "Request failed");
        }
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
      controller.abort();
    };
  }, [path]);

  return { data, error, loading };
}
