import type { ReactNode } from "react";

import { ControllerShell } from "../components/ControllerShell";

export function ControllerStoryFrame({
  children,
  path,
}: {
  children: ReactNode;
  path: string;
}) {
  return (
    <ControllerShell path={path} onRefresh={() => undefined}>
      {children}
    </ControllerShell>
  );
}
