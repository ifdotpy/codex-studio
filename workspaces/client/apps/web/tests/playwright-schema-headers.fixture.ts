async function probe(path: string) {
  const response = await fetch(path);
  return {
    body: await response.text(),
    hash: response.headers.get("x-studio-api-schema"),
  };
}

declare global {
  interface Window {
    schemaHeaderFixture: { probe: typeof probe };
  }
}
window.schemaHeaderFixture = { probe };
export {};
