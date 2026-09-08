import { LoaderCircle } from "lucide-react";

import type { View } from "../useDesktopModel";

const loadingCopy: Record<View, { title: string; detail: string }> = {
  home: {
    title: "Checking status",
    detail: "Connecting to the Agent Desktop daemon…",
  },
  tools: {
    title: "Discovering tools",
    detail: "Reading the local tool inventory…",
  },
  usage: {
    title: "Loading usage",
    detail: "Reading gateway usage…",
  },
};

export function StatusLoading({ view }: { view: View }) {
  const { title, detail } = loadingCopy[view];

  return (
    <section className="status-loading" role="status" aria-live="polite">
      <LoaderCircle className="spin" size={22} />
      <div>
        <h2>{title}</h2>
        <p>{detail}</p>
      </div>
    </section>
  );
}
