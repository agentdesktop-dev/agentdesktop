import { AlertCircle } from "lucide-react";
import type { ErrorInfo, ReactNode } from "react";
import { Component } from "react";

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export class PageBoundary extends Component<
  { children: ReactNode },
  { error: string | null }
> {
  state = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return { error: errorMessage(error) };
  }

  componentDidCatch(error: unknown, info: ErrorInfo) {
    console.error("Desktop page failed to render", error, info);
  }

  render() {
    if (this.state.error) {
      return (
        <div className="page-error" role="alert">
          <AlertCircle size={26} />
          <h2>Couldn’t display this page</h2>
          <p>{this.state.error}</p>
        </div>
      );
    }
    return this.props.children;
  }
}
