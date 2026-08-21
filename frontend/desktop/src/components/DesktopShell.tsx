import agentdesktopLogo from "@brand/logo.svg";
import agentdesktopMark from "@brand/mark.svg";
import { Gauge, Laptop, LoaderCircle, RefreshCw } from "lucide-react";
import type { ReactNode } from "react";

import type { Notice, View } from "../useDesktopModel";

export interface DesktopShellProps {
  children: ReactNode;
  fullWidth: boolean;
  isRefreshing: boolean;
  notice: Notice;
  onNavigate: (view: View) => void;
  onRefresh: () => void;
  pageTitle: string;
  refreshError: string | null;
  view: View;
}

export function DesktopShell({
  children,
  fullWidth,
  isRefreshing,
  notice,
  onNavigate,
  onRefresh,
  pageTitle,
  refreshError,
  view,
}: DesktopShellProps) {
  return (
    <div
      className={fullWidth ? "desktop-shell enrollment-shell" : "desktop-shell"}
    >
      {!fullWidth ? (
        <aside className="desktop-sidebar">
          <div className="desktop-brand">
            <img
              className="desktop-brand-logo"
              src={agentdesktopLogo}
              alt="Agentdesktop"
            />
            <img className="desktop-brand-icon" src={agentdesktopMark} alt="" />
          </div>
          <nav className="desktop-nav" aria-label="Application">
            <button
              type="button"
              className={view === "home" ? "active" : ""}
              aria-current={view === "home" ? "page" : undefined}
              onClick={() => onNavigate("home")}
            >
              <Gauge size={18} />
              Status
            </button>
            <button
              type="button"
              className={view === "tools" ? "active" : ""}
              aria-current={view === "tools" ? "page" : undefined}
              onClick={() => onNavigate("tools")}
            >
              <Laptop size={18} />
              Tools
            </button>
          </nav>
        </aside>
      ) : null}

      <section className="desktop-main">
        <header className="desktop-page-header">
          <h1>{pageTitle}</h1>
          <button
            className="desktop-refresh"
            type="button"
            onClick={onRefresh}
            disabled={isRefreshing}
          >
            {isRefreshing ? (
              <LoaderCircle className="spin" size={14} />
            ) : (
              <RefreshCw size={14} />
            )}{" "}
            Refresh
          </button>
        </header>
        <main className="desktop-content">
          {refreshError ? (
            <div className="notice notice-error" role="alert">
              {refreshError}
            </div>
          ) : null}
          {notice ? (
            <div
              className={`notice notice-${notice.tone}`}
              role={notice.tone === "error" ? "alert" : "status"}
            >
              {notice.message}
            </div>
          ) : null}
          {children}
        </main>
      </section>
    </div>
  );
}
