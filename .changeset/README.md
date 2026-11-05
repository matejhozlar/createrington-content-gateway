# Changesets

Every change to `packages/client` that consumers can notice comes with a changeset, written on the feature branch with `pnpm changeset`. The release PR runs `pnpm version-packages`, which turns the changesets into the version bump and the `CHANGELOG.md` entry of the package, and the merge into `main` publishes that version. The server's version lives in the root `package.json` and `Cargo.toml`, not here.
