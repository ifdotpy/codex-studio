const ENTITY_SCOPE = "state:entities:v1";
const PAGE_SIZE = 500;
const MAX_PAGES = 100;

async function jsonResponse(response, label) {
  let value;
  try {
    value = await response.json();
  } catch {
    throw new Error(`Studio answered with non-JSON for ${label}`);
  }
  if (!response.ok)
    throw new Error(
      `Studio request for ${label} failed (HTTP ${response.status})`,
    );
  return value;
}

/** Read only the runtime identity and local state directory the load harness uses. */
export async function fetchRuntimeIdentity(origin, fetchImpl = fetch) {
  const session = await jsonResponse(
    await fetchImpl(`${origin}/api/session`),
    "/api/session",
  );
  if (typeof session.token !== "string" || !session.token)
    throw new Error("Studio session response did not include a token");
  const headers = { "X-Canvas-Token": session.token };
  const desktop = await jsonResponse(
    await fetchImpl(`${origin}/api/desktop`, { headers }),
    "/api/desktop",
  );
  if (typeof desktop.stateDir !== "string" || !desktop.stateDir)
    throw new Error(
      "Studio desktop response did not include a state directory identity",
    );

  let after = 0;
  let pages = 0;
  let maxSeq;
  const latest = new Map();
  for (;;) {
    if (pages >= MAX_PAGES)
      throw new Error(
        `Studio entity pull exceeded its ${MAX_PAGES}-page limit at checkpoint ${after}`,
      );
    const query = new URLSearchParams({
      scope: ENTITY_SCOPE,
      after: String(after),
      limit: String(PAGE_SIZE),
    });
    const response = await jsonResponse(
      await fetchImpl(`${origin}/api/sync/pull?${query}`, { headers }),
      "/api/sync/pull",
    );
    const checkpoint = response?.checkpoint?.seq;
    maxSeq = response?.maxSeq;
    if (
      !Array.isArray(response?.documents) ||
      !Number.isInteger(checkpoint) ||
      !Number.isInteger(maxSeq)
    )
      throw new Error("Studio entity pull returned an invalid page");
    for (const document of response.documents) {
      let entity;
      try {
        entity = JSON.parse(document.payload);
      } catch {
        throw new Error("Studio entity pull returned malformed JSON");
      }
      if (
        !Number.isInteger(document.seq) ||
        typeof entity?.collection !== "string" ||
        typeof entity?.id !== "string" ||
        !entity.value ||
        typeof entity.value !== "object"
      )
        throw new Error(
          "Studio entity pull returned an invalid entity document",
        );
      const key = `${entity.collection}\0${entity.id}`;
      const previous = latest.get(key);
      if (!previous || document.seq >= previous.seq)
        latest.set(key, {
          seq: document.seq,
          entity: document._deleted === true ? null : entity,
        });
    }
    pages += 1;
    if (checkpoint >= maxSeq) break;
    if (checkpoint <= after)
      throw new Error("Studio entity pull did not advance its checkpoint");
    after = checkpoint;
  }

  return {
    stateDir: desktop.stateDir,
    threads: [...latest.values()]
      .map(({ entity }) => entity)
      .filter((entity) => entity?.collection === "agent")
      .map((entity) => entity.value),
  };
}
