import { ThemeProvider } from "@agentdesktop/ui";

import { DesktopShell } from "./components/DesktopShell";
import { PageBoundary } from "./components/PageBoundary";
import { StatusLoading } from "./components/StatusLoading";
import { useDesktopModel } from "./useDesktopModel";
import { EnrollmentView } from "./views/EnrollmentView";
import { SettingsView } from "./views/SettingsView";
import { StatusView } from "./views/StatusView";
import { ToolsView } from "./views/ToolsView";

export function Desktop() {
  const model = useDesktopModel();

  return (
    <ThemeProvider
      mode={model.settings.colorMode}
      onModeChange={model.setColorMode}
    >
      <DesktopShell
        fullWidth={model.needsEnrollment && model.view !== "settings"}
        isRefreshing={model.isRefreshing}
        notice={model.notice}
        onNavigate={model.navigate}
        onRefresh={model.refresh}
        pageTitle={model.pageTitle}
        refreshError={model.refreshError}
        view={model.view}
      >
        <PageBoundary
          key={`${model.view}-${model.needsEnrollment}-${model.hasLoadedStatus}`}
        >
          {model.view === "settings" ? (
            <SettingsView
              settings={model.settings}
              disabled={!model.bootstrap || model.isSaving}
              onStartupChange={model.setOpenOnStartup}
            />
          ) : !model.hasLoadedStatus ? (
            <StatusLoading view={model.view} />
          ) : model.needsEnrollment && model.managedDevice ? (
            <EnrollmentView
              enrollment={model.managedDevice}
              busy={model.isManaging}
              daemon={model.connector?.runtime?.daemon ?? null}
              onCopy={model.copyDiagnostics}
              onEnroll={model.enroll}
            />
          ) : model.view === "home" ? (
            <StatusView
              bootstrap={model.bootstrap}
              connector={model.connector}
              managedDevice={model.managedDevice}
              discovery={model.discovery}
              isLoggingOut={model.isLoggingOut}
              onCopy={model.copyDiagnostics}
              onLogout={model.logout}
            />
          ) : (
            <ToolsView
              discovery={model.discovery}
              unavailable={!model.discovery}
            />
          )}
        </PageBoundary>
      </DesktopShell>
    </ThemeProvider>
  );
}
