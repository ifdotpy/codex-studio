/** Browser audit of painted text, including transparent ancestor backgrounds. */
export function computedContrast(element: Element) {
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d")!;
  const rgba = (color: string) => {
    context.clearRect(0, 0, 1, 1);
    context.fillStyle = color;
    context.fillRect(0, 0, 1, 1);
    const data = context.getImageData(0, 0, 1, 1).data;
    return [data[0], data[1], data[2], data[3] / 255];
  };
  const compose = (front: number[], back: number[]) =>
    front
      .slice(0, 3)
      .map((value, index) => value * front[3] + back[index] * (1 - front[3]));
  const background = (node: Element | null): number[] =>
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
      .reduce(
        (sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index],
        0,
      );
  const back = background(element);
  const front = compose(rgba(getComputedStyle(element).color), back);
  const [low, high] = [luminance(front), luminance(back)].sort((a, b) => a - b);
  return {
    ratio: (high + 0.05) / (low + 0.05),
    foreground: front,
    background: back,
  };
}
