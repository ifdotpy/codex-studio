const ENTITY_SCOPE = "state:entities:v1";

function isContiguous(after: number, checkpoint: number, through: number) {
  return (
    Number.isSafeInteger(after) &&
    Number.isSafeInteger(checkpoint) &&
    Number.isSafeInteger(through) &&
    after <= checkpoint &&
    checkpoint < through
  );
}

/** The in-memory checkpoint shared by this tab's mutation and pull paths. */
export class EntitySequenceCheckpoint {
  private sequence = -1;
  private epoch: string | undefined;
  private resetGeneration = 0;
  private hasAssignment = false;
  private durableCheckpointValid = true;
  private workspaceId: string | undefined;
  private nextCoverageToken = 0;
  private readonly inFlightCoverage = new Map<
    number,
    { after: number; through: number }
  >();
  private suppressedRequirement: number | undefined;
  private readonly coverageInvalidationListeners = new Set<() => void>();

  get value() {
    return this.sequence;
  }

  get resetVersion() {
    return this.resetGeneration;
  }

  get initialized() {
    return this.hasAssignment;
  }

  get canUseDurableCheckpoint() {
    return this.durableCheckpointValid;
  }

  isSameResetVersion(version: number): boolean {
    return this.resetGeneration === version;
  }

  observeWorkspace(workspaceId: string): boolean {
    if (this.workspaceId === undefined) {
      this.workspaceId = workspaceId;
      return false;
    }
    if (this.workspaceId === workspaceId) return false;
    this.workspaceId = workspaceId;
    this.epoch = undefined;
    this.sequence = -1;
    this.hasAssignment = true;
    this.durableCheckpointValid = false;
    this.resetGeneration++;
    this.clearInFlightCoverage(false);
    return true;
  }

  observeEpoch(epoch: string): boolean {
    if (this.epoch === undefined) {
      this.epoch = epoch;
      return false;
    }
    if (this.epoch === epoch) return false;
    this.epoch = epoch;
    this.sequence = -1;
    this.hasAssignment = true;
    this.durableCheckpointValid = false;
    this.resetGeneration++;
    this.clearInFlightCoverage(false);
    return true;
  }

  assign(sequence: number): void {
    this.sequence = sequence;
    this.hasAssignment = true;
    this.durableCheckpointValid = true;
  }

  /** Preserve a later ack when an older same-epoch pull completes afterward. */
  assignWithinEpoch(sequence: number): void {
    this.sequence = Math.max(this.sequence, sequence);
    this.hasAssignment = true;
  }

  markDurableCheckpointValid(): void {
    this.durableCheckpointValid = true;
  }

  reset(): void {
    this.sequence = -1;
    this.resetGeneration++;
    this.hasAssignment = true;
    this.durableCheckpointValid = false;
    this.clearInFlightCoverage(false);
  }

  covers(sequence: number): boolean {
    return Number.isSafeInteger(sequence) && sequence <= this.sequence;
  }

  beginInFlightCoverage(after: number, through: number): number | undefined {
    if (
      !this.durableCheckpointValid ||
      !isContiguous(after, this.effectiveCoverage(), through)
    )
      return undefined;
    const token = ++this.nextCoverageToken;
    this.inFlightCoverage.set(token, { after, through });
    return token;
  }

  effectiveCoverage(): number {
    let effective = this.sequence;
    if (!Number.isSafeInteger(effective)) return -1;
    let advanced = true;
    while (advanced) {
      advanced = false;
      for (const { after, through } of this.inFlightCoverage.values()) {
        if (isContiguous(after, effective, through)) {
          effective = through;
          advanced = true;
        }
      }
    }
    return effective;
  }

  coversWithInFlight(sequence: number): boolean {
    return (
      Number.isSafeInteger(sequence) && sequence <= this.effectiveCoverage()
    );
  }

  markInFlightCoverageUsed(sequence: number): void {
    if (!this.covers(sequence) && this.coversWithInFlight(sequence))
      this.suppressedRequirement = Math.max(
        this.suppressedRequirement ?? sequence,
        sequence,
      );
  }

  settleInFlightCoverage(token: number): void {
    if (!this.inFlightCoverage.delete(token)) return;
    this.notifyUncoveredSuppression();
  }

  async advanceIfContiguous(
    after: number,
    through: number,
    expectedResetVersion: number,
    durableCheckpoint: number,
    persist: () => Promise<void>,
  ): Promise<boolean> {
    if (
      expectedResetVersion !== this.resetGeneration ||
      !isContiguous(after, this.sequence, through) ||
      !isContiguous(after, durableCheckpoint, through)
    )
      return false;
    await persist();
    if (
      expectedResetVersion !== this.resetGeneration ||
      !isContiguous(after, this.sequence, through)
    )
      return false;
    this.assignWithinEpoch(through);
    return true;
  }

  onInFlightCoverageInvalidated(listener: () => void): () => void {
    this.coverageInvalidationListeners.add(listener);
    return () => this.coverageInvalidationListeners.delete(listener);
  }

  private clearInFlightCoverage(notify = true): void {
    this.inFlightCoverage.clear();
    if (notify) this.notifyUncoveredSuppression();
    else this.suppressedRequirement = undefined;
  }

  private notifyUncoveredSuppression(): void {
    const required = this.suppressedRequirement;
    if (required === undefined || this.sequence >= required) {
      this.suppressedRequirement = undefined;
      return;
    }
    if (this.coversWithInFlight(required)) return;
    this.suppressedRequirement = undefined;
    for (const listener of this.coverageInvalidationListeners) listener();
  }
}

const entitySequenceCheckpoint = new EntitySequenceCheckpoint();
let openWorkspaceId: string | undefined;
let openSchemaHash: string | undefined;

/** Reset the shared object when the open projection database changes identity. */
export function setEntitySequenceProjection(
  workspaceId: string,
  schemaHash: string,
): EntitySequenceCheckpoint {
  if (openSchemaHash !== undefined && openSchemaHash !== schemaHash) {
    openWorkspaceId = undefined;
    entitySequenceCheckpoint.reset();
  }
  openSchemaHash = schemaHash;
  openWorkspaceId = workspaceId;
  entitySequenceCheckpoint.observeWorkspace(workspaceId);
  return entitySequenceCheckpoint;
}

/** Look up the active projection without letting a mutation change its identity. */
export function getEntitySequenceCheckpoint(
  scope: string,
  schemaHash: string,
): EntitySequenceCheckpoint {
  if (
    scope !== ENTITY_SCOPE ||
    openWorkspaceId === undefined ||
    schemaHash !== openSchemaHash
  )
    throw new Error("Entity projection checkpoint is not open");
  return entitySequenceCheckpoint;
}

export function getEntitySequenceCheckpointForWorkspace(
  workspaceId: string,
  scope: string,
  schemaHash: string,
): EntitySequenceCheckpoint | undefined {
  if (
    scope !== ENTITY_SCOPE ||
    workspaceId !== openWorkspaceId ||
    schemaHash !== openSchemaHash
  )
    return undefined;
  return getEntitySequenceCheckpoint(scope, schemaHash);
}

export function canAcknowledgeEntitySequenceBatch(
  checkpoint: number,
  highestSequence: number,
  persistenceCurrent: boolean,
  guardsMatch: boolean,
): boolean {
  return (
    persistenceCurrent &&
    guardsMatch &&
    Number.isSafeInteger(checkpoint) &&
    Number.isSafeInteger(highestSequence) &&
    checkpoint >= highestSequence
  );
}

export function entitySequenceInvalidationCovered(
  checkpoint: number,
  requiredSequence: number | undefined,
  unversionedInvalidation: boolean,
): boolean {
  return (
    !unversionedInvalidation &&
    requiredSequence !== undefined &&
    Number.isSafeInteger(checkpoint) &&
    Number.isSafeInteger(requiredSequence) &&
    checkpoint >= requiredSequence
  );
}
