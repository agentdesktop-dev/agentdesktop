import type { ColorMode } from "@agentdesktop/ui";
import {
  startTransition,
  useEffect,
  useOptimistic,
  useRef,
  useState,
  useTransition,
} from "react";

import {
  applyNetworkRuleChange as applyNetworkRuleChangeRequest,
  getAccessReport,
  desktopReady,
  getBootstrap,
  getConnectorStatus,
  getDiscovery,
  getManagedDeviceStatus,
  getProgramManagement,
  logoutManagedDevice,
  openAccessSource,
  saveSettings,
  setProgramChoice as sendProgramChoice,
  setupManagedDevice,
} from "./backend";
import type {
  AccessReport,
  Bootstrap,
  ConnectorSnapshot,
  Discovery,
  ManagedDeviceSnapshot,
  NetworkRuleChange,
  ProgramChoice,
  ProgramManagement,
  Settings,
} from "./types";

export type View = "home" | "tools" | "usage" | "settings";
export type Notice = { tone: "success" | "error"; message: string } | null;

type StatusSource =
  | "connector"
  | "managedDevice"
  | "discovery"
  | "access"
  | "programs";
type StatusErrors = Partial<Record<StatusSource, string>>;
type StatusUpdate = {
  connector: PromiseSettledResult<ConnectorSnapshot>;
  managedDevice: PromiseSettledResult<ManagedDeviceSnapshot>;
  discovery: PromiseSettledResult<Discovery>;
  programs: PromiseSettledResult<ProgramManagement>;
};

const loadingSettings: Settings = { openOnStartup: true, colorMode: "system" };

const statusSourceLabels: Record<StatusSource, string> = {
  connector: "local daemon status",
  managedDevice: "organization status",
  discovery: "tool inventory",
  access: "local access audit",
  programs: "managed tools",
};

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

async function getStatusUpdate(): Promise<StatusUpdate> {
  const [connector, managedDevice, discovery, programs] =
    await Promise.allSettled([
      getConnectorStatus(),
      getManagedDeviceStatus(),
      getDiscovery(),
      getProgramManagement(),
    ]);
  return { connector, managedDevice, discovery, programs };
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
  const [hasLoadedBootstrap, setHasLoadedBootstrap] = useState(false);
  const [settings, setSettings] = useState<Settings>(loadingSettings);
  const [connector, setConnector] = useState<ConnectorSnapshot | null>(null);
  const [managedDevice, setManagedDevice] =
    useState<ManagedDeviceSnapshot | null>(null);
  const [discovery, setDiscovery] = useState<Discovery | null>(null);
  const [accessReport, setAccessReport] = useState<AccessReport | null>(null);
  const [accessStale, setAccessStale] = useState(false);
  const [programManagement, setProgramManagement] =
    useState<ProgramManagement | null>(null);
  // Shows a choice being saved until the daemon answers.
  const [shownProgramManagement, showProgramChoice] = useOptimistic(
    programManagement,
    (current, choice: ProgramChoice) => current && { ...current, choice },
  );
  // Advanced when a program choice change starts and when it is saved, so a
  // status update read before or during the change cannot replace its result.
  const programChanges = useRef(0);
  const [notice, setNotice] = useState<Notice>(null);
  const [statusErrors, setStatusErrors] = useState<StatusErrors>({});
  const [hasLoadedStatus, setHasLoadedStatus] = useState(false);
  const [hasLoadedAccess, setHasLoadedAccess] = useState(false);
  const [isRefreshing, startRefreshing] = useTransition();
  const [isAssessing, startAssessing] = useTransition();
  const [isManaging, startManaging] = useTransition();
  const [isSaving, startSaving] = useTransition();
  const savingSettings = useRef(false);
  const [isLoggingOut, startLoggingOut] = useTransition();
  const accessRequestId = useRef(0);
  const [isChangingPrograms, startChangingPrograms] = useTransition();
  const needsEnrollment = Boolean(
    managedDevice?.configured && managedDevice.enrollment !== "approved",
  );
  // Usage comes from the controller of an enrolled device, or from the
  // daemon itself when a gateway on this machine reports to it.
  const showUsage =
    Boolean(
      managedDevice?.configured && managedDevice.enrollment === "approved",
    ) || Boolean(connector?.runtime?.daemon.usageReceiver);

  useEffect(() => {
    if (view === "usage" && hasLoadedStatus && !showUsage) setView("home");
  }, [view, hasLoadedStatus, showUsage]);

  /** `programChange` is `programChanges` from before `update` was read. */
  function applyStatusUpdate(update: StatusUpdate, programChange: number) {
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
      if (programChange === programChanges.current) {
        if (update.programs.status === "fulfilled") {
          setProgramManagement(update.programs.value);
        }
      }
      setStatusErrors((current) => {
        const next = { ...current };
        updateStatusError(next, "connector", update.connector);
        updateStatusError(next, "managedDevice", update.managedDevice);
        updateStatusError(next, "discovery", update.discovery);
        if (programChange === programChanges.current) {
          updateStatusError(next, "programs", update.programs);
        }
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

  async function applyNetworkRuleChange(change: NetworkRuleChange) {
    setNotice(null);
    const requestId = ++accessRequestId.current;
    try {
      const report = await applyNetworkRuleChangeRequest(change);
      if (requestId === accessRequestId.current) {
        applyAccessUpdate({ status: "fulfilled", value: report });
      }
      setNotice({
        tone: "success",
        message: "Network rule updated and access rechecked",
      });
    } catch (error: unknown) {
      setNotice({ tone: "error", message: errorMessage(error) });
      throw error;
    }
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
      })
      .finally(() => {
        if (active) setHasLoadedBootstrap(true);
      });
    return () => {
      active = false;
    };
  }, []);

  useEffect(() => {
    if (!hasLoadedBootstrap) return;
    // ThemeProvider's layout effect has applied the restored mode before showing.
    desktopReady().catch((error: unknown) => {
      setNotice({ tone: "error", message: errorMessage(error) });
    });
  }, [hasLoadedBootstrap]);

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
      const programChange = programChanges.current;
      const update = await getStatusUpdate();
      if (active) applyStatusUpdate(update, programChange);
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
      const programChange = programChanges.current;
      applyStatusUpdate(await getStatusUpdate(), programChange);
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
        const [nextConnector, nextManagedDevice] = await Promise.all([
          getConnectorStatus(),
          getManagedDeviceStatus(),
        ]);
        setConnector(nextConnector);
        setManagedDevice(nextManagedDevice);
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

  function setProgramChoice(choice: ProgramChoice) {
    setNotice(null);
    programChanges.current += 1;
    startChangingPrograms(async () => {
      showProgramChoice(choice);
      try {
        const nextProgramManagement = await sendProgramChoice(choice);
        programChanges.current += 1;
        startTransition(() => {
          setProgramManagement(nextProgramManagement);
        });
        setNotice({ tone: "success", message: "Managed tools updated" });
      } catch (error: unknown) {
        setNotice({ tone: "error", message: errorMessage(error) });
      }
    });
  }

  function updateSettings(patch: Partial<Settings>) {
    if (!bootstrap || savingSettings.current) return;
    savingSettings.current = true;
    const previous = settings;
    const next = { ...settings, ...patch };
    setSettings(next);
    setNotice(null);
    startSaving(async () => {
      try {
        setSettings(await saveSettings(next));
      } catch (error: unknown) {
        setSettings(previous);
        setNotice({ tone: "error", message: errorMessage(error) });
      } finally {
        savingSettings.current = false;
      }
    });
  }

  function setOpenOnStartup(checked: boolean) {
    updateSettings({ openOnStartup: checked });
  }

  function setColorMode(colorMode: ColorMode) {
    updateSettings({ colorMode });
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
  const pageTitle =
    view === "settings"
      ? "Settings"
      : needsEnrollment
        ? "Enrollment"
        : view === "home"
          ? "Status"
          : view === "usage"
            ? "Usage"
            : "Tools";

  return {
    applyNetworkRuleChange,
    accessReport,
    accessStale,
    bootstrap,
    connector,
    copyDiagnostics,
    discovery,
    enroll,
    hasLoadedStatus,
    hasLoadedAccess,
    isAssessing,
    isChangingPrograms,
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
    programManagement: shownProgramManagement,
    refresh,
    refreshError: statusErrorMessage(visibleStatusErrors),
    setColorMode,
    setOpenOnStartup,
    setProgramChoice,
    settings,
    showUsage,
    view,
  };
}

export type DesktopModel = ReturnType<typeof useDesktopModel>;
