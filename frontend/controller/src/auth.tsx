import {
  createContext,
  type ReactNode,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";

import type { AdminSession } from "./types";

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
  ) {
    super(
      code === "reauthentication_required"
        ? "Sign in again before performing this action."
        : status === 403
          ? "You do not have permission to perform this action."
          : status === 401
            ? "Your session has ended. Sign in again."
            : status === 503
              ? "The identity provider is unavailable. Try again shortly."
              : code,
    );
  }
}

type AuthContextValue = {
  session: AdminSession | null;
  loading: boolean;
  error: string | null;
  notice: string | null;
  clearNotice: () => void;
  retry: () => void;
  logout: () => Promise<void>;
  /** `quiet` suppresses the global notice for background requests. */
  request: (
    path: string,
    init?: RequestInit,
    quiet?: boolean,
  ) => Promise<Response>;
};

const AuthContext = createContext<AuthContextValue | null>(null);

const signedOut: AdminSession = {
  mode: "authenticated",
  authenticated: false,
  role: null,
  subject: null,
  assurance: null,
  recent_auth_required: false,
};

async function fetchSession(signal?: AbortSignal): Promise<AdminSession> {
  const response = await fetch("/auth/session", {
    credentials: "same-origin",
    cache: "no-store",
    signal,
  });
  if (response.status === 401) return signedOut;
  if (!response.ok) {
    const body = await response.json().catch(() => null);
    throw new ApiError(
      response.status,
      typeof body?.error === "string"
        ? body.error
        : "Unable to check your session.",
    );
  }
  return (await response.json()) as AdminSession;
}

export function useAuth() {
  const auth = useContext(AuthContext);
  if (!auth) throw new Error("AuthProvider is required");
  return auth;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const [session, setSession] = useState<AdminSession | null>(null);
  const sessionRef = useRef(session);
  const generation = useRef(0);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [attempt, setAttempt] = useState(0);

  const replaceSession = useCallback((next: AdminSession | null) => {
    generation.current += 1;
    sessionRef.current = next;
    setSession(next);
  }, []);

  const expireSession = useCallback(() => {
    const current = sessionRef.current;
    if (!current?.authenticated) return;
    replaceSession({
      ...current,
      authenticated: false,
      role: null,
      permissions: [],
      subject: null,
      assurance: null,
    });
    setNotice(null);
  }, [replaceSession]);

  // biome-ignore lint/correctness/useExhaustiveDependencies: attempt explicitly retries a failed session check.
  useEffect(() => {
    const controller = new AbortController();
    const started = generation.current;
    setLoading(true);
    setError(null);
    fetchSession(controller.signal)
      .then((next) => {
        if (!controller.signal.aborted && generation.current === started) {
          replaceSession(next);
          setLoading(false);
        }
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted && generation.current === started) {
          setError(reason instanceof Error ? reason.message : "Session failed");
          setLoading(false);
        }
      });
    return () => controller.abort();
  }, [attempt, replaceSession]);

  const request = useCallback(
    async (path: string, init: RequestInit = {}, quiet = false) => {
      const current = sessionRef.current;
      const started = generation.current;
      if (!current?.authenticated) throw new ApiError(401, "unauthenticated");
      const url = new URL(path, window.location.origin);
      if (url.origin !== window.location.origin) {
        throw new Error("Admin requests must be same-origin.");
      }
      // The server rejects cross-origin mutations from Origin/Fetch Metadata,
      // which the browser sets, so no anti-CSRF token is needed.
      const response = await fetch(url, {
        ...init,
        credentials: "same-origin",
      });
      // Do not let an older request change or repopulate a newer session.
      if (generation.current !== started) {
        throw new DOMException("Session changed", "AbortError");
      }
      if (response.status === 401) expireSession();
      if (!response.ok) {
        const body = await response.json().catch(() => null);
        const failure = new ApiError(
          response.status,
          typeof body?.error === "string" ? body.error : "Request failed",
        );
        if (
          !quiet &&
          generation.current === started &&
          response.status === 403 &&
          failure.code !== "reauthentication_required"
        ) {
          setNotice(failure.message);
        }
        throw failure;
      }
      return response;
    },
    [expireSession],
  );

  useEffect(() => {
    if (!session?.authenticated || session.mode === "local") return;
    let lastActivity: number | null = null;
    let pending = false;
    const activity = (event: Event) => {
      if (
        !event.isTrusted ||
        document.visibilityState !== "visible" ||
        !document.hasFocus() ||
        pending
      )
        return;
      const now = performance.now();
      if (lastActivity !== null && now - lastActivity < 60_000) return;
      lastActivity = now;
      pending = true;
      void request("/auth/activity", { method: "POST" }, true)
        .catch(() => {
          // A 401 is handled centrally; background failures never raise notices.
        })
        .finally(() => {
          pending = false;
        });
    };
    const events = [
      "pointerdown",
      "pointermove",
      "keydown",
      "wheel",
      "touchstart",
    ];
    for (const event of events)
      document.addEventListener(event, activity, { passive: true });
    return () => {
      for (const event of events) document.removeEventListener(event, activity);
    };
  }, [request, session]);

  const logout = useCallback(async () => {
    try {
      await request("/auth/logout", { method: "POST" });
      expireSession();
    } catch (reason) {
      if (
        reason instanceof Error &&
        reason.name !== "AbortError" &&
        !(reason instanceof ApiError && reason.status === 401)
      ) {
        setNotice(reason.message);
      }
    }
  }, [expireSession, request]);

  const value = useMemo(
    () => ({
      session,
      loading,
      error,
      notice,
      request,
      logout,
      retry: () => setAttempt((value) => value + 1),
      clearNotice: () => setNotice(null),
    }),
    [session, loading, error, notice, request, logout],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
