# Cutting a release

Releases are tag-triggered and CI-built. A `v*` tag push runs
`.github/workflows/release.yml`, which builds both boards, **creates the GitHub
release itself**, uploads five assets (`firmware-x4.bin`, `firmware-x3.bin`,
`update.bin`, `FWUPDATE.BIN`, `FWUPDX3.BIN`), and then dispatches the Pages
deploy. The Pages build downloads the latest release's firmware into the site,
so **no populated release ⇒ no site deploy** — an empty or missing release
fails Pages with "release not found".

## Versioning rules

- Bare semver, continuing the shared line with upstream MarigoldOS (this fork's
  first release was v0.4.0 after upstream's v0.3.2). Never reset to v0.1.0.
- The tag, `fw`'s crate version, and the site's version label must agree. The
  app descriptor's `version` stamp comes from `env!("CARGO_PKG_VERSION")` in
  `fw/src/main.rs`, so a tag that doesn't match the crate version ships a lying
  stamp.

## Procedure

1. Start on `main`, synced, clean tree.
2. Run `tools/prepare-release.sh <version>` (e.g. `0.5.0`). It syncs
   `fw/Cargo.toml`, refreshes `Cargo.lock`, builds the X4 image, measures the
   real size, updates the site's Version/Size/release-notes labels, and
   verifies the descriptor stamps in the built ELF. It changes files only —
   no commit, no push, no tag.
3. Review the diff, commit as `Prepare v<version> release`. **Stop here and
   hand off: the maintainer pushes, tags, and pushes the tag.** Agents don't
   push or tag without an explicit go-ahead.
4. `git tag v<version> && git push origin v<version>` (maintainer). The tag
   push does everything else.

**Never pre-create the GitHub release** — not in the UI, not with
`gh release create`. The workflow's own `gh release create` fails on "a release
with the same tag name already exists", leaving an assetless release that also
blocks Pages. If notes beyond the workflow's generic line are wanted, add them
*after* CI publishes: `gh release edit v<version> --notes '...'`.

## Verify after CI finishes

```sh
# all five assets present, with sizes
gh release view v<version> --json assets -q '.assets[] | .name + " " + (.size|tostring) + "B"'

# the published binary really carries the stamps
curl -sL https://github.com/chongfun/calendula-os/releases/latest/download/firmware-x4.bin \
  | strings | grep -E "CalendulaOS \(MarigoldOS\)|<version>"

# Pages deployed and serves the firmware it embedded
curl -sI https://chongfun.github.io/calendula-os/firmware-x4.bin | grep -i "HTTP/\|content-length"
```

The Pages `content-length` must match the release asset's size, and the site's
displayed version label must match the tag (prepare-release.sh set it before
the tag was cut).

## Recovery: release exists but is empty

This is the pre-created-release collision (or a partial upload). Fix:

```sh
gh release view v<version> --json body -q .body   # save any hand-written notes first
gh release delete v<version> --yes                # deletes the release, KEEPS the tag
gh run rerun <failed-run-id> --failed             # re-runs the release job from the tag
gh release edit v<version> --notes '...'          # restore the saved notes
```

Rerunning re-executes the whole job (build + create + upload) from the tagged
ref, so the binaries stay CI-built.
