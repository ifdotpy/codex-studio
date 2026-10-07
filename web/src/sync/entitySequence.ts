/** One object per schema hash and entity scope for this renderer tab. */
const entitySequenceCheckpoints = new Map<string, EntitySequenceCheckpoint>();

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
    { after: number; through: number; resetVersion: number }
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
    return sequence <= this.sequence;
  }

  beginInFlightCoverage(after: number, through: number): number | undefined {
    if (
      !Number.isSafeInteger(after) ||
      !Number.isSafeInteger(through) ||
      through <= after
    )
      return undefined;
    const token = ++this.nextCoverageToken;
    this.inFlightCoverage.set(token, {
      after,
      through,
      resetVersion: this.resetGeneration,
    });
    return token;
  }

  effectiveCoverage(): number {
    let effective = this.sequence;
    if (!Number.isSafeInteger(effective)) return -1;
    let advanced = true;
    while (advanced) {
      advanced = false;
      for (const coverage of this.inFlightCoverage.values()) {
        if (
          coverage.resetVersion === this.resetGeneration &&
          coverage.after <= effective &&
          coverage.through > effective
        ) {
          effective = coverage.through;
          advanced = true;
        }
      }
    }
    return effective;
  }

  coversWithInFlight(sequence: number): boolean {
    return (
      Number.isSafeInteger(sequence) && this.effectiveCoverage() >= sequence
    );
  }

  markInFlightCoverageUsed(sequence: number): void {
    if (!this.covers(sequence) && this.coversWithInFlight(sequence))
      this.suppressedRequirement = Math.max(
        this.suppressedRequirement ?? sequence,
        sequence,
      );
  }

  settleInFlightCoverage(token: number, persisted: boolean): void {
    const coverage = this.inFlightCoverage.get(token);
    if (!coverage) return;
    this.inFlightCoverage.delete(token);
    // A successful persister may have advanced the durable checkpoint. A
    // reset or out-of-order response can still make dependent coverage invalid.
    if (
      !persisted ||
      coverage.resetVersion !== this.resetGeneration ||
      this.suppressedRequirement !== undefined
    )
      this.notifyUncoveredSuppression();
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

/** Return the one entity checkpoint shared by this tab's pull and mutation paths. */
export function getEntitySequenceCheckpoint(
  workspaceId: string,
  scope: string,
  schemaHash: string,
): EntitySequenceCheckpoint {
  if (scope !== "state:entities:v1") return new EntitySequenceCheckpoint();
  // Projection databases include the schema hash in their identity, so an
  // in-memory checkpoint from a different generated API schema is not valid.
  // Workspace changes keep this object and reset it through observeWorkspace.
  const key = `${schemaHash}:${scope}`;
  let checkpoint = entitySequenceCheckpoints.get(key);
  if (!checkpoint) {
    checkpoint = new EntitySequenceCheckpoint();
    entitySequenceCheckpoints.set(key, checkpoint);
  }
  checkpoint.observeWorkspace(workspaceId);
  return checkpoint;
}

export type EntitySequenceGuard = {
  checkpoint: EntitySequenceCheckpoint;
  sequence: number;
  initialized: boolean;
  resetVersion: number;
};

export function captureEntitySequenceGuards(
  checkpoint: EntitySequenceCheckpoint,
): EntitySequenceGuard[] {
  return [
    {
      checkpoint,
      sequence: checkpoint.value,
      initialized: checkpoint.initialized,
      resetVersion: checkpoint.resetVersion,
    },
  ];
}

export function entitySequenceGuardsMatch(
  guards: EntitySequenceGuard[],
  expected: number,
): boolean {
  return guards.some(
    (guard) =>
      guard.sequence === expected &&
      guard.checkpoint.value === expected &&
      guard.checkpoint.isSameResetVersion(guard.resetVersion),
  );
}

export function entitySequenceGuardsCanAdvance(
  guards: EntitySequenceGuard[],
  expected: number,
): boolean {
  return guards.some(
    (guard) =>
      // The persisted checkpoint equality is authoritative when this guard is
      // still at its never-initialized sentinel; resets change resetVersion.
      ((guard.sequence === expected && guard.checkpoint.value === expected) ||
        (guard.sequence === -1 &&
          !guard.initialized &&
          guard.checkpoint.value === -1 &&
          !guard.checkpoint.initialized)) &&
      guard.checkpoint.isSameResetVersion(guard.resetVersion),
  );
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

export async function pullUnlessEntitySequenceInvalidationCovered<T>(
  checkpoint: number,
  requiredSequence: number | undefined,
  unversionedInvalidation: boolean,
  request: () => Promise<T>,
): Promise<{ skipped: true } | { skipped: false; value: T }> {
  if (
    entitySequenceInvalidationCovered(
      checkpoint,
      requiredSequence,
      unversionedInvalidation,
    )
  )
    return { skipped: true };
  return { skipped: false, value: await request() };
}

export function mutationEntityCheckpointAdvance(
  checkpoint: number,
  syncEntitiesAfter: number | null | undefined,
  highestSequence: number,
  persistenceCurrent: boolean,
  guardsMatch: boolean,
): number | undefined {
  if (
    typeof syncEntitiesAfter !== "number" ||
    !Number.isSafeInteger(syncEntitiesAfter) ||
    !Number.isSafeInteger(checkpoint) ||
    !Number.isSafeInteger(highestSequence) ||
    !persistenceCurrent ||
    !guardsMatch ||
    checkpoint !== syncEntitiesAfter ||
    highestSequence <= syncEntitiesAfter
  )
    return undefined;
  return highestSequence;
}

export function advanceEntitySequenceGuards(
  guards: EntitySequenceGuard[],
  expected: number,
  sequence: number,
): boolean {
  if (!entitySequenceGuardsCanAdvance(guards, expected)) return false;
  let advanced = false;
  for (const guard of guards) {
    if (
      ((guard.sequence === expected && guard.checkpoint.value === expected) ||
        (guard.sequence === -1 &&
          !guard.initialized &&
          guard.checkpoint.value === -1 &&
          !guard.checkpoint.initialized)) &&
      guard.checkpoint.isSameResetVersion(guard.resetVersion)
    ) {
      guard.checkpoint.assignWithinEpoch(sequence);
      advanced = true;
    }
  }
  return advanced;
}
