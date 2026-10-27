export const keyboardSequences = {
  "shrink then pan": [
    { height: 323, innerHeight: 323 },
    { offsetTop: 346, pageTop: 346, scrollY: 346 },
  ],
  "pan then shrink": [
    { offsetTop: 346, pageTop: 346, scrollY: 346 },
    { height: 323, innerHeight: 323 },
  ],
  "document scroll before offset": [
    { height: 323, innerHeight: 323 },
    { pageTop: 346, scrollY: 346 },
    { offsetTop: 346 },
  ],
  "offset before document scroll": [
    { height: 323, innerHeight: 323 },
    { pageTop: 346, offsetTop: 346 },
    { scrollY: 346 },
  ],
  "zero offset": [{ height: 323, innerHeight: 323 }],
  "transformed root": [
    {
      height: 323,
      innerHeight: 323,
      offsetTop: 346,
      pageTop: 346,
      scrollY: 346,
    },
  ],
  "device final": [
    {
      height: 323,
      innerHeight: 323,
      offsetTop: 346,
      pageTop: 346,
      scrollY: 346,
    },
  ],
  "visual origin": [
    {
      height: 323,
      innerHeight: 323,
      offsetTop: 346,
      pageTop: 346,
      scrollY: 346,
    },
  ],
};
export const keyboardCycleFrames = (steps) => [
  ...steps,
  { height: 669, innerHeight: 669 },
  { pageTop: 0, offsetTop: 0, scrollY: 0 },
  ...steps,
  { pageTop: 0, offsetTop: 0, scrollY: 0 },
  { height: 669, innerHeight: 669 },
];
