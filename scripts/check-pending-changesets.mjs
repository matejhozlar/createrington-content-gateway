import { readdirSync } from "node:fs";

const pending = readdirSync(".changeset").filter(
  (file) => file.endsWith(".md") && file !== "README.md",
);
if (pending.length > 0) {
  console.error(
    `These changesets were not turned into a version, run pnpm version-packages on the release branch:\n${pending.map((file) => `.changeset/${file}`).join("\n")}`,
  );
  process.exit(1);
}
console.log("Every changeset is turned into a version");
