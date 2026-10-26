import { displayError } from "./errorPresentation";

/** Shared error identity for browser and native API ports. */
export class ApiError extends Error {
  public readonly outcome?: string;
  public readonly catalogPending?: boolean;

  constructor(
    message: unknown,
    public status: number,
    public readonly response: unknown = message,
    public readonly location?: string[],
  ) {
    super(displayError(message) || `Request failed (${status})`);
    if (response && typeof response === "object" && !Array.isArray(response)) {
      if ("outcome" in response && typeof response.outcome === "string")
        this.outcome = response.outcome;
      if (
        "catalogPending" in response &&
        typeof response.catalogPending === "boolean"
      )
        this.catalogPending = response.catalogPending;
    }
  }
}

export class ApiSchemaMismatchError extends ApiError {
  constructor(public readonly markedResponse = false) {
    super("Studio must be updated before using this tab.", 426);
    this.name = "ApiSchemaMismatchError";
  }
}

export class NetworkTimeoutError extends TypeError {
  constructor() {
    super("The server did not respond in time.");
    this.name = "NetworkTimeoutError";
  }
}
