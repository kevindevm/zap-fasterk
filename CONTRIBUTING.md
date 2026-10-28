# Contributing to ZapFasterk

ZapFasterk is built by a daily WhatsApp power user for people who want more from
their desktop client. We want useful workflows inspired by past WhatsApp plugins,
practical improvements for busy conversations, and faster delivery of fixes and
features. Help us turn real daily frustrations into reliable native features.

## Ways to help

- **Report bugs:** describe what you expected, what happened, your app version,
  operating system, and steps someone else can follow.
- **Suggest features:** describe the workflow you want to improve. If an old
  plugin did it well, explain how it worked and why it mattered to you.
- **Write code:** fix a bug or build a focused feature. Discuss large changes
  first so we can agree on behavior and scope before you spend time on them.
- **Test changes:** reproduce bugs, try fixes, and report which platform and
  version you tested. Cross-platform testing is especially useful.
- **Improve translations and docs:** make the app and its guides easier to use.

Search [our issues](https://github.com/kevindevm/zap-fasterk/issues) and
[pull requests](https://github.com/kevindevm/zap-fasterk/pulls) before starting.
You do not need to follow ZapFast's PR backlog to contribute here. When adapting
someone else's work, link the original change, credit its authors and preserve
its license notices. A corresponding upstream PR can still be open.

## Get started

Clone the repository and use the pinned toolchain in `rust-toolchain.toml`:

```sh
git clone https://github.com/kevindevm/zap-fasterk.git
cd zap-fasterk
cargo run --locked --release
```

See [PACKAGING.md](PACKAGING.md) for platform dependencies and build instructions,
[AGENTS.md](AGENTS.md) for architecture and coding conventions, and
[DEMO.md](DEMO.md) for synthetic test conversations and screenshots.
Outside contributors should create a branch in their own fork and open a pull
request against this repository's `main`.

## Before opening an issue

Search open and closed issues first. For a bug, use the bug form and include
the requested diagnostics and exact steps to reproduce it. Inspect logs locally
and redact private data before attaching them. If a report is missing details,
we may ask for a reproduction or close it with an explanation.

For a feature, explain the user problem. Discuss large changes in an issue
before writing code. Existing code does not guarantee that a feature fits the
project.

Product boundaries and upstream responsibilities:

- The protocol comes from [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust).
  A capability it does not support is fixed upstream first, not reimplemented
  here.
- ZapFasterk will not embed a browser engine, add telemetry, or introduce a
  ZapFasterk-operated service. Features that send message content to a third
  party are out of scope.

The guide's [current limitations](docs/_guide/what-is-zapfast.md#what-it-does-not-do-yet)
describe what is implemented today, not permanent exclusions. Missing features,
codec restrictions, and download limits do not by themselves make a report out
of scope. Check the relevant code and reported version when a guide and a
report disagree; leave uncertain product decisions to the maintainer.

Use synthetic conversations for screenshots and recordings. Never upload a real
archive, session database, message contents, contact names, phone numbers,
keys, or linking QR payloads. Do not give personal chats or credentials to an AI
tool. Tests and reproductions should use synthetic fixtures and mock credentials.

Duplicate, out-of-scope, or incomplete issues may be closed with a short
explanation. A bug can be closed once its fix is on `main`, with the commit and
release status stated. Reopen the issue if it persists after updating.

## Design principles

1. **Native and fast.** Startup time, idle work, memory use, and binary size
   are product features. Keep the UI thread free of network and disk waits.
2. **Focused.** Prefer a complete, coherent workflow over a collection of
   settings, modes, and speculative features.
3. **Honest integrations.** Use whatsapp-rust for what it supports. Do not
   advertise a capability merely because a protobuf field exists for it.
4. **Cross-platform by default.** Linux, macOS, and Windows are supported
   products. Platform-specific code must be isolated and the other targets
   must keep compiling.
5. **Small dependency surface.** Reuse the standard library and existing
   crates where practical. A new dependency needs a concrete benefit worth its
   build time, binary size, maintenance, and security cost. Do not vendor or
   fork upstream crates such as egui.
6. **Private data.** The archive is personal data. Never log message contents,
   phone numbers, keys, or QR payloads.

## Pull requests

Keep each pull request to one change. A pull request that bundles unrelated
fixes or features will be closed with a request to split it. Explain why the
change belongs in ZapFasterk, what changed, and how you tested it. Avoid unrelated
formatting, refactors, generated prose, and large mechanical rewrites.

`main` has a linear history. Outside pull requests are squash-merged into one
focused commit with contributor credit; merge commits are not accepted. A
maintainer may push fixes to your branch before merging it.

### AI-assisted contributions and human sign-off

AI coding tools are welcome for implementation, tests, investigation and docs.
The quality bar is the same for hand-written and AI-assisted changes:

- A developer must inspect the complete diff, understand the submitted code,
  and take responsibility for its behavior and dependencies.
- State in the PR description whether AI tools helped and what they helped with.
- Verify the behavior with appropriate tests. Explain what you tested and any
  platform or live-device behavior you could not verify.
- Complete the human review and sign-off checkbox in the PR template. If the
  submitter cannot review the code, name the developer who reviewed it and have
  them confirm their sign-off in the PR.
- Answer review comments with reasoning grounded in the code. An AI-generated
  explanation or an automated approval does not replace developer review.

A maintainer reviews the change before merging, and required CI checks must
pass. We aim to move quickly through focused, reviewable changes; there is no
guaranteed merge date.

Code changes should include tests for behaviour that can regress. User-visible
behaviour, settings, files, or network access must be documented in the same
pull request.

### Screenshots

Every pull request that changes what the app looks like needs before-and-after
screenshots or a short recording **in the pull request description**. Capture
them with the `demo` feature (`cargo run --features demo -- --demo`, or
`--demo-shot` for a headless capture) so they show only synthetic content, and
include light and dark themes when colours or layout change. Do not commit
screenshots, recordings, or other media to the repository.

### Checks

Run the same checks CI runs before submitting:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets
cargo test --locked --all-targets --all-features
RUSTDOCFLAGS='-D warnings' cargo doc --locked --all-features --no-deps
```

Translation changes also need `.github/scripts/update-translations.sh --check`,
using GNU gettext tools with Rust support. Run the script without `--check` when
translatable source strings change, and review any fuzzy or missing entries in
the updated PO files. Keep each translatable literal inside its own `gettext`
call so extraction can find it. Normal Cargo builds compile the catalogs without
gettext tools.

Linux needs the development packages listed in
[the build and packaging guide](PACKAGING.md); `nix develop`
provides the complete development environment. When changing `Cargo.lock` or
`flake.nix`, also verify `nix build` on a Nix host or wait for the Nix CI job.
Passing CI is required, but does not replace review for correctness, product
fit, maintainability, or security.

`AGENTS.md` describes the architecture and conventions in more detail.

By contributing, you agree that your contribution is licensed under the
project's MIT License.
