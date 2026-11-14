# Ladderback

[![CI](https://github.com/nledford/Ladderback/actions/workflows/ci.yml/badge.svg?branch=main&event=push)](https://github.com/nledford/Ladderback/actions/workflows/ci.yml?query=branch%3Amain+event%3Apush)
[![MSRV](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2Fnledford%2FLadderback%2Fmain%2FCargo.toml&query=%24.workspace.package%5B%27rust-version%27%5D&label=MSRV&color=orange)](Cargo.toml)
[![Latest release](https://img.shields.io/github/v/release/nledford/Ladderback?sort=semver&label=release)](https://github.com/nledford/Ladderback/releases/latest)

A self-hosted backup manager for [rustic](https://rustic.cli.rs): plans, schedules and one dashboard for every machine.

> [!WARNING]
> **Ladderback 0.3.0 runs scheduled backups, but it can't restore yet.** To get files back, use the `rustic` CLI, which restores from any repository Ladderback backs up to; try a restore with it before you rely on those backups. The snapshot browser and restores in Ladderback come in M3. Until the hub (M4a), each machine runs its own Ladderback with its own web UI. The [roadmap](#roadmap) has the order.

## What it will do

Without a manager, each machine runs rustic from its own cron job or timer. Failures surface only if someone reads the logs, a laptop that was asleep at 02:00 silently skips its backup, and retention on a shared repository needs hand-coordination between hosts. Ladderback turns rustic into a managed service on each machine, with one place to see them all:

- **Plans and schedules.** What to back up, from which machine, into which repository, how often, and how long to keep it. Missed runs on sleeping laptops catch up at the next wake.
- **One dashboard for every machine.** Status, live progress, history, warnings and errors for every connected computer, with notifications (webhook, ntfy, Gotify, healthchecks.io, email).
- **Safe maintenance on shared repositories.** Backups never wait for a lock, and exactly one place runs forget, prune and check for each repository.
- **No lock-in.** Import existing rustic profiles and export any plan back to one, so the `rustic` CLI always stays an option.
- **Careful restores.** Restore into a new folder by default; in-place restore only from the machine's own UI, after a dry-run summary.
- **Scriptable by AI agents.** `ladderback mcp` is an MCP server over stdio. It drives a running instance, or a headless session of its own, through read, run and preview tools. It can't restore or delete. It will be able to propose a backup setup, which changes nothing until a person confirms it in the web UI.
- **One self-contained program.** It runs headless on servers and as a user service on desktops and laptops, with the web UI built in. It is accessible (WCAG 2.2 AA) and localization-ready.

Ladderback supports **Linux and macOS**. Windows is out of scope. It runs inside WSL2 as a Linux install, for files inside WSL, but it isn't a way to back up Windows: WSL stops when idle, so scheduled backups don't run unless something keeps it running, and files under `/mnt/c` are read without shadow copies or Windows metadata (ACLs, attributes, alternate data streams), so open files may fail and a restore to Windows loses that metadata.

## Ladderback or Backrest?

Ladderback borrows Backrest's user-facing model: repositories, plans, schedules, hooks, an operation log and restore into a new folder. The difference is the engine underneath. Backrest wraps the `restic` CLI. Ladderback links rustic's library, `rustic_core`, and inherits rustic's lock-free repository design.

**Ladderback may suit you better if you:**

- back up **several machines into one shared repository**. In Backrest, each plan runs `restic forget` after its backup, which takes an exclusive lock and blocks other hosts' backups ([backrest#244](https://github.com/garethgeorge/backrest/issues/244)). In Ladderback, backups never wait for a lock, and one owner per repository runs maintenance;
- don't want repository **passwords and backend keys copied to other machines**. Backrest's multi-host sync pushes shared repositories to clients, credentials included. Ladderback's hub keeps its own secrets, and agents never send theirs;
- back up **laptops**. Missed runs catch up by default, and the scheduler notices sleep and wake;
- want **secrets out of the config file**: a password command, the system keyring, a file or an environment variable. A missing secret fails the job instead of silently becoming an empty string;
- want **time-based retention** (`keep-within-*`), not just counts;
- already use **rustic profiles** and want to keep using the CLI on the same repositories.

**Backrest is the better choice if you:**

- need **restores from the web UI** today. Backrest is mature; Ladderback's first usable release (M2, 0.2.0) runs scheduled backups, but its snapshot browser and restores come in M3, and until then you restore with the `rustic` CLI;
- use **Windows**;
- use **restic** and want to keep restic maintaining your repositories. Restic must never prune a repository Ladderback manages;
- need **multiple user accounts** or a way to run **arbitrary repository commands** from the UI. Ladderback has neither;
- prefer restic's longer track record over rustic, which is the younger project.

The [technical design](docs/design/README.md#relationship-to-backrest) has the full comparison, with sources.

## Installation

Releases don't come with binaries yet, so you build one from source, on Linux or macOS. One command does it:

```sh
curl -fsSL https://raw.githubusercontent.com/nledford/Ladderback/main/scripts/install.sh | bash
```

It checks the tools the build needs, listed under "By hand" below, and installs those that need no administrator rights (rustup, just and trunk) after asking you once. Those that need them, a C compiler, git and Node.js 20 or newer, it never installs: it names the missing ones with the command that installs them and stops, and it never runs `sudo`. It keeps Ladderback's source in `~/.local/share/ladderback/source`, checks out the newest release, and installs it with `just install` into `~/.cargo/bin`. The command runs the script on `main`, and the script builds the newest release, not `main` ([decision 79](docs/design/decisions/79-a-from-source-install-script.md)). To update, run it again. Options go after `bash -s --`: `--yes` installs the missing tools without asking, `--dir <folder>` keeps the source elsewhere, and what follows `--` goes to `just install`, so `bash -s -- -- --root ~/.local` installs into `~/.local/bin`. `--help` lists them.

To read the script before it runs, download it, read it, then run it:

```sh
curl -fsSL https://raw.githubusercontent.com/nledford/Ladderback/main/scripts/install.sh -o install.sh
less install.sh
bash install.sh
```

### By hand

You need:

- **Rust**, through [rustup](https://rustup.rs). In the checkout, rustup installs the exact Rust version `rust-toolchain.toml` pins, with the `wasm32-unknown-unknown` target, on the first build, so you build with the compiler CI tested ([decision 78](docs/design/decisions/78-the-rust-toolchain-is-pinned.md)). Without rustup, use Rust 1.100.0 or newer, the [MSRV](Cargo.toml), with that target, and take trunk and just from their prebuilt releases.
- **git and a C compiler**: Xcode's Command Line Tools on macOS, `git` and `build-essential` or your distribution's equivalents on Linux.
- **Node.js** 20 or newer, with npm: the web UI's build runs Tailwind's CLI.
- **[trunk](https://trunkrs.dev)**, which builds the web UI, and **[just](https://just.systems)** 1.40 or newer: `cargo +stable install --locked trunk just`, or your package manager's. trunk's newest release can need a newer Rust than the one the checkout pins, hence `+stable`. Building trunk from source needs more than about 2 GB of memory; on a smaller machine, take trunk's [prebuilt release](https://github.com/trunk-rs/trunk/releases) instead. trunk downloads the `wasm-bindgen` and `wasm-opt` the build needs.

On 64-bit ARM Linux with a glibc older than 2.38 (Debian 12, Ubuntu 22.04), the `wasm-bindgen` trunk downloads doesn't run. There, between the `git checkout` and `just install` below, and again after each update, build the version the checkout pins, which trunk then finds on the `PATH`: `cargo install --locked wasm-bindgen-cli --version "$(just _wasm-bindgen-version)"`.

Clone the repository, check out the newest release, and install it:

```sh
git clone https://github.com/nledford/Ladderback.git
cd Ladderback
# The newest release tag, leaving out release candidates (vX.Y.Z-rc.N).
git checkout "$(git tag --list 'v*' --sort=-v:refname | grep -v -- - | head -n 1)" && just install
```

`just install` builds the web UI, then `ladderback` with the UI built in, as the release archives are built (the `dist` profile, with thin LTO), and installs it with `cargo install` into `~/.cargo/bin`. Arguments go to `cargo install`: `just install --root ~/.local` installs into `~/.local/bin`. If `just` answers that there is no recipe `install`, that release predates it; run this instead, which builds with the `release` profile (older releases have no `dist` profile):

```sh
(cd crates/web && trunk build --release --no-default-features) && cargo install --locked --path . --no-default-features
```

### After installing

Start it with `ladderback serve` and open the address it prints, `listening on http://127.0.0.1:9898`; `--listen` picks another address or port (beyond loopback only with `--allowed-host`, below). Its configuration and data live in your platform's config and data folders ([technical design §4.2](docs/design/components.md#42-daemon-agent), "Config"). To update, run the install command again, or by hand `git fetch --tags`, the same `git checkout` line, and `just install`; either replaces the installed binary. Releases up to 0.2.0 track the web UI's stylesheet, which their build can rewrite: coming from one of them by hand, first put it back with `git checkout -- crates/web/tw-out.css`, or git refuses to check out the new release.

To run it in the background instead, from each login on, run `ladderback install-service` as yourself, not under `sudo`. On Linux it writes a systemd user unit and has systemd start it; on macOS it writes a launchd agent, which logs to `~/Library/Logs/ladderback.log`. The service runs `ladderback serve` with no options, so it listens on `http://127.0.0.1:9898`. On Linux a user's services stop when the user logs out, and the command says to run `loginctl enable-linger` to keep it running. After an update, run `ladderback install-service` again: it replaces the service and starts it on the new binary. From a checkout, `just install-restart` does both without interrupting a backup: it runs `just install` (its other arguments go there), waits until the service has no operation running or queued (up to 2 hours; `--wait <minutes>` changes that, `--now` skips the wait; `INSTALL_RESTART_ADDRESS=<host:port>` names the address of a service whose file you edited to listen elsewhere than `127.0.0.1:9898`), restarts it, and checks that it now runs the build it installed. It doesn't restart a service it can't reach, or one with sign-in on, which won't list its operations to it, unless `--now`. `ladderback remove-service` stops the service and removes its file. [Decision 64](docs/design/decisions/64-installing-the-agent-as-a-user-service.md) says what the service file holds.

On macOS, grant the installed `ladderback` Full Disk Access by hand, in System Settings → Privacy & Security, and again after each update: a build from source loses the grant when it is rebuilt ([technical design §12](docs/design/operations.md#12-packaging-and-deployment), [decision 71](docs/design/decisions/71-signing-macos-builds-without-a-developer-id.md)).

Sign-in is off until you set an admin password with `ladderback set-password`, which turns it on the next time Ladderback starts (for the service, run `ladderback install-service` again). With sign-in off, anyone with an account on the machine can use Ladderback, so set a password on a machine others use. `ladderback serve --listen` takes an address beyond loopback only with at least one `--allowed-host`, the name the browser reaches Ladderback by. With sign-in off it then listens too, as Backrest does, and warns at start and on every page that **anyone who can reach that address can run and change backups**: do that only on a network you trust. It serves plain HTTP, so with sign-in on put a reverse proxy with TLS in front of it. [Technical design §10.1](docs/design/security.md#101-web-ui) has the details.

#### A first plan

A plan says what to back up, into which repository, when, and how long to keep its snapshots. A plan backs up to a repository by its name, so make one in this order:

1. **Connect a repository** on the Repositories page (the link at the top of every page): fill in its location and password, test it, give it a name such as `nas`, and connect it, or create a new one there. To have this machine forget old snapshots by your plans' keep policies, turn on "Maintain this repository from this agent", unless another machine already maintains it.
2. **Add a plan to `config.toml`**, in Ladderback's config folder: `~/.config/ladderback` on Linux, `~/Library/Application Support/ladderback` on macOS (`$XDG_CONFIG_HOME/ladderback` on Linux when that is set, and `$LADDERBACK_CONFIG_DIR` on either when that is set). The Repositories page wrote the repository there; add a `[[plans]]` table after it. Ladderback reads the file again by itself, with no restart, and its start page lists the plan with its next run.

This is a whole `config.toml` with one plan, which backs up a home folder every hour and leaves out caches and game libraries; on macOS, change `/home/demo` to `/Users/demo` in `iglobs` too:

```toml
schema = 1

# The repository, as the Repositories page wrote it, with its password moved
# out of this file (see below). Yours may have more lines, such as its
# repository-id.
[repositories.nas]
repository = "rest:https://nas.example:8000/demo-laptop"
password-keyring = "ladderback/nas"

[[plans]]
id = "home"                        # the plan's name: lower-case letters, digits and hyphens
repository = "nas"                 # the name the repository was connected under
sources = ["/home/demo"]           # absolute paths; on macOS, /Users/demo
schedule = { cron = "0 * * * *" }  # on the hour, in this machine's time zone; or every = "6h"
# Skip every folder that holds one of these files, as many caches mark theirs.
exclude-if-present = ["CACHEDIR.TAG", ".nobackup"]
# rustic's globs, matched without regard to case; a leading "!" leaves out what matches.
iglobs = [
  "!/home/demo/.cache",
  "!/home/demo/.local/share/Steam",                # Steam's games
  "!/home/demo/.var/app/com.valvesoftware.Steam",  # Steam from Flathub
  "!/home/demo/Games",                             # Lutris, Heroic and the like
  "!node_modules",
  "!target",
]

# The keep policy: a snapshot an hour for a day, one a day for a week, one a
# week for a month, and one a month for a year.
[plans.retention]
keep-within-hourly = "1 day"
keep-within-daily = "7 days"
keep-within-weekly = "1 month"
keep-within-monthly = "1 year"
```

A run the machine slept through runs when it wakes. Ladderback doesn't forget on a schedule yet: the keep policy takes effect when you press "Forget snapshots" on the Snapshots page, on a repository this machine maintains. Every key a plan takes, among them tags, a time zone, retries and hooks, is in [technical design §6.1](docs/design/scheduling-and-config.md#61-source-of-truth); the filter and `keep-*` keys are rustic's own.

#### Moving a repository's password out of `config.toml`

The Repositories page saves the password you type into `config.toml`, and the page and the log warn that it is there. To move it, keep it in one of these places, then replace the repository's `password = "…"` line with the line that names it; a repository has exactly one:

- **The system keyring**, on a desktop or laptop. Store the password under the service `ladderback` and the repository's name; each command asks for it, so it stays out of your shell's history:

  ```sh
  # macOS: the login keychain
  security add-generic-password -s ladderback -a nas -w
  # Linux: the Secret Service, such as GNOME Keyring
  secret-tool store --label="Ladderback: nas" service ladderback username nas
  ```

  Then write `password-keyring = "ladderback/nas"`, the service and the name.
- **A password file** that only you can read. Ladderback reads its first line, and warns when others can read it:

  ```sh
  touch ~/.config/ladderback/nas.password && chmod 600 ~/.config/ladderback/nas.password
  ```

  Write the password on its first line with an editor, then write `password-file = "/home/demo/.config/ladderback/nas.password"`: the path is absolute, since nothing expands `~`. On macOS, keep the file in the config folder there, and quote the path, since it holds a space:

  ```sh
  touch "$HOME/Library/Application Support/ladderback/nas.password" && chmod 600 "$HOME/Library/Application Support/ladderback/nas.password"
  ```
- **An environment variable**: `password-env = "NAS_PASSWORD"`. The service doesn't get your shell's variables, so this suits `ladderback serve` started by hand, or a container.

The copies of earlier versions of `config.toml` in the `history` folder beside it may still hold the password: delete those that do. [Technical design §10.2](docs/design/security.md#102-repository-secrets) has every source, a password command among them.

## Roadmap

The skeleton (M0), the backup engine (M1a), the single-machine MVP (M1b, released as [0.1.0](https://github.com/nledford/Ladderback/releases/tag/v0.1.0)) and schedules and errors (M2, released as [0.2.0](https://github.com/nledford/Ladderback/releases/tag/v0.2.0), with fixes from running it on real machines in [0.3.0](https://github.com/nledford/Ladderback/releases/tag/v0.3.0)) are in place: backups run on their schedules, with no restore in Ladderback yet. Each milestone is done when its acceptance scenarios pass, including keyboard-only and screen-reader checks for the UI. M2 is the first release worth using on your own machines; M3 adds browsing and restoring snapshots.

- [x] **M0: Skeleton.** Workspace, CI, test tooling (acceptance scenarios, mutation testing, browser tests with accessibility checks), and an embedded web UI shell.
- [x] **M1a: Engine.** Backups through `rustic_core` in an isolated worker process, with live progress, warnings, cancellation and deadlines.
- [x] **M1b: Single-machine MVP.** Connect or create repositories; secrets; plans and hooks in `config.toml`; rustic profile import and export; manual backups with live progress; snapshot list; operation history; manual forget, prune and check.
- [x] **M2: Schedules and errors.** Scheduler with catch-up after sleep, time zones and retries; notifications to webhooks, ntfy and healthchecks; systemd and launchd service install; optional sign-in. **First usable release.**
- [ ] **M3: Browse and restore.** Snapshot browser, restore into a new folder or in place, downloads as tar or zip, repository stats; on macOS, builds from source that keep their Full Disk Access across upgrades.
- [ ] **M4a: Hub, visibility.** Pair machines with a hub; one dashboard for all of them; offline alerts.
- [ ] **M4b: Hub, control and maintenance.** Repository ownership; forget, prune and check managed from the hub; remote run and cancel; opt-in restores from the hub.
- [ ] **M5: Polish.** Run conditions (AC power, metered networks), charts, Docker image, release packages, database rebuild, user docs.

Not planned for v1: Windows, multiple users, and rustic's `copy`, `merge`, `rewrite`, `repair`, `webdav` and `mount` commands (use the CLI).

## Documentation

- [PRODUCT.md](PRODUCT.md): who Ladderback is for, what it does and doesn't do.
- [docs/design/](docs/design/README.md): architecture, safety invariants, data model, milestones and decisions.

## Contributing

Ladderback is built for its author's own machines first and published for other rustic users, without support promises. Issues and discussion about the design are welcome. Development is behaviour-first and test-first; see [docs/design/development.md §13](docs/design/development.md#13-development-method) for the method and [AGENTS.md](AGENTS.md) for the working rules.

**Help test it.** Some setups haven't been tried on a real machine yet: other Linux distributions and desktops, keyrings, ARM, headless servers, Wi-Fi rules, WSL2. Each [issue labeled `needs real-world test`](https://github.com/nledford/Ladderback/issues?q=is%3Aissue%20is%3Aopen%20label%3A%22needs%20real-world%20test%22) lists the steps and what to report; use a scratch repository and a made-up password.

### Developing on Fedora

`just setup` installs the tools and Playwright's Chromium, but not the system libraries Chromium needs to run the `@ui` journeys (`just test-ui`). On Fedora, work in a [toolbox](https://containertoolbx.org/) or [distrobox](https://distrobox.it/) container: it is the only place `dnf` works on an atomic host (Silverblue, Bazzite), and it keeps the host clean on Workstation and Server too.

```sh
toolbox create ladderback && toolbox enter ladderback
sudo dnf install nss atk at-spi2-core libdrm libxkbcommon libXcomposite libXdamage libXfixes libXrandr mesa-libgbm alsa-lib
```

Outside a container, Workstation and Server take the same list with `sudo dnf install`; an atomic host can layer it with `rpm-ostree install`, which needs a reboot. This list is what headless Chromium needs to launch on Fedora 45.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.
