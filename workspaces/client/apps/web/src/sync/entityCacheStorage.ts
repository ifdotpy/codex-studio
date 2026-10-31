/** Name of the in-memory draft collection of one workspace. */
export function workspaceDatabaseName(workspaceId: string, serverSuffix = "") {
  return `studio${workspaceId}${serverSuffix}`;
}

/** Name of the projection store of one workspace and API schema. */
export function entityProjectionDatabaseName(
  workspaceId: string,
  schemaHash: string,
  serverSuffix = "",
) {
  return `studio-entity-projection-${workspaceId}${serverSuffix}-${schemaHash}`;
}
