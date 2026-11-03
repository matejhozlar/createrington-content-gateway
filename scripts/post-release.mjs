// After a release: brings main and dev up to date, merges main into dev so dev
// holds the release merge, and pushes dev. Refuses when tracked files have
// uncommitted changes. Leaves you on dev.
import {
  checkout,
  fail,
  fetch,
  git,
  requireCleanTree,
  update,
} from "./git.mjs";

function run(args, message) {
  try {
    const output = git(args);
    if (output) console.log(output);
  } catch {
    fail(`${message}, see git's message above.`);
  }
}

requireCleanTree();

fetch();
for (const branch of ["main", "dev"]) {
  checkout(branch);
  update(branch);
}
run(
  ["merge", "--no-edit", "main"],
  "Could not merge main into dev. Resolve the conflicts, commit, then run: git push origin dev",
);
run(["push", "origin", "dev"], "Could not push dev");
