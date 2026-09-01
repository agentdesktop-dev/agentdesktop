import {
  startTransition,
  useEffect,
  useRef,
  useState,
  useTransition,
} from "react";

import {
  getAccessReport,
  getBootstrap,
  getConnectorStatus,
  getDiscovery,
  getManagedDeviceStatus,
  getRemoteConfig,
  logoutManagedDevice,
  openAccessSource,
  saveSettings,
  setupManagedDevice,
} from "./backend";
import type {
  AccessReport,
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
  Settings,
} from "./types";

export type View = "home" | "tools";
export type Notice = { tone: "success" | "error"; message: string } | null;

type StatusSource =
  | "connector"
  | "managedDevice"
  | "discovery"
  | "access"
  | "remoteConfig";
type StatusErrors = Partial<Record<StatusSource, string>>;
type StatusUpdate = {
  connector: PromiseSettledResult<ConnectorSnapshot>;
  managedDevice: PromiseSettledResult<ManagedDeviceSnapshot>;
  discovery: PromiseSettledResult<Discovery>;
  remoteConfig: PromiseSettledResult<string | null>;
};

const loadingSettings: Settings = { openOnStartup: true };

const statusSourceLabels: Record<StatusSource, string> = {
  connector: "local daemon status",
  managedDevice: "organization status",
  discovery: "tool inventory",
  access: "local access audit",
  remoteConfig: "advanced configuration",
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function getStatusUpdate(): Promise<StatusUpdate> {
  const [connector, managedDevice, discovery, remoteConfig] =
    await Promise.allSettled([
      getConnectorStatus(),
      getManagedDeviceStatus(),
      getDiscovery(),
      getRemoteConfig(),
    ]);
  return { connector, managedDevice, discovery, remoteConfig };
}

function updateStatusError(
  errors: StatusErrors,
  source: StatusSource,
  result: PromiseSettledResult<unknown>,
) {
  if (result.status === "fulfilled") {
    delete errors[source];
  } else {
    errors[source] = errorMessage(result.reason);
  }
}

async function getAccessUpdate(): Promise<PromiseSettledResult<AccessReport>> {
  const [result] = await Promise.allSettled([getAccessReport()]);
  return result;
}

function statusErrorMessage(errors: StatusErrors): string | null {
  const sources = Object.keys(errors) as StatusSource[];
  if (!sources.length) return null;
  const labels = sources.map((source) => statusSourceLabels[source]);
  const subject =
    labels.length === 1 ? labels[0] : `some information (${labels.join(", ")})`;
  return `Couldn’t refresh ${subject}. Showing available or last known information.`;
}

export function useDesktopModel() {
  const [view, setView] = useState<View>("home");
  const [bootstrap, setBootstrap] = useState<Bootstrap | null>(null);
  const [settings, setSettings] = useState<Settings>(loadingSettings);
  const [connector, setConnector] = useState<ConnectorSnapshot | null>(null);
  const [managedDevice, setManagedDevice] =
    useState<ManagedDeviceSnapshot | null>(null);
  const [discovery, setDiscovery] = useState<Discovery | null>(null);
  const [accessReport, setAccessReport] = useState<AccessReport | null>(null);
  const [accessStale, setAccessStale] = useState(false);
  const [remoteConfig, setRemoteConfig] = useState<string | null>(null);
  const [notice, setNotice] = useState<Notice>(null);
  const [statusErrors, setStatusErrors] = useState<StatusErrors>({});
  const [hasLoadedStatus, setHasLoadedStatus] = useState(false);
  const [hasLoadedAccess, setHasLoadedAccess] = useState(false);
  const [isRefreshing, startRefreshing] = useTransition();
  const [isAssessing, startAssessing] = useTransition();
  const [isManaging, startManaging] = useTransition();
  const [isSaving, startSaving] = useTransition();
  const [isLoggingOut, startLoggingOut] = useTransition();
  const accessRequestId = useRef(0);
  const needsEnrollment = Boolean(
    managedDevice?.configured && managedDevice.enrollment !== "approved",
  );

  function applyStatusUpdate(update: StatusUpdate) {
    startTransition(() => {
      if (update.connector.status === "fulfilled") {
        setConnector(update.connector.value);
      }
      if (update.managedDevice.status === "fulfilled") {
        setManagedDevice(update.managedDevice.value);
      }
      if (update.discovery.status === "fulfilled") {
        setDiscovery(update.discovery.value);
      }
      if (update.remoteConfig.status === "fulfilled") {
        setRemoteConfig(update.remoteConfig.value);
      }
      setStatusErrors((current) => {
        const next = { ...current };
        updateStatusError(next, "connector", update.connector);
        updateStatusError(next, "managedDevice", update.managedDevice);
        updateStatusError(next, "discovery", update.discovery);
        updateStatusError(next, "remoteConfig", update.remoteConfig);
        return next;
      });
      setHasLoadedStatus(true);
    });
  }

  function applyAccessUpdate(update: PromiseSettledResult<AccessReport>) {
    startTransition(() => {
      if (update.status === "fulfilled") {
        setAccessReport(update.value);
        setAccessStale(false);
      } else {
        setAccessStale(true);
      }
      setStatusErrors((current) => {
        const next = { ...current };
        updateStatusError(next, "access", update);
        return next;
      });
      setHasLoadedAccess(true);
    });
  }

  async function loadAccess() {
    const requestId = ++accessRequestId.current;
    const update = await getAccessUpdate();
    if (requestId === accessRequestId.current) {
      applyAccessUpdate(update);
    }
  }

  function assessAccess() {
    startAssessing(loadAccess);
  }

  useEffect(() => {
    let active = true;
    getBootstrap()
      .then((nextBootstrap) => {
        if (!active) return;
        setBootstrap(nextBootstrap);
        setSettings(nextBootstrap.settings);
      })
      .catch((error: unknown) => {
        if (active) setNotice({ tone: "error", message: errorMessage(error) });
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (
      !managedDevice ||
      !["pending", "issuing"].includes(managedDevice.enrollment)
    )
      return;
    let active = true;
    let timeout: number | undefined;
    let polling = false;
    const stopPolling = () => {
      if (timeout === undefined) return;
      window.clearTimeout(timeout);
      timeout = undefined;
    };
    const schedulePoll = () => {
      stopPolling();
      if (active && !document.hidden) {
        timeout = window.setTimeout(pollApproval, 5000);
      }
    };
    const pollApproval = async () => {
      if (polling) return;
      polling = true;
      try {
        const nextManagedDevice = await getManagedDeviceStatus();
        if (active) {
          setStatusErrors((current) => {
            if (!current.managedDevice) return current;
            const next = { ...current };
            delete next.managedDevice;
            return next;
          });
          startTransition(() => {
            setManagedDevice(nextManagedDevice);
          });
          if (nextManagedDevice.enrollment === "approved") {
            setNotice({
              tone: "success",
              message: "Organization access is ready",
            });
          }
        }
      } catch (error: unknown) {
        if (active) {
          setStatusErrors((current) => ({
            ...current,
            managedDevice: errorMessage(error),
          }));
        }
      } finally {
        polling = false;
        schedulePoll();
      }
    };
    const refreshWhenVisible = () => {
      if (document.hidden) {
        stopPolling();
      } else {
        pollApproval();
      }
    };
    schedulePoll();
    document.addEventListener("visibilitychange", refreshWhenVisible);
    return () => {
      active = false;
      stopPolling();
      document.removeEventListener("visibilitychange", refreshWhenVisible);
    };
  }, [managedDevice?.enrollment]);

  useEffect(() => {
    let active = true;
    let interval: number | undefined;
    const refreshStatus = async () => {
      const update = await getStatusUpdate();
      if (active) applyStatusUpdate(update);
    };
    const stopPolling = () => {
      if (interval === undefined) return;
      window.clearInterval(interval);
      interval = undefined;
    };
    const startPolling = () => {
      if (interval === undefined) {
        interval = window.setInterval(refreshStatus, 5000);
      }
    };
    refreshStatus();
    const refreshWhenVisible = () => {
      if (document.hidden) {
        stopPolling();
      } else {
        refreshStatus();
        startPolling();
      }
    };
    if (!document.hidden) startPolling();
    document.addEventListener("visibilitychange", refreshWhenVisible);
    return () => {
      active = false;
      stopPolling();
      document.removeEventListener("visibilitychange", refreshWhenVisible);
    };
  }, []);

  function refresh() {
    setNotice(null);
    startRefreshing(async () => {
      const access = view === "tools" ? loadAccess() : null;
      applyStatusUpdate(await getStatusUpdate());
      if (access) await access;
    });
  }

  function enroll() {
    setNotice(null);
    startManaging(async () => {
      try {
        const nextManagedDevice = await setupManagedDevice();
        setManagedDevice(nextManagedDevice);
        const approved = nextManagedDevice.enrollment === "approved";
        setNotice({
          tone: "success",
          message: approved
            ? "Organization access is ready"
            : "Enrollment is starting; continue in your browser when prompted",
        });
        setConnector(await getConnectorStatus());
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function logout() {
    setNotice(null);
    startLoggingOut(async () => {
      try {
        await logoutManagedDevice();
        const [nextConnector, nextManagedDevice, nextRemoteConfig] =
          await Promise.all([
            getConnectorStatus(),
            getManagedDeviceStatus(),
            getRemoteConfig(),
          ]);
        setConnector(nextConnector);
        setManagedDevice(nextManagedDevice);
        setRemoteConfig(nextRemoteConfig);
        setView("home");
        setNotice({
          tone: "success",
          message: "Signed out of the organization on this device",
        });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function setOpenOnStartup(checked: boolean) {
    const previous = settings;
    const next = { ...settings, openOnStartup: checked };
    setSettings(next);
    startSaving(async () => {
      try {
        setSettings(await saveSettings(next));
      } catch (error: unknown) {
        setSettings(previous);
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  async function copyDiagnostics() {
    try {
      const managed = managedDevice
        ? {
            configured: managedDevice.configured,
            organizationName: managedDevice.organizationName,
            enrollment: managedDevice.enrollment,
          }
        : null;
      await navigator.clipboard.writeText(
        JSON.stringify({ desktop: bootstrap, connector, managed }, null, 2),
      );
      setNotice({ tone: "success", message: "Diagnostics copied" });
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
    }
  }

  async function copyRemoteConfig() {
    if (!remoteConfig) return;
    try {
      await navigator.clipboard.writeText(remoteConfig);
      setNotice({ tone: "success", message: "Configuration copied" });
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
    }
  }

  async function openAccessSettings(path: string) {
    try {
      await openAccessSource(path);
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
      throw error;
    }
  }

  function navigate(nextView: View) {
    setView(nextView);
    setNotice(null);
    if (nextView === "tools" && view !== "tools" && !isAssessing) {
      assessAccess();
    }
  }

  const visibleStatusErrors = { ...statusErrors };
  if (view !== "tools") delete visibleStatusErrors.access;

  const pageTitle = needsEnrollment
    ? "Enrollment"
    : view === "home"
      ? "Status"
      : "Tools";

  return {
    accessReport,
    accessStale,
    bootstrap,
    connector,
    copyDiagnostics,
    copyRemoteConfig,
    discovery,
    enroll,
    hasLoadedStatus,
    hasLoadedAccess,
    isAssessing,
    isLoggingOut,
    isManaging,
    isRefreshing,
    isSaving,
    logout,
    managedDevice,
    navigate,
    needsEnrollment,
    notice,
    openAccessSettings,
    pageTitle,
    refresh,
    refreshError: statusErrorMessage(visibleStatusErrors),
    remoteConfig,
    setOpenOnStartup,
    settings,
    view,
  };
}

export type DesktopModel = ReturnType<typeof useDesktopModel>;
