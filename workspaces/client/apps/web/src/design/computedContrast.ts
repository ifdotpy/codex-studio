/** Browser audit of painted text, including transparent ancestor backgrounds. */
export function computedContrast(element: Element) {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d");
  if (!context) throw new Error("Canvas context is unavailable.");
  type RGB = [number, number, number];
  type RGBA = [...RGB, number];
  const rgba = (color: string): RGBA => {
    context.clearRect(0, 0, 1, 1);
    context.fillStyle = color;
    context.fillRect(0, 0, 1, 1);
    const [red, green, blue, alpha] = context.getImageData(0, 0, 1, 1).data;
    if (
      red === undefined ||
      green === undefined ||
      blue === undefined ||
      alpha === undefined
    )
      throw new Error("Canvas returned an incomplete pixel.");
    return [red, green, blue, alpha / 255];
  };
  const compose = (front: RGBA, back: RGB): RGB => {
    const blend = (value: number, background: number) =>
      value * front[3] + background * (1 - front[3]);
    return [
      blend(front[0], back[0]),
      blend(front[1], back[1]),
      blend(front[2], back[2]),
    ];
  };
  const background = (node: Element | null): RGB =>
    node
      ? compose(
          rgba(getComputedStyle(node).backgroundColor),
          background(node.parentElement),
        )
      : [255, 255, 255];
  const luminance = (rgb: number[]) =>
    rgb
      .map((value) => value / 255)
      .map((value) =>
        value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4,
      )
      .reduce((sum, value, index) => {
        const weight = [0.2126, 0.7152, 0.0722][index];
        if (weight === undefined)
          throw new Error("Expected three color channels.");
        return sum + value * weight;
      }, 0);
  const back = background(element);
  const front = compose(rgba(getComputedStyle(element).color), back);
  const frontLight = luminance(front),
    backLight = luminance(back);
  const low = Math.min(frontLight, backLight),
    high = Math.max(frontLight, backLight);
  return {
    ratio: (high + 0.05) / (low + 0.05),
    foreground: front,
    background: back,
  };
}
