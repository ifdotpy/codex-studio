export { resolveSettings } from "../sdk-fixtures.ts";
export const query = () => {
  throw new Error("History must not start a query");
};
export const tool = () => ({});
export const createSdkMcpServer = (value: unknown) => value;
export const getSessionMessages = () => {
  throw new Error("No SDK history read expected");
};
export const forkSession = () => {
  throw new Error("No fork expected");
};
