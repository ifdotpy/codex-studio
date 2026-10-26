/** Every sync page fixture creates its mount element before loading its module. */
export function fixtureElement(id: string) {
  const element = document.getElementById(id);
  if (!element) throw new Error(`Missing sync fixture element: ${id}`);
  return element;
}
