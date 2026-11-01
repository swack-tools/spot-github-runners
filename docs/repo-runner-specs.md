# Repository runner specs

A repository that the fleet serves can describe the runners its workflows need in a file it owns, `.github/spot-runners.yml`. The file uses the `spot-runner-spec-v1` format. It names the labels that jobs put in `runs-on`, the architecture, the size of one runner, how many runners the repository wants, whether jobs need a Docker daemon, CPU features, Rust toolchains, and Ubuntu packages. The Worker reads the file from the repository's default branch, checks it, and builds the runner image from it, so a repository gets Rust and its packages preinstalled without an operator editing the central configuration repository.

A spec is a request. The Worker compares each pool with what a floor host can give a runner. When a floor host can't serve any pool, the Worker holds the launch and reports why, rather than starting a runner whose jobs would fail.

## Before you begin

- The repository must already be a target of the fleet. The Worker reads specs only from the repositories it serves, and an operator adds those in code.
- The Worker's GitHub token must be able to read the repository's contents. GitHub answers HTTP 404 for a private repository that the token can't read, or HTTP 403 when the token lacks Contents access, and the Worker launches as if the repository had no spec. `GET /api/repo-specs` reports a 403 as `unreadable`.

## How a spec relates to runner patches

| Setting | Repository spec (`.github/spot-runners.yml`) | Runner patch (configuration repository) |
| --- | --- | --- |
| Owner | The repository | The fleet operator |
| Labels, architecture, CPU features | Yes | Yes |
| Ubuntu packages in the runner image | Yes | Yes |
| Rust toolchains | Yes, as names the Worker turns into `rustup` commands | Through `dockerfile_run` |
| Runner size and count | Yes, as a request | No |
| Host packages, files, `sysctl`, mounts, boot scripts | No | Yes |
| Shell commands | No | Yes (`dockerfile_run`, `boot_script`) |

The Worker first merges the configuration repository's `defaults.yaml` and `repos/<owner>/<repo>.yaml` into a runner patch, as described in [Runner patches](runner-patches.md). It then adds the spec pool that a floor host serves on top. Lists such as labels and packages are combined, the configuration repository's `dockerfile_run` runs before the toolchain installs, and host settings come only from the configuration repository.

## Example

```yaml
# Runners this repository asks the fleet for.
schema: spot-runner-spec-v1

pools:
  # Jobs with runs-on: ubuntu-latest or ubuntu-24.04
  - name: linux-x64
    arch: x64
    labels: [ubuntu-latest, ubuntu-24.04]
    cpus: 2
    memory_gib: 8
    min: 0
    max: 10
    needs:
      docker: true # The test job starts a database service container.
    toolchains:
      rust:
        toolchains: ["1.98.0"]
        components: [clippy, rustfmt]
    apt_packages: [par2, unrar, python3]

  # Jobs with runs-on: ubuntu-24.04-arm
  - name: linux-arm64
    arch: arm64
    labels: [ubuntu-24.04-arm]
    cpus: 2
    memory_gib: 8
    max: 10
    toolchains:
      rust:
        toolchains: ["1.98.0"]
        components: [clippy, rustfmt]
```

## Schema

The top level has two keys, and both are required.

| Key | Value |
| --- | --- |
| `schema` | `spot-runner-spec-v1` |
| `pools` | A list of one to eight pools |

Each pool takes the following keys. Only `name` and `labels` are required.

| Key | Value | Default |
| --- | --- | --- |
| `name` | 1 to 32 lowercase letters, digits, or dashes. Unique in the file. | None |
| `arch` | `x64` or `arm64`. `x86_64`, `amd64`, and `aarch64` are accepted as aliases. | `x64` |
| `labels` | 1 to 16 labels that the pool's jobs put in `runs-on`. Each is 1 to 64 letters, digits, dots, dashes, or underscores. | None |
| `cpus` | vCPUs one runner needs, 1 to 64. | Any |
| `memory_gib` | Memory one runner needs in GiB, 1 to 256. | Any |
| `min` | Idle runners to keep online, 0 to 50, counted in runners. | `0` |
| `max` | Most runners at once, 1 to 50, counted in runners. | No limit |
| `needs.docker` | `true` when jobs need a Docker daemon, for example for `services:` containers. | `false` |
| `isa` | Up to 8 CPU features, such as `avx2` or `avx512-vbmi2`. | None |
| `toolchains.rust.toolchains` | 1 to 4 toolchains: a release such as `"1.98.0"` or `"1.98"`, `stable`, `beta`, `nightly`, or `nightly-YYYY-MM-DD`. Quote release numbers so YAML doesn't read them as numbers. | None |
| `toolchains.rust.components` | Up to 8 of `clippy`, `rustfmt`, `rust-src`, `rust-analyzer`, `llvm-tools`, `llvm-tools-preview`, `miri`, and `rust-docs`. `miri` needs nightly toolchains only. | None |
| `toolchains.rust.targets` | Up to 16 target triples, such as `x86_64-unknown-linux-musl`. | None |
| `apt_packages` | Up to 64 Ubuntu package names. Versions and options aren't allowed. | None |

The Worker refuses a spec with any of these problems, and lists every error it finds:

- An unknown key at any level, so that a typo can't drop a setting.
- A line that isn't part of the document because of its indentation.
- A label that the fleet adds on its own (`self-hosted`, `Linux`, `X64`, `ARM64`, `spot`), or a label that names a machine type or a zone. The fleet picks the shape and zone, so a repository can't claim one it won't get.
- A file larger than 32 KiB, or one that nests deeper than the parser allows.

The parser supports the same YAML subset as runner patches: mappings, lists, flow lists such as `[a, b]`, quoted strings, and comments.

## How the Worker uses a spec

1. When a floor host for the repository is due, the Worker reads `.github/spot-runners.yml` from the repository's default branch.
2. It picks the first pool that a floor host can serve. A floor host today runs two runners with 7 vCPUs and 14 GiB each on an x64 machine, inside containers without a Docker daemon.
3. It adds the pool's labels, CPU features, packages, and toolchains to the runner patch. The Worker writes the `rustup toolchain install` commands itself from the validated names, so text from the repository never reaches a shell. The toolchains go into the runner image's read-only shared Rust store, where `rustup which --toolchain <version>` finds them without a download.
4. When no pool fits, or the spec is invalid or can't be read, the Worker holds the repository's floor launch and logs the reason with the scheduled run. `GET /api/repo-specs` shows the same problem. Fix the spec, and a later scheduled run tries again.

A pool can't be served by a floor host when it asks for any of the following:

| Request | Status |
| --- | --- |
| `arch: arm64` | The fleet launches only x64 runners so far. |
| `needs.docker: true` | Runner containers have no Docker daemon yet. |
| `cpus` more than 7 or `memory_gib` more than 14 | Larger than one floor runner. |
| `isa` that the floor machine lacks | Held by the existing CPU feature check. |

The Worker also holds the launch when the chosen pool's architecture differs from the one the configuration repository sets for the repository, because the operator's setting wins.

The Worker records `min`, `max`, `cpus`, and `memory_gib` for the runner pools work, which sizes and counts runners per pool. Until then, a floor host provides two runners for each repository while the fleet's floor is on.

## Check a spec

An administrator can see every repository's spec in the dashboard API:

| Route | What it returns |
| --- | --- |
| `GET /api/repo-specs` | For each repository target: `status` (`valid`, `invalid`, `missing`, or `unreadable`), errors, each pool with `floor_host_can_serve` and `floor_host_gaps`, and `floor_pool`, the pool a floor host would serve. |
| `POST /api/repo-specs/validate` | The same report for spec text sent as `{"text": "<YAML>"}`, without reading GitHub. |

Both routes require an administrator session.
