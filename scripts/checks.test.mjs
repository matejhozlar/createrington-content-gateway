import assert from "node:assert/strict";
import { test } from "node:test";
import { subjectProblem } from "./check-commits.mjs";
import { topHeading } from "./check-python-version.mjs";
import {
  envNames,
  inReadmeTable,
  linesWith,
  lockedVersion,
  versionIn,
} from "./check-repo.mjs";

test("a subject with a listed type and scope, or no scope, passes", () => {
  assert.equal(subjectProblem("fix(server): keep the hold"), null);
  assert.equal(subjectProblem("chore: run ci weekly"), null);
  assert.equal(subjectProblem("docs(client): name the CDN host"), null);
});

test("a subject outside the list names what is wrong", () => {
  assert.match(subjectProblem("ci: run weekly"), /type\(scope\)/);
  assert.match(subjectProblem("fix(deploy): roll back"), /type\(scope\)/);
  assert.match(subjectProblem("fix:no space"), /type\(scope\)/);
  assert.equal(subjectProblem("fix: "), "has no description");
  assert.equal(
    subjectProblem("feat: Add a route"),
    "starts its description in uppercase",
  );
  assert.equal(subjectProblem("feat: add a route."), "ends with a period");
});

test("versionIn reads the version of the named table only", () => {
  const toml = [
    "[workspace]",
    'members = ["crates/*"]',
    "",
    "[workspace.package]",
    'version = "1.2.3"',
    "",
    "[workspace.dependencies]",
    'axum = { version = "0.8.9" }',
  ].join("\n");
  assert.equal(versionIn(toml, "workspace.package"), "1.2.3");
  assert.equal(
    versionIn('[project]\nname = "x"\nversion = "0.2.2"\n', "project"),
    "0.2.2",
  );
  assert.equal(versionIn(toml, "project"), null);
});

test("lockedVersion finds the named package", () => {
  const lock = [
    "version = 4",
    "",
    "[[package]]",
    'name = "axum"',
    'version = "0.8.9"',
    "",
    "[[package]]",
    'name = "content-gateway"',
    'version = "0.1.7"',
    "dependencies = [",
  ].join("\n");
  assert.equal(lockedVersion(lock, "content-gateway"), "0.1.7");
  assert.equal(lockedVersion(lock, "tokio"), null);
});

test("envNames reads the variable constants of config.rs", () => {
  const config = [
    'const HOST_ENV: &str = "CONTENT_GATEWAY_HOST";',
    'const REDIS_ENV: &str = "REDIS_URL";',
    'const DEFAULT_PORT: &str = "5005";',
  ].join("\n");
  assert.deepEqual(envNames(config), ["CONTENT_GATEWAY_HOST", "REDIS_URL"]);
});

test("inReadmeTable wants the variable as a row of its own", () => {
  const readme =
    "Set `REDIS_URL` first.\n| `CONTENT_GATEWAY_PORT` | `5005` | Port |";
  assert.equal(inReadmeTable(readme, "CONTENT_GATEWAY_PORT"), true);
  assert.equal(inReadmeTable(readme, "REDIS_URL"), false);
});

test("linesWith gives one-based line numbers", () => {
  assert.deepEqual(linesWith("a\nb x\nc\nx", "x"), [2, 4]);
});

test("topHeading reads the first second-level heading", () => {
  assert.equal(
    topHeading("# client\n\n## 0.2.2\n\ntext\n\n## 0.2.1\n"),
    "0.2.2",
  );
  assert.equal(topHeading("# client\n"), null);
});
