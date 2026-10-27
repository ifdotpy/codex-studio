import { describe, expect, test } from "vitest";
import { edgeRailGeometry } from "./edgeRailPolicy";

const metrics = { minimumInset: 44, statusReserve: 112, targetSize: 44 };
const duo = {
  coarsePointer: true,
  viewportHeight: 669,
  insets: { top: 0, right: 84, bottom: 34, left: 0 },
};

describe("edge rail geometry", () => {
  test("uses the measured right strip below the status cluster and above Home", () => {
    expect(edgeRailGeometry(duo, metrics)).toEqual({
      side: "right",
      width: 84,
      top: 112,
      bottom: 34,
      height: 523,
      targetSize: 44,
      center: 42,
    });
  });

  test("centers buttons on the system status column, inside the strip", () => {
    const status = { ...metrics, targetSize: 48, statusColumnCenter: 48 };
    expect(
      edgeRailGeometry({ ...duo, insets: { ...duo.insets, right: 82 } }, status)
        ?.center,
    ).toBe(48);
    // A narrow strip keeps the whole button inside it.
    expect(
      edgeRailGeometry({ ...duo, insets: { ...duo.insets, right: 64 } }, status)
        ?.center,
    ).toBe(40);
    expect(
      edgeRailGeometry(duo, { ...status, statusColumnCenter: 10 })?.center,
    ).toBe(24);
  });

  test("supports the physical left strip after rotation", () => {
    expect(
      edgeRailGeometry(
        { ...duo, insets: { ...duo.insets, right: 0, left: 84 } },
        metrics,
      )?.side,
    ).toBe("left");
  });

  test("requires a coarse pointer and a side inset at the contract boundary", () => {
    expect(
      edgeRailGeometry({ ...duo, coarsePointer: false }, metrics),
    ).toBeNull();
    expect(
      edgeRailGeometry(
        { ...duo, insets: { ...duo.insets, right: 43.99 } },
        metrics,
      ),
    ).toBeNull();
    expect(
      edgeRailGeometry(
        { ...duo, insets: { ...duo.insets, right: 44 } },
        metrics,
      )?.width,
    ).toBe(44);
  });

  test("does not infer a strip from viewport height or device names", () => {
    expect(
      edgeRailGeometry(
        { ...duo, insets: { top: 0, right: 0, bottom: 34, left: 0 } },
        metrics,
      ),
    ).toBeNull();
  });

  test("uses the larger physical inset and preserves top safe clearance", () => {
    expect(
      edgeRailGeometry(
        { ...duo, insets: { ...duo.insets, left: 90, top: 120 } },
        metrics,
      ),
    ).toMatchObject({ side: "left", width: 90, top: 120, height: 515 });
  });

  test("hides if the keyboard leaves less than one full target", () => {
    expect(
      edgeRailGeometry({ ...duo, viewportHeight: 189 }, metrics),
    ).toBeNull();
    expect(
      edgeRailGeometry({ ...duo, viewportHeight: 190 }, metrics)?.height,
    ).toBe(44);
  });

  test("uses changed contract values rather than device constants", () => {
    expect(
      edgeRailGeometry(duo, {
        minimumInset: 90,
        statusReserve: 150,
        targetSize: 48,
      }),
    ).toBeNull();
    expect(
      edgeRailGeometry(duo, {
        minimumInset: 48,
        statusReserve: 150,
        targetSize: 48,
      }),
    ).toMatchObject({ top: 150, height: 485, targetSize: 48 });
  });

  test("rejects non-finite or negative geometry", () => {
    for (const right of [NaN, Infinity, -1]) {
      expect(
        edgeRailGeometry({ ...duo, insets: { ...duo.insets, right } }, metrics),
      ).toBeNull();
    }
    expect(
      edgeRailGeometry({ ...duo, viewportHeight: NaN }, metrics),
    ).toBeNull();
  });
});

describe("top strip rail (iPhone Duo portrait)", () => {
  const portrait = {
    coarsePointer: true,
    viewportHeight: 951,
    viewportWidth: 669,
    insets: { top: 0, right: 0, bottom: 34, left: 0 },
    topStrip: 82,
  };
  const top = {
    ...metrics,
    targetSize: 48,
    statusColumnCenter: 48,
    topStatusReserve: 136,
  };

  test("makes a row in the strip that ends before the clock", () => {
    expect(edgeRailGeometry(portrait, top)).toEqual({
      side: "top",
      width: 82,
      top: 0,
      bottom: 34,
      height: 669 - 136,
      targetSize: 48,
      center: 48,
    });
  });

  test("needs the viewport owner's top strip and prefers a side strip", () => {
    expect(
      edgeRailGeometry({ ...portrait, topStrip: undefined }, top),
    ).toBeNull();
    expect(edgeRailGeometry({ ...portrait, topStrip: 40 }, top)).toBeNull();
    expect(
      edgeRailGeometry(
        { ...portrait, insets: { ...portrait.insets, right: 82 } },
        top,
      )?.side,
    ).toBe("right");
  });

  test("ends the row at the measured status column left edge", () => {
    expect(
      edgeRailGeometry(
        { ...portrait, statusColumn: { bottom: 82, left: 420 } },
        top,
      )?.height,
    ).toBe(420);
  });
});

describe("measured status column", () => {
  test("starts a side rail at the status column bottom, which a call moves down", () => {
    expect(
      edgeRailGeometry(
        { ...duo, statusColumn: { bottom: 124, left: 867 } },
        metrics,
      ),
    ).toMatchObject({ top: 124, height: 669 - 124 - 34 });
    expect(
      edgeRailGeometry(
        { ...duo, statusColumn: { bottom: 190, left: 867 } },
        metrics,
      ),
    ).toMatchObject({ top: 190, height: 669 - 190 - 34 });
  });

  test("falls back to the contract reserve without a valid measurement", () => {
    expect(
      edgeRailGeometry(
        { ...duo, statusColumn: { bottom: Number.NaN, left: 867 } },
        metrics,
      )?.top,
    ).toBe(112);
  });
});
