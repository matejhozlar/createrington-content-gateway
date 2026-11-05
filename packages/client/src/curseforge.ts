import { z } from "zod/v4";

/** Minecraft's game id, the `gameId` of searches and the `/fingerprints/432` route. */
export const CURSEFORGE_MINECRAFT_GAME_ID = 432;

/** How many ids one call of `POST /mods`, `POST /mods/files` or `POST /fingerprints` may ask for. */
export const CURSEFORGE_BATCH_SIZE = 100;

export const CurseForgeHashAlgo = {
  sha1: 1,
  md5: 2,
} as const;

export const CurseForgeRelationType = {
  embeddedLibrary: 1,
  optionalDependency: 2,
  requiredDependency: 3,
  tool: 4,
  incompatible: 5,
  include: 6,
} as const;

export const CurseForgeModLoaderType = {
  any: 0,
  forge: 1,
  cauldron: 2,
  liteLoader: 3,
  fabric: 4,
  quilt: 5,
  neoForge: 6,
} as const;

export const CurseForgeReleaseType = {
  release: 1,
  beta: 2,
  alpha: 3,
} as const;

export const CurseForgeSortField = {
  featured: 1,
  popularity: 2,
  lastUpdated: 3,
  name: 4,
  author: 5,
  totalDownloads: 6,
  category: 7,
  gameVersion: 8,
  earlyAccess: 9,
  featuredReleased: 10,
  releasedDate: 11,
  rating: 12,
} as const;

export const curseforgeLogoSchema = z.looseObject({
  thumbnailUrl: z.string(),
  url: z.string().nullish(),
});

export const curseforgeAuthorSchema = z.looseObject({
  id: z.number(),
  name: z.string(),
  url: z.string(),
  avatarUrl: z.string().nullish(),
});

export const curseforgeCategorySchema = z.looseObject({
  id: z.number(),
  gameId: z.number().nullish(),
  name: z.string(),
  slug: z.string(),
  url: z.string().nullish(),
  iconUrl: z.string().nullish(),
  dateModified: z.string().nullish(),
  isClass: z.boolean().nullish(),
  classId: z.number().nullish(),
  parentCategoryId: z.number().nullish(),
});

export const curseforgeScreenshotSchema = z.looseObject({
  title: z.string(),
  thumbnailUrl: z.string(),
  url: z.string(),
});

/** One row of a project's `latestFilesIndexes`. */
export const curseforgeFileIndexSchema = z.looseObject({
  gameVersion: z.string(),
  fileId: z.number(),
  filename: z.string(),
  releaseType: z.number(),
  gameVersionTypeId: z.number().nullish(),
  modLoader: z.number().nullish(),
});

export const curseforgeHashSchema = z.looseObject({
  value: z.string(),
  algo: z.number(),
});

export const curseforgeDependencySchema = z.looseObject({
  modId: z.number(),
  relationType: z.number(),
});

/** A file as every file route describes it; only the ids are certain. */
export const curseforgeFileSchema = z.looseObject({
  id: z.number(),
  gameId: z.number().nullish(),
  modId: z.number(),
  isAvailable: z.boolean().nullish(),
  displayName: z.string().nullish(),
  fileName: z.string().nullish(),
  releaseType: z.number().nullish(),
  fileStatus: z.number().nullish(),
  hashes: z.array(curseforgeHashSchema).nullish(),
  fileDate: z.string().nullish(),
  fileLength: z.number().nullish(),
  downloadCount: z.number().nullish(),
  /** Null when the author opted out of third-party distribution. */
  downloadUrl: z.string().nullish(),
  gameVersions: z.array(z.string()).nullish(),
  dependencies: z.array(curseforgeDependencySchema).nullish(),
  alternateFileId: z.number().nullish(),
  isServerPack: z.boolean().nullish(),
  serverPackFileId: z.number().nullish(),
  parentProjectFileId: z.number().nullish(),
  fileFingerprint: z.number().nullish(),
});

/** A project (a "mod" in CurseForge's words, whatever its class), as `GET /mods/search`, `GET /mods/{id}` and `POST /mods` describe it. */
export const curseforgeModSchema = z.looseObject({
  id: z.number(),
  gameId: z.number().nullish(),
  classId: z.number(),
  slug: z.string(),
  name: z.string(),
  summary: z.string(),
  links: z.looseObject({
    websiteUrl: z.string(),
    wikiUrl: z.string().nullish(),
    issuesUrl: z.string().nullish(),
    sourceUrl: z.string().nullish(),
  }),
  status: z.number().nullish(),
  logo: curseforgeLogoSchema.nullish(),
  authors: z.array(curseforgeAuthorSchema),
  categories: z.array(curseforgeCategorySchema),
  primaryCategoryId: z.number().nullish(),
  screenshots: z.array(curseforgeScreenshotSchema).nullish(),
  mainFileId: z.number().nullish(),
  downloadCount: z.number(),
  isAvailable: z.boolean(),
  allowModDistribution: z.boolean().nullish(),
  dateCreated: z.string().nullish(),
  dateModified: z.string(),
  dateReleased: z.string(),
  latestFiles: z.array(curseforgeFileSchema).nullish(),
  latestFilesIndexes: z.array(curseforgeFileIndexSchema).nullish(),
  latestEarlyAccessFilesIndexes: z.array(curseforgeFileIndexSchema).nullish(),
});

/** The `pagination` of a list answer. */
export const curseforgePaginationSchema = z.looseObject({
  index: z.number().nullish(),
  pageSize: z.number().nullish(),
  resultCount: z.number().nullish(),
  totalCount: z.number(),
});

/** One row of `GET /minecraft/version`. */
export const curseforgeMinecraftVersionSchema = z.looseObject({
  id: z.number().nullish(),
  gameVersionId: z.number().nullish(),
  versionString: z.string(),
  approved: z.boolean().nullish(),
  dateModified: z.string().nullish(),
});

/** One row of `GET /minecraft/modloader`. */
export const curseforgeModLoaderSchema = z.looseObject({
  name: z.string(),
  gameVersion: z.string(),
  latest: z.boolean().nullish(),
  recommended: z.boolean().nullish(),
  dateModified: z.string().nullish(),
  type: z.number().nullish(),
});

/** One match of a fingerprint lookup: the file that has the fingerprint, which `file.fileFingerprint` carries. */
export const curseforgeFingerprintMatchSchema = z.looseObject({
  id: z.number().nullish(),
  file: curseforgeFileSchema.extend({ fileFingerprint: z.number() }),
  latestFiles: z.array(curseforgeFileSchema).nullish(),
});

/** The `data` of `POST /fingerprints/432` and `POST /fingerprints`. */
export const curseforgeFingerprintMatchesSchema = z.looseObject({
  isCacheBuilt: z.boolean().nullish(),
  exactMatches: z.array(curseforgeFingerprintMatchSchema).nullish(),
  exactFingerprints: z.array(z.number()).nullish(),
  partialMatches: z.array(curseforgeFingerprintMatchSchema).nullish(),
  installedFingerprints: z.array(z.number()).nullish(),
  unmatchedFingerprints: z.array(z.number()).nullish(),
});

/** An answer of one thing: `{ data: ... }`, as `GET /mods/{id}`, download URLs and the HTML routes answer. */
export function curseforgeData<T extends z.ZodType>(data: T) {
  return z.looseObject({ data });
}

/** An answer of rows without a page: `{ data: [...] }`, as `POST /mods`, `POST /mods/files`, categories and versions answer. */
export function curseforgeList<T extends z.ZodType>(row: T) {
  return z.looseObject({ data: z.array(row) });
}

/** One page of rows: `{ data: [...], pagination }`, as `GET /mods/search` and `GET /mods/{id}/files` answer. */
export function curseforgePage<T extends z.ZodType>(row: T) {
  return z.looseObject({
    data: z.array(row),
    pagination: curseforgePaginationSchema,
  });
}

export type CurseForgeLogo = z.infer<typeof curseforgeLogoSchema>;
export type CurseForgeAuthor = z.infer<typeof curseforgeAuthorSchema>;
export type CurseForgeCategory = z.infer<typeof curseforgeCategorySchema>;
export type CurseForgeScreenshot = z.infer<typeof curseforgeScreenshotSchema>;
export type CurseForgeFileIndex = z.infer<typeof curseforgeFileIndexSchema>;
export type CurseForgeHash = z.infer<typeof curseforgeHashSchema>;
export type CurseForgeDependency = z.infer<typeof curseforgeDependencySchema>;
export type CurseForgeFile = z.infer<typeof curseforgeFileSchema>;
export type CurseForgeMod = z.infer<typeof curseforgeModSchema>;
export type CurseForgePagination = z.infer<typeof curseforgePaginationSchema>;
export type CurseForgeMinecraftVersion = z.infer<
  typeof curseforgeMinecraftVersionSchema
>;
export type CurseForgeModLoader = z.infer<typeof curseforgeModLoaderSchema>;
export type CurseForgeFingerprintMatch = z.infer<
  typeof curseforgeFingerprintMatchSchema
>;
export type CurseForgeFingerprintMatches = z.infer<
  typeof curseforgeFingerprintMatchesSchema
>;
