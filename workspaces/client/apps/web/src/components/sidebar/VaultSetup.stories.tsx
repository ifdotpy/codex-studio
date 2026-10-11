import type { Meta, StoryObj } from "@storybook/react-vite";
import { expect, fn, userEvent, waitFor, within } from "storybook/test";
import VaultSetup from "./VaultSetup";

type Reply = Record<string, unknown>;

/** A fake of this Mac's server: the setup facts, the AWS sign-in and the setup job. */
function server({ keys = false, signedIn = true } = {}) {
  const state = { keys, signedIn, polls: 0, posts: [] as [string, Reply][] };
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
  const post = async (path: string, body: Reply) => {
    state.posts.push([path, body]);
    if (path === "/api/vault/setup/facts") return facts();
    if (path === "/api/vault/aws-login") {
      state.polls += 1;
      if (state.polls < 3)
        return {
          requestId: body.requestId,
          state: "running",
          ...(state.polls > 1
            ? { page: "https://device.example/?code=WXYZ", code: "WXYZ" }
            : {}),
        };
      state.signedIn = true;
      return { requestId: body.requestId, state: "done" };
    }
    if (path === "/api/vault/setup") {
      const runs = state.posts.filter(([p]) => p === path).length;
      return runs < 2
        ? {
            requestId: body.requestId,
            state: "running",
            prefix: "my-project",
            progress: "studio-vault-my-project: CREATE_IN_PROGRESS",
          }
        : {
            requestId: body.requestId,
            state: "done",
            prefix: "my-project",
            site: "https://desk.example.com/my-project/",
          };
    }
    throw new Error(`no fake for ${path}`);
  };
  return { state, backend: { post: post as never } };
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
    window.codexDesktop = original;
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
      page.getByRole("button", { name: "Show the paper key in Terminal" }),
    );
    await expect(shown).toEqual(["my-project"]);
    await page.findByText("The paper key of my-project is made.", undefined, {
      timeout: 5000,
    });
    await waitFor(() => expect(create).toBeEnabled());
    await userEvent.click(create);
    await page.findByText(/CREATE_IN_PROGRESS/);
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
    await expect(setup[0][1]).toMatchObject({
      projectId: "p1",
      prefix: "my-project",
      profile: "work",
      region: "eu-central-1",
    });
    // Every poll of the job keeps its request id.
    await expect(new Set(setup.map(([, b]) => b.requestId)).size).toBe(1);
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
    await waitFor(() => expect(create).toBeEnabled(), { timeout: 5000 });
  },
};
