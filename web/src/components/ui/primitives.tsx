import { Button, type ButtonProps, type ElementProps } from "@mantine/core";
import type { ReactNode } from "react";
import "./primitives.css";

export type ActionRole = "primary" | "secondary" | "quiet" | "destructive";
export interface ActionButtonProps
  extends ButtonProps, ElementProps<"button", keyof ButtonProps> {
  actionRole?: ActionRole;
}
/** Preserve native button props and accessible names from each caller. */
export function ActionButton({
  actionRole = "secondary",
  ...props
}: ActionButtonProps) {
  const variant =
    actionRole === "quiet"
      ? "subtle"
      : actionRole === "secondary"
        ? "default"
        : "filled";
  const color =
    actionRole === "destructive"
      ? "red"
      : actionRole === "primary"
        ? "indigo"
        : "gray";
  return <Button variant={variant} color={color} {...props} />;
}

type HeaderProps = { title: ReactNode; actions?: ReactNode; help?: ReactNode };
export function PanelHeader({ title, actions, help }: HeaderProps) {
  return (
    <header className="ui-panel-header">
      <div className="ui-label">
        <h3>{title}</h3>
        {help && <div className="ui-help">{help}</div>}
      </div>
      {actions && <div className="ui-actions">{actions}</div>}
    </header>
  );
}
export function SettingsSection({
  title,
  help,
  action,
  children,
}: Omit<HeaderProps, "actions"> & { action?: ReactNode; children: ReactNode }) {
  return (
    <section className="ui-settings-section">
      <PanelHeader title={title} help={help} actions={action} />
      <div className="ui-section-body">{children}</div>
    </section>
  );
}
export function SettingsRow({
  label,
  help,
  children,
}: {
  label: ReactNode;
  help?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="ui-settings-row">
      <div className="ui-label">
        <div className="ui-row-label">{label}</div>
        {help && <div className="ui-help">{help}</div>}
      </div>
      <div className="ui-row-control">{children}</div>
    </div>
  );
}
export function EmptyState({
  title,
  help,
  action,
}: {
  title: ReactNode;
  help?: ReactNode;
  action?: ReactNode;
}) {
  return (
    <div className="ui-empty-state">
      <div className="ui-label">
        <div className="ui-row-label">{title}</div>
        {help && <div className="ui-help">{help}</div>}
      </div>
      {action && <div className="ui-actions">{action}</div>}
    </div>
  );
}
