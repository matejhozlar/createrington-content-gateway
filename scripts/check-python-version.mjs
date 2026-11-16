import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { versionIn } from "./check-repo.mjs";
import { git } from "./git.mjs";

const dir = "packages/python-client";
const pyproject = `${dir}/pyproject.toml`;
const changelog = `${dir}/CHANGELOG.md`;

export function topHeading(markdown) {
  return /^## +(\S+)/m.exec(markdown)?.[1] ?? null;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const base = process.argv[2];
  const version = versionIn(readFileSync(pyproject, "utf8"), "project");
  const problems = [];

  const heading = topHeading(readFileSync(changelog, "utf8"));
  if (heading !== version) {
    problems.push(
      `The top heading of ${changelog} is ${heading ? `## ${heading}` : "missing"}, but ${pyproject} says ${version}: describe the version in a ## ${version} section`,
    );
  }

  if (base) {
    const changed = git([
      "diff",
      "--name-only",
      `${base}...HEAD`,
      "--",
      `${dir}/src`,
    ])
      .split("\n")
      .filter(Boolean);
    const exists = git(["ls-tree", "--name-only", base, pyproject]) !== "";
    const baseVersion = exists
      ? versionIn(git(["show", `${base}:${pyproject}`]), "project")
      : null;
    if (changed.length > 0 && baseVersion === version) {
      problems.push(
        `${dir}/src changed against ${base} but the version in ${pyproject} is still ${version}: bump it and add its ${changelog} entry, or PyPI keeps the old code`,
      );
    }
  }

  for (const problem of problems) console.error(problem);
  if (problems.length > 0) process.exit(1);
  console.log(`The Python client's version ${version} matches its changelog`);
}
