/** One object per entity scope for the lifetime of this renderer tab. */
const entitySequenceCheckpoints = new Map<string, EntitySequenceCheckpoint>();

export class EntitySequenceCheckpoint {
  private sequence = -1;
  private epoch: string | undefined;
  private resetGeneration = 0;
  private hasAssignment = false;
  private durableCheckpointValid = true;
  private workspaceId: string | undefined;

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
  }

  covers(sequence: number): boolean {
    return sequence <= this.sequence;
  }
}

/** Return the one entity checkpoint shared by this tab's pull and mutation paths. */
export function getEntitySequenceCheckpoint(
  workspaceId: string,
  scope: string,
): EntitySequenceCheckpoint {
  let checkpoint = entitySequenceCheckpoints.get(scope);
  if (!checkpoint) {
    checkpoint = new EntitySequenceCheckpoint();
    entitySequenceCheckpoints.set(scope, checkpoint);
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
