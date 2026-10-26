import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, waitFor, within } from "storybook/test";
import VaultSetup from "./VaultSetup";
import type {
  ResourceRef,
  ResourceVersion,
} from "../../sync/subscriptions/events";

type Reply = Record<string, unknown>;

/** A fake of this Mac's server: the setup facts, the AWS sign-in and the setup job. */
function server({ keys = false, signedIn = true } = {}) {
  const state = { keys, signedIn, posts: [] as [string, Reply][] };
  const facts = () => ({
    prefix: "my-project",
    keys: state.keys,
    site: "https://desk.example.com/",
    region: "eu-central-1",
    gatewayProfile: "work",
    profiles: [
      { name: "home", sso: true, signedIn: true },
      { name: "work", sso: true, signedIn: state.signedIn },
    ],
  });
  const jobs = new Map<string, Reply>();
  const listeners = new Map<string, (version?: ResourceVersion) => void>();
  const resources = {
    watchResourceChanges(
      resource: ResourceRef,
      accept: (version?: ResourceVersion) => void,
    ) {
      if (resource.kind === "vault-readiness") {
        accept({
          epoch: "story",
          revision: 1,
          progress: { resource, value: state.keys },
        });
        return () => {};
      }
      if (resource.kind !== "vault-job-progress") return () => {};
      const key = resource.requestId;
      listeners.set(key, accept);
      accept({
        epoch: "story",
        revision: 1,
        progress: { resource, value: jobs.get(key) as never },
      });
      return () => {
        listeners.delete(key);
      };
    },
    watchResourceConnection: () => () => {},
  };
  const post = async (path: string, body: Reply) => {
    state.posts.push([path, body]);
    if (path === "/api/vault/setup/facts") return facts();
    const requestId = String(body.requestId);
    if (path === "/api/vault/aws-login" || path === "/api/vault/setup") {
      const value = {
        requestId,
        state: "running",
        ...(path === "/api/vault/aws-login"
          ? { page: "https://device.example/?code=WXYZ", code: "WXYZ" }
          : {
              prefix: "my-project",
              progress: "studio-vault-my-project: CREATE_IN_PROGRESS",
            }),
      };
      jobs.set(requestId, value);
      return value;
    }
    throw new Error(`no fake for ${path}`);
  };
  const finish = (job: "login" | "setup") => {
    const [requestId] = listeners.keys();
    if (requestId === undefined) throw new Error("Missing vault job listener");
    if (job === "login") state.signedIn = true;
    const value = {
      requestId,
      state: "done" as const,
      prefix: "my-project",
      site: "https://desk.example.com/my-project/",
    };
    jobs.set(requestId, value);
    listeners.get(requestId)?.({
      epoch: "story",
      revision: 2,
      progress: {
        resource: { kind: "vault-job-progress", job, requestId },
        value,
      },
    });
  };
  return { state, finish, backend: { post: post as never, resources } };
}

const meta = {
  title: "Sidebar/Vault setup",
  component: VaultSetup,
  args: {
    entry: { vault: { projectId: "p1", state: "none" } },
    name: "My Project",
    backend: server().backend,
    onClose: fn(),
    onDone: fn(),
  },
} satisfies Meta<typeof VaultSetup>;
export default meta;
type Story = StoryObj<typeof meta>;

let shown: string[] = [];
const desktop = (keysAfter: () => void) => {
  shown = [];
  const original = window.codexDesktop;
  window.codexDesktop = {
    ...original,
    showVaultKeys: async (prefix: string) => {
      shown.push(prefix);
      keysAfter();
    },
  } as never;
  return () => {
    if (original === undefined) delete window.codexDesktop;
    else window.codexDesktop = original;
  };
};

const fake = server();
export const PaperKeyThenVault: Story = {
  args: { backend: fake.backend },
  beforeEach: () => {
    fake.state.keys = false;
    fake.state.posts = [];
    return desktop(() => {
      fake.state.keys = true;
    });
  },
  play: async ({ args }) => {
    const page = within(document.body);
    const create = await page.findByRole("button", { name: "Create vault" });
    await expect(create).toBeDisabled();
    await userEvent.click(
      page.getByRole("button", { name: "Show the paper key in a terminal" }),
    );
    await expect(shown).toEqual(["my-project"]);
    await page.findByText("The paper key of my-project is made.", undefined, {
      timeout: 5000,
    });
    await waitFor(() => expect(create).toBeEnabled());
    await userEvent.click(create);
    await page.findByText(/CREATE_IN_PROGRESS/);
    fake.finish("setup");
    const open = await page.findByRole(
      "link",
      { name: "Open in Desk" },
      { timeout: 5000 },
    );
    await expect(open).toHaveAttribute(
      "href",
      "https://desk.example.com/my-project/",
    );
    await expect(args.onDone).toHaveBeenCalledWith(
      "My Project: the vault my-project is ready",
    );
    const setup = fake.state.posts.filter(([p]) => p === "/api/vault/setup");
    const [firstSetup] = setup;
    if (!firstSetup) throw new Error("Missing vault setup request");
    await expect(firstSetup[1]).toMatchObject({
      projectId: "p1",
      prefix: "my-project",
      profile: "work",
      region: "eu-central-1",
    });
    await expect(setup).toHaveLength(1);
  },
};

const signin = server({ keys: true, signedIn: false });
export const SignInFirst: Story = {
  args: { backend: signin.backend },
  play: async () => {
    const page = within(document.body);
    const create = await page.findByRole("button", { name: "Create vault" });
    await expect(create).toBeDisabled();
    await userEvent.click(page.getByRole("button", { name: "Sign in to AWS" }));
    const link = await page.findByRole(
      "link",
      { name: "Open the AWS sign-in page" },
      { timeout: 5000 },
    );
    await expect(link).toHaveAttribute(
      "href",
      "https://device.example/?code=WXYZ",
    );
    await expect(page.getByText("WXYZ")).toBeVisible();
    signin.finish("login");
    await waitFor(() => expect(create).toBeEnabled(), { timeout: 5000 });
  },
};
