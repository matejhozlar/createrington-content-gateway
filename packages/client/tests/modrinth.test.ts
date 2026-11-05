import { describe, expect, it } from "vitest";
import {
  modrinthFileUrl,
  modrinthVersionFilesSchema,
  modrinthVersionSchema,
} from "../src/index.js";

const SHA1 = "3e1dd9fb5ea2f0dd4ffcc2b1a7d0bb1a1a9d6a8e";

const version = {
  id: "AAAAAAAA",
  project_id: "BBBBBBBB",
  name: "JEI 15.0.0",
  version_number: "15.0.0",
  date_published: "2026-10-01T00:00:00Z",
  files: [
    {
      hashes: { sha1: "ffffffffffffffffffffffffffffffffffffffff", sha512: "x" },
      url: "https://cdn.modrinth.com/data/BBBBBBBB/versions/AAAAAAAA/jei-sources.jar",
      filename: "jei-sources.jar",
      primary: false,
      size: 5,
    },
    {
      hashes: { sha1: SHA1.toUpperCase() },
      url: "https://cdn.modrinth.com/data/BBBBBBBB/versions/AAAAAAAA/jei.jar",
      filename: "jei.jar",
      primary: true,
      size: 10,
    },
  ],
};

describe("the Modrinth schemas", () => {
  it("read a version and keep what Modrinth adds", () => {
    const parsed = modrinthVersionSchema.parse(version);
    expect(parsed.files[1]?.filename).toBe("jei.jar");
    expect((parsed as Record<string, unknown>).date_published).toBe(
      "2026-10-01T00:00:00Z",
    );
  });

  it("read the versions per hash", () => {
    const parsed = modrinthVersionFilesSchema.parse({ [SHA1]: version });
    expect(parsed[SHA1]?.id).toBe("AAAAAAAA");
  });
});

describe("modrinthFileUrl", () => {
  it("picks the file with the hash itself, on Modrinth's CDN only", () => {
    expect(modrinthFileUrl(modrinthVersionSchema.parse(version), SHA1)).toBe(
      "https://cdn.modrinth.com/data/BBBBBBBB/versions/AAAAAAAA/jei.jar",
    );
    expect(
      modrinthFileUrl(
        modrinthVersionSchema.parse(version),
        "ffffffffffffffffffffffffffffffffffffffff",
      ),
    ).toBe(
      "https://cdn.modrinth.com/data/BBBBBBBB/versions/AAAAAAAA/jei-sources.jar",
    );
    expect(
      modrinthFileUrl(modrinthVersionSchema.parse(version), "0000"),
    ).toBeNull();

    const elsewhere = modrinthVersionSchema.parse({
      ...version,
      files: [{ ...version.files[1], url: "http://evil.example/jei.jar" }],
    });
    expect(modrinthFileUrl(elsewhere, SHA1)).toBeNull();
  });
});
