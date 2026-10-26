export const resolveSettings = async () => ({ effective: {} });
export const query = () => {
  throw new Error("History must not start a query");
};
export const tool = () => ({});
export const createSdkMcpServer = (value: unknown) => value;
export const getSessionMessages = async () => [];
export const forkSession = async () => {};
