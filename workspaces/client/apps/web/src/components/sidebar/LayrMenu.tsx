import { Menu } from "@mantine/core";
import { useEffect, useState, type ReactNode } from "react";
import ProjectLayrReadiness from "./LayrReadiness";
import type { ProjectChip } from "./MacSync";

/**
 * The LAYR chip and the project's layr menu. Besides Mantine's own checks, the menu closes on
 * Escape anywhere in the window and when the window loses focus: a click in the frame of the
 * main view moves focus there, and Mantine's outside-click check does not see that click.
 */
export default function LayrMenu({
  chip,
  children,
}: {
  chip: ProjectChip;
  children: ReactNode;
}) {
  const [opened, setOpened] = useState(false);
  useEffect(() => {
    if (!opened) return;
    const close = () => setOpened(false);
    const key = (event: KeyboardEvent) => {
      if (event.key === "Escape") close();
    };
    window.addEventListener("keydown", key, true);
    window.addEventListener("blur", close);
    return () => {
      window.removeEventListener("keydown", key, true);
      window.removeEventListener("blur", close);
    };
  }, [opened]);
  return (
    <div className="project-tree-layr">
      <Menu
        withinPortal
        position="bottom-start"
        opened={opened}
        onChange={setOpened}
      >
        <Menu.Target>
          <ProjectLayrReadiness
            summary={chip.summary}
            attention={chip.attention}
            vault={chip.vault}
          />
        </Menu.Target>
        <Menu.Dropdown>{children}</Menu.Dropdown>
      </Menu>
    </div>
  );
}
