import {
  Alert,
  Button,
  Code,
  Group,
  Modal,
  Select,
  Stack,
  Text,
  TextInput,
  Title,
} from "@mantine/core";
import { useCallback, useEffect, useRef, useState } from "react";
import { errorText } from "../../api";
import type { SidebarBackend } from "./services";
import {
  firstProfile,
  runJob,
  type VaultEntry,
  type VaultLogin,
  type VaultSetupFacts,
  type VaultSetupJob,
} from "./Vault";

/**
 * Set up the vault of a project on this Mac (layr-desk docs/design.md, "Set up a vault for a
 * project"): the paper key shows in Terminal only, an AWS profile signs in through the
 * browser, and this Mac's server runs layr-desk deploy, which also gives the VM writer its keys.
 */
export default function VaultSetup({
  entry,
  name,
  backend,
  onClose,
  onDone,
}: {
  entry: VaultEntry;
  name: string;
  backend: Pick<SidebarBackend, "post">;
  onClose: () => void;
  onDone: (text: string) => void;
}) {
  const projectId = entry.vault.projectId;
  const [facts, setFacts] = useState<VaultSetupFacts | null>(null);
  const [prefix, setPrefix] = useState("");
  const [profile, setProfile] = useState<string | null>(null);
  const [region, setRegion] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [waitingForKeys, setWaitingForKeys] = useState(false);
  const [login, setLogin] = useState<VaultLogin | null>(null);
  const [job, setJob] = useState<VaultSetupJob | null>(null);
  const open = useRef(true);
  useEffect(
    () => () => {
      open.current = false;
    },
    [],
  );

  const load = useCallback(
    async (name?: string) => {
      try {
        const found = (await backend.post(
          "/api/vault/setup/facts",
          { projectId, ...(name ? { prefix: name } : {}) },
          { timeoutMs: 30000 },
        )) as VaultSetupFacts;
        if (!open.current) return null;
        setFacts(found);
        setPrefix(found.prefix);
        setRegion((current) => current || found.region);
        setProfile((current) => current ?? firstProfile(found));
        setError(null);
        return found;
      } catch (failure) {
        if (open.current) setError(errorText(failure));
        return null;
      }
    },
    [backend, projectId],
  );
  useEffect(() => {
    void load();
  }, [load]);

  // After Terminal shows the words, look for the keys until they are there.
  useEffect(() => {
    if (!waitingForKeys) return;
    const timer = setInterval(() => {
      void load(prefix).then((found) => {
        if (found?.keys) setWaitingForKeys(false);
      });
    }, 2000);
    return () => clearInterval(timer);
  }, [waitingForKeys, load, prefix]);

  const showKeys = async () => {
    setError(null);
    try {
      await window.codexDesktop!.showVaultKeys(prefix);
      setWaitingForKeys(true);
    } catch (failure) {
      setError(errorText(failure));
    }
  };

  const signIn = async () => {
    if (!profile) return;
    setError(null);
    try {
      const done = await runJob<VaultLogin>(
        backend,
        "/api/vault/aws-login",
        { profile, requestId: crypto.randomUUID() },
        { onUpdate: (update) => open.current && setLogin(update) },
      );
      if (!open.current) return;
      if (done.state === "failed") setError(done.error ?? "The sign-in failed");
      setLogin(null);
      // A setup that asked for this sign-in can start again.
      setJob(null);
      await load(prefix);
    } catch (failure) {
      if (open.current) setError(errorText(failure));
    }
  };

  const create = async () => {
    if (!profile) return;
    setError(null);
    try {
      const done = await runJob<VaultSetupJob>(
        backend,
        "/api/vault/setup",
        { projectId, requestId: crypto.randomUUID(), prefix, profile, region },
        { onUpdate: (update) => open.current && setJob(update) },
      );
      if (!open.current) return;
      if (done.state === "done")
        onDone(`${name}: the vault ${prefix} is ready`);
      if (done.state === "signin") await load(prefix);
      if (done.state === "failed" || done.state === "signin")
        setError(done.error ?? "The setup failed");
    } catch (failure) {
      if (open.current) setError(errorText(failure));
    }
  };

  const chosen = facts?.profiles.find((p) => p.name === profile);
  const needsSignIn =
    job?.state === "signin" || (chosen && chosen.signedIn === false);
  const running = job?.state === "running";
  const done = job?.state === "done";
  const desktop = !!window.codexDesktop?.showVaultKeys;
  return (
    <Modal opened onClose={onClose} title={`Set up a LAYR vault for ${name}`}>
      <Stack>
        <Text size="sm">
          The vault keeps an end-to-end encrypted copy of each saved state of
          the project in your AWS account. Desk and this Mac decrypt it; AWS and
          the gateway see only ciphertext.
        </Text>
        {!facts && !error && <Text size="sm">Loading…</Text>}
        {facts && !facts.site && (
          <Alert color="yellow">
            This Mac has no Desk gateway yet (~/.config/studio-vault/
            gateway.json). Set one up first: layr-desk docs/self-hosted.md.
          </Alert>
        )}
        {facts && !done && (
          <>
            <TextInput
              label="Vault name"
              description="The vault's address on Desk"
              value={prefix}
              disabled={facts.keys || running}
              onChange={(event) => setPrefix(event.currentTarget.value)}
              onBlur={() => prefix !== facts.prefix && void load(prefix)}
            />
            <Title order={6}>1. Paper key</Title>
            {facts.keys ? (
              <Text size="sm" c="green">
                The paper key of {facts.prefix} is made.
              </Text>
            ) : desktop ? (
              <>
                <Text size="sm">
                  Terminal shows the 24 words once. Write them on paper twice,
                  then press Enter in Terminal. Studio does not see the words.
                </Text>
                <Group>
                  <Button
                    variant="default"
                    loading={waitingForKeys}
                    onClick={() => void showKeys()}
                  >
                    Show the paper key in Terminal
                  </Button>
                </Group>
              </>
            ) : (
              <Text size="sm">
                Run this in a terminal of this Mac, then reopen this dialog:{" "}
                <Code>{`layr-desk keys --project ${facts.prefix} --clear`}</Code>
              </Text>
            )}
            <Title order={6}>2. AWS account</Title>
            <Group grow align="flex-start">
              <Select
                label="AWS profile"
                data={facts.profiles
                  .filter((p) => !p.error)
                  .map((p) => ({
                    value: p.name,
                    label: p.signedIn ? p.name : `${p.name} (sign-in needed)`,
                  }))}
                value={profile}
                onChange={setProfile}
                disabled={running}
                allowDeselect={false}
              />
              <TextInput
                label="Region"
                value={region}
                disabled={running}
                onChange={(event) => setRegion(event.currentTarget.value)}
              />
            </Group>
            {needsSignIn &&
              (login?.page ? (
                <Stack gap={4}>
                  <Text size="sm">
                    Confirm the code <Code>{login.code}</Code> on the AWS page,
                    then come back here.
                  </Text>
                  <Group>
                    <Button
                      component="a"
                      href={login.page}
                      target="_blank"
                      rel="noreferrer"
                      variant="default"
                    >
                      Open the AWS sign-in page
                    </Button>
                  </Group>
                </Stack>
              ) : (
                <Group>
                  <Button
                    variant="default"
                    loading={login?.state === "running"}
                    onClick={() => void signIn()}
                  >
                    Sign in to AWS
                  </Button>
                </Group>
              ))}
            <Title order={6}>3. Vault</Title>
            {!running && (
              <Text size="sm">
                Create vault makes the vault in this AWS account, adds it to
                Desk, and gives the VM its write key.
              </Text>
            )}
            {running && (
              <Text size="sm">
                {job?.progress ?? "Starting…"} This takes one to three minutes.
              </Text>
            )}
          </>
        )}
        {done && (
          <Text size="sm">
            The vault is ready. Open it on Desk, unlock it with the paper key,
            and add a passkey. The VM saves a new state within 10 minutes.
          </Text>
        )}
        {error && (
          <Text size="sm" c="red" role="alert">
            {error}
          </Text>
        )}
        <Group justify="flex-end">
          <Button variant="default" onClick={onClose}>
            {done ? "Close" : "Cancel"}
          </Button>
          {done && job?.site ? (
            <Button
              component="a"
              href={job.site}
              target="_blank"
              rel="noreferrer"
            >
              Open in Desk
            </Button>
          ) : (
            <Button
              loading={running}
              disabled={
                !facts?.keys ||
                !facts.site ||
                !profile ||
                !region ||
                !!needsSignIn
              }
              onClick={() => void create()}
            >
              Create vault
            </Button>
          )}
        </Group>
      </Stack>
    </Modal>
  );
}
