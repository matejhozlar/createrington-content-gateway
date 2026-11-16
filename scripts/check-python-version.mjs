import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { versionIn } from "./check-repo.mjs";
import { fail, git } from "./git.mjs";

const dir = "packages/python-client";
const pyproject = `${dir}/pyproject.toml`;
const changelog = `${dir}/CHANGELOG.md`;

export function topHeading(markdown) {
  return /^## +(\S+)/m.exec(markdown)?.[1] ?? null;
}

export function versionProblems({
  version,
  heading,
  released,
  releasedVersion,
  sourceChanged,
}) {
  const problems = [];
  if (heading !== version) {
    problems.push(
      `The top heading of ${changelog} is ${heading ? `## ${heading}` : "missing"}, but ${pyproject} says ${version}: describe the version in a ## ${version} section`,
    );
  }
  if (sourceChanged && releasedVersion === version) {
    problems.push(
      `${dir}/src changed since the release on ${released} but the version in ${pyproject} is still the released ${version}: bump it and add its ${changelog} entry, or PyPI keeps the old code`,
    );
  }
  return problems;
}

if (import.meta.url === pathToFileURL(process.argv[1]).href) {
  const released = process.argv[2] ?? "origin/main";
  try {
    git(["rev-parse", "--verify", "--quiet", `${released}^{commit}`]);
  } catch {
    fail(`${released} is not available, fetch it first: git fetch origin main`);
  }
  const releasedManifest =
    git(["ls-tree", "--name-only", released, pyproject]) === ""
      ? null
      : git(["show", `${released}:${pyproject}`]);
  const version = versionIn(readFileSync(pyproject, "utf8"), "project");
  const problems = versionProblems({
    version,
    heading: topHeading(readFileSync(changelog, "utf8")),
    released,
    releasedVersion: releasedManifest
      ? versionIn(releasedManifest, "project")
      : null,
    sourceChanged:
      git(["diff", "--name-only", `${released}...HEAD`, "--", `${dir}/src`]) !==
      "",
  });
  for (const problem of problems) console.error(problem);
  if (problems.length > 0) process.exit(1);
  console.log(
    `The Python client's version ${version} matches its changelog and its source against ${released}`,
  );
}
