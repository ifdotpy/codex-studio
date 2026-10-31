import { outboxRequests } from "../../../browserPersistence";
import { randomUUID } from "../../../uuid";
import type { paths } from "../../../generated/api";

type ComplaintDetail =
  paths["/api/complaint"]["get"]["responses"][200]["content"]["application/json"];
type ComplaintRequest =
  paths["/api/complaints"]["post"]["requestBody"]["content"]["application/json"];
export type UserComplaintResponse = Omit<
  ComplaintRequest,
  "action" | "complaint_id" | "status" | "version"
> & {
  action: "respond";
  complaint_id: string;
  status: NonNullable<ComplaintRequest["status"]>;
  version: NonNullable<ComplaintRequest["version"]>;
};

/** Request timeout leaves the server outcome unknown. */
export function isDefinitiveComplaintRefusal(status: number): boolean {
  return status >= 400 && status < 500 && status !== 408;
}

/** Preserve complete response bodies for retry under their existing scoped key. */
export function complaintResponseStore(storage: Storage, stateDir: string) {
  const outbox = outboxRequests(storage);
  return {
    read(): Record<string, UserComplaintResponse> {
      const value: unknown = JSON.parse(
        outbox.get("complaintResponse", stateDir) || "{}",
      );
      if (
        !value ||
        typeof value !== "object" ||
        Array.isArray(value) ||
        Object.entries(value).some(([id, entry]) => {
          if (!entry || typeof entry !== "object" || Array.isArray(entry))
            return true;
          const request = entry as Record<string, unknown>;
          return (
            request.action !== "respond" ||
            request.complaint_id !== id ||
            typeof request.id !== "string" ||
            !request.id ||
            typeof request.version !== "number" ||
            typeof request.text !== "string" ||
            typeof request.status !== "string"
          );
        })
      )
        throw new Error("The saved response request cannot be read.");
      return value as Record<string, UserComplaintResponse>;
    },
    write(requests: Map<string, UserComplaintResponse>) {
      if (requests.size)
        outbox.set(
          "complaintResponse",
          stateDir,
          JSON.stringify(Object.fromEntries(requests)),
        );
      else outbox.remove("complaintResponse", stateDir);
    },
  };
}

export async function submitComplaintResponse(
  payload: UserComplaintResponse,
  sessionToken: string,
  postResponse: (
    request: UserComplaintResponse,
    token: string,
  ) => Promise<unknown>,
  readDetail: (id: string) => Promise<ComplaintDetail>,
): Promise<{ detail: ComplaintDetail } | { reloadError: unknown }> {
  await postResponse(payload, sessionToken);
  try {
    return { detail: await readDetail(payload.complaint_id) };
  } catch (reloadError) {
    return { reloadError };
  }
}

export function complaintReplyRequest(
  existing: UserComplaintResponse | undefined,
  detail: Pick<ComplaintDetail, "id" | "version">,
  text: string,
  status: UserComplaintResponse["status"],
): UserComplaintResponse {
  return (
    existing ?? {
      id: randomUUID(),
      action: "respond",
      complaint_id: detail.id,
      version: detail.version,
      text,
      status,
    }
  );
}
