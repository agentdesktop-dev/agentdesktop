import { CircleAlert, Laptop } from "lucide-react";

import { Link } from "../router";

export function EmptyDevices({ searching = false }: { searching?: boolean }) {
  return (
    <div className="empty-state">
      <Laptop size={28} />
      <h3>{searching ? "No matching devices" : "No devices enrolled"}</h3>
      <p>
        {searching
          ? "Try a different hostname, platform, or version."
          : "Devices will appear here after their first enrollment."}
      </p>
    </div>
  );
}

export function ErrorState({ message }: { message?: string | null }) {
  return (
    <div className="empty-state error">
      <CircleAlert size={28} />
      <h2>Couldn’t load controller data</h2>
      <p>{message || "The controller returned an unexpected response."}</p>
    </div>
  );
}

export function PageSkeleton({ rows = 3 }: { rows?: number }) {
  const skeletonRows = ["first", "second", "third", "fourth", "fifth", "sixth"];
  return (
    <div className="skeleton-card">
      {skeletonRows.slice(0, rows).map((row) => (
        <div className="skeleton-line" key={row} />
      ))}
    </div>
  );
}

export function NotFound() {
  return (
    <div className="empty-state">
      <CircleAlert size={28} />
      <h2>Page not found</h2>
      <Link href="/" className="button secondary">
        Return to overview
      </Link>
    </div>
  );
}
