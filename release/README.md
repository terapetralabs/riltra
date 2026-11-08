# Release candidate packaging

How a Riltra release candidate is built, checked and verified. Nothing here
publishes a release, creates a tag or signs anything: the `v0.1.0` release is a
separate maintainer action.

## What the workflow does

`.github/workflows/release-candidate.yml` runs by hand (`workflow_dispatch`) or
for a `v*-rc*` tag, only from `master`, on Windows, Linux and macOS x64, and for each of them:

1. builds the release-profile binary with `cargo build --release --locked`;
2. checks the release documents against the locked dependency graph
   (`cargo run --locked --example release_docs -- --check`);
3. packages `riltra-<version>-<platform>.zip` (Windows) or `.tar.gz`, with a
   `.sha256` file (`release/package.sh`);
4. unpacks the archive in a scratch directory and runs the clean-install smoke
   on the shipped binary alone (`release/smoke.sh`);
5. keeps the archive and its checksum as a workflow artifact.

The macOS runner is the x64 pool (see the comment in `ci.yml`); macOS ARM64 is
not covered.

Only `master` may start the workflow. A `guard` job (2026-10-26) refuses a
`workflow_dispatch` from any other branch, and a `v*-rc*` tag whose commit is not
an ancestor of `master`. `workflow_dispatch` also needs the workflow file on
`master`; until the file is merged there, dispatching it returns HTTP 404. The
workflow does not publish a release or create a tag.

## Reproduce locally

```bash
cargo fetch --locked                              # includes optional crates for full notices
cargo build --release --locked -p riltra
bash release/package.sh linux-x64 tar.gz        # or: windows-x64 zip, macos-x64 tar.gz
bash release/smoke.sh dist/riltra-*-linux-x64.tar.gz
sha256sum -c dist/riltra-*.sha256                # shasum -a 256 -c on macOS
```

## The generated documents

`sbom.cdx.json` is a CycloneDX 1.5 document of the locked dependency graph
(every package in `Cargo.lock`, for every platform and workspace feature, with
its license expression, crate checksum and dependency edges). For the two
optional crates that bundle separately licensed native code, it combines the
crate and bundled-code license expressions. It is a superset
of what the default core build links; optional packages are included for audit
and notice coverage. The committed copy is
`release/sbom.cdx.json`; `--check` fails when it no longer matches `Cargo.lock`
or when a package has no license or no checksum. After a dependency change:

```bash
cargo run --locked --example release_docs -- --out release   # rewrites the SBOM
```

Do not commit `release/THIRD_PARTY_LICENSES.txt`: it is regenerated into every
archive from the license and notice files that each published crate carries,
and a package whose crate carries none is named in the tool's output.

The `new_dependencies_are_audited_for_core_and_semantic_feature_graphs` test
checks core and `semantic-knowledge` Cargo trees separately, including the
active FastEmbed/ORT feature selection. It keeps the existing core license
expressions and separately reviews the optional graph's SPDX expressions and
the `option-ext` MPL-2.0 notice. All locked package rows are recorded in
`docs/licensing.md`.

The optional model/runtime layout, runtime manifest fields, and their trust
limits are documented in [`docs/semantic-inference.md`](../docs/semantic-inference.md).
Release and installer jobs fetch the locked dependency sources before
generating notices, so optional crates' license files are available even though
the shipped default binary does not link those crates. Fetching is packaging
tooling; it does not make application inference download a model or runtime.

## Gates

The [changelog](../CHANGELOG.md#unreleased) records the verified scope and
limits of the post-release workflow run.
