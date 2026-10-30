# Releasing

Versions are `X.Y.Z+xz.<xz version>` (SemVer for this crate's API, the
matched xz release as build metadata), tagged `vX.Y.Z`. Publishing to
crates.io is permanent: a version can be yanked but never replaced.

## Each release

1. Branch, then update `version` in the workspace `Cargo.toml` and move the
   `[Unreleased]` notes in `CHANGELOG.md` under the new version.
2. Check the package: `cargo package -p xz-native --list` and
   `cargo publish -p xz-native --dry-run`.
3. PR, squash-merge once CI is green.
4. Tag `main` and push the tag:
   `git tag v0.1.1 && git push origin v0.1.1`.
5. The Release workflow (`.github/workflows/release.yml`) checks that the
   tag matches the version, runs the tests and a dry run, then publishes
   through crates.io trusted publishing (after approval, if the `release`
   environment requires reviewers). docs.rs builds the docs by itself.
6. Create the GitHub Release from the tag with the changelog section.

## First release (once)

Trusted publishing is set up per crate, so the crate must exist first:

1. `cargo login` with a crates.io API token (scope: publish-new), then
   `cargo publish -p xz-native` from the tagged commit.
2. On crates.io, crate settings -> Trusted Publishing: add this repository,
   workflow `release.yml`, environment `release`.
3. In the GitHub repository settings, create the `release` environment
   (optionally with required reviewers), and revoke the API token.

From then on, releases go through the workflow without a stored token.
