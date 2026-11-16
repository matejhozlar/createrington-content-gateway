import {
  checkout,
  fetch,
  git,
  gitOrFail,
  requireCleanTree,
  update,
} from "./git.mjs";

const base = "dev";
const force = process.argv.includes("--force");

const branch = git(["rev-parse", "--abbrev-ref", "HEAD"]);
requireCleanTree();

if (branch !== base) checkout(base);
fetch();
const unpushed = update(base);
if (unpushed) {
  console.log(
    `${base} has ${unpushed} commit(s) that are not on origin, so origin/${base} was merged into it. Push when ready: git push origin ${base}`,
  );
}

if (branch === base || branch === "HEAD") {
  console.log(
    `Nothing to delete, you were ${branch === base ? `on ${base}` : "detached"}.`,
  );
  process.exit(0);
}
gitOrFail(
  ["branch", force ? "-D" : "-d", branch],
  `Kept ${branch}: git does not see it as merged into ${base}. If it was squash merged, run: pnpm branch:done --force`,
);
