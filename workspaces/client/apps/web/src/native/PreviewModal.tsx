import { Modal, ModalStackContext, type ModalProps } from "@mantine/core";
import { useContext, useId, useLayoutEffect, useRef } from "react";

function Stacked(props: ModalProps) {
  const stack = useContext(ModalStackContext);
  const id = useId();
  const trigger = useRef<HTMLElement | null>(null);
  const opened = useRef(false);
  useLayoutEffect(() => {
    if (props.opened && !opened.current)
      trigger.current =
        document.activeElement instanceof HTMLElement
          ? document.activeElement
          : null;
    if (
      !props.opened &&
      opened.current &&
      props.returnFocus !== false &&
      trigger.current?.isConnected
    )
      trigger.current.focus({ preventScroll: true });
    opened.current = props.opened;
  }, [props.opened, props.returnFocus]);
  useLayoutEffect(
    () => () => {
      stack?.removeModal(id);
    },
    [id, stack],
  );
  return <Modal {...props} stackId={id} />;
}
export default function PreviewModal(props: ModalProps) {
  return useContext(ModalStackContext) ? (
    <Stacked {...props} />
  ) : (
    <Modal.Stack>
      <Stacked {...props} />
    </Modal.Stack>
  );
}
