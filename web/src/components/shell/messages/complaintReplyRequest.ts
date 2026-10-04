import type { paths } from "../../../generated/api";

type ComplaintDetail =
  paths["/api/complaint"]["get"]["responses"][200]["content"]["application/json"];
type ComplaintRequest =
  paths["/api/complaints"]["post"]["requestBody"]["content"]["application/json"];
export type UserComplaintResponse = Extract<
  ComplaintRequest,
  { action: "respond" }
>;

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
