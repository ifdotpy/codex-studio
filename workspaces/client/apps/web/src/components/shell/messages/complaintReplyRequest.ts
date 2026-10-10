import type { paths } from "../../../generated/api";

type ComplaintDetail =
  paths["/api/complaint"]["get"]["responses"][200]["content"]["application/json"];
type ComplaintRequest =
  paths["/api/complaints"]["post"]["requestBody"]["content"]["application/json"];
export type UserComplaintResponse = Omit<
  ComplaintRequest,
  "action" | "complaint_id"
> & {
  action: "respond";
  complaint_id: string;
};

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
      id: crypto.randomUUID(),
      action: "respond",
      complaint_id: detail.id,
      version: detail.version,
      text,
      status,
    }
  );
}
