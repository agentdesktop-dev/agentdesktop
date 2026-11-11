export function SessionScreen({
  loading,
  error,
  onRetry,
}: {
  loading: boolean;
  error: string | null;
  onRetry: () => void;
}) {
  return (
    <main className="session-screen">
      <section className="card session-card">
        <h1>Agentdesktop administration</h1>
        {loading ? (
          <p role="status">Checking your session…</p>
        ) : error ? (
          <>
            <p role="alert">{error}</p>
            <button
              type="button"
              className="button secondary"
              onClick={onRetry}
            >
              Try again
            </button>
          </>
        ) : (
          <>
            <p>Sign in with your organization to access the controller.</p>
            <a className="button primary" href="/auth/login">
              Sign in
            </a>
          </>
        )}
      </section>
    </main>
  );
}
