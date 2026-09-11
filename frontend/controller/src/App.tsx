import { useState } from "react";

import { useApi } from "./api";
import { ControllerShell } from "./components/ControllerShell";
import { DeviceUsage } from "./components/DeviceUsage";
import { ErrorState, NotFound, PageSkeleton } from "./components/ViewStates";
import { navigate, usePath } from "./router";
import type {
  ActiveDaemonConfig,
  ControllerSettings,
  Device,
  DeviceDetail,
  LlmFleetUsageSummary,
  LlmUsageRange,
  Overview,
} from "./types";
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
    return <DevicePage id={decodeURIComponent(path.slice(9))} />;
  }
  if (path === "/usage") return <UsagePage />;
  if (path === "/configuration") return <ConfigurationPage />;
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

function UsagePage() {
  const [range, setRange] = useState<LlmUsageRange>("day");
  const query = useApi<LlmFleetUsageSummary | null>(
    `/api/v1/usage?range=${range}`,
  );
  return (
    <UsageView
      usage={query.data}
      range={range}
      onRangeChange={setRange}
      error={query.error}
      loading={query.loading}
    />
  );
}

function DevicePage({ id }: { id: string }) {
  const query = useApi<DeviceDetail>(
    `/api/v1/devices/${encodeURIComponent(id)}`,
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
      usage={<DeviceUsage deviceId={id} />}
    />
  );
}

function ConfigurationPage() {
  const query = useApi<ActiveDaemonConfig>("/api/v1/daemon-config");
  return <ConfigurationView initialConfig={query.data?.config} />;
}

function SettingsPage() {
  const query = useApi<ControllerSettings>("/api/v1/settings");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  return <SettingsView data={query.data} />;
}
