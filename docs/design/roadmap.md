## 14. Milestones

Rough effort assumes one experienced developer working part-time (~10–15 h/week). Everything that is expensive to retrofit after the first real backup (repo identity, snapshot tags, the worker deadline, the config schema version, request checks) is in M1. Each milestone is finished when its feature files pass (§13.2). A [GitHub milestone](https://github.com/nledford/Ladderback/milestones) of the same name collects the issues that must close before it is finished; this section, not the milestone, says what each one delivers.

| # | Milestone | Deliverables | Effort |
|---|---|---|---|
| **M0** | Skeleton | Workspace in the §8.3 layout; CI with fmt, clippy, tests, the `domain` wasm build, banned-crate and banned-call checks; cucumber-rs runners, red-first check and `cargo mutants` job; fake clock and port fakes with contract-suite scaffold; worker protocol with a fake operation; daemon serving an embedded "hello" Leptos app; Node toolchain with the `@ui` runner, Playwright, axe-core and a keyboard-only journey on that app | 2–3 wks |
| **M1a** | Engine | Pinned `rustic_core` revision; worker with `EventProgressBars`, warnings, SIGTERM handling, deadline watchdog and per-worker cache dir; identify, backup, prune and check jobs (prune and check so the kill test covers prune); cancel and deadline in `RunOperation`; a minimal `Redactor` over the job's password and repository URL credentials (full secret sources in M1b); agent ID and repository ID; `ladderback-*` tags and explicit parents with fallback; kill-at-random-points test (local + one OpenDAL backend); `*-tmp-*` cleanup | 2–3 wks |
| **M1b** | Single machine MVP | Repo connect/init/test, including optional hot/cold repos; secret sources, `${…}` substitution and redaction (§10.2); plans in `config.toml` with schema version and migrations; hook levels; profile **import** and export with the golden test; manual backup with live progress in UI; snapshot list; unmanaged and orphaned groups with local adoption; op log and history; forget/prune/check run manually on the local owner; request checks (§10.1); `l10n` crate with the `en-US` catalog, error codes in the API, catalog tests and dev pseudolocales (§8.4); accessible component set (dialog, table, form field, status, progress, toast) with its gallery, the announcer, route focus handling, token contrast test, CLI `NO_COLOR`/`--plain`/`--json`, and the first Orca and VoiceOver pass (§4.5) | 4–5 wks |
| **M2** | Schedules and errors | Scheduler with catch-up, jitter, time zones, wake detection, network wait, persistent queue and retries; warnings vs failures; notifications (webhook, ntfy, healthchecks) with templates and plain-text forms; service install (systemd, launchd); optional auth with an accessible sign-in form (A5, A6); "Pause live updates" | 2–3 wks |
| **M3** | Browse, restore and automation | Snapshot tree browser (accessible tree pattern, §4.5); restore to a new folder by default, in-place from the local UI, step-up confirmation (§10.4); download via `ls` + `dump` (tar/zip); repo stats page; scoped API tokens (§10.1); the data-dir lock for `mcp --headless` (§9; `serve` took it in M1b); MCP server in connect, headless and scratch modes with the read, run and preview tools, and in connect mode the proposal tools (§4.6; decision 89); on macOS, `just install` signing with a self-signed certificate, and the Full Disk Access tests (decision 71) | 2–3 wks |
| **M4a** | Hub: visibility | Pairing with unique hostnames, sync protocol, persistent outbox and replay, aggregated dashboard, agent offline alerts | 3–4 wks |
| **M4b** | Hub: control and maintenance | Repo ownership registry with leases; hub-managed forget/prune/check by plan tag, serialized per repo; adoption from the hub; owner prune guard; hub downloads; remote run/cancel and opt-in new-folder restore (plans view-only from the hub); MCP connect mode against a hub, with an `agent` argument (§4.6) | 3–4 wks |
| **M5** | Polish | Conditions (AC power / metered), charts with text summaries and table views (§4.5), Docker image, release packaging, `db rebuild`, docs | 2–3 wks |

M2 ends with its release, `0.2.0`: after the tag, it runs on real machines before M3 starts, and what that run turns up is fixed or filed first. On macOS that run uses an ad-hoc-signed build from source, with Full Disk Access granted by hand (decision 71). That run found that a repository has one location, so a laptop that reaches its server by a LAN address at home and a public one away takes the slow route at home (#424): decision 80's one pull request (ordered locations, a bounded probe, the same-repository check, and rules by network link and by Wi-Fi name) is that fix and lands before M3 starts; M5's conditions reuse its platform readers.

Setting up backups with an AI agent's help, a person confirming every change (#518, decision 89), lands in this order, each step usable alone: first plan writing as a use case, with the plan editor on the web UI's Plans page; then proposals, with their REST API, the "Pending changes" page and the dry run, which need M3's scoped tokens (for the `propose` scope) and step-up confirmation, and writing to the keyring (#476); then the MCP proposal tools, with the MCP server (#517) in M3. The steps before them are outside the effort estimates above; if they aren't done when the rest of M3 is, the MCP proposal tools follow M3 rather than holding it back. Proposals made on the hub come later, with editing plans from the hub, through `UpdatePlan` (decision 2).

| Milestone | Feature files |
|---|---|
| M1a | `execution/backup.feature` (warnings), `execution/cancel.feature`, `execution/deadline.feature` (`@e2e` with a real worker) |
| M1b | `planning/config.feature`, `planning/hooks.feature`, `repositories/connect.feature` (init/test/connect; `@e2e` against scratch repositories), `repositories/api.feature` (`@e2e`: the routes the page uses, over HTTP), `ui/repositories.feature` (`@ui`: the Repositories page by keyboard alone), `snapshots/list.feature` (`@e2e`; a read, not an operation), `interop/import.feature`, `interop/export.feature`, `cli/output.feature` (`@e2e`: `--plain`, `NO_COLOR`, `import-profile --json`, the statuses, a closed pipe), `maintenance/forget.feature`, `maintenance/adopt.feature`, `maintenance/serialization.feature`, `secrets/substitution.feature`, `secrets/redaction.feature`, `security/request_checks.feature`, `security/body_limits.feature` and `security/response_headers.feature` (`@e2e`), `ui/accessibility.feature` (`@ui`: first backup by keyboard alone; announcements; reflow; form errors) |
| M2 | `execution/schedule.feature` (catch-up, wake, time zones), `execution/retry.feature`, `execution/overlap.feature`, `notifications/rules.feature` |
| M3 | `restore/restore.feature`, `restore/download.feature`, `automation/mcp.feature` |
| M4a | `fleet/pairing.feature`, `fleet/sync_replay.feature` |
| M4b | `maintenance/ownership.feature`, `maintenance/prune_guard.feature`, `restore/hub_restore.feature` |
| 0.2.x follow-up | `repositories/repository_locations.feature` and the scheduler's location skip in `execution/` (decision 80, with the `@e2e` and `@ui` scenarios) |
| M5 | `execution/conditions.feature`, `operations/db_rebuild.feature` |

A **usable personal tool** (M0–M2: scheduled, lock-free backups with notifications on each machine) takes about **10–14 weeks part-time**. **Backrest-like multi-host management** (through M4b) takes about **18–24 weeks**, roughly **4½–6 months part-time**. These ranges include writing scenarios and tests first and building the accessible component set in M1b; M0 is longer than in v0.2 because the test tooling, including the browser toolchain, is built there.

---

## 15. Risks

| Risk | Mitigation |
|---|---|
| `rustic_core` is pre-1.0, so its API changes | Pin one revision for both crates. All usage goes through the `engine` crate. Contract tests run against a temp repo in CI |
| The pinned revision is unreleased code | Move to the first release with `Progress::error`. Until then, upgrade the pin only deliberately, with the kill test and contract tests green, and the engine's start of an `rclone:` server compared with rustic's `rclone.rs` (decision 58) |
| A `rustic_core` upgrade adds or changes option fields | Ladderback-owned option types with a parity test per mapped struct (§6.1) |
| Divergence from rustic CLI profile semantics (list replacement, hook levels, `use-profiles`) | Golden tests: parse the rustic repo's `config/*.toml` examples and compare against expected plans; export round-trip test (§6.2) |
| rustic CLI behaviour Ladderback's export relies on changes (e.g. `[forget] filter-*` starts working, `[snapshot-filter]` semantics change) | Export golden test runs against the rustic CLI version CI pins |
| A long backup collides with prune's `keep-delete` window | Worker deadline, owner prune guard and duration warnings (§7.3); 6-day default `keep-delete` |
| Clock skew between agent and hub weakens the deadline | Clock offset recorded per agent; warning above 5 min; the 1 h margin absorbs smaller skew |
| SQLite failure on unusual filesystems | Daily copy, `db rebuild`, CI on btrfs and Docker volumes (§9) |
| Config upgrades break existing installs | Schema version and migrations from M1b; refuse newer schemas (§6.1) |
| Leptos 0.8 → 0.9 migration | Keep UI components thin and logic in `api`/view-models. Migrate once 0.9 is stable |
| Hard-coded strings creep into the UI and make localization a rewrite | Generated message IDs, the unused-ID lint and Playwright pseudolocale runs from M1b (§8.4) |
| Accessibility erodes as the UI grows | Invariants A1–A6 (§0); one component set with contract tests; role-and-name-only test locators; axe-core in CI; a screen-reader pass in each milestone's definition of done (§4.5) |
| ICU4X data bloats the daemon binary | Measured in M1b: with every locale's data compiled in, ICU4X adds 1.1 MB (4%) to the stripped release daemon, so v1 doesn't trim it. Generate data for the shipped locales only if a later change makes it much larger (§8.4) |
| The bundled time-zone data bloats the daemon binary | Measured at M1b: the copy of the IANA database adds 248 KB (0.7%) to the stripped release daemon and nothing to the web UI's wasm, which looks up no zone. Kept, because without it a host with no `tzdata` reads every zone as UTC (§16 decision 30) |
| macOS TCC permissions | A code signature that stays the same across upgrades (decision 71). Detect denials and link to the settings pane. Document this in setup |
| Memory use on very large repos | Worker isolation. Restore and `dump` need the full index (`to_indexed()`); measure early with a large test repo. Document `--no-cache` and cache dir options |
| Hub is a single point of control | Agents back up without the hub. Agent owner leases last 7 days without renewal, so an outage longer than that pauses agent-run maintenance; backups continue |
| Test-first discipline erodes under schedule pressure | Red-first check, mutation testing on changed code and the definition of done (§13); no coverage gate to game |
| Fakes drift from real adapters, so green tests prove nothing | Port contract tests run against both the fake and the real adapter (§13.3) |

---

## 16. Decisions

Decisions 1–6 recorded 2026-11-08. Decisions 7–12 recorded 2026-11-08 after the adversarial review. Decisions 13–16 recorded 2026-11-08 with the development method and license. Decisions 17–18 recorded 2026-11-08. Decisions 21–22 recorded 2026-11-08 with the robustness rules and the time library. Decision 25 recorded 2026-11-09 with manual maintenance. Decision 26 recorded 2026-11-09 with adoption. Decision 27 recorded 2026-11-10 with profile interop. Decision 29 recorded 2026-11-10 with the config history. Decision 30 recorded 2026-11-10 with the time-zone data.

| # | Question |
|---|---|
| [1](decisions/01-name.md) | Name |
| [2](decisions/02-editing-plans-from-the-hub.md) | Editing plans from the hub |
| [3](decisions/03-secrets-on-the-hub.md) | Secrets on the hub |
| [4](decisions/04-windows.md) | Windows |
| [5](decisions/05-hot-cold-repos.md) | Hot/cold repos |
| [6](decisions/06-restore-safety.md) | Restore safety |
| [7](decisions/07-rustic-core-dependency.md) | `rustic_core` dependency |
| [8](decisions/08-linking-snapshots-to-plans.md) | Linking snapshots to plans |
| [9](decisions/09-long-backup-guard.md) | Long-backup guard |
| [10](decisions/10-hooks-model.md) | Hooks model |
| [11](decisions/11-authentication.md) | Authentication |
| [12](decisions/12-restore-and-download-from-the-hub.md) | Restore and download from the hub |
| [13](decisions/13-code-architecture.md) | Code architecture |
| [14](decisions/14-development-method.md) | Development method |
| [15](decisions/15-option-types.md) | Option types |
| [16](decisions/16-license.md) | License |
| [17](decisions/17-internationalization.md) | Internationalization |
| [18](decisions/18-accessibility.md) | Accessibility |
| [19](decisions/19-versioning.md) | Versioning |
| [20](decisions/20-mcp-server.md) | MCP server |
| [21](decisions/21-robustness-rules.md) | Robustness rules |
| [22](decisions/22-one-time-library.md) | One time library |
| [23](decisions/23-request-checks-json-for-every-mutating.md) | Request checks: JSON for every mutating request |
| [24](decisions/24-project-name.md) | Project name |
| [25](decisions/25-who-decides-a-forgets-deletions.md) | Who decides a forget's deletions |
| [26](decisions/26-what-an-adoption-writes.md) | What an adoption writes |
| [27](decisions/27-profile-interop-what-an-import-refuses.md) | Profile interop: what an import refuses, what an export says |
| [28](decisions/28-repositories-page-and-its-routes.md) | Repositories page and its routes |
| [29](decisions/29-config-history-symlinks-order-and-flushing.md) | Config history: symlinks, order and flushing |
| [30](decisions/30-time-zone-data.md) | Time-zone data |
| [31](decisions/31-a-secret-a-url-parser-splits.md) | A secret a URL parser splits |
| [32](decisions/32-worker-messages-that-outgrow-the-cap.md) | Worker messages that outgrow the cap |
| [33](decisions/33-body-limits-and-security-headers.md) | Body limits and security headers |
| [34](decisions/34-a-hooks-output-after-the-hook.md) | A hook's output after the hook has ended |
| [35](decisions/35-the-zone-of-an-instant-with.md) | The zone of an instant with no zone |
| [36](decisions/36-repository-jobs-have-a-deadline.md) | Repository jobs have a deadline |
| [37](decisions/37-a-secret-written-in-the-config.md) | A secret written in the config |
| [38](decisions/38-the-command-lines-output.md) | The command line's output |
| [39](decisions/39-live-rows-are-stable.md) | Live rows are stable |
| [40](decisions/40-crash-recovery-and-the-data-dir.md) | Crash recovery and the data-dir lock |
| [41](decisions/41-the-end-of-an-operation-is.md) | The end of an operation is announced from the run |
| [42](decisions/42-a-maintenance-start-the-client-drops.md) | A maintenance start the client drops, and the writes the log refuses |
| [43](decisions/43-dependency-policy.md) | Dependency policy |
| [44](decisions/44-reviewing-maintenance-before-it-runs.md) | Reviewing maintenance before it runs |
| [45](decisions/45-masking-a-text-cut-at-a.md) | Masking a text cut at a cap |
| [46](decisions/46-the-toolings-language.md) | The tooling's language |
| [47](decisions/47-msrv-1-94.md) | MSRV 1.94 (superseded by decision 78) |
| [48](decisions/48-who-maintains-a-saved-repository.md) | Who maintains a saved repository |
| [49](decisions/49-one-name-per-repository.md) | One name per repository |
| [50](decisions/50-a-cancelled-backup-keeps-its-packs-indexed.md) | A cancelled backup keeps its packs indexed |
| [51](decisions/51-what-a-checks-findings-are.md) | What a check's findings are |
| [52](decisions/52-the-operation-log-keeps-the-repositorys-alias.md) | The operation log keeps the repository's alias |
| [53](decisions/53-creating-an-imported-repository-from-its-row.md) | Creating an imported repository from its row |
| [54](decisions/54-early-release-builds.md) | Early release builds |
| [55](decisions/55-a-backup-stopped-at-its-deadline-winds-down.md) | A backup stopped at its deadline winds down |
| [56](decisions/56-the-calendar-a-retention-rule-counts-on.md) | The calendar a retention rule counts on |
| [57](decisions/57-recording-the-id-of-a-repository-a-job-opened.md) | Recording the ID of a repository a job opened |
| [58](decisions/58-one-child-starts-at-a-time.md) | One child starts at a time |
| [59](decisions/59-where-the-workers-keep-rustics-cache.md) | Where the workers keep rustic's cache |
| [60](decisions/60-how-a-plan-s-next-run-is-worked-out.md) | How a plan's next run is worked out |
| [61](decisions/61-tidying-the-workers-cache-at-start.md) | Tidying the workers' cache at start |
| [62](decisions/62-another-agents-recent-snapshots-come-from-another-machine.md) | Another agent's recent snapshots come from another machine |
| [63](decisions/63-the-ports-for-the-conditions-before-a-run.md) | The ports for the conditions before a run |
| [64](decisions/64-installing-the-agent-as-a-user-service.md) | Installing the agent as a user service |
| [65](decisions/65-pausing-live-updates.md) | Pausing live updates |
| [66](decisions/66-the-server-accepts-through-the-spawn-gate.md) | The server accepts through the spawn gate |
| [68](decisions/68-the-start-of-an-operation-is-announced-from-the-run.md) | The start of an operation is announced from the run |
| [69](decisions/69-a-save-edits-the-file-a-person-wrote.md) | A save edits the file a person wrote |
| [70](decisions/70-sign-in.md) | Sign-in |
| [71](decisions/71-signing-macos-builds-without-a-developer-id.md) | Signing macOS builds without a Developer ID |
| [72](decisions/72-a-schedule-s-interval-and-time-zone-and-a-plan-s-overlap-policy.md) | A schedule's interval and time zone, and a plan's overlap policy |
| [73](decisions/73-the-persistent-queue-and-the-scheduler-loop.md) | The persistent queue and the scheduler loop |
| [74](decisions/74-how-notifications-are-configured-and-delivered.md) | How notifications are configured and delivered |
| [75](decisions/75-the-overlap-policy-and-the-concurrency-limit.md) | The overlap policy and the concurrency limit |
| [76](decisions/76-retries-of-a-failed-backup.md) | Retries of a failed backup |
| [77](decisions/77-which-characters-go-prints.md) | Which characters Go prints |
| [78](decisions/78-the-rust-toolchain-is-pinned.md) | The Rust toolchain is pinned, and the MSRV is the pin |
| [79](decisions/79-a-from-source-install-script.md) | A from-source install script |
| [80](decisions/80-repository-locations.md) | A repository reached at several locations |
| [81](decisions/81-identify-a-repository-before-its-first-job.md) | A job of an entry that records no repository ID runs after an identify |
| [82](decisions/82-the-step-a-running-operation-is-in.md) | How a page learns which step a running operation is in |
| [83](decisions/83-a-progress-bar-a-screen-reader-can-reach.md) | A progress bar a screen reader can reach, and that never chatters |
| [84](decisions/84-view-blocks-are-formatted-by-leptosfmt.md) | Formatting the inside of `view!` blocks |
| [85](decisions/85-a-server-s-refusal-of-the-credentials.md) | How a REST server's refusal of the credentials is told |
| [86](decisions/86-trying-a-repositorys-locations-at-the-same-time.md) | Trying a repository's locations at the same time |
| [87](decisions/87-listening-on-the-network-with-sign-in-off.md) | Listening on the network with sign-in off |
| [88](decisions/88-hub-and-agent-version-compatibility.md) | How far hub and agent versions may differ |
| [89](decisions/89-agent-proposed-setup.md) | How an AI agent proposes a backup setup for a person to confirm |
| [90](decisions/90-the-build-a-program-names.md) | The build a program names beside its version |
| [91](decisions/91-the-operations-a-start-ends-are-announced.md) | The operations a start ends are announced |

## 17. Open questions

To be answered by experiments in M1a to M3 rather than by decision:

1. Does a SIGKILL during an S3, B2 or SFTP upload leave a partial object under a final pack name? The kill test (§4.1) must cover one OpenDAL backend.
2. Does a Full Disk Access grant on the launchd-started daemon carry over to the worker child process, or does the worker need its own grant?
3. How long does prune take on a large repo? That sets `prune_margin` (§7.3).

To be decided in M4a, when hub and agents first talk:

4. How far may hub and agent versions differ? **Decided** in [decision 88](decisions/88-hub-and-agent-version-compatibility.md).
