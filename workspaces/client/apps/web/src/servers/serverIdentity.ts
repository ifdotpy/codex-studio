/** Server identity and saved credential reference shared by server clients. */
export type StudioServerIdentity = {
  id: string;
  label: string;
  origin: string;
  credentialId?: string;
};
