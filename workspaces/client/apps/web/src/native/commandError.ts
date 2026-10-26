/** Preserve the Rust command error without changing an uncertain outcome. */
export class NativeCommandError extends Error {
  readonly code?: string;
  readonly status?: number;
  readonly details?: unknown;
  constructor(value: {
    message?: string;
    code?: string;
    status?: number;
    details?: unknown;
  }) {
    super(value.message || "The native command failed.");
    this.name = "NativeCommandError";
    this.code = value.code;
    this.status = value.status;
    this.details = value.details;
  }
}
