/** Every sync page fixture creates its mount element before loading its module. */
export function fixtureElement(id: string) {
  return document.getElementById(id)!;
}
