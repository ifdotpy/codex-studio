#!/usr/bin/env node
// Make the keys of a new vault. Run it yourself in a terminal: it shows the paper key once and
// keeps only public data (the recipient and the encrypted paper slot) in ~/.config/studio-vault.
//
//   node vault-keys.mjs <project prefix>
import { existsSync, mkdirSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { generateIdentity, recipientOf, sealSlot } from "../src/keys.mjs";

const prefix = process.argv[2];
if (!/^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$/.test(prefix ?? "")) {
  console.error("usage: vault-keys <project prefix>");
  process.exit(2);
}
const folder = join(homedir(), ".config/studio-vault", prefix);
if (existsSync(join(folder, "recipient.txt"))) {
  console.error(
    `${folder} has keys already; a new vault identity would cut off the old states.`,
  );
  process.exit(1);
}
const vault = await generateIdentity();
const paper = await generateIdentity();
const slot = await sealSlot(vault, await recipientOf(paper));
mkdirSync(folder, { recursive: true, mode: 0o700 });
writeFileSync(join(folder, "recipient.txt"), (await recipientOf(vault)) + "\n");
writeFileSync(join(folder, "paper.age"), slot);
const groups = paper.match(/.{1,6}/g).join(" ");
process.stdout.write(`
Paper key of the vault "${prefix}". Write it on paper twice and keep the copies apart.
It is shown once and stored nowhere. Without it and without a passkey the vault cannot be read.

    ${paper}

Grouped for writing:
    ${groups}

Next: vault-deploy reads ${folder}.
`);
