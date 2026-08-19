import { LoaderCircle } from "lucide-react";

import type { View } from "../useDesktopModel";

export function StatusLoading({ view }: { view: View }) {
  return (
    <section className="status-loading" role="status" aria-live="polite">
      <LoaderCircle className="spin" size={22} />
      <div>
        <h2>{view === "tools" ? "Discovering tools" : "Checking status"}</h2>
        <p>
          {view === "tools"
            ? "Reading the local tool inventory…"
            : "Connecting to the Agent Desktop daemon…"}
        </p>
      </div>
    </section>
  );
}
