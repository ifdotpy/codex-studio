/** Tracks the last server checkpoint used to suppress already-covered hints. */
export class EntitySequenceCheckpoint {
  private sequence = -1;
  private epoch: string | undefined;
  private resetGeneration = 0;

  get value() {
    return this.sequence;
  }

  get resetVersion() {
    return this.resetGeneration;
  }

  isSameResetVersion(version: number): boolean {
    return this.resetGeneration === version;
  }

  observeEpoch(epoch: string): boolean {
    if (this.epoch === undefined) {
      this.epoch = epoch;
      return false;
    }
    if (this.epoch === epoch) return false;
    this.epoch = epoch;
    this.sequence = -1;
    this.resetGeneration++;
    return true;
  }

  assign(sequence: number): void {
    this.sequence = sequence;
  }

  /** Preserve a later ack when an older same-epoch pull completes afterward. */
  assignWithinEpoch(sequence: number): void {
    this.sequence = Math.max(this.sequence, sequence);
  }

  reset(): void {
    this.sequence = -1;
    this.resetGeneration++;
  }

  advanceContiguous(sequences: number[]): boolean {
    if (this.sequence < 0 || !sequences.length) return false;
    const ordered = [...new Set(sequences)].sort((left, right) => left - right);
    if (ordered[0] !== this.sequence + 1) return false;
    for (let index = 1; index < ordered.length; index++)
      if (ordered[index] !== ordered[index - 1] + 1) return false;
    this.sequence = ordered.at(-1)!;
    return true;
  }

  covers(sequence: number): boolean {
    return sequence <= this.sequence;
  }
}
