import type { ComponentType } from "react";

/** Compiled-in page/navigation contribution to the shared controller UI.
 * Permission hints control visibility only; backend handlers enforce access.
 */
export type ControllerExtension = {
  path: string;
  label: string;
  permission: string;
  icon: ComponentType<{ size?: number }>;
  component: ComponentType;
};
