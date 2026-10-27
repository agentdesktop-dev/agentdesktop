import type { UsageDimension } from "@agentdesktop/ui";
import { useEffect, useState } from "react";

import { useApi } from "./api";
import { ControllerShell } from "./components/ControllerShell";
import { ErrorState, NotFound, PageSkeleton } from "./components/ViewStates";
import { navigate, usePath } from "./router";
import type {
  ActiveDaemonConfig,
  ControllerSettings,
  Device,
  DeviceDetail,
  Overview,
  UsageRange,
  UsageReport,
} from "./types";
import { type UsageFilters, usageDimensions } from "./usage";
import { ConfigurationView } from "./views/ConfigurationView";
import { DevicesView } from "./views/DevicesView";
import { DeviceView } from "./views/DeviceView";
import { OverviewView } from "./views/OverviewView";
import { SettingsView } from "./views/SettingsView";
import { UsageView } from "./views/UsageView";

export function App() {
  const path = usePath();

  return (
    <ControllerShell path={path} onRefresh={() => window.location.reload()}>
      <ControllerRoute path={path} />
    </ControllerShell>
  );
}

function ControllerRoute({ path }: { path: string }) {
  if (path === "/") return <OverviewPage />;
  if (path === "/devices") return <DevicesPage />;
  if (path.startsWith("/devices/")) {
    const id = decodeURIComponent(path.slice(9));
    // A new device starts without the previous device's data.
    return <DevicePage key={id} id={id} />;
  }
  if (path === "/configuration") return <ConfigurationPage />;
  if (path === "/usage") return <UsagePage />;
  if (path === "/settings") return <SettingsPage />;
  return <NotFound />;
}

function OverviewPage() {
  const query = useApi<Overview>("/api/v1/overview");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  return <OverviewView data={query.data} />;
}

function DevicesPage() {
  const query = useApi<Device[]>("/api/v1/devices");
  return (
    <DevicesView
      devices={query.data ?? []}
      error={query.error}
      loading={query.loading}
    />
  );
}

function DevicePage({ id }: { id: string }) {
  const query = useApi<DeviceDetail>(
    `/api/v1/devices/${encodeURIComponent(id)}`,
  );
  const usage = useApi<UsageReport>(
    `/api/v1/usage?${new URLSearchParams({
      range: "30d",
      by: "tool,model",
      device: id,
      utcOffsetMinutes: String(utcOffsetMinutes()),
    })}`,
  );
  const [showDelete, setShowDelete] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [deleteError, setDeleteError] = useState<string | null>(null);

  async function deleteDevice() {
    setDeleting(true);
    setDeleteError(null);
    try {
      const response = await fetch(
        `/api/v1/devices/${encodeURIComponent(id)}`,
        { method: "DELETE" },
      );
      if (!response.ok) throw new Error(await response.text());
      navigate("/devices");
    } catch (error) {
      setDeleteError(error instanceof Error ? error.message : "Delete failed");
      setDeleting(false);
    }
  }

  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  return (
    <DeviceView
      deleteError={deleteError}
      deleteOpen={showDelete}
      deleting={deleting}
      device={query.data}
      onDeleteCancel={() => {
        if (!deleting) {
          setShowDelete(false);
          setDeleteError(null);
        }
      }}
      onDeleteConfirm={deleteDevice}
      onDeleteRequest={() => setShowDelete(true)}
      usage={usage.data}
    />
  );
}

function utcOffsetMinutes() {
  return -new Date().getTimezoneOffset();
}

const usageRanges: UsageRange[] = ["24h", "7d", "30d", "90d"];

type UsageSelection = {
  dimension: UsageDimension;
  filters: UsageFilters;
  range: UsageRange;
};

/** The page's selection lives in the query string so it can be linked. */
function readUsageSelection(search: string): UsageSelection {
  const params = new URLSearchParams(search);
  const range = params.get("range");
  const group = params.get("group");
  const filters: UsageFilters = {};
  for (const { value } of usageDimensions) {
    const filter = params.get(value);
    if (filter !== null) filters[value] = filter;
  }
  return {
    dimension:
      usageDimensions.find((option) => option.value === group)?.value ?? "user",
    filters,
    range: usageRanges.find((option) => option === range) ?? "7d",
  };
}

function withFilters(params: URLSearchParams, filters: UsageFilters) {
  for (const [dimension, value] of Object.entries(filters)) {
    params.set(dimension, value);
  }
  return params;
}

function UsagePage() {
  const [selection, setSelection] = useState(() =>
    readUsageSelection(window.location.search),
  );
  const { dimension, filters, range } = selection;
  const pageSearch = withFilters(
    new URLSearchParams({ range, group: dimension }),
    filters,
  ).toString();
  const query = useApi<UsageReport>(
    `/api/v1/usage?${withFilters(
      new URLSearchParams({
        range,
        by: usageDimensions.map(({ value }) => value).join(","),
        utcOffsetMinutes: String(utcOffsetMinutes()),
      }),
      filters,
    )}`,
  );

  useEffect(() => {
    window.history.replaceState(null, "", `/usage?${pageSearch}`);
  }, [pageSearch]);

  return (
    <UsageView
      dimension={dimension}
      error={query.error}
      filters={filters}
      loading={query.loading}
      onDimensionChange={(next) =>
        setSelection((current) => ({ ...current, dimension: next }))
      }
      onFiltersChange={(next) =>
        setSelection((current) => ({ ...current, filters: next }))
      }
      onRangeChange={(next) =>
        setSelection((current) => ({ ...current, range: next }))
      }
      range={range}
      report={query.data}
    />
  );
}

function ConfigurationPage() {
  const query = useApi<ActiveDaemonConfig>("/api/v1/daemon-config");
  return <ConfigurationView initialConfig={query.data?.config} />;
}

function SettingsPage() {
  const query = useApi<ControllerSettings>("/api/v1/settings");
  return (
    <SettingsView
      data={query.data}
      loading={query.loading}
      error={query.error}
    />
  );
}
