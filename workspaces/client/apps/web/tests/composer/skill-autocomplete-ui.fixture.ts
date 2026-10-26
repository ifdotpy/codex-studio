import type { GetResult } from "../../src/apiTypes.ts";
export const catalogSkills = [
  {
    name: "review",
    description: "Review a change",
    path: "/skills/review",
  },
  {
    name: "release",
    description: "Prepare a release",
    path: "/skills/release",
  },
  {
    name: "research",
    description: "Research a topic",
    path: "/skills/research",
  },
  {
    name: "plugin-management:plugin-management",
    description: "Installed namespaced skill",
    path: "/skills/plugin-management/plugin-management",
  },
  {
    name: "openai-templates:artifact-template-analytics-dashboard",
    description: "A long namespaced native skill for narrow composer layouts",
    path: "/skills/openai-templates/artifact-template-analytics-dashboard",
  },
  ...Array.from({ length: 500 }, (_, index) => ({
    name: `review-extra-${index}`,
    description: "Large-catalog fixture",
    path: `/skills/review-extra-${index}`,
  })),
] satisfies GetResult<"/api/skills">["skills"];
