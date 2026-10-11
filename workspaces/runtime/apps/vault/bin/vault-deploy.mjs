#!/usr/bin/env node
// Deploy or update the AWS side of a project vault (cloud/template.yaml) with the keys that
// vault-keys made, upload the site and the paper slot, and give the VM a write-only key.
//
//   node vault-deploy.mjs --project <prefix> --profile <aws profile> --region <region>
//        [--folder <Studio project folder>] [--new-writer-key]
//
// --folder configures the VM (codex_vault.py configure) when the writer has no key yet or
// --new-writer-key asks for one; the secret goes through a pipe, never to a file.
import {
  CloudFormationClient,
  CreateStackCommand,
  DescribeStacksCommand,
  UpdateStackCommand,
  waitUntilStackCreateComplete,
  waitUntilStackUpdateComplete,
} from "@aws-sdk/client-cloudformation";
import {
  CreateAccessKeyCommand,
  DeleteAccessKeyCommand,
  IAMClient,
  ListAccessKeysCommand,
} from "@aws-sdk/client-iam";
import {
  CreateBucketCommand,
  HeadBucketCommand,
  HeadObjectCommand,
  PutBucketVersioningCommand,
  PutObjectCommand,
  PutPublicAccessBlockCommand,
  S3Client,
} from "@aws-sdk/client-s3";
import { GetCallerIdentityCommand, STSClient } from "@aws-sdk/client-sts";
import { fromIni } from "@aws-sdk/credential-providers";
import { execFileSync, spawnSync } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { extname, join, relative } from "node:path";
import { parseArgs } from "node:util";

const { values: args } = parseArgs({
  options: {
    project: { type: "string" },
    profile: { type: "string" },
    region: { type: "string", default: "eu-central-1" },
    folder: { type: "string" },
    "new-writer-key": { type: "boolean", default: false },
  },
});
if (!args.project || !args.profile) {
  console.error(
    "usage: vault-deploy --project <prefix> --profile <aws profile> [--region <region>] [--folder <project folder>] [--new-writer-key]",
  );
  process.exit(2);
}
const here = new URL("..", import.meta.url).pathname;
const keys = join(homedir(), ".config/studio-vault", args.project);
const recipient = readFileSync(join(keys, "recipient.txt"), "utf8").trim();
const paperSlot = readFileSync(join(keys, "paper.age"));
const config = {
  region: args.region,
  credentials: fromIni({ profile: args.profile }),
};
const s3 = new S3Client(config);
const cfn = new CloudFormationClient(config);
const iam = new IAMClient(config);
const account = (
  await new STSClient(config).send(new GetCallerIdentityCommand({}))
).Account;
const stack = `studio-vault-${args.project.replace(/[^A-Za-z0-9-]/g, "-")}`;
const log = (...parts) => console.error(...parts);

// 1. The function code goes to an artifacts bucket of this account and region.
const artifacts = `studio-vault-artifacts-${account}-${args.region}`;
try {
  await s3.send(new HeadBucketCommand({ Bucket: artifacts }));
} catch {
  log(`creating ${artifacts}`);
  await s3.send(
    new CreateBucketCommand({
      Bucket: artifacts,
      CreateBucketConfiguration: { LocationConstraint: args.region },
    }),
  );
  await s3.send(
    new PutPublicAccessBlockCommand({
      Bucket: artifacts,
      PublicAccessBlockConfiguration: {
        BlockPublicAcls: true,
        BlockPublicPolicy: true,
        IgnorePublicAcls: true,
        RestrictPublicBuckets: true,
      },
    }),
  );
  await s3.send(
    new PutBucketVersioningCommand({
      Bucket: artifacts,
      VersioningConfiguration: { Status: "Enabled" },
    }),
  );
}
execFileSync(process.execPath, [join(here, "lambda/build.mjs")], {
  stdio: ["ignore", "ignore", "inherit"],
});
const zip = readFileSync(join(here, "dist/lambda.zip"));
const codeKey = `lambda/${createHash("sha256").update(zip).digest("hex")}.zip`;
await s3.send(
  new PutObjectCommand({ Bucket: artifacts, Key: codeKey, Body: zip }),
);

// 2. The stack. The token secret is new on every deploy; open tokens then expire at once.
const parameters = Object.entries({
  Prefix: args.project,
  Recipient: recipient,
  TokenSecret: randomBytes(32).toString("hex"),
  CodeBucket: artifacts,
  CodeKey: codeKey,
}).map(([ParameterKey, ParameterValue]) => ({ ParameterKey, ParameterValue }));
const template = readFileSync(join(here, "cloud/template.yaml"), "utf8");
let exists = true;
try {
  await cfn.send(new DescribeStacksCommand({ StackName: stack }));
} catch {
  exists = false;
}
const common = {
  StackName: stack,
  TemplateBody: template,
  Parameters: parameters,
  Capabilities: ["CAPABILITY_IAM"],
};
if (exists) {
  try {
    await cfn.send(new UpdateStackCommand(common));
    log(`updating ${stack}`);
    await waitUntilStackUpdateComplete(
      { client: cfn, maxWaitTime: 1800 },
      { StackName: stack },
    );
  } catch (error) {
    if (!String(error.message).includes("No updates are to be performed"))
      throw error;
  }
} else {
  log(`creating ${stack} (CloudFront takes some minutes)`);
  await cfn.send(new CreateStackCommand({ ...common, OnFailure: "DELETE" }));
  await waitUntilStackCreateComplete(
    { client: cfn, maxWaitTime: 3600 },
    { StackName: stack },
  );
}
const outputs = Object.fromEntries(
  (
    await cfn.send(new DescribeStacksCommand({ StackName: stack }))
  ).Stacks[0].Outputs.map((o) => [o.OutputKey, o.OutputValue]),
);

// 3. The site, its configuration, the public vault record and the paper slot.
const site = join(here, "../../../client/apps/vault-web/dist");
const types = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript",
  ".css": "text/css",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
};
const upload = async (Key, Body, ContentType) =>
  s3.send(
    new PutObjectCommand({
      Bucket: outputs.BucketName,
      Key,
      Body,
      ContentType,
    }),
  );
const walk = (dir) =>
  readdirSync(dir).flatMap((n) =>
    statSync(join(dir, n)).isDirectory() ? walk(join(dir, n)) : [join(dir, n)],
  );
for (const file of walk(site)) {
  await upload(
    `site/${relative(site, file)}`,
    readFileSync(file),
    types[extname(file)] ?? "application/octet-stream",
  );
}
await upload(
  "site/config.json",
  JSON.stringify({ functionUrl: outputs.FunctionUrl, prefix: args.project }),
  "application/json",
);
await upload(
  `${args.project}/vault.json`,
  JSON.stringify({
    format: 1,
    recipient,
    created: Math.floor(Date.now() / 1000),
  }),
  "application/json",
);
try {
  await s3.send(
    new HeadObjectCommand({
      Bucket: outputs.BucketName,
      Key: `${args.project}/slots/paper.age`,
    }),
  );
} catch {
  await s3.send(
    new PutObjectCommand({
      Bucket: outputs.BucketName,
      Key: `${args.project}/slots/paper.age`,
      Body: paperSlot,
      IfNoneMatch: "*",
    }),
  );
}

// 4. A write-only key for the VM, passed through a pipe.
if (args.folder) {
  const listed = (
    await iam.send(
      new ListAccessKeysCommand({ UserName: outputs.WriterUserName }),
    )
  ).AccessKeyMetadata;
  if (!listed.length || args["new-writer-key"]) {
    const key = (
      await iam.send(
        new CreateAccessKeyCommand({ UserName: outputs.WriterUserName }),
      )
    ).AccessKey;
    const configure = spawnSync(
      "python3",
      [join(here, "../server/src/codex_vault.py"), "configure", args.folder],
      {
        input: JSON.stringify({
          bucket: outputs.BucketName,
          region: args.region,
          prefix: args.project,
          recipient,
          accessKeyId: key.AccessKeyId,
          secretAccessKey: key.SecretAccessKey,
        }),
        stdio: ["pipe", "inherit", "inherit"],
      },
    );
    if (configure.status !== 0) {
      await iam.send(
        new DeleteAccessKeyCommand({
          UserName: outputs.WriterUserName,
          AccessKeyId: key.AccessKeyId,
        }),
      );
      throw new Error(
        "the VM did not take the writer key; the key is deleted again",
      );
    }
    for (const old of listed)
      await iam.send(
        new DeleteAccessKeyCommand({
          UserName: outputs.WriterUserName,
          AccessKeyId: old.AccessKeyId,
        }),
      );
  }
}
// Public facts for vault-restore and vault-view.
writeFileSync(
  join(keys, "cloud.json"),
  JSON.stringify(
    {
      profile: args.profile,
      region: args.region,
      bucket: outputs.BucketName,
      site: outputs.SiteUrl,
      stack,
    },
    null,
    2,
  ) + "\n",
);
console.log(
  JSON.stringify(
    {
      stack,
      site: outputs.SiteUrl,
      function: outputs.FunctionUrl,
      bucket: outputs.BucketName,
    },
    null,
    2,
  ),
);
