# Changelog

All notable changes to Ladderback are listed here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and Ladderback uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html) as described in
[docs/design/operations.md §12.1](docs/design/operations.md#121-versioning-and-releases).

## [Unreleased]

## [0.3.0](https://github.com/nledford/Ladderback/compare/v0.2.0...v0.3.0) - 2026-11-14

Fixes and polish from running 0.2.0 every day on real machines: Linux servers, a Mac and a Raspberry Pi. Ladderback installs with one command; a repository can be reached at several locations, chosen by the network you are on; the Operations page says which step a run is in and why one failed; refusals name the password or credential that was refused; and the header names the running build. There is still no restore: use the `rustic` CLI, and try a restore before relying on the backups. Restore comes in 0.4.

### Upgrading

- **From a checkout of v0.2.0:** run `git checkout -- crates/web/tw-out.css` before `git pull`. Building 0.2.0 rewrote that file, which is no longer tracked, and git won't remove a changed file.
- **On macOS,** a locally built binary is signed ad hoc, so each install is a new program to macOS: it asks again for the keychain, and for Photos or Full Disk Access if Ladderback had them. Choose Always Allow and Allow, or the next backup reads less than it should (it says so with a warning).

### Breaking

- A repository password the repository refuses now answers `503` with the code `password-not-accepted`, where it answered `422` with `operation-failed`. A client that checks for the old code should check for the new one too ([#520](https://github.com/nledford/Ladderback/pull/520))

### Added

- Install Ladderback with one command ([#419](https://github.com/nledford/Ladderback/pull/419))
- Show which step a running operation is in ([#436](https://github.com/nledford/Ladderback/pull/436))
- Reach a repository at several locations, chosen by network ([#443](https://github.com/nledford/Ladderback/pull/443))
- Tell a wired network link on macOS ([#454](https://github.com/nledford/Ladderback/pull/454))
- Warn that a network rule can't keep a plain-HTTP location's credentials from an impostor ([#470](https://github.com/nledford/Ladderback/pull/470))
- Guide a first plan from the start page and the README ([#487](https://github.com/nledford/Ladderback/pull/487))
- Show why an operation failed on the Operations page ([#496](https://github.com/nledford/Ladderback/pull/496))
- Try a repository's HTTPS and local locations at the same time ([#500](https://github.com/nledford/Ladderback/pull/500))
- Keep a scheduled run refused for its secret as a failed backup ([#501](https://github.com/nledford/Ladderback/pull/501))
- Show the running version in the web UI's header ([#507](https://github.com/nledford/Ladderback/pull/507))
- Allow listening on the network with sign-in off, as Backrest does ([#509](https://github.com/nledford/Ladderback/pull/509))
- Name the build beside the version, with the commit between releases ([#541](https://github.com/nledford/Ladderback/pull/541))

### Fixed

- Build with the pinned Rust whatever RUSTUP_TOOLCHAIN says, and install it explicitly ([#427](https://github.com/nledford/Ladderback/pull/427))
- Give the first job of a newly added repository its cache and its repository ([#435](https://github.com/nledford/Ladderback/pull/435))
- Show a running operation of unknown size as working, not 0% ([#434](https://github.com/nledford/Ladderback/pull/434))
- Make the progress bar focusable and calm for screen readers ([#439](https://github.com/nledford/Ladderback/pull/439))
- Keep keyboard focus on the row when a run's Cancel button goes away ([#441](https://github.com/nledford/Ladderback/pull/441))
- Keep a Linux backup running when the thread that started its worker ends ([#449](https://github.com/nledford/Ladderback/pull/449))
- Warn about and mask a location's userinfo given by a ${file:…} reference ([#467](https://github.com/nledford/Ladderback/pull/467))
- Report a locked keyring as unavailable, not as a missing entry ([#491](https://github.com/nledford/Ladderback/pull/491))
- Say when a REST server refuses the repository's credentials ([#489](https://github.com/nledford/Ladderback/pull/489))
- Warn when an exclude with a slash in the middle can never match ([#511](https://github.com/nledford/Ladderback/pull/511))
- Warn about every plain-HTTP location that sends credentials ([#514](https://github.com/nledford/Ladderback/pull/514))
- Name a refused password or credential as such, and keep a resumed run's refusal ([#520](https://github.com/nledford/Ladderback/pull/520))
- Give the Result column room, keep addresses whole and name tables by the heading ([#523](https://github.com/nledford/Ladderback/pull/523))
- Name warnings on history rows, keep API answers out of caches, and warn of readable secret files and proxied hosts ([#540](https://github.com/nledford/Ladderback/pull/540))
- Reinstalling or removing the service on macOS waits for the old daemon to stop ([#545](https://github.com/nledford/Ladderback/pull/545))

## [0.2.0](https://github.com/nledford/Ladderback/compare/v0.1.1...v0.2.0) - 2026-11-12

The first release worth using on your own machines: plans run on their schedules, with catch-up after sleep, cron in a time zone, a choice of skipping or queuing a run while the last one is going, a limit on how many run at once, and retries of a failed scheduled backup. Notifications go to webhooks, ntfy and healthchecks. Ladderback installs as a systemd user service or launchd agent, and an optional sign-in protects the web UI. There is still no restore: use the `rustic` CLI, and try a restore before relying on the backups. Restore comes in 0.3.

### Breaking

- Run maintenance only on repositories this agent maintains ([#201](https://github.com/nledford/Ladderback/pull/201))
- Refuse a second name for a configured repository ([#213](https://github.com/nledford/Ladderback/pull/213))
- Refuse a relative glob file in a plan or an imported profile ([#242](https://github.com/nledford/Ladderback/pull/242))
- Refuse a location or option over 64 KiB and more than 64 options, and mask in linear time ([#251](https://github.com/nledford/Ladderback/pull/251)): a `config.toml` whose `repository` or `repo-hot`, or one of whose option values, is longer than 64 KiB, or whose `options`, `options-hot` or `options-cold` holds more than 64 options, no longer loads, and the repository routes refuse such settings with `invalid-body`. Shorten the location or value (keep credentials in a secret reference or rclone's own config), and split or drop options.
- Refuse a relative path a repository, a hook or a password command names ([#250](https://github.com/nledford/Ladderback/pull/250))

### Added

- Warn when a repository maintained here holds snapshots from other hosts ([#235](https://github.com/nledford/Ladderback/pull/235))
- Record a saved repository's ID the first time a job opens it ([#269](https://github.com/nledford/Ladderback/pull/269))
- Say when Linux has no keyring service to ask, and warn when an export's keyring command is missing ([#288](https://github.com/nledford/Ladderback/pull/288))
- Work out when a plan runs next, with catch-up, jitter and changes of the clocks ([#294](https://github.com/nledford/Ladderback/pull/294))
- A save keeps what a person wrote in config.toml around its change ([#335](https://github.com/nledford/Ladderback/pull/335))
- Read the machine's power source, network state and wakes for the conditions before a scheduled run ([#301](https://github.com/nledford/Ladderback/pull/301))
- Let a reader pause the Operations page's live updates ([#303](https://github.com/nledford/Ladderback/pull/303))
- Install and remove Ladderback as a user service ([#304](https://github.com/nledford/Ladderback/pull/304))
- Read and write a schedule's interval and time zone, and a plan's overlap policy, in config.toml ([#349](https://github.com/nledford/Ladderback/pull/349))
- Tidy the workers' rustic cache each time the daemon starts ([#295](https://github.com/nledford/Ladderback/pull/295))
- Show each listed snapshot's time as its own clock read it ([#311](https://github.com/nledford/Ladderback/pull/311))
- Run scheduled plans, and queued backups again after a restart ([#390](https://github.com/nledford/Ladderback/pull/390))
- Skip or queue a scheduled backup while its last run is going, and limit how many run at once ([#404](https://github.com/nledford/Ladderback/pull/404))
- Send notifications to webhooks, ntfy and healthchecks ([#405](https://github.com/nledford/Ladderback/pull/405))
- Optional sign-in with an admin password ([#395](https://github.com/nledford/Ladderback/pull/395))
- Retry a failed scheduled backup with a doubling back-off ([#414](https://github.com/nledford/Ladderback/pull/414))

### Fixed

- Keep the packs a cancelled backup finished ([#211](https://github.com/nledford/Ladderback/pull/211))
- Let go of the operation log's file when the store closes ([#210](https://github.com/nledford/Ladderback/pull/210))
- Show what a repository check found ([#225](https://github.com/nledford/Ladderback/pull/225))
- Close the operation log when the daemon stops ([#227](https://github.com/nledford/Ladderback/pull/227))
- Let a backup stopped at its deadline keep its finished packs ([#226](https://github.com/nledford/Ladderback/pull/226))
- Warn about credentials kept in a repository's location ([#230](https://github.com/nledford/Ladderback/pull/230))
- Mask output cut off by a timeout as cut text ([#231](https://github.com/nledford/Ladderback/pull/231))
- Mask credentials written in an rclone connection string ([#239](https://github.com/nledford/Ladderback/pull/239))
- Show what each operation did on the Operations page ([#241](https://github.com/nledford/Ladderback/pull/241))
- Mask cut output whoever cut it, and never quote a cut secret ([#238](https://github.com/nledford/Ladderback/pull/238))
- Create an imported repository from its row on the Repositories page ([#253](https://github.com/nledford/Ladderback/pull/253))
- Never use or quote against a secret command's stdout cut partway through a line ([#254](https://github.com/nledford/Ladderback/pull/254))
- Refuse a job whose secrets come to more than 256 KiB to mask ([#264](https://github.com/nledford/Ladderback/pull/264))
- Start every child process through one spawn gate ([#270](https://github.com/nledford/Ladderback/pull/270))
- Keep rustic's cache between jobs in the agent's cache folder ([#282](https://github.com/nledford/Ladderback/pull/282))
- Keep the Operations table's columns still and its sort button apart from the header's text ([#293](https://github.com/nledford/Ladderback/pull/293))
- Let button labels and other form controls grow with the text size ([#298](https://github.com/nledford/Ladderback/pull/298))
- Name the repository a first job opened when it starts ([#307](https://github.com/nledford/Ladderback/pull/307))
- Refuse a repository location a reference resolves to a relative path ([#313](https://github.com/nledford/Ladderback/pull/313))
- Open the e2e steps' HTTP connections only once they are connected ([#318](https://github.com/nledford/Ladderback/pull/318))
- Let a stopped backup wind down when its events went unread ([#322](https://github.com/nledford/Ladderback/pull/322))
- Refuse a backup start whose secret can't be resolved with the secret's code ([#327](https://github.com/nledford/Ladderback/pull/327))
- Keep repository credentials out of the daemon's log during retention ([#334](https://github.com/nledford/Ladderback/pull/334))
- Mask credentials where rclone quotes them as Go's %q does ([#336](https://github.com/nledford/Ladderback/pull/336))
- Refuse rclone locations and flags that name a relative path ([#323](https://github.com/nledford/Ladderback/pull/323))
- Hold the spawn gate only while rclone starts, not until it serves ([#329](https://github.com/nledford/Ladderback/pull/329))
- Announce a run's start only once the history has it running ([#332](https://github.com/nledford/Ladderback/pull/332))
- Stop an rclone that never serves at a start deadline, rather than leaving it running ([#348](https://github.com/nledford/Ladderback/pull/348))
- Warn when another agent recently wrote to a repository maintained here under this machine's name ([#297](https://github.com/nledford/Ladderback/pull/297))
- Keep a starting child from holding the server's connections on macOS ([#320](https://github.com/nledford/Ladderback/pull/320))
- Say where config.toml is wrong on every page and route that reads it ([#391](https://github.com/nledford/Ladderback/pull/391))
- Show the warnings a run recorded before the Operations page was listening ([#389](https://github.com/nledford/Ladderback/pull/389))
- Stop the Operations page's live updates when you leave it ([#392](https://github.com/nledford/Ladderback/pull/392))
- Mask secrets as rclone quotes them whatever Rust toolchain builds Ladderback ([#407](https://github.com/nledford/Ladderback/pull/407))

### Changed

- Building Ladderback from source needs Rust 1.100.0 or newer, and the checkout pins that version for rustup ([#411](https://github.com/nledford/Ladderback/pull/411))

## [0.1.1](https://github.com/nledford/Ladderback/compare/v0.1.0...v0.1.1) - 2026-11-11

### Fixed

- Apply a plan's source filter and host to its backups ([#197](https://github.com/nledford/Ladderback/pull/197))
- Name each plan's start backup button with its plan ([#203](https://github.com/nledford/Ladderback/pull/203))

### Changed

- Building Ladderback from source needs Rust 1.94 or newer ([#196](https://github.com/nledford/Ladderback/pull/196))

## [0.1.0](https://github.com/nledford/Ladderback/releases/tag/v0.1.0) - 2026-11-11

The first release: Ladderback on a single machine. Connect or initialize repositories, plan backups in `config.toml`, import and export rustic profiles, and run backups by hand with live progress in the web UI. Snapshots, unmanaged and orphaned groups (with adoption) and the operation history are listed. Forget, prune and check run by hand on this machine, the repository's one maintenance owner, and prune never deletes instantly. Secrets come from a command, the keyring, a file or the environment, and are masked wherever output appears, even where a cap cuts it. A second daemon on the same data folder is refused. Scheduling and notifications come in 0.2, restore in 0.3.

### Added

- Run backup, prune, check and identify jobs in a rustic worker (M1a) ([#2](https://github.com/nledford/Ladderback/pull/2))
- Refuse web requests that fail the request checks (§10.1) ([#7](https://github.com/nledford/Ladderback/pull/7))
- Record every operation in a SQLite operation log, and time out stuck workers ([#8](https://github.com/nledford/Ladderback/pull/8))
- Read repository secrets from files, commands and the keyring, and keep them out of everything reported ([#21](https://github.com/nledford/Ladderback/pull/21))
- Render every error code and daemon message through an en-US Fluent catalog ([#20](https://github.com/nledford/Ladderback/pull/20))
- Render the web UI through the catalog, and add the en-XA and ar-XB pseudolocales ([#42](https://github.com/nledford/Ladderback/pull/42))
- Pass a repository's hot and cold fields to rustic ([#41](https://github.com/nledford/Ladderback/pull/41))
- Serialize maintenance operations per repository (I7) ([#52](https://github.com/nledford/Ladderback/pull/52))
- Write numbers and instants per locale in daemon text with ICU4X ([#54](https://github.com/nledford/Ladderback/pull/54))
- The configuration model and its ConfigStore port ([#56](https://github.com/nledford/Ladderback/pull/56))
- Write numbers and instants per locale in the web UI with the browser's Intl ([#63](https://github.com/nledford/Ladderback/pull/63))
- Config.toml with a schema version, migrations and version history ([#64](https://github.com/nledford/Ladderback/pull/64))
- Routes with focus on arrival, and a live-region announcer ([#70](https://github.com/nledford/Ladderback/pull/70))
- Initialise, test and connect a repository ([#71](https://github.com/nledford/Ladderback/pull/71))
- The accessible component set, its gallery and a token contrast gate ([#72](https://github.com/nledford/Ladderback/pull/72))
- Run plan, repository and agent hooks around a backup ([#73](https://github.com/nledford/Ladderback/pull/73))
- List a repository's snapshots ([#77](https://github.com/nledford/Ladderback/pull/77))
- Wire the daemon and stream events to the UI ([#79](https://github.com/nledford/Ladderback/pull/79))
- The operation lifecycle over HTTP ([#81](https://github.com/nledford/Ladderback/pull/81))
- The UI runs and watches backups ([#82](https://github.com/nledford/Ladderback/pull/82))
- Forget, prune and check run manually on the local owner ([#84](https://github.com/nledford/Ladderback/pull/84))
- Adopt orphaned and unmanaged history into a plan ([#85](https://github.com/nledford/Ladderback/pull/85))
- A Repositories page that tests, connects and creates repositories ([#102](https://github.com/nledford/Ladderback/pull/102))
- Import and export rustic profiles ([#105](https://github.com/nledford/Ladderback/pull/105))
- Warn of a secret written in the config, in the log and on the repository's row ([#130](https://github.com/nledford/Ladderback/pull/130))
- Refuse each secret and template failure with a code of its own ([#133](https://github.com/nledford/Ladderback/pull/133))
- --plain and import-profile --json, and a quiet stop when nobody reads the output ([#137](https://github.com/nledford/Ladderback/pull/137))
- Review maintenance before it runs (previews, held selections, history groups) ([#155](https://github.com/nledford/Ladderback/pull/155))
