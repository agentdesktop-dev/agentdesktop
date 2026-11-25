import type { UsageDimension } from "@agentdesktop/ui";
import { useEffect, useRef, useState } from "react";

import { useApi } from "./api";
import { ApiError, AuthProvider, useAuth } from "./auth";
import { ControllerShell } from "./components/ControllerShell";
import { SessionScreen } from "./components/SessionScreen";
import { ErrorState, NotFound, PageSkeleton } from "./components/ViewStates";
import type { ControllerExtension } from "./extensions";
import { navigate, usePath } from "./router";
import type {
  ActiveDaemonConfig,
  ControllerSettings,
  DaemonConfigRevision,
  DaemonConfigRevisionList,
  DaemonConfigRevisionSummary,
  Device,
  DeviceDetail,
  Overview,
  UsageRange,
  UsageReport,
} from "./types";
import { type UsageFilters, usageDimensions } from "./usage";
import { ConfigurationView } from "./views/ConfigurationView";
import { DatabaseConfigurationEditor } from "./views/DatabaseConfigurationEditor";
import { DevicesView } from "./views/DevicesView";
import { DeviceView } from "./views/DeviceView";
import { OverviewView } from "./views/OverviewView";
import { SettingsView } from "./views/SettingsView";
import { UsageView } from "./views/UsageView";

export function App({
  extensions = [],
}: {
  extensions?: ControllerExtension[];
}) {
  return (
    <AuthProvider>
      <AuthenticatedApp extensions={extensions} />
    </AuthProvider>
  );
}

function AuthenticatedApp({
  extensions,
}: {
  extensions: ControllerExtension[];
}) {
  const path = usePath();
  const auth = useAuth();

  if (auth.loading || auth.error || !auth.session?.authenticated) {
    return (
      <SessionScreen
        loading={auth.loading}
        error={auth.error}
        onRetry={auth.retry}
      />
    );
  }

  return (
    <ControllerShell
      path={path}
      session={auth.session}
      onLogout={auth.logout}
      onRefresh={() => window.location.reload()}
      extensions={extensions.filter((extension) =>
        auth.session?.permissions?.includes(extension.permission),
      )}
    >
      {auth.notice && (
        <div className="auth-notice" role="alert">
          <span>{auth.notice}</span>
          <button
            type="button"
            className="button secondary"
            onClick={auth.clearNotice}
          >
            Dismiss
          </button>
        </div>
      )}
      <ControllerRoute path={path} extensions={extensions} />
    </ControllerShell>
  );
}

function ControllerRoute({
  path,
  extensions,
}: {
  path: string;
  extensions: ControllerExtension[];
}) {
  const { session } = useAuth();
  const extension = extensions.find((extension) => extension.path === path);
  if (extension) {
    if (!session?.permissions?.includes(extension.permission)) {
      return (
        <ErrorState message="You do not have permission to access this page." />
      );
    }
    return <extension.component />;
  }
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
  const { request, session } = useAuth();
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
  const [reauthenticate, setReauthenticate] = useState(false);
  const deletion = useRef<AbortController | null>(null);

  useEffect(() => () => deletion.current?.abort(), []);

  async function deleteDevice() {
    if (deletion.current) return;
    const controller = new AbortController();
    deletion.current = controller;
    setDeleting(true);
    setDeleteError(null);
    setReauthenticate(false);
    try {
      await request(`/api/v1/devices/${encodeURIComponent(id)}`, {
        method: "DELETE",
        signal: controller.signal,
      });
      if (!controller.signal.aborted) navigate("/devices");
    } catch (error) {
      if (
        controller.signal.aborted ||
        (error instanceof Error && error.name === "AbortError")
      )
        return;
      setDeleteError(error instanceof Error ? error.message : "Delete failed");
      setReauthenticate(
        error instanceof ApiError && error.code === "reauthentication_required",
      );
      setDeleting(false);
    } finally {
      if (deletion.current === controller) deletion.current = null;
    }
  }

  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  return (
    <DeviceView
      canDelete={
        session?.permissions
          ? session.permissions.includes("devices.delete")
          : session?.role === "operator"
      }
      deleteError={deleteError}
      reauthenticate={reauthenticate}
      deleteOpen={showDelete}
      deleting={deleting}
      device={query.data}
      onDeleteCancel={() => {
        if (!deleting) {
          setShowDelete(false);
          setDeleteError(null);
          setReauthenticate(false);
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
    window.history.replaceState(
      window.history.state,
      "",
      `/usage?${pageSearch}`,
    );
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
  const { request, session } = useAuth();
  const [refreshKey, setRefreshKey] = useState(0);
  const [devicesRefreshKey, setDevicesRefreshKey] = useState(0);
  const [selectedRevision, setSelectedRevision] = useState<number | null>(null);
  const [historyBefore, setHistoryBefore] = useState<number | null>(null);
  const [historyRows, setHistoryRows] = useState<DaemonConfigRevisionSummary[]>(
    [],
  );
  const [lastHistoryPageSize, setLastHistoryPageSize] = useState(0);
  const query = useApi<ActiveDaemonConfig>("/api/v1/daemon-config");
  const devicesQuery = useApi<Device[]>("/api/v1/devices", devicesRefreshKey);
  const historyPath =
    query.data?.source === "database"
      ? `/api/v1/daemon-config/revisions?limit=50&refresh=${refreshKey}${
          historyBefore === null ? "" : `&before=${historyBefore}`
        }`
      : null;
  const historyQuery = useApi<DaemonConfigRevisionList>(historyPath);
  const detailPath =
    selectedRevision === null
      ? null
      : `/api/v1/daemon-config/revisions/${selectedRevision}`;
  const revisionQuery = useApi<DaemonConfigRevision>(detailPath);
  const previousRevision =
    selectedRevision !== null && selectedRevision > 1
      ? selectedRevision - 1
      : undefined;
  const previousQuery = useApi<DaemonConfigRevision>(
    previousRevision === undefined
      ? null
      : `/api/v1/daemon-config/revisions/${previousRevision}`,
  );
  const historyPage = historyQuery.data?.revisions;

  useEffect(() => {
    if (query.data?.source !== "database") return;
    const timer = window.setInterval(
      () => setDevicesRefreshKey((current) => current + 1),
      10_000,
    );
    return () => window.clearInterval(timer);
  }, [query.data?.source]);

  useEffect(() => {
    if (!historyPage) return;
    setHistoryRows((current) =>
      historyBefore === null ? historyPage : [...current, ...historyPage],
    );
    setLastHistoryPageSize(historyPage.length);
  }, [historyPage, historyBefore]);

  function refreshConfiguration() {
    setHistoryBefore(null);
    setHistoryRows([]);
    setLastHistoryPageSize(0);
    setSelectedRevision(null);
    setRefreshKey((current) => current + 1);
    setDevicesRefreshKey((current) => current + 1);
  }

  async function saveConfiguration(input: {
    base_revision: number;
    yaml: string;
    message?: string;
  }) {
    const response = await request("/api/v1/daemon-config", {
      method: "PUT",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(input),
    });
    return (await response.json()) as ActiveDaemonConfig;
  }

  async function loadLatestConfiguration() {
    const response = await request("/api/v1/daemon-config");
    return (await response.json()) as ActiveDaemonConfig;
  }

  if (query.loading) return <PageSkeleton />;
  if (query.error || !query.data) return <ErrorState message={query.error} />;
  if (query.data.source === "database") {
    return (
      <DatabaseConfigurationEditor
        active={query.data}
        devices={devicesQuery.data}
        devicesLoading={devicesQuery.loading}
        devicesError={devicesQuery.error}
        canUpdate={
          query.data.writable &&
          (session?.permissions?.includes("daemon-config.update") ??
            session?.role === "operator")
        }
        history={historyRows}
        historyError={historyQuery.error}
        historyLoading={historyQuery.loading}
        historyHasMore={lastHistoryPageSize === 50}
        selectedRevision={selectedRevision}
        selectedRevisionData={revisionQuery.data}
        selectedRevisionError={revisionQuery.error}
        selectedRevisionLoading={revisionQuery.loading}
        previousRevisionData={previousQuery.data}
        previousRevisionError={previousQuery.error}
        previousRevisionLoading={previousQuery.loading}
        onLoadMore={() => {
          if (historyRows.length > 0) {
            setHistoryBefore(historyRows[historyRows.length - 1].revision);
          }
        }}
        onSelectRevision={setSelectedRevision}
        onSave={saveConfiguration}
        onLoadLatest={loadLatestConfiguration}
        onSaved={refreshConfiguration}
      />
    );
  }
  if (query.data.source === "file") {
    return (
      <div className="stack">
        <p className="config-file-banner">
          File-managed. Edit the source file or copy YAML from the builder.
        </p>
        {query.data.reload_error && (
          <p className="config-notice config-error" role="alert">
            <span>{query.data.reload_error}</span>
          </p>
        )}
        <ConfigurationView initialConfig={query.data.config} />
      </div>
    );
  }
  return <ConfigurationView initialConfig={query.data.config} />;
}

function SettingsPage() {
  const { session } = useAuth();
  const query = useApi<ControllerSettings>("/api/v1/settings");
  return (
    <SettingsView
      authMode={session?.mode}
      data={query.data}
      loading={query.loading}
      error={query.error}
    />
  );
}
