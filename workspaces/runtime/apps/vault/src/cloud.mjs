// CloudFormation and local settings shared by vault-deploy and vault-gateway (docs/vault.md).
import {
  CloudFormationClient,
  CreateStackCommand,
  DescribeStacksCommand,
  UpdateStackCommand,
  waitUntilStackCreateComplete,
  waitUntilStackUpdateComplete,
} from "@aws-sdk/client-cloudformation";
import { fromIni } from "@aws-sdk/credential-providers";
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export const here = new URL("..", import.meta.url).pathname;
export const settings = join(homedir(), ".config/studio-vault");
export const log = (...parts) => console.error(...parts);

export const awsConfig = (profile, region) => ({
  region,
  credentials: fromIni({ profile }),
});

/** The stack's outputs and parameters after it is created or updated. */
export async function stackOutputs(cfn, StackName) {
  const [stack] = (await cfn.send(new DescribeStacksCommand({ StackName })))
    .Stacks;
  return {
    outputs: Object.fromEntries(
      (stack.Outputs ?? []).map((o) => [o.OutputKey, o.OutputValue]),
    ),
    parameters: Object.fromEntries(
      (stack.Parameters ?? []).map((p) => [p.ParameterKey, p.ParameterValue]),
    ),
  };
}

export async function stackExists(cfn, StackName) {
  try {
    await cfn.send(new DescribeStacksCommand({ StackName }));
    return true;
  } catch {
    return false;
  }
}

/** Create or update a stack from cloud/<template> and give its outputs. */
export async function deployStack(cfn, StackName, template, parameters = {}) {
  const common = {
    StackName,
    TemplateBody: readFileSync(join(here, "cloud", template), "utf8"),
    Parameters: Object.entries(parameters).map(
      ([ParameterKey, ParameterValue]) => ({ ParameterKey, ParameterValue }),
    ),
    Capabilities: ["CAPABILITY_IAM", "CAPABILITY_NAMED_IAM"],
  };
  if (await stackExists(cfn, StackName)) {
    try {
      await cfn.send(new UpdateStackCommand(common));
      log(`updating ${StackName}`);
      await waitUntilStackUpdateComplete(
        { client: cfn, maxWaitTime: 3600 },
        { StackName },
      );
    } catch (error) {
      if (!String(error.message).includes("No updates are to be performed"))
        throw error;
    }
  } else {
    log(`creating ${StackName}`);
    await cfn.send(new CreateStackCommand({ ...common, OnFailure: "DELETE" }));
    await waitUntilStackCreateComplete(
      { client: cfn, maxWaitTime: 3600 },
      { StackName },
    );
  }
  return (await stackOutputs(cfn, StackName)).outputs;
}

export { CloudFormationClient };
