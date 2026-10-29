import { expect, it } from "vitest";
import { SchemaMismatchError } from "./summary";
import { recoverMatchingServers } from "./schemaRecovery";

const server = (id: string) => ({ id, label: id, origin: `https://${id}` });

it("recreates a latched server once its summary shows the matching schema", async () => {
  const probed: string[] = [];
  const recovered = await recoverMatchingServers(
    [server("mbp"), server("fine")],
    (id) => id === "mbp",
    async (item) => {
      probed.push(item.id);
    },
  );
  expect(recovered).toEqual(["mbp"]);
  expect(probed).toEqual(["mbp"]);
});

it("keeps a real mismatch or an unreachable server latched", async () => {
  const recovered = await recoverMatchingServers(
    [server("kukuka"), server("down")],
    () => true,
    async (item) => {
      throw item.id === "kukuka"
        ? new SchemaMismatchError()
        : new Error("offline");
    },
  );
  expect(recovered).toEqual([]);
});
