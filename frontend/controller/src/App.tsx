import { useState } from "react";

import { useApi } from "./api";
import { ControllerShell } from "./components/ControllerShell";
import { ErrorState, NotFound, PageSkeleton } from "./components/ViewStates";
import { navigate, usePath } from "./router";
import type {
  ControllerSettings,
  Device,
  DeviceDetail,
  FleetConfigurationResponse,
  Overview,
} from "./types";
import { ConfigurationView } from "./views/ConfigurationView";
import { DevicesView } from "./views/DevicesView";
import { DeviceView } from "./views/DeviceView";
import { OverviewView } from "./views/OverviewView";
import { SettingsView } from "./views/SettingsView";

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
    />
  );
}

function ConfigurationPage() {
  const query = useApi<FleetConfigurationResponse>(
    "/api/v1/fleet-configuration",
  );
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;

  async function saveConfiguration(yaml: string, version: string) {
    const response = await fetch("/api/v1/fleet-configuration", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ yaml, version }),
    });
    if (response.status === 409) {
      throw new Error(
        "The fleet configuration changed on the server. Refresh the page and reapply your edits.",
      );
    }
    if (!response.ok) throw new Error(await response.text());
    return response.json() as Promise<FleetConfigurationResponse>;
  }

  return (
    <ConfigurationView
      initialYaml={query.data.yaml}
      initialRevision={query.data.revision}
      initialVersion={query.data.version}
      sourceError={query.data.sourceError}
      writable={query.data.writable}
      onSave={saveConfiguration}
    />
  );
}

function SettingsPage() {
  const query = useApi<ControllerSettings>("/api/v1/settings");
  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  return <SettingsView data={query.data} />;
}
