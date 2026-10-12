import type { ReactNode } from "react";

import { DesktopShell } from "../components/DesktopShell";
import type { View } from "../useDesktopModel";

export function DesktopStoryFrame({
  children,
  fullWidth = false,
  pageTitle,
  showUsage = false,
  view = "home",
}: {
  children: ReactNode;
  fullWidth?: boolean;
  pageTitle: string;
  showUsage?: boolean;
  view?: View;
}) {
  return (
    <DesktopShell
      fullWidth={fullWidth}
      isRefreshing={false}
      notice={null}
      onNavigate={() => undefined}
      onRefresh={() => undefined}
      pageTitle={pageTitle}
      refreshError={null}
      showUsage={showUsage}
      view={view}
    >
      {children}
    </DesktopShell>
  );
}
