# Releasing alforria

This checklist covers a binary release and the public site. The release
workflow is in `.github/workflows/release.yml`; website deployment is handled
outside this repository.

## 1. Prepare the repository

- Confirm `main` is green on all three operating systems.
- Decide the version. alforria is pre-1.0, so a minor release may include
  breaking changes.
- Update the workspace version in `Cargo.toml`.
- Update `CHANGELOG.md`: move `Unreleased` entries under the new version and
  add the date.
- Run the local gates:

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo nextest run
```

- Commit the release changes and merge them to `main`.

## 2. Refresh the website

The website source is in the separate `alforria-website` repository.

- Keep docs version-agnostic. Link to the releases page instead of hardcoding
  a tag.
- Re-read `content/docs/install.md` when assets, platforms, or installer
  behavior change.
- Confirm the download page still matches the release process: installer,
  prebuilt binaries, source build, and upgrade notes.
- Run the site gates:

```sh
npm ci
npm run build
npx tsc --noEmit
npx playwright test
```

- Deploy `dist/` to `alforria.ai`. The host must serve `dist/404.html` with
  HTTP 404 for unknown paths.
- Verify `https://alforria.ai/`, `/download.html`, `/docs/`, the canonical
  links, `robots.txt`, and `sitemap.xml` after deployment.

## 3. Draft the GitHub Release

The release workflow requires the GitHub Release to exist before it uploads
assets. The tag must point at a commit on the default branch.

1. Draft a release for the new `vX.Y.Z` tag.
2. Write human notes. The `.github/release.yml` categories are a starting
   point, not a substitute for an edited summary.
3. Include install commands and any platform changes.
4. Publish the release, then push the tag.

If a tag already exists without a release, draft the matching release before
pushing any new tag. Do not move a published tag.

## 4. Push the tag

```sh
git tag vX.Y.Z
git push origin vX.Y.Z
```

The workflow:

- checks that the tag version matches `Cargo.toml`;
- checks that the tag is an ancestor of the default branch;
- checks that the GitHub Release exists;
- runs the test suite on Linux, macOS, and Windows;
- builds Linux x86_64, Windows x86_64, macOS x86_64, and macOS aarch64
  binaries;
- uploads each binary with a SHA-256 sidecar.

Linux builds use Ubuntu 22.04 so the binary runs on glibc 2.35 or newer.

## 5. Verify the release

- Run the installer on a clean machine:
  `curl -fsSL https://raw.githubusercontent.com/alforria-ai/alforria/main/packaging/install.sh | sh`
- Check `alforria --version` and `alforria --help`.
- Download each asset and verify its `.sha256` sidecar.
- Run `alforria auth login`, `alforria models`, and one prompt on a clean
  install.
- Open `alforria web` and confirm the embedded UI loads.
- Confirm the website install commands and docs links resolve.
- Check the GitHub release shows all four binaries and sidecars.

## 6. After release

- Announce the release with the version and the install command.
- Watch issues for upgrade and platform problems.
- Start the next `Unreleased` section in `CHANGELOG.md`.

## Rollback

- A published GitHub Release can be edited or marked as a pre-release, but do
  not move or delete a tag that users may have installed.
- If a binary is broken, publish a new patch release with a clear note. The
  installer always resolves the latest release.
- If the website is wrong, redeploy the previous `dist/` build and fix the
  source in a new commit.
