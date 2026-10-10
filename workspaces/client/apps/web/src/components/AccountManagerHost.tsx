import { Modal } from "@mantine/core";
import type { ReactNode } from "react";

export default function AccountManagerHost({
  inline,
  opened,
  title,
  closeBlocked,
  onClose,
  children,
}: {
  inline: boolean;
  opened: boolean;
  title: string;
  closeBlocked: boolean;
  onClose: () => void;
  children: ReactNode;
}) {
  if (inline)
    return <section className="accounts-manager-inline">{children}</section>;
  return (
    <Modal
      opened={opened}
      onClose={onClose}
      title={title}
      closeOnEscape={!closeBlocked}
      closeOnClickOutside={!closeBlocked}
      size="lg"
      classNames={{ body: "accounts-manager" }}
    >
      {children}
    </Modal>
  );
}
