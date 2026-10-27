/** Browser audit of painted text, including transparent ancestor backgrounds. */
export function computedContrast(element: Element) {
  type RGB = [number, number, number];
  type RGBA = [number, number, number, number];
  const canvas = document.createElement("canvas");
  canvas.width = canvas.height = 1;
  const context = canvas.getContext("2d")!;
  const rgba = (color: string): RGBA => {
    context.clearRect(0, 0, 1, 1);
    context.fillStyle = color;
    context.fillRect(0, 0, 1, 1);
    const data = new DataView(context.getImageData(0, 0, 1, 1).data.buffer);
    return [
      data.getUint8(0),
      data.getUint8(1),
      data.getUint8(2),
      data.getUint8(3) / 255,
    ];
  };
  const compose = ([red, green, blue, alpha]: RGBA, back: RGB): RGB => [
    red * alpha + back[0] * (1 - alpha),
    green * alpha + back[1] * (1 - alpha),
    blue * alpha + back[2] * (1 - alpha),
  ];
  const background = (node: Element | null): RGB =>
    node
      ? compose(
          rgba(getComputedStyle(node).backgroundColor),
          background(node.parentElement),
        )
      : [255, 255, 255];
  const linear = (channel: number) => {
    const value = channel / 255;
    return value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
  };
  const luminance = ([red, green, blue]: RGB) =>
    linear(red) * 0.2126 + linear(green) * 0.7152 + linear(blue) * 0.0722;
  const back = background(element);
  const front = compose(rgba(getComputedStyle(element).color), back);
  const frontLuminance = luminance(front),
    backLuminance = luminance(back);
  const low = Math.min(frontLuminance, backLuminance),
    high = Math.max(frontLuminance, backLuminance);
  return {
    ratio: (high + 0.05) / (low + 0.05),
    foreground: front,
    background: back,
  };
}
