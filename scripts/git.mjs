import { execFileSync } from "node:child_process";

export function git(args) {
  return execFileSync("git", args, {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "inherit"],
  }).trim();
}

export function fail(message) {
  console.error(message);
  process.exit(1);
}

export function gitOrFail(args, message) {
  let output;
  try {
    output = git(args);
  } catch {
    fail(message);
  }
  if (output) console.log(output);
  return output;
}

export function requireCleanTree() {
  const dirty = git(["status", "--porcelain", "--untracked-files=no"]);
  if (dirty) fail(`Commit or stash your changes first:\n${dirty}`);
}

export function fetch() {
  gitOrFail(
    ["fetch", "--prune", "origin"],
    "Could not fetch, see git's message above.",
  );
}

export function checkout(branch) {
  gitOrFail(
    ["checkout", branch],
    `Could not check out ${branch}, see git's message above.`,
  );
}

export function unpushed(branch) {
  return Number(git(["rev-list", "--count", `origin/${branch}..${branch}`]));
}

export function update(branch) {
  const remote = `origin/${branch}`;
  const ahead = unpushed(branch);
  gitOrFail(
    ["merge", ahead ? "--no-edit" : "--ff-only", remote],
    ahead
      ? `Could not merge ${remote} into ${branch}. Resolve the conflicts and commit, then run this again.`
      : `Could not fast-forward ${branch}, see git's message above.`,
  );
  return ahead;
}
