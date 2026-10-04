// Stryker disable next-line ArrayDeclaration: Caller-shaped Russian/English input matches the host default segmenter.
const segmenter = new Intl.Segmenter(["ru", "en"], {
  granularity: "sentence",
});
export function sentences(text: string) {
  return Array.from(segmenter.segment(text), ({ segment, index }) => ({
    text: segment,
    index,
  }));
}

// Partial text is readable immediately. Rich previews separately require a closed fence.
export function sentencePrefix(text: string, _streaming: boolean) {
  return text;
}
