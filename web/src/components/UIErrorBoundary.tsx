import { Button, MantineContext, MantineProvider } from "@mantine/core";
import { Component, useContext, type ErrorInfo, type ReactNode } from "react";
import { theme } from "../theme";
import { displayError } from "../errorPresentation";
import "./ui-error-boundary.css";

// The root boundary sits outside the application's provider.
function ErrorSurface({ children }: { children: ReactNode }) {
  const context = useContext(MantineContext);
  return context ? (
    children
  ) : (
    <MantineProvider theme={theme} defaultColorScheme="auto">
      {children}
    </MantineProvider>
  );
}

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
      <ErrorSurface>
        <section
          className={`ui-error-boundary${this.props.fullPage ? " ui-error-page" : ""}`}
          role="alert"
          aria-label={`${this.props.label} display error`}
        >
          <h2>Could not display {this.props.label}</h2>
          <p>Your saved chats and drafts remain available.</p>
          <Button
            variant="filled"
            color="indigo"
            onClick={() => this.setState({ error: null })}
          >
            Try again
          </Button>
          {this.props.fullPage && (
            <Button variant="default" onClick={() => window.location.reload()}>
              Reload Studio
            </Button>
          )}
          <details>
            <summary>Error details</summary>
            <pre>{displayError(this.state.error)}</pre>
          </details>
        </section>
      </ErrorSurface>
    );
  }
}
