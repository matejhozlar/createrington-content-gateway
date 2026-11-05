Self-hosters of the public mirror have nothing to download: the Gitea deploy workflow only serves one installation, and the Docker image has to be built locally. This PR adds a GitHub Actions release workflow that turns every `vX.Y.Z` tag into a GitHub release with static binaries and a multi-arch image on GHCR.

The trigger is the tag, not a push to `main`: the release automation tags the release merge and the mirror carries the tag over, so one tag gives exactly one release named after the version, and a manual run can rebuild a tag through `workflow_dispatch`. The `check` job refuses a tag that does not match `package.json` and `Cargo.toml`, resolves the commit and build time once so every artifact reports the same `/v1/healthz`, and cuts the release notes from the tag's section of `CHANGELOG.md`.

The `build` job runs natively on `ubuntu-24.04` and `ubuntu-24.04-arm`, so no emulation: it exports the binary from the Dockerfile's `binary` stage, then builds the final image from the same builder (the `build` stage is reused from the local BuildKit cache) and pushes it by digest. The `image` job merges the two digests into one manifest tagged `X.Y.Z` and `X.Y`, with `latest` from the semver tags. The `release` job attaches both binaries, `openapi.json` and a `SHA256SUMS` to the release.

Every job carries the `github.server_url` guard, since Gitea Actions can fall back to `.github/workflows`. The README's Docker section names the published image, and the development section says how a server release reaches GitHub.

Nothing is published until the mirror has a `v*` tag; the next release merge will exercise it.
