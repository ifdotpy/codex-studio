// The wire contract is owned by runtime-schema/src/worker_capabilities.rs.
import fs from "node:fs/promises";
import { listSkills } from "./skills.mjs";
import { localConfigs } from "./move-external.mjs";

export function capabilityError(field, name, reason) {
  return Object.assign(
    new Error(`capabilityProfile.${field} ${JSON.stringify(name)}: ${reason}`),
    {
      capabilityPreparationFailed: true,
      data: { turnStartOutcome: "not_applied", inputSubmitted: false },
    },
  );
}

function matches(pattern, name) {
  const escaped = pattern
    .split("*")
    .map((part) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"))
    .join(".*");
  return new RegExp(`^${escaped}$`).test(name);
}

export function capabilityToolDenial(profile, name, mcpServer) {
  // Current CLIs supply provenance. Keep the prefix fallback for older CLIs.
  const studio = mcpServer
    ? mcpServer.name === "studio" && mcpServer.source === "sdk"
    : name.startsWith("mcp__studio__");
  if (studio) return null;
  if (name === "Agent") return "Studio manages agents. Native Agent is denied.";
  if (profile?.disallowedTools?.some((pattern) => matches(pattern, name)))
    return `capabilityProfile.disallowedTools denies ${name}`;
  if (
    profile?.allowedTools != null &&
    !profile.allowedTools.some((pattern) => matches(pattern, name))
  )
    return `capabilityProfile.allowedTools does not include ${name}`;
  return null;
}

export function capabilityToolOptions(profile) {
  const allowed = profile?.allowedTools;
  const denied = profile?.disallowedTools || [];
  const options = {
    // CLI deny rules do not support arbitrary tool-name globs. The hook below
    // handles these and MCP names, whose CLI rules can also match whole servers.
    disallowedTools: [
      ...new Set([
        "Agent",
        ...denied.filter(
          (name) => !name.includes("*") && !name.startsWith("mcp__"),
        ),
      ]),
    ],
  };
  if (allowed != null) {
    // --tools controls built-ins only. MCP tools and wildcard rules need the
    // execution guard too, including under bypassPermissions and SDK approvals.
    const builtins = allowed.filter((name) => !name.startsWith("mcp__"));
    options.tools = builtins.some((name) => name.includes("*"))
      ? { type: "preset", preset: "claude_code" }
      : builtins.filter((name) => !capabilityToolDenial(profile, name));
  }
  if (allowed != null || denied.length) {
    options.hooks = {
      PreToolUse: [
        {
          hooks: [
            async (input) => {
              const reason = capabilityToolDenial(
                profile,
                input.tool_name,
                input.mcp_server,
              );
              return reason
                ? {
                    hookSpecificOutput: {
                      hookEventName: "PreToolUse",
                      permissionDecision: "deny",
                      permissionDecisionReason: reason,
                    },
                  }
                : {};
            },
          ],
        },
      ],
    };
  }
  return options;
}

export async function prepareCapabilities(session) {
  const profile = session.capabilityProfile;
  const servers = profile?.mcpServers;
  let mcpServers;
  if (servers != null) {
    let configured = {};
    if (servers.length) {
      try {
        configured = await localConfigs(session.cwd);
      } catch {
        throw capabilityError(
          "mcpServers",
          servers.join(", "),
          "cannot read MCP configuration",
        );
      }
    }
    mcpServers = {};
    for (const name of servers) {
      if (name === "studio")
        throw capabilityError("mcpServers", name, "reserved for Studio");
      if (!Object.hasOwn(configured, name) || !configured[name])
        throw capabilityError("mcpServers", name, "not configured");
      Object.defineProperty(mcpServers, name, {
        value: configured[name],
        enumerable: true,
        configurable: true,
      });
    }
  }
  const instructions = [];
  if (profile?.skills?.length) {
    const { data } = await listSkills([session.cwd]);
    for (const name of profile.skills) {
      const skill = data[0].skills.find((entry) => entry.name === name);
      if (!skill) throw capabilityError("skills", name, "not found");
      try {
        instructions.push(
          `Skill ${name} (${skill.path}):\n${await fs.readFile(skill.path, "utf8")}`,
        );
      } catch {
        throw capabilityError("skills", name, "cannot read SKILL.md");
      }
    }
  }
  return { mcpServers, instructions: instructions.join("\n\n") };
}

export async function preflightCapabilityServers(query, profile) {
  if (!profile?.mcpServers?.length) return;
  let statuses;
  try {
    statuses = await query.mcpServerStatus();
    for (const name of profile.mcpServers) {
      if (
        statuses.find((server) => server.name === name)?.status === "pending"
      ) {
        try {
          await query.reconnectMcpServer(name);
        } catch {
          throw capabilityError("mcpServers", name, "connection failed");
        }
      }
    }
    statuses = await query.mcpServerStatus();
  } catch (error) {
    if (error.capabilityPreparationFailed) throw error;
    throw capabilityError(
      "mcpServers",
      profile.mcpServers.join(", "),
      "status check failed",
    );
  }
  for (const name of profile.mcpServers) {
    const status = statuses.find((server) => server.name === name)?.status;
    if (status !== "connected")
      throw capabilityError(
        "mcpServers",
        name,
        status || "missing from SDK status",
      );
  }
}
