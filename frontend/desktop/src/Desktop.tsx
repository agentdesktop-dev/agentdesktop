import { DesktopShell } from "./components/DesktopShell";
import { PageBoundary } from "./components/PageBoundary";
import { StatusLoading } from "./components/StatusLoading";
import { useDesktopModel } from "./useDesktopModel";
import { EnrollmentView } from "./views/EnrollmentView";
import { StatusView } from "./views/StatusView";
import { ToolsView } from "./views/ToolsView";

export function Desktop() {
  const model = useDesktopModel();

  return (
    <DesktopShell
      fullWidth={model.needsEnrollment}
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
        {!model.hasLoadedStatus ? (
          <StatusLoading view={model.view} />
        ) : model.needsEnrollment && model.managedDevice ? (
          <EnrollmentView
            enrollment={model.managedDevice}
            busy={model.isManaging}
            onEnroll={model.enroll}
          />
        ) : model.view === "home" ? (
          <StatusView
            bootstrap={model.bootstrap}
            connector={model.connector}
            managedDevice={model.managedDevice}
            discovery={model.discovery}
            remoteConfig={model.remoteConfig}
            settings={model.settings}
            isSaving={model.isSaving}
            isLoggingOut={model.isLoggingOut}
            onStartupChange={model.setOpenOnStartup}
            onCopy={model.copyDiagnostics}
            onCopyRemoteConfig={model.copyRemoteConfig}
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
  );
}
