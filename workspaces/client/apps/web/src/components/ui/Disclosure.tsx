import { Collapse, UnstyledButton } from "@mantine/core";
import { ChevronRight } from "lucide-react";
import { useId, useState, type ReactNode } from "react";

/** Button controls participate in Mantine's focus trap; closed content cannot. */
export default function Disclosure({
  className = "",
  summary,
  ariaLabel,
  open,
  defaultOpen = false,
  onChange,
  chevron = false,
  children,
}: {
  className?: string;
  summary: ReactNode;
  ariaLabel?: string;
  open?: boolean;
  defaultOpen?: boolean;
  onChange?: (opened: boolean) => void;
  chevron?: boolean;
  children: ReactNode;
}) {
  const id = useId();
  const [localOpened, setLocalOpened] = useState(defaultOpen);
  const expanded = open ?? localOpened;
  return (
    <div
      className={`disclosure ${className}`}
      data-expanded={expanded || undefined}
    >
      <UnstyledButton
        className="disclosure-control"
        aria-label={ariaLabel}
        title={ariaLabel}
        aria-expanded={expanded}
        aria-controls={id}
        onClick={() => {
          setLocalOpened(!expanded);
          onChange?.(!expanded);
        }}
      >
        {chevron && (
          <ChevronRight className="disclosure-chevron" aria-hidden="true" />
        )}
        {summary}
      </UnstyledButton>
      <Collapse id={id} expanded={expanded}>
        {expanded && children}
      </Collapse>
    </div>
  );
}
