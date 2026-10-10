type Listener = (active: boolean) => void;
type Entry = {
  inView: boolean;
  active: boolean;
  listeners: Set<Listener>;
};

const trackers = new WeakMap<Document, VisualActivityTracker>();

function insideClosedDetails(element: HTMLElement) {
  let parent = element.parentElement;
  while (parent) {
    if (parent.tagName === "DETAILS" && !parent.hasAttribute("open")) {
      const summary = [...parent.children].find(
        (child) => child.tagName === "SUMMARY",
      );
      if (!summary?.contains(element)) return true;
    }
    parent = parent.parentElement;
  }
  return false;
}

class VisualActivityTracker {
  private entries = new Map<HTMLElement, Entry>();
  private observer: IntersectionObserver | undefined;

  constructor(private document: Document) {}

  private inViewport(element: HTMLElement) {
    const view = this.document.defaultView;
    if (!view) return true;
    const bounds = element.getBoundingClientRect();
    return (
      bounds.width + bounds.height > 0 &&
      bounds.bottom > 0 &&
      bounds.right > 0 &&
      bounds.top < view.innerHeight &&
      bounds.left < view.innerWidth
    );
  }

  private publish(element: HTMLElement, entry: Entry) {
    const active =
      !this.document.hidden && entry.inView && !insideClosedDetails(element);
    element.dataset.visualActive = String(active);
    if (entry.active === active) return;
    entry.active = active;
    for (const listener of entry.listeners) listener(active);
  }

  private visibility = () => {
    this.document.documentElement.dataset.visualActive = String(
      !this.document.hidden,
    );
    for (const [element, entry] of this.entries) {
      if (!this.document.hidden)
        entry.inView = this.observer ? this.inViewport(element) : true;
      this.publish(element, entry);
    }
  };

  private toggle = (event: Event) => {
    const target = event.target;
    if (!(target instanceof HTMLElement) || target.tagName !== "DETAILS")
      return;
    for (const [element, entry] of this.entries) {
      if (!target.contains(element)) continue;
      entry.inView = this.observer ? this.inViewport(element) : true;
      this.publish(element, entry);
    }
  };

  private start() {
    const Observer = this.document.defaultView?.IntersectionObserver;
    if (Observer) {
      this.observer = new Observer((changes) => {
        for (const change of changes) {
          const element = change.target as HTMLElement;
          const entry = this.entries.get(element);
          if (!entry) continue;
          entry.inView =
            change.isIntersecting &&
            change.boundingClientRect.width + change.boundingClientRect.height >
              0;
          this.publish(element, entry);
        }
      });
    }
    this.document.addEventListener("visibilitychange", this.visibility);
    this.document.addEventListener("toggle", this.toggle, true);
    this.document.documentElement.dataset.visualActive = String(
      !this.document.hidden,
    );
  }

  watch(element: HTMLElement, listener: Listener) {
    if (!this.entries.size) this.start();
    let entry = this.entries.get(element);
    if (!entry) {
      // Browsers without IntersectionObserver retain visible-tab updates.
      entry = {
        inView: this.observer ? this.inViewport(element) : true,
        active: true,
        listeners: new Set(),
      };
      this.entries.set(element, entry);
      this.observer?.observe(element);
    }
    this.publish(element, entry);
    entry.listeners.add(listener);
    listener(entry.active);
    let stopped = false;
    return () => {
      if (stopped) return;
      stopped = true;
      entry.listeners.delete(listener);
      if (entry.listeners.size) return;
      this.entries.delete(element);
      this.observer?.unobserve(element);
      delete element.dataset.visualActive;
      if (this.entries.size) return;
      this.observer?.disconnect();
      this.observer = undefined;
      this.document.removeEventListener("visibilitychange", this.visibility);
      this.document.removeEventListener("toggle", this.toggle, true);
      delete this.document.documentElement.dataset.visualActive;
    };
  }
}

export function watchVisualActivity(element: HTMLElement, listener: Listener) {
  const document = element.ownerDocument;
  let tracker = trackers.get(document);
  if (!tracker) {
    tracker = new VisualActivityTracker(document);
    trackers.set(document, tracker);
  }
  return tracker.watch(element, listener);
}
