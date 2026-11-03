// Fails when the client package changed against the base branch without a
// changeset or a version bump, so no change reaches consumers unversioned.
// Usage: node scripts/check-client-changeset.mjs [origin/dev]
import { git } from "./git.mjs";

const base = process.argv[2] ?? "origin/dev";
const manifest = "packages/client/package.json";

const changed = git(["diff", "--name-only", `${base}...HEAD`])
  .split("\n")
  .filter(Boolean);
const clientChanged = changed.some(
  (file) => file.startsWith("packages/client/src/") || file === manifest,
);
if (!clientChanged) {
  console.log("The client package did not change");
  process.exit(0);
}

const hasChangeset = changed.some(
  (file) =>
    file.startsWith(".changeset/") &&
    file.endsWith(".md") &&
    file !== ".changeset/README.md",
);
const versionOf = (ref) =>
  git(["ls-tree", "--name-only", ref, manifest]) === ""
    ? null
    : JSON.parse(git(["show", `${ref}:${manifest}`])).version;
const bumped = versionOf(base) !== versionOf("HEAD");

if (hasChangeset || bumped) {
  console.log(
    hasChangeset
      ? "The client package changed and a changeset describes it"
      : "The client package changed and its version was bumped",
  );
  process.exit(0);
}
console.error(
  `The client package changed against ${base} without a changeset. Run pnpm changeset and commit the file it writes.`,
);
process.exit(1);
