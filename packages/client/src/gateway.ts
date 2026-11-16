import type { components, paths } from "./generated/openapi.js";

export type { components, paths };

/** What `GET /v1/healthz` answers. */
export type Health = components["schemas"]["Health"];

/** What `GET /v1/budget` answers: one `UpstreamBudget` for CurseForge and one for Modrinth. */
export type BudgetSnapshot = components["schemas"]["BudgetSnapshot"];

/** One upstream's budget: this minute, the 429 hold and the daily counts. */
export type UpstreamBudget = components["schemas"]["UpstreamBudget"];

export type PoolSnapshot = components["schemas"]["PoolSnapshot"];

export type DaySnapshot = components["schemas"]["DaySnapshot"];

export type DayPoolSnapshot = components["schemas"]["DayPoolSnapshot"];

export type DayCounts = components["schemas"]["DayCounts"];

/** Where a budget readout's counts come from. */
export type BudgetSource = components["schemas"]["Source"];

/** What `POST /v1/resolve` answers. */
export type Resolved = components["schemas"]["Resolved"];

/** One file's download address and where it comes from. */
export type ResolvedFile = components["schemas"]["ResolvedFile"];

/** Where a resolved file's address comes from. */
export type FileSource = components["schemas"]["FileSource"];

/** The body of `POST /v1/resolve`. */
export type ResolveRequest = components["schemas"]["ResolveRequest"];

/** The body of `POST /v1/modrinth/version_files`. */
export type VersionFilesRequest = components["schemas"]["VersionFilesRequest"];

/** What `GET /v1/minecraft/versions` answers. */
export type MinecraftVersions = components["schemas"]["MinecraftVersions"];

/** The newest release and snapshot Mojang names. */
export type Latest = components["schemas"]["Latest"];

export type MinecraftVersion = components["schemas"]["MinecraftVersion"];

/** What `GET /v1/minecraft/versions/{id}` answers. */
export type MinecraftVersionDetails =
  components["schemas"]["MinecraftVersionDetails"];

/** Mojang's kinds of versions. */
export type VersionType = components["schemas"]["VersionType"];

/** Which versions to list: `release`, `snapshot` or `all`. */
export type VersionFilter = components["schemas"]["VersionFilter"];

/** The mod loaders the gateway lists builds for. */
export type Loader = components["schemas"]["Loader"];

/** What `GET /v1/minecraft/loaders/{loader}/versions` answers. */
export type LoaderVersions = components["schemas"]["LoaderVersions"];

export type LoaderVersion = components["schemas"]["LoaderVersion"];

/** What `GET /v1/minecraft/java` answers. */
export type JavaRuntime = components["schemas"]["JavaRuntime"];

/** Mojang's manifest of a runtime's files. */
export type RuntimeManifest = components["schemas"]["RuntimeManifest"];

/** The error codes this version of the client knows. */
export type KnownErrorCode = components["schemas"]["ErrorCode"];

/** What went wrong: one of the known codes, or a code a newer gateway added inside `/v1`. */
export type ErrorCode = KnownErrorCode | (string & {});

/** The JSON body of every error the gateway answers itself. */
export type ApiError = Omit<components["schemas"]["ApiError"], "code"> & {
  code: ErrorCode;
};
