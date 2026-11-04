import { describe, expect, it } from "vitest";
import {
  curseforgeData,
  curseforgeFileSchema,
  curseforgeFingerprintMatchesSchema,
  curseforgeList,
  curseforgeModSchema,
  curseforgePage,
} from "../src/index.js";

const mod = {
  id: 238222,
  gameId: 432,
  classId: 6,
  slug: "jei",
  name: "Just Enough Items (JEI)",
  summary: "View Items and Recipes",
  links: { websiteUrl: "https://www.curseforge.com/minecraft/mc-mods/jei" },
  logo: { thumbnailUrl: "https://media.forgecdn.net/thumb.png", url: null },
  authors: [{ id: 1, name: "mezz", url: "https://www.curseforge.com/mezz" }],
  categories: [
    { id: 423, name: "Map and Information", slug: "map-information" },
  ],
  downloadCount: 300_000_000,
  isAvailable: true,
  dateModified: "2026-10-01T00:00:00Z",
  dateReleased: "2026-10-01T00:00:00Z",
  latestFilesIndexes: [
    {
      gameVersion: "1.21.1",
      fileId: 5_000_000,
      filename: "jei.jar",
      releaseType: 1,
      modLoader: 6,
    },
  ],
  somethingNew: { nested: true },
};

describe("the CurseForge schemas", () => {
  it("keep the fields they do not know", () => {
    const parsed = curseforgeData(curseforgeModSchema).parse({ data: mod });
    expect(parsed.data.name).toBe("Just Enough Items (JEI)");
    expect(parsed.data["somethingNew"]).toEqual({ nested: true });
    expect(parsed.data.latestFilesIndexes?.[0]?.modLoader).toBe(6);
  });

  it("read a page of search hits and a list of files", () => {
    const page = curseforgePage(curseforgeModSchema).parse({
      data: [mod],
      pagination: { index: 0, pageSize: 50, resultCount: 1, totalCount: 1 },
    });
    expect(page.pagination.totalCount).toBe(1);

    const files = curseforgeList(curseforgeFileSchema).parse({
      data: [
        {
          id: 5_000_000,
          modId: 238222,
          displayName: "jei-1.21.1-neoforge.jar",
          downloadUrl: null,
          hashes: [{ value: "abc", algo: 1 }],
          gameVersions: ["1.21.1", "NeoForge"],
        },
      ],
    });
    expect(files.data[0]?.downloadUrl).toBeNull();
    expect(files.data[0]?.hashes?.[0]?.algo).toBe(1);
  });

  it("read fingerprint matches in CurseForge's shape", () => {
    const matches = curseforgeData(curseforgeFingerprintMatchesSchema).parse({
      data: {
        isCacheBuilt: true,
        exactMatches: [
          {
            id: 238222,
            file: { id: 5_000_000, modId: 238222, fileFingerprint: 123 },
            latestFiles: [],
          },
        ],
        exactFingerprints: [123],
        partialMatches: [],
        partialMatchFingerprints: {},
        installedFingerprints: [123, 456],
        unmatchedFingerprints: null,
      },
    });
    expect(matches.data.exactMatches?.[0]?.file.fileFingerprint).toBe(123);
    expect(matches.data.unmatchedFingerprints).toBeNull();
  });

  it("refuse a project without its id", () => {
    const { id: _id, ...withoutId } = mod;
    expect(curseforgeModSchema.safeParse(withoutId).success).toBe(false);
  });
});
