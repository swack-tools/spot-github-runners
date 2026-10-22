# Development

This page covers the development workflow for the pure-Rust architecture: toolchains, test suites,
code formatting, documentation style checks, and pull request review gates. For operational safety
rules that apply to live environments, see [Operations and troubleshooting](operations.md).

## Environment and prerequisites

The repository consists of two Rust workspaces and edge deployment tooling:

- **Host agent** (`host-agent/`): compiled with Rust 1.85 or later on Linux and macOS. Uses standard
  system libraries and POSIX descriptors without runtime dependencies.
- **Edge control plane** (`cloudflare/`): compiled with Rust target `wasm32-unknown-unknown` and packaged
  using the Wrangler command-line tool.
- **Task automation** (`justfile`): recipes for building, testing, linting, and formatting.
- **Documentation site** (`docs/` and `mkdocs.yml`): built using MkDocs Material (`docs/requirements.txt`).
- **Secrets and keys**: maintain `.env` (mode `0600`), SSH private keys, and operational state outside
  the Git checkout. Document new environment variables in `.env.example` and [Configuration](configuration.md).

## Build and test workflows

Run focused tests while iterating, and run the complete workspace test suite before committing:

```bash
# Run all 268 tests across host-agent and cloudflare workspaces
just test

# Alternatively, test workspaces directly via Cargo
cargo test --manifest-path host-agent/Cargo.toml
cargo test --manifest-path cloudflare/Cargo.toml
```

### Static analysis and style checks

Ensure all code passes compiler warnings and formatting checks before pushing commits:

```bash
# Run clippy and compiler checks across both workspaces
just check

# Check formatting without modifying files
just fmt-check

# Format Rust code automatically
just fmt
```

Local tests, proof runs, GitHub CI checks, merged commits on `main`, and live cloud observations
prove different properties. State the exact command, target, and environment for every verification claim.

## Documentation and prose checks

Documentation follows the [Google developer documentation style guide](https://developers.google.com/style):
sentence-case headings, active voice, direct instructions, and explicit prerequisites.

Install Vale from its official documentation. Synchronize the configured Google style rules and run the linter:

```bash
vale sync
vale README.md AGENTS.md CLAUDE.md docs/*.md
```

When adding a new Markdown documentation file:

1. Register it in `.vale.ini` under `BasedOnStyles = Google`.
1. Add it to the `nav` section of `mkdocs.yml`. The documentation build runs in strict mode, so an unlisted
   page, broken internal link, or missing anchor fails the build.
1. Allowlist any non-standard path in `.gitignore`.

### Build the documentation site locally

Create a virtual environment outside the checkout, install the build dependencies, and build in strict mode:

```bash
python3 -m venv "$HOME/oxidex-ops/docs-venv"
"$HOME/oxidex-ops/docs-venv/bin/pip" install -r docs/requirements.txt
"$HOME/oxidex-ops/docs-venv/bin/mkdocs" build --strict --site-dir "$HOME/oxidex-ops/docs-site"
```

Preview the site locally using `mkdocs serve` at `http://127.0.0.1:8000`. The GitHub Pages workflow
(`.github/workflows/docs.yml`) executes the same strict build on pull requests and publishes on merges to `main`.

!!! warning "The documentation site is public"
    A GitHub Pages site is publicly readable. Describing the architecture, edge design, and admission protocols
    is expected, but do not include deployment identifiers: project IDs, account IDs, namespace IDs, runner group
    IDs, private hostnames, tokens, or secret values. Use generic placeholders such as `your-project` or
    `your-unique-name`.

## Git and review workflow

A task prompt authorizes working on an isolated feature branch, committing changes, pushing the branch,
opening a pull request against `main`, resolving review feedback, and squash-merging once all review and CI
gates pass. It does not authorize force-pushing, changing branch protections, or mutating live cloud resources.

### Branch, identity, and accounts

- Develop on a dedicated feature branch or isolated Git worktree based on latest `origin/main`. Never push
  directly to `main`.
- Author and SSH-sign commits as `swackhamer` (`user.name=swackhamer`,
  `user.email=swackhamer@users.noreply.github.com`, `commit.gpgsign=true`). Verify local signatures with
  `git cat-file commit HEAD`, and verify GitHub's verification status on pushed commits.
- When multiple `gh` accounts exist, export the `swackhamer` token for the shell session:

  ```bash
  export GH_TOKEN="$(gh auth token --user swackhamer)"
  gh pr status --repo swack-tools/spot-github-runners
  ```

- Stage task files by explicit path with `git add <path>` so scratch files and unrelated edits are never committed.

### Review loop

1. Run the full unit test suite and Vale checks on every modified documentation file.
1. Obtain an independent local review of the complete diff against `origin/main` before opening a pull request.
1. Push the branch and open a pull request targeting `main`. Request independent review.
1. Inspect each review comment against the codebase. Address every valid issue, rerun unit tests and Vale checks,
   push a signed commit, reply with the fix and verification evidence, and resolve the review thread.
1. Request a fresh review on the new head commit. Repeat until the reviewer reports no remaining issues, all
   review threads are resolved, and required GitHub checks pass.
1. Confirm that `origin/main` has not advanced in a conflicting way and that the reviewed head commit matches
   the pull request head. Then squash-merge while pinning the reviewed commit SHA:

   ```bash
   gh pr merge --repo swack-tools/spot-github-runners \
     --squash --delete-branch --match-head-commit "$REVIEWED_HEAD_SHA"
   ```
