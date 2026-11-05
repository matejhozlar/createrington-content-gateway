import { z } from "zod/v4";

/** The host every Modrinth download address is on; an address elsewhere is not Modrinth's copy of a file. */
export const MODRINTH_CDN_HOST = "cdn.modrinth.com";

// The answers consumers read, as https://docs.modrinth.com/api/ describes
// them. Every object is loose: fields Modrinth adds pass through.

export const modrinthHashesSchema = z.looseObject({
  sha1: z.string().nullish(),
  sha512: z.string().nullish(),
});

/** One file of a version, as Modrinth lists it. */
export const modrinthVersionFileSchema = z.looseObject({
  hashes: modrinthHashesSchema,
  url: z.string(),
  filename: z.string(),
  primary: z.boolean().nullish(),
  size: z.number().nullish(),
});

/** A version, as `GET /v1/modrinth/version_file/{hash}` answers it and `POST /v1/modrinth/version_files` lists it per hash. */
export const modrinthVersionSchema = z.looseObject({
  id: z.string(),
  project_id: z.string().nullish(),
  name: z.string().nullish(),
  version_number: z.string().nullish(),
  files: z.array(modrinthVersionFileSchema),
});

/** What `POST /v1/modrinth/version_files` answers: the versions found, keyed by lowercase SHA-1. */
export const modrinthVersionFilesSchema = z.record(
  z.string(),
  modrinthVersionSchema,
);

export type ModrinthVersionFile = z.infer<typeof modrinthVersionFileSchema>;
export type ModrinthVersion = z.infer<typeof modrinthVersionSchema>;

/**
 * The address of Modrinth's own copy of the file with this SHA-1 in a version,
 * or `null`: only the file whose own hash matches counts, never another file
 * of the same version, and only an address on Modrinth's CDN.
 */
export function modrinthFileUrl(
  version: ModrinthVersion,
  sha1: string,
): string | null {
  const wanted = sha1.toLowerCase();
  for (const file of version.files) {
    if (file.hashes.sha1?.toLowerCase() !== wanted) continue;
    try {
      const url = new URL(file.url);
      if (url.protocol === "https:" && url.hostname === MODRINTH_CDN_HOST) {
        return file.url;
      }
    } catch {
      // not an address at all
    }
  }
  return null;
}
