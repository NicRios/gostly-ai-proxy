# Contributing to gostly

Thanks for your interest. Read this first — it'll save you time.

## Scope freeze for v1

The OSS proxy is intentionally narrow: record, replay, smart-swap, OpenAPI/Postman/HAR import, basic chaos primitives, single-binary distribution. **That's the v1 scope and it is frozen** through the v0.1 release (target: Sat May 23 2026).

Anything beyond that — AI gap-fill, multi-user dashboards, drift detection, repair PRs, SAML/RBAC, hosted compliance — lives in the closed cloud product at [gostly.ai](https://gostly.ai). PRs that add those features here will be closed with a pointer to the cloud roadmap.

This freeze exists so a single maintainer can keep up with reviews. It will be revisited after v1 ships.

## Filing issues

Bug reports are very welcome. A good report includes:

- gostly version (`gostly --version`)
- OS + arch
- The exact command you ran
- What you expected vs. what happened
- Minimal repro (a curl command + the upstream URL pattern is usually enough)

Feature requests: open an issue tagged `proposal`. If it fits the v1 scope, great. If it's cloud-shaped, expect a redirect to gostly.ai. Either response is fine — don't take it personally.

## Pull requests

Bug-fix PRs and small CLI-ergonomics improvements: open directly. Keep diffs focused.

New features (even small ones): open a discussion issue first. We'll confirm scope fit before you write code.

Required for any PR:

- `cargo build` and `cargo build --features cloud` both succeed
- `cargo test` passes
- `cargo clippy` adds no new warnings
- The `oss-binary-no-cloud-symbols` CI gate passes

## Build instructions

```
git clone https://github.com/NicRios/gostly-ai-proxy
cd gostly-ai-proxy
cargo build --release --no-default-features --features oss
./target/release/gostly --version
```

For the cloud-feature build (requires the closed-product source tree, not in this repo):

```
cargo build --release --features cloud
```

## Code of Conduct

This project follows the [Contributor Covenant 2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/). Be decent to people. Report problems to hello@gostly.ai.

## Sign-off

DCO sign-off (`git commit -s`) is appreciated but optional for v1. We may make it required later; we'll update this file if so.

## Trademark

"Gostly" is a trademark of Gostly, Inc. Forks may use the code under the FSL-1.1-Apache-2.0 license; forks may not call themselves Gostly. See [TRADEMARKS.md](TRADEMARKS.md) when published.
