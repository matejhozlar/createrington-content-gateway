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

/** The JSON body of every error the gateway answers itself. */
export type ApiError = components["schemas"]["ApiError"];

export type ErrorCode = components["schemas"]["ErrorCode"];
