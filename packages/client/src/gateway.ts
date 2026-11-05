import type { components, paths } from "./generated/openapi.js";

export type { components, paths };

/** What `GET /v1/healthz` answers. */
export type Health = components["schemas"]["Health"];

/** What `GET /v1/budget` answers. */
export type BudgetSnapshot = components["schemas"]["BudgetSnapshot"];

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

export type ResolveRequest = components["schemas"]["ResolveRequest"];

export type VersionFilesRequest = components["schemas"]["VersionFilesRequest"];

/** The JSON body of every error the gateway answers itself. */
export type ApiError = components["schemas"]["ApiError"];

export type ErrorCode = components["schemas"]["ErrorCode"];
