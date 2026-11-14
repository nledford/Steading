# Install Ladderback

Releases don't come with binaries yet, so you build one from source, on Linux or macOS. The [README](../../README.md#install) has the one command that does it. This guide says what that command does, how to build by hand, how to update, and how to keep Ladderback running in the background.

## The install script

It checks the tools the build needs, listed under [By hand](#by-hand), and installs those that need no administrator rights (rustup, just and trunk) after asking you once. Those that need them, a C compiler, git and Node.js 20 or newer, it never installs: it names the missing ones with the command that installs them and stops, and it never runs `sudo`. It keeps Ladderback's source in `~/.local/share/ladderback/source`, checks out the newest release, and installs it with `just install` into `~/.cargo/bin`. The command runs the script on `main`, and the script builds the newest release, not `main` ([decision 79](../design/decisions/79-a-from-source-install-script.md)). Options go after `bash -s --`: `--yes` installs the missing tools without asking, `--dir <folder>` keeps the source elsewhere, and what follows `--` goes to `just install`, so `bash -s -- -- --root ~/.local` installs into `~/.local/bin`. `--help` lists them.

## By hand

You need:

- **Rust**, through [rustup](https://rustup.rs). In the checkout, rustup installs the exact Rust version `rust-toolchain.toml` pins, with the `wasm32-unknown-unknown` target, on the first build, so you build with the compiler CI tested ([decision 78](../design/decisions/78-the-rust-toolchain-is-pinned.md)). Without rustup, use Rust 1.100.0 or newer, the [MSRV](../../Cargo.toml), with that target, and take trunk and just from their prebuilt releases.
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

## After installing

Start it with `ladderback serve` and open the address it prints, `listening on http://127.0.0.1:9898`; `--listen` picks another address or port (beyond loopback only with `--allowed-host`; [Remote access](remote-access.md) has the details). Its configuration and data live in your platform's config and data folders ([technical design §4.2](../design/components.md#42-daemon-agent), "Config").

## Updating

To update, run the install command again, or by hand `git fetch --tags`, the same `git checkout` line, and `just install`; either replaces the installed binary. Releases up to 0.2.0 track the web UI's stylesheet, which their build can rewrite: coming from one of them by hand, first put it back with `git checkout -- crates/web/tw-out.css`, or git refuses to check out the new release.

## Running as a service

To run it in the background instead, from each login on, run `ladderback install-service` as yourself, not under `sudo`. On Linux it writes a systemd user unit and has systemd start it; on macOS it writes a launchd agent, which logs to `~/Library/Logs/ladderback.log`. The service runs `ladderback serve` with no options, so it listens on `http://127.0.0.1:9898`. On Linux a user's services stop when the user logs out, and the command says to run `loginctl enable-linger` to keep it running.

After an update, run `ladderback install-service` again: it replaces the service and starts it on the new binary. From a checkout, `just install-restart` does both without interrupting a backup: it runs `just install` (its other arguments go there), waits until the service has no operation running or queued (up to 2 hours; `--wait <minutes>` changes that, `--now` skips the wait; `INSTALL_RESTART_ADDRESS=<host:port>` names the address of a service whose file you edited to listen elsewhere than `127.0.0.1:9898`), restarts it, and checks that it now runs the build it installed. It doesn't restart a service it can't reach, or one with sign-in on, which won't list its operations to it, unless `--now`. `ladderback remove-service` stops the service and removes its file. [Decision 64](../design/decisions/64-installing-the-agent-as-a-user-service.md) says what the service file holds.

## macOS: Full Disk Access

On macOS, grant the installed `ladderback` Full Disk Access by hand, in System Settings → Privacy & Security, and again after each update: a build from source loses the grant when it is rebuilt ([technical design §12](../design/operations.md#12-packaging-and-deployment), [decision 71](../design/decisions/71-signing-macos-builds-without-a-developer-id.md)). A reinstall from source can also make macOS ask again for access to the keychain, to Photos and to Full Disk Access; [technical design §12](../design/operations.md#12-packaging-and-deployment) says why, and what a prompt you decline or leave unanswered costs.
