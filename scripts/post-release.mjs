import {
  checkout,
  fail,
  fetch,
  gitOrFail,
  requireCleanTree,
  unpushed,
  update,
} from "./git.mjs";

requireCleanTree();

fetch();
for (const branch of ["main", "dev"]) {
  checkout(branch);
  const ahead = unpushed(branch);
  if (ahead) {
    fail(
      `${branch} has ${ahead} commit(s) that are not on origin. Push them or drop them, then run this again.`,
    );
  }
  update(branch);
}
gitOrFail(
  ["merge", "--no-edit", "main"],
  "Could not merge main into dev. Resolve the conflicts, commit, then run: git push origin dev",
);
gitOrFail(
  ["push", "origin", "dev"],
  "Could not push dev, see git's message above.",
);
