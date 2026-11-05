import { z } from "zod/v4";

/** The host every Modrinth download address is on; an address elsewhere is not Modrinth's copy of a file. */
export const MODRINTH_CDN_HOST = "cdn.modrinth.com";

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

/** A dependency of a version on another project or version. */
export const modrinthDependencySchema = z.looseObject({
  version_id: z.string().nullish(),
  project_id: z.string().nullish(),
  file_name: z.string().nullish(),
  dependency_type: z.string(),
});

/** A version, as `GET /v1/modrinth/version/{id}`, `GET /v1/modrinth/project/{id}/version`, `GET /v1/modrinth/version_file/{hash}` and `POST /v1/modrinth/version_files` (per hash) describe it. */
export const modrinthVersionSchema = z.looseObject({
  id: z.string(),
  project_id: z.string().nullish(),
  name: z.string().nullish(),
  version_number: z.string().nullish(),
  version_type: z.string().nullish(),
  game_versions: z.array(z.string()).nullish(),
  loaders: z.array(z.string()).nullish(),
  date_published: z.string().nullish(),
  downloads: z.number().nullish(),
  dependencies: z.array(modrinthDependencySchema).nullish(),
  files: z.array(modrinthVersionFileSchema),
});

/** What `POST /v1/modrinth/version_files` answers: the versions found, keyed by lowercase SHA-1. */
export const modrinthVersionFilesSchema = z.record(
  z.string(),
  modrinthVersionSchema,
);

/** A project, as `GET /v1/modrinth/project/{id|slug}` and `GET /v1/modrinth/projects?ids=[...]` describe it. */
export const modrinthProjectSchema = z.looseObject({
  id: z.string(),
  slug: z.string(),
  project_type: z.string(),
  title: z.string(),
  description: z.string().nullish(),
  icon_url: z.string().nullish(),
  body: z.string().nullish(),
  downloads: z.number().nullish(),
  followers: z.number().nullish(),
  categories: z.array(z.string()).nullish(),
  additional_categories: z.array(z.string()).nullish(),
  client_side: z.string().nullish(),
  server_side: z.string().nullish(),
  game_versions: z.array(z.string()).nullish(),
  loaders: z.array(z.string()).nullish(),
  versions: z.array(z.string()).nullish(),
  source_url: z.string().nullish(),
  issues_url: z.string().nullish(),
  wiki_url: z.string().nullish(),
  published: z.string().nullish(),
  updated: z.string().nullish(),
});

/** One hit of `GET /v1/modrinth/search`. */
export const modrinthSearchHitSchema = z.looseObject({
  project_id: z.string(),
  project_type: z.string(),
  slug: z.string(),
  author: z.string().nullish(),
  title: z.string(),
  description: z.string().nullish(),
  icon_url: z.string().nullish(),
  downloads: z.number().nullish(),
  categories: z.array(z.string()).nullish(),
  versions: z.array(z.string()).nullish(),
  latest_version: z.string().nullish(),
  date_modified: z.string().nullish(),
});

/** What `GET /v1/modrinth/search` answers. */
export const modrinthSearchSchema = z.looseObject({
  hits: z.array(modrinthSearchHitSchema),
  offset: z.number().nullish(),
  limit: z.number().nullish(),
  total_hits: z.number(),
});

/** One tag of `GET /v1/modrinth/tag/category` or `tag/loader`. */
export const modrinthTagSchema = z.looseObject({
  name: z.string(),
  project_type: z.string().nullish(),
  header: z.string().nullish(),
  icon: z.string().nullish(),
  supported_project_types: z.array(z.string()).nullish(),
});

/** One entry of `GET /v1/modrinth/tag/game_version`. */
export const modrinthGameVersionSchema = z.looseObject({
  version: z.string(),
  version_type: z.string(),
  date: z.string().nullish(),
  major: z.boolean().nullish(),
});

export type ModrinthVersionFile = z.infer<typeof modrinthVersionFileSchema>;
export type ModrinthVersion = z.infer<typeof modrinthVersionSchema>;
export type ModrinthProject = z.infer<typeof modrinthProjectSchema>;
export type ModrinthSearchHit = z.infer<typeof modrinthSearchHitSchema>;
export type ModrinthSearch = z.infer<typeof modrinthSearchSchema>;
export type ModrinthTag = z.infer<typeof modrinthTagSchema>;
export type ModrinthGameVersion = z.infer<typeof modrinthGameVersionSchema>;

/** The address of Modrinth's own copy of the file with this SHA-1 in a version, or `null`. */
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
    } catch {}
  }
  return null;
}
