import { Component, type ErrorInfo, type ReactNode } from "react";
import { displayError } from "../errorPresentation";
import "./ui-error-boundary.css";

type Props = {
  children: ReactNode;
  label: string;
  resetKey?: string | null;
  fullPage?: boolean;
};

export default class UIErrorBoundary extends Component<
  Props,
  { error: Error | null }
> {
  state: { error: Error | null } = { error: null };

  static getDerivedStateFromError(error: unknown) {
    return {
      error:
        error instanceof Error ? error : new Error("Unexpected display error."),
    };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error(
      `Studio display error (${this.props.label})`,
      error,
      info.componentStack,
    );
  }

  componentDidUpdate(previous: Props) {
    if (this.state.error && previous.resetKey !== this.props.resetKey)
      this.setState({ error: null });
  }

  render() {
    if (!this.state.error) return this.props.children;
    return (
      <section
        className={`ui-error-boundary${this.props.fullPage ? " ui-error-page" : ""}`}
        role="alert"
        aria-label={`${this.props.label} display error`}
      >
        <h2>Could not display {this.props.label}</h2>
        <p>Your saved chats and drafts remain available.</p>
        <button type="button" onClick={() => this.setState({ error: null })}>
          Try again
        </button>
        {this.props.fullPage && (
          <button type="button" onClick={() => window.location.reload()}>
            Reload Studio
          </button>
        )}
        <details>
          <summary>Error details</summary>
          <pre>{displayError(this.state.error)}</pre>
        </details>
      </section>
    );
  }
}
