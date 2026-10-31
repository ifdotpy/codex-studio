/** A replaying callback history for assertions against streamed test events. */
export class EventHistory<T> {
  private readonly values: T[] = [];
  private readonly listeners = new Set<() => void>();

  private readonly limit: number;

  constructor(limit = Infinity) {
    this.limit = limit;
  }

  next(value: T): void {
    this.values.push(value);
    if (this.values.length > this.limit) this.values.shift();
    for (const listener of this.listeners) listener();
  }

  wait(accept: (value: T) => boolean = () => true, skip = 0): Promise<T> {
    return new Promise((resolve, reject) => {
      const check = () => {
        const match = this.values.slice(skip).find(accept);
        if (match !== undefined) {
          clearTimeout(timer);
          this.listeners.delete(check);
          resolve(match);
        }
      };
      const timer = setTimeout(() => {
        this.listeners.delete(check);
        reject(new Error("Timed out waiting for a streamed event"));
      }, 5000);
      this.listeners.add(check);
      check();
    });
  }
}
