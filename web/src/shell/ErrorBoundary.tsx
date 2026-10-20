import { Component, type ReactNode } from "react";

/** A bug in one panel shows an error in that panel instead of blanking the dashboard. */
export class ErrorBoundary extends Component<{ name: string; children: ReactNode }, { error: Error | null }> {
  state: { error: Error | null } = { error: null };
  static getDerivedStateFromError(error: Error) {
    return { error };
  }
  componentDidCatch(error: Error) {
    console.error(`[${this.props.name}]`, error);
  }
  render() {
    if (this.state.error)
      return (
        <div className="pr-note" style={{ color: "var(--danger)" }}>
          This section hit an error: {this.state.error.message}{" "}
          <button className="btn btn-sm" onClick={() => this.setState({ error: null })}>
            Retry
          </button>
        </div>
      );
    return this.props.children;
  }
}
