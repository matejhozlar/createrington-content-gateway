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

export function requireCleanTree() {
  const dirty = git(["status", "--porcelain", "--untracked-files=no"]);
  if (dirty) fail(`Commit or stash your changes first:\n${dirty}`);
}

export function fetch() {
  try {
    git(["fetch", "--prune", "origin"]);
  } catch {
    fail("Could not fetch, see git's message above.");
  }
}

export function checkout(branch) {
  try {
    git(["checkout", branch]);
  } catch {
    fail(`Could not check out ${branch}, see git's message above.`);
  }
}

export function update(branch) {
  const remote = `origin/${branch}`;
  const unpushed = Number(git(["rev-list", "--count", `${remote}..${branch}`]));
  try {
    const output = git(["merge", unpushed ? "--no-edit" : "--ff-only", remote]);
    if (output) console.log(output);
  } catch {
    fail(
      unpushed
        ? `Could not merge ${remote} into ${branch}. Resolve the conflicts and commit, then run this again.`
        : `Could not fast-forward ${branch}, see git's message above.`,
    );
  }
  return unpushed;
}
