import {
  type LlmUsageInteractions,
  type LlmUsageRange,
  type LlmUsageSummary,
  UsageReport,
} from "@agentdesktop/ui";
import { useEffect, useState } from "react";

import { useApi } from "../api";
import { ErrorState, PageSkeleton } from "./ViewStates";

/** Model and agent breakdown for one enrolled device, scoped by the controller. */
export function DeviceUsage({ deviceId }: { deviceId: string }) {
  const [range, setRange] = useState<LlmUsageRange>("day");
  const encodedId = encodeURIComponent(deviceId);
  const query = useApi<LlmUsageSummary | null>(
    `/api/v1/devices/${encodedId}/usage?range=${range}`,
  );
  const [loadedRange, setLoadedRange] = useState<LlmUsageRange>(range);
  useEffect(() => {
    if (!query.loading) setLoadedRange(range);
  }, [query.loading, range]);

  if (query.error) return <ErrorState message={query.error} />;
  if (query.loading && !query.data) return <PageSkeleton rows={3} />;

  return (
    <div className="device-usage">
      <UsageReport
        llmUsage={query.data}
        range={range}
        loadedRange={loadedRange}
        isRangeLoading={query.loading}
        onRangeChange={setRange}
        onLoadInteractions={(from, to, model, agent, cursor) =>
          loadInteractions(encodedId, { from, to, model, agent }, cursor)
        }
        title="Device usage"
        unavailableMessage="Usage reporting is not configured on this controller."
      />
    </div>
  );
}

async function loadInteractions(
  encodedId: string,
  filters: Record<"from" | "to" | "model" | "agent", string>,
  cursor?: string | null,
): Promise<LlmUsageInteractions | null> {
  const params = new URLSearchParams(filters);
  if (cursor) params.set("cursor", cursor);
  const response = await fetch(
    `/api/v1/devices/${encodedId}/usage/interactions?${params}`,
  );
  if (!response.ok) throw new Error(await response.text());
  return (await response.json()) as LlmUsageInteractions | null;
}
