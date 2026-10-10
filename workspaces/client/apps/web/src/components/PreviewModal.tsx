import { Modal, ModalStackContext, type ModalProps } from "@mantine/core";
import { useContext, useId, useLayoutEffect, useRef } from "react";

function StackedPreviewModal(props: ModalProps) {
  const stack = useContext(ModalStackContext);
  const stackId = useId();
  const currentStack = useRef(stack);
  currentStack.current = stack;
  const content = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLElement | null>(null);
  const wasOpened = useRef(false);
  const returnEnabled = useRef(props.returnFocus !== false);
  returnEnabled.current = props.returnFocus !== false;
  const cancelReturn = useRef<(() => void) | null>(null);

  const returnToTrigger = () => {
    cancelReturn.current?.();
    if (!returnEnabled.current || !trigger.current) return;
    const target = trigger.current;
    const closingDialog = content.current;
    const parentDialog = target.closest('[role="dialog"]');
    const activeAtClose = document.activeElement;
    // The stack reactivates the parent's focus trap when its child closes.
    // Return after that autofocus, but never override new keyboard/pointer input.
    const cancel = () => {
      window.clearTimeout(timer);
      document.removeEventListener("keydown", cancelOnKey, true);
      document.removeEventListener("pointerdown", cancel, true);
      if (cancelReturn.current === cancel) cancelReturn.current = null;
    };
    // Closing can synchronously commit during the same Escape event. Do not
    // interpret that event as new input after the close.
    const cancelOnKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") cancel();
    };
    const timer = window.setTimeout(() => {
      cancel();
      const active = document.activeElement;
      if (
        target.isConnected &&
        !target.closest("[data-hidden], [hidden]") &&
        (!active ||
          active === document.body ||
          active === activeAtClose ||
          closingDialog?.contains(active) ||
          parentDialog?.contains(active))
      )
        target.focus({ preventScroll: true });
    }, 10);
    document.addEventListener("keydown", cancelOnKey, true);
    document.addEventListener("pointerdown", cancel, true);
    cancelReturn.current = cancel;
  };

  useLayoutEffect(() => {
    if (props.opened) cancelReturn.current?.();
    if (props.opened && !wasOpened.current) {
      // Capture before Mantine's focus trap runs its deferred autofocus, including
      // a FilePreview that is first mounted with opened=true.
      trigger.current =
        document.activeElement instanceof HTMLElement
          ? document.activeElement
          : null;
    } else if (!props.opened && wasOpened.current) returnToTrigger();
    wasOpened.current = props.opened;
  }, [props.opened]);

  useLayoutEffect(
    () => () => {
      // Mantine removes closed entries but not an open modal on unmount. A file
      // switch can remove a nested preview before it receives opened=false.
      currentStack.current?.removeModal(stackId);
      if (wasOpened.current) returnToTrigger();
    },
    [stackId],
  );
  return (
    <Modal {...props} ref={content} stackId={stackId} returnFocus={false} />
  );
}

// Each top-level preview owns one stack. Nested file and image previews reuse it.
export default function PreviewModal(props: ModalProps) {
  const stack = useContext(ModalStackContext);
  return stack ? (
    <StackedPreviewModal {...props} />
  ) : (
    <Modal.Stack>
      <StackedPreviewModal {...props} />
    </Modal.Stack>
  );
}
