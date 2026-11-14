//! The daemon's wiring (technical design §3.2, §4.2): the one place beside
//! `main` that knows every adapter. `serve` opens the store and the config,
//! builds the engine, secrets and the event bus, and hands the result to the
//! HTTP router. Adapters stay constructor-injected; nothing else sees them.
//!
//! A startup that cannot happen is a typed exit: [`Refused`] carries the
//! catalog message and its arguments, and `serve` prints it and fails — never
//! a silent empty configuration or a panic.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use api::{ErrorCode, MsgArg, UiEvent, WarningCode};
use app::announce::failure;
use app::event_bus::BroadcastEventBus;
use app::execution::{Finished, RunOperation};
use app::history::{GetOperation, ListOperations};
use app::interop::{ExportError, ImportError};
use app::locations::{ChooseLocation, LocationReport};
use domain::execution::locations::{LocationOutcome, LocationsRecord};

use app::conditions::{NameWatch, NetworkWait};
use app::notifications::{Notify, Report};
use app::ports::DELIVERY_TIMEOUT;
use app::ports::Retention;
use app::ports::SecretResolver;
use app::ports::{
    AdminPasswordError, BackupJob, Clock, ConfigError, ConfigStore, Engine, EngineError,
    EngineEvent, EngineResult, EventBus, ExportRefusal, HookRunner, HookSetup, Job, Network,
    NetworkLink, NormalizeSources, OperationRecord, OperationStore, PowerSupply, ProfileError,
    ProfileNotice, Random, RunningJob, ScheduleStore, SnapshotSummary,
};
use app::progress::LiveProgress;
use app::queue::{Place, RunQueue};
use app::recovery::RecoverOperations;
use app::repositories::{
    ConfigWrites, ConnectRepository, CreateOffers, InitRepository, RecordOpened, Recording,
    RepositoryError, SavedEntryError, TestRepository,
};
use app::schedule::{PlanRuns, ScheduleReport, Scheduler};
use app::secrets::{Cause, Field, ResolveError, ResolveRepository};
use app::snapshots::{ListSnapshots, SnapshotError};
use async_trait::async_trait;
use config_adapter::FileConfigStore;
use domain::RepositoryAlias;
use domain::execution::FailureReason;
use domain::execution::deadline::backup_budget;
use domain::execution::plan_tags::PlanTags;
use domain::execution::{OperationKind, OperationStatus, Target, Trigger, WarningKind};
use domain::planning::{
    Location, LocationRule, Maintenance, NameFilter, NotRecorded, Plan, RepositoryConfig,
    RepositoryField, RepositorySettings, RuleNote,
};
use domain::secrets::{
    FileRead, KeyringEntry, MaskedSecrets, Part, Redactor, RepositorySecrets, SecretRef,
    SecretValue, SecretWarning, Template,
};
use domain::{AgentId, OperationId, PlanId, RepositoryId};
use engine::{Recorded, RemovedKind, WorkerEngine};
use futures::stream::StreamExt;
use hooks::ProcessHookRunner;
use http_adapter::{
    AppState, Catalog, CatalogError, MaintenanceRefusal, Operations, Repositories,
    RepositoriesError, SecretRefusal, SignIn, SnapshotsError, StartError,
};
use l10n::{MessageId, msg};
use notify_adapter::HttpNotifier;
use platform::SystemClock;
use platform::admin_password::FileAdminPassword;
use platform::data_dir_lock::{DataDirLock, LOCK_FILE, LockError};
use platform::{agent_id, dirs};
use secrets::SystemSecrets;
use store::SqliteStore;
use uuid::Uuid;

mod maintenance;

/// Why the daemon cannot serve, as a message for the person who started it.
pub(crate) struct Refused {
    pub(crate) message: MessageId,
    pub(crate) args: Vec<(&'static str, MsgArg)>,
}

/// A refusal with no arguments.
fn plain(message: MessageId) -> Refused {
    Refused {
        message,
        args: Vec::new(),
    }
}

/// A text argument, the kind identifiers, paths and versions are.
fn text(value: impl std::fmt::Display) -> MsgArg {
    MsgArg::Text(value.to_string())
}

/// Says in the log, once at load, each secret the configuration holds as
/// text (§10.2), in every location, and each location that sends its
/// credentials where whoever answers at its address can read them (§10.6):
/// the repository and the setting or the cause, in the catalog's words,
/// never the secret (I4) nor the address.
fn log_inline_secrets(config: &domain::planning::AgentConfig) {
    let localizer = l10n::Localizer::new(&l10n::PosixEnv::from_process().preferences());
    for (alias, repository) in config.repositories() {
        let secrets = inline_secret_warnings(repository)
            .into_iter()
            .map(|(_, code, args)| (code, args));
        let exposed = repository.location_warnings().into_iter().map(|warning| {
            (
                WarningCode::LocationPlainHttpCredentials,
                exposed_args(repository, warning),
            )
        });
        for (code, args) in secrets.chain(exposed) {
            let message = localizer.warning(code, &args, None);
            log::warn!("repository {}: {message}", alias.as_str());
        }
    }
}

/// How long the daemon waits on one look at a plan's sources for its globs
/// (#510): a source on a mount that stopped answering is then read as
/// written, and holds nothing, so neither the daemon's start nor the plans
/// list waits on it.
const SOURCES_LOOK_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// `S`, with each look at the disk given up after its limit, as one that
/// can't be resolved.
struct WithinLimit<S>(S, std::time::Duration);

#[async_trait]
impl<S: NormalizeSources> NormalizeSources for WithinLimit<S> {
    async fn normalized(
        &self,
        sources: &[domain::planning::Source],
    ) -> Result<Vec<domain::planning::Source>, app::ports::SourcesError> {
        tokio::time::timeout(self.1, self.0.normalized(sources))
            .await
            .unwrap_or_else(|_| {
                Err(app::ports::SourcesError::Unreadable {
                    detail: format!("no answer within {:?}", self.1),
                })
            })
    }
}

/// Says in the log, once at load, each glob of a plan that can never match
/// (#510): the plan, the glob and the whole path to write instead, read
/// against the plan's sources as the backup walks them.
async fn log_never_matching_globs(
    config: &domain::planning::AgentConfig,
    sources: &dyn NormalizeSources,
) {
    let localizer = l10n::Localizer::new(&l10n::PosixEnv::from_process().preferences());
    for plan in config.plans() {
        for (code, args) in never_matching_glob_warnings(plan, sources).await {
            let message = localizer.warning(code, &args, None);
            log::warn!("{message}");
        }
    }
}

/// A `glob-never-matches` warning, with its arguments, for each glob of
/// `plan` that can never match, read against its sources as `sources` walks
/// them (#510).
async fn never_matching_glob_warnings(
    plan: &Plan,
    sources: &dyn NormalizeSources,
) -> Vec<(WarningCode, Vec<(&'static str, MsgArg)>)> {
    app::globs::never_matching_globs(plan, sources)
        .await
        .into_iter()
        .map(|glob| glob_never_matches(plan.id.as_str(), &glob.glob, glob.absolute.as_deref()))
        .collect()
}

/// The `glob-never-matches` warning for `glob` of the plan `plan`, with
/// `absolute`, the whole path to write instead, left out when there is none.
fn glob_never_matches(
    plan: &str,
    glob: &str,
    absolute: Option<&str>,
) -> (WarningCode, Vec<(&'static str, MsgArg)>) {
    let mut args = vec![("plan", text(plan)), ("glob", text(glob))];
    args.extend(absolute.map(|absolute| ("absolute", text(absolute))));
    (WarningCode::GlobNeverMatches, args)
}

/// A warning's arguments, by name.
type WarningArgs = Vec<(&'static str, MsgArg)>;

/// The warning for each secret `repository` holds as text, with the
/// position, from 1, of the location that holds it.
fn inline_secret_warnings(repository: &RepositoryConfig) -> Vec<(u8, WarningCode, WarningArgs)> {
    let settings = repository.location_settings();
    repository
        .inline_secrets()
        .into_iter()
        .filter_map(|(position, secret)| {
            let at = settings.get(usize::from(position).checked_sub(1)?)?;
            let (code, args) = inline_secret_warning(at, &secret, position);
            Some((position, code, args))
        })
        .collect()
}

/// The arguments of the `location-plain-http-credentials` warning about
/// `warning`, a location of `repository`: by position, cause and whether it
/// has a rule, and whether the repository has one location or several, for
/// the text of a repository of one names no location (#473).
fn exposed_args(
    repository: &RepositoryConfig,
    warning: domain::planning::LocationWarning,
) -> Vec<(&'static str, MsgArg)> {
    vec![
        ("position", text(warning.position)),
        ("cause", text(warning.cause.code())),
        ("rule", text(warning.rule_code())),
        ("locations", text(repository.locations_code())),
    ]
}

/// The warning for a secret the configuration holds as text, with its
/// arguments: one in a location is moved differently from a setting of its
/// own, and one in an rclone location differently again (§10.2). `settings`
/// open the location at `position`, from 1; a further one is named by it.
fn inline_secret_warning(
    settings: &RepositorySettings,
    secret: &domain::secrets::InlineSecret,
    position: u8,
) -> (WarningCode, Vec<(&'static str, MsgArg)>) {
    let field = ("field", MsgArg::Text(secret.field()));
    let (code, mut args) = match settings.location_of(secret) {
        Some(location) => (
            WarningCode::LocationSecret,
            vec![
                field,
                (
                    "kind",
                    location_kind(domain::secrets::rclone_location(location)),
                ),
            ],
        ),
        None => (WarningCode::ConfigInlineSecret, vec![field]),
    };
    if position > 1 {
        args.push(("position", text(position)));
    }
    (code, args)
}

/// The `kind` argument of a `location-secret` warning.
fn location_kind(rclone: bool) -> MsgArg {
    text(if rclone { "rclone" } else { "url" })
}

/// What `serve` runs: the router's state, the operation log it writes, the
/// hold on the data folder that keeps every other process off it, and the
/// work it does with no request: the queue left from before it started and
/// the scheduler. The caller closes the log once it has stopped serving, and
/// keeps the lock until the daemon is gone (§9).
pub(crate) struct Daemon {
    pub(crate) state: AppState,
    pub(crate) log: OperationLog,
    pub(crate) lock: DataDirLock,
    pub(crate) background: Background,
}

/// What the conditions before a scheduled run ask the machine (§5): what
/// powers it, whether it is online, and the network link the rules of a
/// repository's locations are evaluated against (decision 80), its reads of
/// the Wi-Fi network's name watched for the Repositories page.
pub(crate) struct RunConditions {
    pub(crate) power: Arc<dyn PowerSupply>,
    pub(crate) network: Arc<dyn Network>,
    pub(crate) link: Arc<NameWatch>,
}

impl RunConditions {
    /// This machine's. The first name it can't read is said once in the
    /// log, never with what was read (decision 80).
    fn system() -> Self {
        Self {
            power: platform::power::system_power(),
            network: platform::network::system_network(),
            link: Arc::new(NameWatch::new(
                platform::network_link::system_link(),
                || {
                    log::warn!(
                        "can't read the name of the Wi-Fi network this machine is on: a location's rule that names Wi-Fi networks doesn't hold here, so that location is skipped; a rule of `wired` or `wifi` alone needs no name"
                    );
                },
            )),
        }
    }
}

/// The daemon's work that no request starts (§5): the backups the operation
/// log still holds as queued from before the start, then the scheduler.
pub(crate) struct Background {
    runner: Arc<PlanRunner>,
    /// The queued operations recovery found, oldest first.
    queued: Vec<OperationRecord>,
    config: Arc<dyn ConfigStore>,
    schedules: Arc<dyn ScheduleStore>,
    store: Arc<dyn OperationStore>,
    conditions: RunConditions,
    /// How each backup ended, which the scheduler reads to retry a failed
    /// one (§5).
    ends: app::retry::RunEnds,
}

impl Background {
    /// Runs the queue's leftovers, oldest first, then the scheduler, with
    /// `random` spreading its runs over their jitter, until the future is
    /// dropped.
    pub(crate) async fn run(self, random: Arc<dyn Random>) {
        let queue = Arc::clone(&self.runner.queue);
        let events = Arc::clone(&self.runner.events);
        let runs: Arc<dyn PlanRuns> = self.runner;
        resume_queued(
            self.queued,
            self.config.as_ref(),
            self.store,
            Arc::clone(&events),
            runs.as_ref(),
        )
        .await;
        Scheduler {
            clock: Arc::new(SystemClock),
            random,
            schedules: self.schedules,
            config: self.config,
            power: self.conditions.power,
            network: self.conditions.network,
            link: self.conditions.link,
            runs,
            queue,
            zone: domain::TimeZone::system(),
            ends: self.ends,
            events,
        }
        .run()
        .await;
    }
}

/// Hands each of `queued` to the queue, oldest first, against the
/// configuration read once now. A configuration that can't be read builds
/// no job, so each ends as failed with `daemon-stopped`, and the log says
/// why (decision 73): left queued, it would show as queued with nothing to
/// run it until the next start. Each that ends is announced on `events`.
async fn resume_queued(
    queued: Vec<OperationRecord>,
    config: &dyn ConfigStore,
    store: Arc<dyn OperationStore>,
    events: Arc<dyn EventBus>,
    runs: &dyn PlanRuns,
) {
    let config = match config.load().await {
        Ok(config) => Some(config),
        Err(error) => {
            log::warn!(
                "the configuration can't be read, so any operation queued before the start ends as failed: {error}"
            );
            None
        }
    };
    RecoverOperations::new(store)
        .with_events(events)
        .resume(queued, config.as_ref(), runs, SystemClock.now())
        .await;
}

/// How long the daemon waits for the operation log to close once it stops
/// (§9): as long as a run waits for one write of its own
/// ([`app::log_write::SAVE_BOUND`]), which is past the store's own bound on
/// a call (about 15 s: 10 s for a connection, 5 s for the write lock). A
/// close waits for the calls in flight, so a busy store still finishes
/// them; one that hasn't by then is stalled (a disk that stopped
/// answering), and the daemon stops without it rather than hold up the
/// service manager, which would kill it anyway.
const LOG_CLOSE_BOUND: std::time::Duration =
    std::time::Duration::from_secs(app::log_write::SAVE_BOUND.as_secs().unsigned_abs());

/// The operation log the daemon writes, which it closes before it lets go
/// of the data folder.
pub(crate) struct OperationLog {
    store: Arc<SqliteStore>,
    database: std::path::PathBuf,
}

impl OperationLog {
    /// Closes the log once the calls in flight have ended, so no connection
    /// to it, and none of its `-wal` and `-shm` files, outlives the daemon.
    /// It waits [`LOG_CLOSE_BOUND`] at most, and says in the log when it
    /// gave up; what a call cut off then was still writing is ended at the
    /// next start, as after a crash.
    pub(crate) async fn close(&self) {
        close_within(self.store.close(), &self.database, LOG_CLOSE_BOUND).await;
    }
}

/// Whether sign-in is on (§10.1, decision 70), decided once as the daemon
/// starts: on when the data folder keeps an admin password's hash, off when
/// it keeps none. A hash file that can't be read, or isn't a hash this build
/// checks, is a refusal to start, never sign-in off. Sessions draw their
/// tokens from `random`.
pub(crate) fn sign_in(random: Arc<dyn Random>) -> Result<SignIn, Refused> {
    let data_dir = dirs::data_dir().ok_or_else(|| plain(msg::CLI_NO_DATA_DIR))?;
    let kept = Arc::new(FileAdminPassword::in_data_dir(&data_dir));
    match kept.is_set() {
        Ok(false) => Ok(SignIn::Off),
        Ok(true) => Ok(SignIn::On(Arc::new(app::access::Access::new(
            kept,
            random,
            Arc::new(SystemClock),
        )))),
        Err(error) => {
            // `is_set` never answers `NotSet`: no file is sign-in off.
            let detail = match error {
                AdminPasswordError::Unreadable(said) | AdminPasswordError::Unwritable(said) => said,
                AdminPasswordError::NotSet => String::new(),
            };
            Err(Refused {
                message: msg::CLI_ADMIN_PASSWORD_UNREADABLE,
                args: vec![
                    (
                        "path",
                        text(data_dir.join(platform::admin_password::FILE).display()),
                    ),
                    ("detail", text(detail)),
                ],
            })
        }
    }
}

/// What the daemon's router runs against (§4.2), with `sign_in` deciding who
/// may use it, or why it cannot, and `exposure` who can reach it, which the
/// session route tells the page.
pub(crate) async fn daemon(
    sign_in: SignIn,
    exposure: http_adapter::Exposure,
) -> Result<Daemon, Refused> {
    let mut daemon = daemon_at(dirs::config_dir(), dirs::data_dir(), dirs::cache_dir()).await?;
    daemon.state.sign_in = sign_in;
    daemon.state.exposure = exposure;
    Ok(daemon)
}

/// [`daemon`] with the folders supplied, for tests. The data folder is locked
/// before anything else is read or opened (§9), and what an earlier run left
/// unfinished is ended before the state is handed over, so nothing serves or
/// listens until both are done. The cache folder is no reason to refuse:
/// without one, the workers run without a cache.
async fn daemon_at(
    config_dir: Option<std::path::PathBuf>,
    data_dir: Option<std::path::PathBuf>,
    cache_dir: Option<std::path::PathBuf>,
) -> Result<Daemon, Refused> {
    let config_dir = config_dir.ok_or_else(|| plain(msg::CLI_NO_CONFIG_DIR))?;
    let data_dir = data_dir.ok_or_else(|| plain(msg::CLI_NO_DATA_DIR))?;
    // The data dir must be there before it is locked and the store opens in it.
    std::fs::create_dir_all(&data_dir)
        .map_err(|detail| store_refused(&data_dir, &detail.to_string()))?;
    // One daemon per data folder, before anything is read or opened: a second
    // process must not migrate the configuration the first is running on, read
    // what it runs, or end it as the leftover of a crash (§9).
    let lock = DataDirLock::acquire(&data_dir).map_err(|error| lock_refused(&data_dir, error))?;
    // The configuration next: a file this build cannot read is a refusal,
    // whatever the rest of the machine says.
    let config: Arc<dyn ConfigStore> = Arc::new(FileConfigStore::new(
        config_dir.join("config.toml"),
        Arc::new(SystemClock),
    ));
    let loaded = config.load().await.map_err(config_refused)?;
    log_inline_secrets(&loaded);
    log_never_matching_globs(
        &loaded,
        &WithinLimit(engine::RusticSources, SOURCES_LOOK_LIMIT),
    )
    .await;
    let recorded = recorded_repositories(&loaded);
    let database = data_dir.join("ladderback.db");
    let log = OperationLog {
        store: Arc::new(
            SqliteStore::open(&database)
                .await
                .map_err(|detail| store_refused(&database, &detail.to_string()))?,
        ),
        database,
    };
    // Every refusal from here on closes the log first: the refused daemon
    // leaves it as tidy as one that served and stopped.
    match state_over(
        config,
        &data_dir,
        cache_dir,
        recorded,
        &log,
        RunConditions::system(),
    )
    .await
    {
        Ok((state, background)) => Ok(Daemon {
            state,
            log,
            lock,
            background,
        }),
        Err(refused) => {
            log.close().await;
            Err(refused)
        }
    }
}

/// The router's state over the operation log `log`, and the work the daemon
/// does with no request, once it has ended what an earlier run left
/// running, or why the daemon cannot serve. The workers' cache folder is
/// tidied first, keeping the caches of the repositories `recorded` names.
async fn state_over(
    config: Arc<dyn ConfigStore>,
    data_dir: &std::path::Path,
    cache_dir: Option<std::path::PathBuf>,
    recorded: Recorded,
    log: &OperationLog,
    conditions: RunConditions,
) -> Result<(AppState, Background), Refused> {
    let store: Arc<dyn OperationStore> = Arc::<SqliteStore>::clone(&log.store);
    let schedules: Arc<dyn ScheduleStore> = Arc::<SqliteStore>::clone(&log.store);
    // The bus and the notification rules come first, so that recovery can
    // announce each operation it ends (decision NN).
    let bus: Arc<dyn EventBus> = Arc::new(BroadcastEventBus::new());
    notify_over(bus.as_ref(), Arc::clone(&config), Arc::clone(&store)).await;
    // Nothing has run yet, so an operation the log still shows as running was
    // left by a process that stopped. It ends as failed now, which also lets
    // go of the repository a running maintenance operation held (I7). A log
    // that cannot be changed is a refusal: serving over it would keep that
    // repository held for good. Queued operations stay queued (decision 73).
    let recovered = RecoverOperations::new(Arc::clone(&store))
        .with_events(Arc::clone(&bus))
        .recover(SystemClock.now())
        .await
        .map_err(|error| recovery_refused(&log.database, &error))?;
    let ended = recovered.ended.len();
    if ended > 0 {
        log::warn!(
            "ended {ended} operation(s) the operation log still showed as running: the daemon that ran them stopped before they finished"
        );
    }
    // By its code alone: the store's account of a row repeats what the row
    // holds, which nobody has checked for a value (I4).
    for error in &recovered.unreadable {
        log::warn!(
            "an operation in the operation log can't be read: a queued one stays queued, and a running one has ended without being named ({})",
            error.code()
        );
    }
    let agent_id = agent_id::load_or_create(data_dir)
        .map_err(|detail| store_refused(data_dir, &detail.to_string()))?;
    // Workers are this program: `ladderback worker` (§4.1).
    let program = std::env::current_exe().map_err(|detail| Refused {
        message: msg::CLI_CANNOT_FIND_OWN_PATH,
        args: vec![("detail", text(detail))],
    })?;
    let mut workers = WorkerEngine::new(program, ["worker"]);
    if let Some(dir) = workers_cache(cache_dir) {
        tidy_workers_cache(&dir, recorded).await;
        workers = workers.with_cache_root(dir);
    }
    // One lock for every write of the configuration the daemon makes: a
    // connection, a creation and the record of an opened repository's ID
    // (§6.1, decision 57).
    let writes = Arc::new(ConfigWrites::default());
    let resolve = Arc::new(
        ResolveRepository::new(Arc::new(SystemSecrets::default()))
            .telling(Arc::new(log_secret_warning)),
    );
    // The choice of a location comes first (decision 80): a job opens the
    // location it chose, and the record of an opened repository's ID
    // (decision 81) sees that job, never a probe.
    let locations = Arc::new(ChooseLocation::new(
        Arc::new(RecordOpened::new(
            Arc::new(EventEngine {
                inner: Arc::new(workers),
                bus: Arc::clone(&bus),
            }),
            Arc::clone(&config),
            Arc::clone(&writes),
            Arc::new(log_recording),
        )),
        Arc::new(SystemClock),
        Arc::<NameWatch>::clone(&conditions.link),
        resolve.as_ref().clone(),
        Arc::new(log_location),
    ));
    let engine: Arc<dyn Engine> = Arc::<ChooseLocation>::clone(&locations);
    let (run_ends, ends) = app::retry::run_ends();
    let runner = Arc::new(PlanRunner {
        config: Arc::clone(&config),
        resolve: Arc::clone(&resolve),
        agent_id,
        engine: Arc::clone(&engine),
        locations: Some(locations),
        store: Arc::clone(&store),
        retention: Arc::new(engine::RusticRetention),
        registry: Arc::new(CancelRegistry::new()),
        hooks: Arc::new(ProcessHookRunner::new()),
        events: Arc::clone(&bus),
        queue: Arc::new(RunQueue::new()),
        ends: run_ends,
    });
    let operations: Arc<dyn Operations> = Arc::<PlanRunner>::clone(&runner);
    let repositories: Arc<dyn Repositories> = Arc::new(RepositoryService::new(
        Arc::new(SystemClock),
        Arc::clone(&engine),
        Arc::<NameWatch>::clone(&conditions.link),
        resolve.as_ref().clone(),
        Arc::clone(&config),
        Arc::clone(&store),
        writes,
    ));
    let catalog: Arc<dyn Catalog> = Arc::new(ConfigCatalog {
        config: Arc::clone(&config),
        engine,
        operations: Arc::clone(&store),
        schedules: Arc::clone(&schedules),
        link: Arc::clone(&conditions.link),
        sources: Arc::new(WithinLimit(engine::RusticSources, SOURCES_LOOK_LIMIT)),
    });
    let background = Background {
        runner,
        queued: recovered.queued,
        config,
        schedules,
        store,
        conditions,
        ends,
    };
    Ok((
        AppState {
            events: bus,
            operations,
            catalog,
            repositories,
            sign_in: SignIn::Off,
            exposure: http_adapter::Exposure::Loopback,
            build: crate::build_info::running(),
        },
        background,
    ))
}

/// Tells the channels of the agent's notification rules of the news on `bus`
/// for as long as the daemon runs (§11). A notification that wasn't delivered
/// is said in the log by its warning code, never with a secret (I4), and an
/// event the configuration couldn't be read for by the configuration's error
/// code. A client that can't be built is said once, and nothing is sent.
async fn notify_over(
    bus: &dyn EventBus,
    config: Arc<dyn ConfigStore>,
    store: Arc<dyn OperationStore>,
) {
    let localizer = || l10n::Localizer::new(&l10n::PosixEnv::from_process().preferences());
    let notifier = match HttpNotifier::new(localizer(), DELIVERY_TIMEOUT) {
        Ok(notifier) => notifier,
        Err(error) => {
            log::warn!("no notification is sent: {error}");
            return;
        }
    };
    let notify = Notify::new(
        config,
        store,
        Arc::new(SystemSecrets::default()),
        Arc::new(notifier),
    )
    .telling(Arc::new(log_secret_warning));
    // Subscribed before any run can start, so no news is missed.
    let news = bus.subscribe().await;
    let localizer = localizer();
    tokio::spawn(async move {
        notify
            .run(news, |report| {
                log::warn!("{}", report_line(&localizer, &report))
            })
            .await;
    });
}

/// What telling an event left to report, as the log says it, by its code: a
/// notification that wasn't delivered, with the event, the rule, the channel
/// and why; or a configuration that couldn't be read, as its code's message
/// says it.
fn report_line(localizer: &l10n::Localizer, report: &Report) -> String {
    match report {
        Report::Undelivered(undelivered) => {
            let args = [
                ("event", text(undelivered.event.name())),
                ("rule", text(undelivered.rule.as_str())),
                ("channel", text(undelivered.channel.as_str())),
                ("reason", text(undelivered.reason.reason())),
            ];
            let message = localizer.warning(WarningCode::NotificationUndelivered, &args, None);
            format!("{:?}: {message}", WarningCode::NotificationUndelivered)
        }
        Report::ConfigUnavailable(error) => config_unusable(error.clone(), |code, args, detail| {
            let message = localizer.error(code, &args, detail.as_deref());
            format!("no notification was sent for an event: {code:?}: {message}")
        }),
    }
}

/// The folder the workers keep rustic's caches in (§4.1): `dir`, made for
/// the daemon's user alone when it isn't there yet. `None`, said once in the
/// log, when there is no such folder, it can't be made or files can't be
/// made in it: the workers then run without a cache, which costs each job
/// time and nothing else, instead of each job warning about it again.
fn workers_cache(dir: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    let Some(dir) = dir else {
        log::warn!(
            "workers run without a cache: there is no home directory to keep one under; set LADDERBACK_CACHE_DIR to give them one"
        );
        return None;
    };
    if let Err(error) = dirs::create_private_dir(&dir) {
        log::warn!(
            "workers run without a cache: the cache folder {} can't be made: {error}",
            dir.display()
        );
        return None;
    }
    if let Err(error) = dirs::probe_writable(&dir) {
        log::warn!(
            "workers run without a cache: the cache folder {} can't be written to: {error}",
            dir.display()
        );
        return None;
    }
    Some(dir)
}

/// Says in the log what became of the ID of a repository a job opened,
/// whose entry recorded none. It names the entry by its alias and the
/// repository by its ID, neither of which opens it (I4).
fn log_recording(recording: Recording) {
    match recording {
        Recording::Recorded { alias, id } => log::info!(
            "recorded the repository ID {} in the entry {} of the configuration: a job opened it",
            id.as_str(),
            alias.as_str()
        ),
        Recording::NotRecorded {
            alias,
            id,
            why: NotRecorded::SameRepository { other },
        } => log::warn!(
            "did not record the repository ID {} in the entry {} of the configuration: the entry {} records it, so the two entries are one repository; remove one of them",
            id.as_str(),
            alias.as_str(),
            other.as_str()
        ),
        Recording::NotRecorded { alias, id, why } => log::info!(
            "did not record the repository ID {} in the entry {} of the configuration ({why:?}); the next job that opens it tries again",
            id.as_str(),
            alias.as_str()
        ),
        Recording::NotInPlace { alias, id, why } => log::warn!(
            "did not record the repository ID {} in the entry {} of the configuration: it can't be added without changing something else in the file ({why:?}); the next job that opens it tries again",
            id.as_str(),
            alias.as_str()
        ),
        Recording::Busy { alias, id } => log::info!(
            "did not record the repository ID {} in the entry {} of the configuration: another change to it was being saved; the next job that opens it tries again",
            id.as_str(),
            alias.as_str()
        ),
        Recording::Failed { alias, id, error } => log::warn!(
            "did not record the repository ID {} in the entry {} of the configuration ({error:?}); the next job that opens it tries again",
            id.as_str(),
            alias.as_str()
        ),
    }
}

/// The repositories the entries of `config` record. While an entry records
/// no ID, its repository's cache may be any in the cache folder (§16
/// decision 61).
fn recorded_repositories(config: &domain::planning::AgentConfig) -> Recorded {
    config
        .repositories()
        .values()
        .map(|repository| repository.id.clone())
        .collect::<Option<BTreeSet<RepositoryId>>>()
        .map_or(Recorded::NotAllKnown, Recorded::Exactly)
}

/// Tidies the workers' cache folder `dir` before any worker runs (§4.1,
/// §16 decision 61), keeping the caches of the repositories `recorded`
/// names, and says in the log what went and its size. Nothing here stops
/// the daemon from starting.
async fn tidy_workers_cache(dir: &std::path::Path, recorded: Recorded) {
    let root = dir.to_owned();
    let tidied = tokio::task::spawn_blocking(move || {
        engine::tidy_cache(&root, &recorded, std::time::SystemTime::now())
    })
    .await;
    match tidied {
        Ok(tidying) => log_tidying(&tidying),
        Err(error) => log::warn!("the cache folder {} was not tidied: {error}", dir.display()),
    }
}

/// Says in the log what tidying the cache removed, and what it couldn't.
fn log_tidying(tidying: &engine::CacheTidying) {
    for removed in &tidying.removed {
        let (id, count, bytes) = (removed.repository.as_str(), removed.count, removed.bytes);
        match removed.what {
            RemovedKind::Unrecorded => log::info!(
                "removed the cache of repository {id}, which no repository entry records: {count} cache slot(s), {bytes} bytes"
            ),
            RemovedKind::SpareSlots => {
                log::info!("removed {count} spare cache slot(s) of repository {id}: {bytes} bytes")
            }
            RemovedKind::UnfinishedFiles => log::info!(
                "removed {count} unfinished cache file(s) of repository {id}: {bytes} bytes"
            ),
        }
    }
    for failed in &tidying.failed {
        log::warn!(
            "the cache {} could not be tidied: {}",
            failed.path.display(),
            failed.error
        );
    }
}

/// Waits for `close`, the closing of the operation log at `database`, for
/// at most `bound`, and says in the log when it gave up.
async fn close_within(
    close: impl Future<Output = ()>,
    database: &std::path::Path,
    bound: std::time::Duration,
) {
    if tokio::time::timeout(bound, close).await.is_err() {
        log::warn!(
            "the operation log {} did not close within {} s, and the daemon stops without it: an operation it was still recording is ended at the next start",
            database.display(),
            bound.as_secs_f64()
        );
    }
}

/// A data folder another process holds, or a lock that couldn't be taken, as
/// the message for whoever started the daemon.
fn lock_refused(data_dir: &std::path::Path, error: LockError) -> Refused {
    match error {
        LockError::InUse { holder: Some(pid) } => Refused {
            message: msg::CLI_DATA_DIR_IN_USE,
            args: vec![("path", text(data_dir.display())), ("pid", text(pid))],
        },
        LockError::InUse { holder: None } => Refused {
            message: msg::CLI_DATA_DIR_IN_USE_UNNAMED,
            args: vec![("path", text(data_dir.display()))],
        },
        LockError::Unavailable { detail } => Refused {
            message: msg::CLI_CANNOT_LOCK_DATA_DIR,
            args: vec![
                ("path", text(data_dir.join(LOCK_FILE).display())),
                ("detail", text(detail)),
            ],
        },
    }
}

/// An operation log that couldn't end what an earlier run left unfinished, as
/// the message for whoever started the daemon.
fn recovery_refused(database: &std::path::Path, error: &app::ports::StoreError) -> Refused {
    let (app::ports::StoreError::Unavailable { detail }
    | app::ports::StoreError::Corrupt { detail }) = error;
    Refused {
        message: msg::CLI_CANNOT_RECOVER_OPERATIONS,
        args: vec![("path", text(database.display())), ("detail", text(detail))],
    }
}

/// A configuration the daemon cannot use, as the message for whoever wrote it.
fn config_refused(error: ConfigError) -> Refused {
    fn refused(message: MessageId, args: Vec<(&'static str, MsgArg)>) -> Refused {
        Refused { message, args }
    }
    match error {
        ConfigError::Unavailable { file, detail } => refused(
            msg::CLI_CONFIG_UNAVAILABLE,
            vec![("file", text(file)), ("detail", text(detail))],
        ),
        ConfigError::TooLarge { file, limit } => refused(
            msg::CLI_CONFIG_TOO_LARGE,
            vec![("file", text(file)), ("limit", MsgArg::Bytes(limit))],
        ),
        ConfigError::SchemaMissing { file } => {
            refused(msg::CLI_CONFIG_SCHEMA_MISSING, vec![("file", text(file))])
        }
        ConfigError::SchemaNewer {
            file,
            found,
            supported,
        } => refused(
            msg::CLI_CONFIG_SCHEMA_NEWER,
            vec![
                ("file", text(file)),
                ("found", text(found)),
                ("supported", text(supported)),
            ],
        ),
        ConfigError::Invalid {
            file,
            key,
            at,
            problem,
            detail,
        } => {
            // `repository-id` is required only of a repository with further
            // locations (decision 80), and the way out is to record it.
            let needs_id = problem == app::ports::Problem::MissingKey
                && key
                    .as_deref()
                    .is_some_and(|key| key.ends_with(".repository-id"));
            let mut where_ = key.unwrap_or_else(|| "an earlier rule".to_owned());
            if let Some(at) = at {
                where_ = format!("{where_} (line {}, column {})", at.line, at.column);
            }
            if needs_id {
                return refused(
                    msg::CLI_CONFIG_LOCATIONS_NEED_ID,
                    vec![("file", text(file)), ("where", text(where_))],
                );
            }
            refused(
                msg::CLI_CONFIG_INVALID,
                vec![
                    ("file", text(file)),
                    ("where", text(where_)),
                    ("detail", text(format!("{problem:?}: {detail}"))),
                ],
            )
        }
    }
}

/// A folder or database the daemon cannot open.
fn store_refused(path: &std::path::Path, detail: &str) -> Refused {
    Refused {
        message: msg::CLI_CANNOT_OPEN_STORE,
        args: vec![("path", text(path.display())), ("detail", text(detail))],
    }
}

/// The engine as the daemon runs it (§4.2): a job's progress and warnings go
/// to the event bus as [`UiEvent`]s on their way through, in the order the
/// engine reported them. The daemon never coalesces: that is the worker's,
/// once (§4.1). How an operation started and ended is not announced here:
/// the run that records it announces each once the log has it (§4.4,
/// #305). A job that is no operation tells the pages nothing (#308).
struct EventEngine {
    inner: Arc<dyn Engine>,
    bus: Arc<dyn EventBus>,
}

#[async_trait]
impl Engine for EventEngine {
    async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
        if !is_operation(&job) {
            return self.inner.run(op, job).await;
        }
        let operation = op.as_uuid().hyphenated().to_string();
        let running = self.inner.run(op, job).await?;
        let mut publish = Publish {
            operation,
            progress: LiveProgress::new(),
        };
        let bus = Arc::clone(&self.bus);
        Ok(RunningJob {
            // `then` keeps the engine's order: an event is published before
            // the runner sees it.
            events: running
                .events
                .then(move |event| {
                    let bus = Arc::clone(&bus);
                    let ui = publish.ui(&event);
                    async move {
                        if let Some(ui) = ui {
                            bus.publish(ui).await;
                        }
                        event
                    }
                })
                .boxed(),
            control: running.control,
        })
    }
}

/// What the choice of a location tells the daemon's log (decision 80): for
/// each job on a repository reached at several, which location it used and
/// why each earlier one was skipped, by position and outcome code, never an
/// address or a network's name.
fn log_location(report: LocationReport) {
    match report {
        LocationReport::ExposedCredentials {
            alias,
            position: Some(position),
            cause,
        } => log::warn!(
            "repository {}: location {position} sends its credentials where whoever answers at its address can read them ({})",
            alias.as_str(),
            cause.code()
        ),
        LocationReport::ExposedCredentials {
            alias,
            position: None,
            cause,
        } => log::warn!(
            "repository {}: this repository sends its credentials where whoever answers at its address can read them ({})",
            alias.as_str(),
            cause.code()
        ),
        LocationReport::OtherRepository { alias, position } => log::warn!(
            "repository {}: location {position} holds another repository, so it was not used",
            alias.as_str()
        ),
        LocationReport::Chosen { alias, record } => {
            let count = record.locations.len();
            let skipped: Vec<String> = record
                .locations
                .iter()
                .filter(|tried| {
                    !matches!(
                        tried.outcome,
                        LocationOutcome::Used | LocationOutcome::NotTried
                    )
                })
                .map(|tried| format!("location {} {}", tried.position, tried.outcome.code()))
                .collect();
            let skipped = if skipped.is_empty() {
                String::new()
            } else {
                format!(" (skipped: {})", skipped.join(", "))
            };
            match record.used() {
                Some(used) => {
                    log::info!(
                        "repository {}: used location {used} of {count}{skipped}",
                        alias.as_str()
                    );
                }
                None => {
                    log::warn!(
                        "repository {}: no location of {count} could be used{skipped}",
                        alias.as_str()
                    )
                }
            }
        }
    }
}

/// Whether the job is an operation, which the history has a row for, so a
/// page can show its progress and warnings there (§4.4). The reads (the look
/// that names a repository, a listing, a prune's plan) and the initialising
/// of a repository answer their request and are written to no log (§9): a
/// page told of their news would show it on no row, or on the row of the
/// run that read, which keeps none of it (#308).
fn is_operation(job: &Job) -> bool {
    match job {
        Job::Identify(_) | Job::Probe(_) | Job::Init(_) | Job::Ls(_) | Job::PrunePlan(_) => false,
        Job::Fake(_)
        | Job::Backup(_)
        | Job::Forget(_)
        | Job::Rewrite(_)
        | Job::Prune(_)
        | Job::Check(_) => true,
    }
}

/// Turns one run's [`EngineEvent`]s into [`UiEvent`]s, but for how the run
/// started and ended (§4.4). Of its progress bars, a page is told of the one
/// [`LiveProgress`] shows: the bar begun last that is still open.
struct Publish {
    operation: String,
    progress: LiveProgress,
}

impl Publish {
    fn ui(&mut self, event: &EngineEvent) -> Option<UiEvent> {
        use EngineEvent as E;
        match event {
            E::Progress(progress) => self
                .progress
                .apply(progress)
                .map(|shown| shown.ui(self.operation.clone())),
            E::Warning(warning) => Some(UiEvent::OperationWarning {
                operation: self.operation.clone(),
                code: warning_code(warning.kind),
                args: warning_args(warning),
            }),
            // The engine's start comes before the run's write that says the
            // operation is running, and its end before the run's ending hooks
            // and final write, so a page told of either could read a history
            // that doesn't have it yet. The run announces both.
            // The choice of location is the run's record, which the
            // operation's row shows once the run writes it.
            E::Started { .. } | E::Succeeded(_) | E::Failed(_) | E::Located(_) => None,
        }
    }
}

/// Starts the operations the API asks for: a backup of a named plan, run to
/// its end on a task of its own. What happens to the run arrives over the
/// events WebSocket, so the route answers with the operation's ID alone.
struct PlanRunner {
    config: Arc<dyn ConfigStore>,
    resolve: Arc<ResolveRepository>,
    agent_id: AgentId,
    engine: Arc<dyn Engine>,
    /// Chooses the location retention opens in-process for a repository
    /// reached at several (decision 80); `engine` makes the same choice for
    /// every job. Without it, such a repository's retention fails.
    locations: Option<Arc<ChooseLocation>>,
    store: Arc<dyn OperationStore>,
    /// Evaluates each plan's retention for a forget run (§7.3).
    retention: Arc<dyn Retention>,
    /// The running operations' cancel switches, shared with the routes.
    registry: Arc<CancelRegistry>,
    /// Runs the agent, repository and plan hooks around every run (§6.1).
    hooks: Arc<dyn HookRunner>,
    /// Where each run announces how its operation ended, once the log has the
    /// end (§4.4).
    events: Arc<dyn EventBus>,
    /// The agent's one queue of backups, which the scheduler shares: every
    /// backup takes its turn in it (§5, the concurrency limit).
    queue: Arc<RunQueue>,
    /// Where each backup's run says how it ended, so the scheduler can
    /// retry a failed one (§5).
    ends: app::retry::RunEndSender,
}

impl PlanRunner {
    /// The backup job of plan `id`, the hooks around it and what reading its
    /// secrets told, as `config` stands now: a rewrite between runs is
    /// picked up.
    async fn backup_of(
        &self,
        config: &domain::planning::AgentConfig,
        id: &PlanId,
    ) -> Result<BuiltBackup, Unbuilt> {
        let plan = config
            .plan(id)
            .ok_or(Unbuilt::Refused(StartError::UnknownPlan))?;
        let repository = config
            .repository(&plan.repository)
            .ok_or(Unbuilt::Refused(StartError::UnknownPlan))?;
        // The engine opens only the repository the configuration recorded,
        // when it recorded one (§7.3), and the job carries the entry it was
        // made from, so the ID it opens is recorded in an entry that records
        // none (§6.1, decision 57). A secret that can't be resolved is
        // refused with its own code, masked of every secret of the
        // repository, as the repository routes refuse it (I4, #309).
        let resolved = match self
            .resolve
            .resolve_saved(&plan.repository, repository)
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                let redactor =
                    refusal_redactor(&error, &repository.settings, &SystemSecrets::default()).await;
                return Err(Unbuilt::Secret(Box::new(SecretUnbuilt {
                    refusal: refusal_with(&error, &redactor),
                    failure: error.failure(&redactor),
                    target: Target {
                        plan: Some(plan.id.clone()),
                        repository: repository.id.clone(),
                    },
                })));
            }
        };
        let job = Job::Backup(BackupJob {
            repository: resolved.access,
            sources: plan.sources.iter().map(|s| s.as_str().to_owned()).collect(),
            tags: PlanTags::new(plan.id.clone(), self.agent_id),
            extra_tags: plan.tags.clone(),
            label: plan.label.clone(),
            host: plan.snapshot_host(config.agent()).cloned(),
            filter: plan.filter.clone(),
            deadline_budget: backup_budget(None, None),
        });
        // The agent, repository and plan hooks of §6.1 wrap the run.
        let setup = HookSetup {
            agent: config.hooks().clone(),
            repository: repository.hooks.clone(),
            plan: plan.hooks.clone(),
            plan_id: plan.id.clone(),
            repository_alias: plan.repository.clone(),
        };
        Ok(BuiltBackup {
            job,
            setup,
            warnings: resolved
                .warnings
                .iter()
                .map(SecretWarning::warning)
                .collect(),
        })
    }

    /// The use case that runs a backup with `setup`'s hooks.
    fn backup_use_case(&self, setup: HookSetup) -> RunOperation {
        RunOperation::new(
            Arc::clone(&self.engine),
            Arc::new(SystemClock),
            Arc::clone(&self.store),
        )
        .with_hooks(Arc::clone(&self.hooks), setup)
        .with_events(Arc::clone(&self.events))
    }

    /// Parks `operation`'s cancel switch before its run starts — not lazily
    /// when the run first polls it — so a cancel that arrives the instant
    /// the route answers finds it (§4.2). A cancel that fires only resolves;
    /// whether the receiver was still listening is nobody's business here.
    fn cancel_switch(&self, operation: OperationId) -> impl Future<Output = ()> + Send + use<> {
        let cancel = self.registry.register(operation);
        async move {
            let _ = cancel.await;
        }
    }

    /// Starts a backup of plan `id`, started by `trigger`, on a task of its
    /// own, and returns its operation's ID once the log has it queued. It
    /// runs in `place` when the scheduler gave it one, else in a place it
    /// takes in the queue now, as any run started by hand does (§5).
    async fn start_plan(
        &self,
        id: &PlanId,
        trigger: Trigger,
        place: Option<Place>,
    ) -> Result<OperationId, Unbuilt> {
        let config = self.config.load().await.map_err(|error| {
            Unbuilt::Refused(config_unusable(error, |code, args, detail| {
                StartError::Unavailable { code, args, detail }
            }))
        })?;
        let BuiltBackup {
            job,
            setup,
            warnings,
        } = self.backup_of(&config, id).await?;
        let place = place.unwrap_or_else(|| self.queue.push(id, config.agent().concurrency));
        // A UUIDv7: the log orders by ID, so the newest run is the highest.
        let operation = OperationId::from_uuid(Uuid::now_v7());
        let cancel = self.cancel_switch(operation);
        let use_case = self.backup_use_case(setup);
        // Recorded as queued before the route answers, so a cancel that
        // arrives the instant it does finds the operation in the log. The
        // task does the recording and the run, so a handler that is dropped
        // meanwhile (the client went away) strands neither.
        let (recorded, wait_for_log) = futures::channel::oneshot::channel::<()>();
        let registry = Arc::clone(&self.registry);
        let ends = self.ends.clone();
        tokio::spawn(async move {
            let queued = use_case
                .queue(operation, trigger, job)
                .await
                .with_warnings(warnings);
            let _ = recorded.send(());
            // Nothing waits on the run: it reports through the event bus. It
            // waits in the log as queued until its turn comes.
            let finished = use_case.run_in_turn(queued, place, cancel).await;
            registry.finish(&operation);
            log_ended(&finished);
            say_how_it_ended(&ends, &finished);
        });
        // A task that ended without recording has nothing to wait for.
        let _ = wait_for_log.await;
        Ok(operation)
    }
}

/// A plan's backup as [`PlanRunner::backup_of`] builds it.
struct BuiltBackup {
    job: Job,
    setup: HookSetup,
    /// What reading the repository's secrets told (#386), which the run
    /// carries from its start.
    warnings: Vec<domain::execution::Warning>,
}

/// Why a plan's backup couldn't be built, so its run doesn't start.
enum Unbuilt {
    /// Refused as a request to start it is, and nothing is recorded.
    Refused(StartError),
    /// A setting of the plan's repository couldn't be resolved.
    Secret(Box<SecretUnbuilt>),
}

/// A plan's backup unbuilt for a setting of its repository: `refusal` is
/// what a request to start it is answered with, and `failure` and `target`
/// what a run the schedule started records (§5).
struct SecretUnbuilt {
    refusal: SecretRefusal,
    failure: FailureReason,
    target: Target,
}

impl Unbuilt {
    /// The refusal of a request to start the run.
    fn into_start_error(self) -> StartError {
        match self {
            Self::Refused(error) => error,
            Self::Secret(secret) => StartError::SecretRefused(secret.refusal),
        }
    }
}

/// Logs that plan `plan`'s run, started by `trigger`, didn't start.
fn log_not_started(plan: &PlanId, trigger: Trigger, error: &StartError) {
    log::warn!(
        "{} of plan {} didn't start: {}",
        run_named(trigger),
        plan.as_str(),
        start_refused(error)
    );
}

/// A run started by `trigger`, in the log's words: "the scheduled run",
/// never the trigger's name in the code.
fn run_named(trigger: Trigger) -> &'static str {
    match trigger {
        Trigger::Schedule => "the scheduled run",
        Trigger::Manual => "the manual run",
        Trigger::CatchUp => "the catch-up run",
        Trigger::Retry => "the retry",
        Trigger::Hub => "the hub's run",
        Trigger::Mcp => "the MCP client's run",
    }
}

/// `code` as the API and the page name it, such as `secret-missing`, never
/// its name in the code.
fn wire_name(code: api::ErrorCode) -> String {
    match serde_json::to_value(code) {
        Ok(serde_json::Value::String(name)) => name,
        _ => format!("{code:?}"),
    }
}

/// Why a plan's run didn't start, for the daemon's log: a code, and for a
/// secret the setting and where it was looked for (an entry, a file, a
/// variable or a program, masked as the refusal shows them), never the
/// words of a secret's refusal, which may quote what was read (I4).
fn start_refused(error: &StartError) -> String {
    match error {
        StartError::UnknownPlan => "the plan or its repository isn't configured".to_owned(),
        StartError::Unavailable { code, .. } => {
            format!(
                "the configuration can't be read or used ({})",
                wire_name(*code)
            )
        }
        StartError::SecretRefused(refusal) => {
            let named = |key: &str| {
                refusal.args.iter().find_map(|(name, arg)| match arg {
                    MsgArg::Text(text) if *name == key => Some(text.as_str()),
                    _ => None,
                })
            };
            let place = match (named("field"), named("source")) {
                (Some(field), Some(source)) => format!(": its {field}, from {source}"),
                (Some(field), None) => format!(": its {field}"),
                _ => String::new(),
            };
            format!(
                "a secret of its repository was refused ({}){place}",
                wire_name(refusal.code)
            )
        }
    }
}

#[async_trait]
impl PlanRuns for PlanRunner {
    async fn start(&self, plan: &PlanId, trigger: Trigger, place: Place) {
        match self.start_plan(plan, trigger, Some(place)).await {
            Ok(_) => {}
            // Refused for a secret of its repository, it ends failed with the
            // refusal, so the history and the page say why the schedule's
            // backups stopped (§5), and it is retried as any failed backup the
            // schedule started (decision 76). Its place is given up already.
            Err(Unbuilt::Secret(secret)) => {
                let SecretUnbuilt {
                    refusal,
                    failure,
                    target,
                } = *secret;
                let operation = OperationId::from_uuid(Uuid::now_v7());
                // As a refusal by hand is logged (#483), never the refusal's
                // words, which only the history shows, masked (I4).
                log::warn!(
                    "{} of plan {} didn't start, and ends as failed operation {}: {}",
                    run_named(trigger),
                    plan.as_str(),
                    operation.as_uuid().hyphenated(),
                    start_refused(&StartError::SecretRefused(refusal))
                );
                let finished = RunOperation::new(
                    Arc::clone(&self.engine),
                    Arc::new(SystemClock),
                    Arc::clone(&self.store),
                )
                .with_events(Arc::clone(&self.events))
                .refused(operation, trigger, target, failure)
                .await;
                log_recording_errors(&finished);
                say_how_it_ended(&self.ends, &finished);
            }
            Err(Unbuilt::Refused(error)) => log_not_started(plan, trigger, &error),
        }
    }

    async fn resume(&self, mut operation: domain::execution::Operation) {
        let id = operation.id();
        let built = match operation.target().plan.clone() {
            Some(plan) => match self.config.load().await {
                Ok(config) => self
                    .backup_of(&config, &plan)
                    .await
                    .map(|built| (built, plan, config.agent().concurrency)),
                Err(error) => Err(Unbuilt::Refused(config_unusable(
                    error,
                    |code, args, detail| StartError::Unavailable { code, args, detail },
                ))),
            },
            None => Err(Unbuilt::Refused(StartError::UnknownPlan)),
        };
        // Refused for a secret of the repository its plan still backs up to,
        // it ends failed with the refusal, as a refused scheduled run does
        // (§5, #490), and is heard as any run's end, so retries follow it.
        let built = match built {
            Err(Unbuilt::Secret(secret))
                if operation.target().is_on(secret.target.repository.as_ref()) =>
            {
                let SecretUnbuilt {
                    refusal, failure, ..
                } = *secret;
                log::warn!(
                    "the queued backup {} can't run again, and ends as failed: {}",
                    id.as_uuid().hyphenated(),
                    start_refused(&StartError::SecretRefused(refusal))
                );
                let finished = RunOperation::new(
                    Arc::clone(&self.engine),
                    Arc::new(SystemClock),
                    Arc::clone(&self.store),
                )
                .with_events(Arc::clone(&self.events))
                .refuse_resumed(operation, failure)
                .await;
                log_recording_errors(&finished);
                say_how_it_ended(&self.ends, &finished);
                return;
            }
            Err(Unbuilt::Secret(_)) => {
                Err("its plan now backs up to another repository".to_owned())
            }
            other => other.map_err(|error| start_refused(&error.into_start_error())),
        };
        let built = built.and_then(|(built, plan, limit)| {
            // The plan may have moved to another repository since the
            // queue's own read: it isn't run there under a record of the old.
            if operation
                .target()
                .is_on(built.job.target().repository.as_ref())
            {
                Ok((built, plan, limit))
            } else {
                Err("its plan now backs up to another repository".to_owned())
            }
        });
        match built {
            Ok((
                BuiltBackup {
                    job,
                    setup,
                    warnings,
                },
                plan,
                limit,
            )) => {
                // Oldest first, as the queue hands them on: each waits for
                // its turn like any other backup (§5).
                let place = self.queue.push(&plan, limit);
                let cancel = self.cancel_switch(id);
                let use_case = self.backup_use_case(setup);
                let registry = Arc::clone(&self.registry);
                let ends = self.ends.clone();
                tokio::spawn(async move {
                    let queued =
                        app::execution::Queued::resumed(operation, job).with_warnings(warnings);
                    let finished = use_case.run_in_turn(queued, place, cancel).await;
                    registry.finish(&id);
                    log_ended(&finished);
                    say_how_it_ended(&ends, &finished);
                });
            }
            // The queue can't run it again, as decision 40 ended every
            // queued operation: its plan or repository is gone or changed, its
            // plan moved and the secret is refused, or the configuration can't be read. Left
            // queued, it would show as queued with nothing to run it until
            // the next start (decision 73).
            Err(why) => {
                let hyphenated = id.as_uuid().hyphenated();
                log::warn!(
                    "the queued backup {hyphenated} can't run again, and ends as failed: {why}"
                );
                if operation
                    .fail(FailureReason::DaemonStopped, SystemClock.now())
                    .is_ok()
                    && let Err(refused) = self.store.save(&operation, None).await
                {
                    log::warn!(
                        "operation {hyphenated}: a write to the operation log failed ({}); it stays queued",
                        refused.code()
                    );
                }
            }
        }
    }

    fn report(&self, report: ScheduleReport) {
        match report {
            ScheduleReport::Skipped { plan, unmet } => log::info!(
                "the scheduled run of plan {} was skipped: a condition it sets isn't met ({unmet:?})",
                plan.as_str()
            ),
            ScheduleReport::Overlapped { plan, overlapping } => log::info!(
                "the scheduled run of plan {} was skipped: {}",
                plan.as_str(),
                match overlapping {
                    domain::execution::queue::Overlapping::Running =>
                        "its last run is still running",
                    domain::execution::queue::Overlapping::Waiting =>
                        "a run of it is already waiting for its turn",
                }
            ),
            ScheduleReport::PowerUnknown { plan, error } => log::warn!(
                "the scheduled run of plan {} starts, though what powers the machine is unknown: {error:?}",
                plan.as_str()
            ),
            ScheduleReport::NetworkWaited(NetworkWait::Online { waited }) => {
                log::info!("the catch-up runs after a wake waited {waited:#} for the network")
            }
            ScheduleReport::NetworkWaited(NetworkWait::GaveUp) => log::warn!(
                "the catch-up runs after a wake start, though the machine is still offline"
            ),
            ScheduleReport::NetworkWaited(NetworkWait::Unknown(error)) => log::warn!(
                "the catch-up runs after a wake start, though whether the machine is online is unknown: {error:?}"
            ),
            ScheduleReport::NoNextRun { plan } => log::warn!(
                "plan {} isn't run again: its schedule has no run after now",
                plan.as_str()
            ),
            ScheduleReport::Store(error) => log::warn!(
                "the scheduler couldn't read or write the operation log ({}); it goes on with what it holds",
                error.code()
            ),
            ScheduleReport::Config(error) => log::warn!(
                "the scheduler couldn't read the configuration, and goes on with the plans it last read: {error}"
            ),
            ScheduleReport::QueuedEnded {
                operation,
                kind,
                why,
            } => log::warn!(
                "the queued {kind:?} {} can't run again, and ends as failed ({why:?})",
                operation.as_uuid().hyphenated()
            ),
            ScheduleReport::Retrying { plan, retry } => log::info!(
                "the backup of plan {} failed, and retry {} runs at {}",
                plan.as_str(),
                retry.attempt,
                retry.at
            ),
            ScheduleReport::RetriesEnded { plan, why } => log::info!(
                "the retries of plan {} ended without a success: {}",
                plan.as_str(),
                match why {
                    domain::execution::retry::NoRetry::Exhausted => "its attempts are spent",
                    domain::execution::retry::NoRetry::NextRun =>
                        "its next scheduled run comes first",
                    domain::execution::retry::NoRetry::RunGoing => "a run of it is going",
                }
            ),
        }
    }
}

#[async_trait]
impl Operations for PlanRunner {
    async fn start_backup(&self, plan: String) -> Result<String, StartError> {
        let id = PlanId::try_from(plan).map_err(|_| StartError::UnknownPlan)?;
        // Refused by hand as on a schedule, the run leaves a line in the log
        // too, not only the answer the page shows for a moment (#483).
        let operation = self
            .start_plan(&id, Trigger::Manual, None)
            .await
            .map_err(Unbuilt::into_start_error)
            .inspect_err(|error| log_not_started(&id, Trigger::Manual, error))?;
        Ok(operation.as_uuid().hyphenated().to_string())
    }

    async fn start_maintenance(
        &self,
        repository: String,
        request: http_adapter::MaintenanceRequest,
        selection: Option<String>,
    ) -> Result<String, MaintenanceRefusal> {
        maintenance::start(self, repository, request, selection).await
    }

    async fn preview(
        &self,
        repository: String,
        request: http_adapter::MaintenanceRequest,
    ) -> Result<api::MaintenancePreviewDto, MaintenanceRefusal> {
        maintenance::preview(self, repository, request).await
    }

    async fn groups(
        &self,
        repository: String,
    ) -> Result<api::SnapshotGroupsDto, MaintenanceRefusal> {
        maintenance::groups(self, repository).await
    }

    async fn history(
        &self,
        plan: Option<String>,
        after: Option<String>,
        limit: Option<u64>,
    ) -> Result<api::OperationsPageDto, http_adapter::HistoryError> {
        let empty = || api::OperationsPageDto {
            operations: Vec::new(),
            next: None,
        };
        // A filter that names no plan that could exist matches nothing.
        let plan = match plan.map(PlanId::try_from) {
            Some(Ok(plan)) => Some(plan),
            Some(Err(invalid)) => {
                log::warn!("a plan filter no plan could match: {invalid}");
                return Ok(empty());
            }
            None => None,
        };
        let after = match after {
            Some(cursor) => match cursor.parse::<Uuid>() {
                Ok(uuid) => Some(OperationId::from_uuid(uuid)),
                Err(_) => return Err(http_adapter::HistoryError::BadCursor),
            },
            None => None,
        };
        let limit = limit.map(|asked| usize::try_from(asked).unwrap_or(usize::MAX));
        let page = match ListOperations::new(Arc::clone(&self.store))
            .list(plan, after, limit)
            .await
        {
            Ok(page) => page,
            Err(error) => {
                log::error!("the operation log could not be read: {error}");
                return Ok(empty());
            }
        };
        Ok(api::OperationsPageDto {
            operations: page.operations.iter().filter_map(summary).collect(),
            next: page.next.map(|id| id.as_uuid().hyphenated().to_string()),
        })
    }

    async fn operation(
        &self,
        id: String,
    ) -> Result<api::OperationDetailDto, http_adapter::LookupError> {
        let id = operation_of(&id).ok_or(http_adapter::LookupError::Unknown)?;
        let detail = GetOperation::new(Arc::clone(&self.store))
            .get(id)
            .await
            .map_err(|error| {
                log::error!("the operation log could not be read: {error}");
                http_adapter::LookupError::Unknown
            })?
            .ok_or(http_adapter::LookupError::Unknown)?;
        let Some(operation) = summary(&detail.record) else {
            log::error!(
                "a stored operation the page cannot show: {}",
                detail.record.id.as_uuid().hyphenated()
            );
            return Err(http_adapter::LookupError::Unknown);
        };
        Ok(api::OperationDetailDto {
            operation,
            warnings: detail
                .warnings
                .iter()
                .map(|warning| api::OperationWarningDto {
                    code: warning_code(warning.kind),
                    item: warning.item.clone(),
                    during: warning.during.clone(),
                    detail: warning.detail.clone(),
                })
                .collect(),
        })
    }

    async fn cancel(&self, id: String) -> Result<(), http_adapter::CancelError> {
        let Some(id) = operation_of(&id) else {
            return Err(http_adapter::CancelError::Unknown);
        };
        // The log says which of the two refusals this is: unknown, or known
        // but ended. Only a running operation has a handle to fire (§4.2).
        let running = match GetOperation::new(Arc::clone(&self.store)).get(id).await {
            Ok(Some(detail)) => !detail.record.status.is_finished(),
            Ok(None) => return Err(http_adapter::CancelError::Unknown),
            Err(error) => {
                log::error!("the operation log could not be read: {error}");
                return Err(http_adapter::CancelError::Unknown);
            }
        };
        if running && self.registry.cancel(&id) {
            Ok(())
        } else {
            Err(http_adapter::CancelError::NotRunning)
        }
    }
}

/// Tells the scheduler how a backup's run ended, so that it can retry a
/// failed one (§5). The run's place has left the queue by now.
fn say_how_it_ended(ends: &app::retry::RunEndSender, finished: &Finished) {
    if let Some(end) = app::retry::RunEnd::of(&finished.operation) {
        ends.send(end);
    }
}

/// Says in the log that an operation ended, and each write the operation log
/// refused it (§9): a run goes on without a log that refuses its writes, so
/// these lines are the only sign that the log has stopped taking them. A
/// refused write is named by the code of its failure and never by the
/// failure's own words, which nobody has checked for a value (I4).
fn log_ended(finished: &Finished) {
    log::info!(
        "operation {} ended: {:?}",
        finished.operation.id().as_uuid().hyphenated(),
        finished.operation.status()
    );
    log_recording_errors(finished);
}

/// Says in the log each write of `finished` the operation log refused, as
/// [`log_ended`] does.
fn log_recording_errors(finished: &Finished) {
    let operation = finished.operation.id().as_uuid().hyphenated();
    for refused in &finished.recording_errors {
        log::warn!(
            "operation {operation}: a write to the operation log failed ({}); its history may be incomplete",
            refused.code()
        );
    }
}

/// An operation's ID as the wire carries it, when the text is one.
fn operation_of(text: &str) -> Option<OperationId> {
    text.parse::<Uuid>().ok().map(OperationId::from_uuid)
}

/// One log record as the history page shows it, or `None` when it holds
/// something no page could show. The page carries the status and each
/// warning's code once, from the kept warnings (#508); the warnings
/// themselves come with the detail.
fn summary(record: &OperationRecord) -> Option<api::OperationSummaryDto> {
    let failure = match &record.status {
        OperationStatus::Failed { reason } => Some(failure(reason)),
        _ => None,
    };
    api::OperationSummaryDto::new(
        record.id.as_uuid().hyphenated().to_string(),
        record
            .target
            .plan
            .as_ref()
            .map_or_default(|plan| plan.as_str().to_owned()),
        kind_text(record.kind).to_owned(),
        status_text(&record.status).to_owned(),
        record.queued_at.to_string(),
        record.started_at.map(|at| at.to_string()),
        record.finished_at.map(|at| at.to_string()),
        record
            .warning_kinds
            .iter()
            .map(|kind| warning_code(*kind))
            .collect(),
        failure,
        record
            .result
            .as_ref()
            .and_then(|result| result_dto(result, record)),
        record
            .repository_alias
            .as_ref()
            .map(|alias| alias.as_str().to_owned()),
    )
    .map(|summary| {
        summary.with_locations(record.locations.as_ref().map_or_default(locations_tried))
    })
    .map_err(|invalid| log::warn!("an operation the page cannot show: {invalid:?}"))
    .ok()
}

/// What a successful operation produced, as the page shows it: the small
/// typed summary of its kind, with the run's duration.
fn result_dto(result: &EngineResult, record: &OperationRecord) -> Option<api::OperationResultDto> {
    let duration_ms = record
        .started_at
        .zip(record.finished_at)
        .and_then(|(from, to)| u64::try_from(to.as_millisecond() - from.as_millisecond()).ok())
        .unwrap_or(0);
    match result {
        EngineResult::Backup(backup) => Some(api::OperationResultDto::Backup {
            files: backup
                .files_new
                .saturating_add(backup.files_changed)
                .saturating_add(backup.files_unmodified),
            bytes: backup.data_added,
            duration_ms,
            snapshot: Some(backup.snapshot.as_str().to_owned()),
            bytes_processed: backup.bytes_processed,
        }),
        EngineResult::Prune(prune) => Some(api::OperationResultDto::Prune {
            packs_deleted: prune.packs_deleted,
            duration_ms,
        }),
        EngineResult::Check(check) => Some(api::OperationResultDto::Check {
            errors: check.errors,
            warnings: check.warnings,
            duration_ms,
        }),
        EngineResult::Forget(forget) => Some(api::OperationResultDto::Forget {
            removed: u64::try_from(forget.removed.len()).unwrap_or(0),
            duration_ms,
        }),
        EngineResult::Rewrite(rewrite) => Some(api::OperationResultDto::Adopt {
            rewritten: u64::try_from(rewrite.created.len()).unwrap_or(0),
            duration_ms,
        }),
        // Reads: a result no operation row holds, so the page shows none.
        EngineResult::Fake { .. }
        | EngineResult::Repository { .. }
        | EngineResult::PrunePlan(_)
        | EngineResult::Probed(_)
        | EngineResult::Snapshots(_) => None,
    }
}

/// An operation's kind, as the wire names it.
fn kind_text(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::Fake => "fake",
        OperationKind::Identify => "identify",
        OperationKind::Init => "init",
        OperationKind::Backup => "backup",
        OperationKind::Forget => "forget",
        OperationKind::Adopt => "adopt",
        OperationKind::Prune => "prune",
        OperationKind::Check => "check",
        OperationKind::Ls => "ls",
    }
}

/// An operation's status, as the wire names it.
fn status_text(status: &OperationStatus) -> &'static str {
    match status {
        OperationStatus::Queued => "queued",
        OperationStatus::Running => "running",
        OperationStatus::Succeeded => "succeeded",
        OperationStatus::SucceededWithWarnings => "succeeded-with-warnings",
        OperationStatus::Failed { .. } => "failed",
        OperationStatus::Cancelled => "cancelled",
    }
}

/// A warning's kind, as the wire names it.
fn warning_code(kind: WarningKind) -> WarningCode {
    match kind {
        WarningKind::SourceUnreadable => WarningCode::SourceUnreadable,
        WarningKind::Hook => WarningCode::HookWarning,
        WarningKind::MaintenancePartial => WarningCode::MaintenancePartial,
        WarningKind::CheckWarning => WarningCode::CheckWarning,
        WarningKind::CheckError => WarningCode::CheckError,
        WarningKind::OtherHosts => WarningCode::MaintenanceOtherHosts,
        WarningKind::LocationSkipped => WarningCode::LocationSkipped,
        WarningKind::SecretFileReadable => WarningCode::SecretFileReadable,
        WarningKind::Other => WarningCode::Unknown,
    }
}

/// The arguments a warning's message takes, as the page gets them: a
/// readable secret file's path and mode (#386), otherwise the item the
/// warning names, if any.
fn warning_args(warning: &domain::execution::Warning) -> Vec<MsgArg> {
    match warning.kind {
        WarningKind::SecretFileReadable => warning
            .item
            .iter()
            .cloned()
            .chain([warning.detail.clone()])
            .map(MsgArg::Text)
            .collect(),
        _ => warning
            .item
            .as_ref()
            .map_or_default(|item| vec![MsgArg::Text(item.clone())]),
    }
}

/// An import that saved nothing, as the code, arguments and detail the
/// catalog renders (§8.4).
pub(crate) struct Refusal {
    pub(crate) code: ErrorCode,
    pub(crate) args: Vec<(&'static str, MsgArg)>,
    pub(crate) detail: Option<String>,
}

/// Something an import did that the person should know, as the code and
/// arguments the catalog renders.
pub(crate) struct Notice {
    pub(crate) code: WarningCode,
    pub(crate) args: Vec<(&'static str, MsgArg)>,
}

/// The `line` argument of a message about a place in a file, left out when
/// no place is known.
fn line_arg(at: Option<app::ports::Position>) -> Option<(&'static str, MsgArg)> {
    at.map(|at| ("line", text(at.line)))
}

/// What came of a refused import. `config_file` is the configuration's own
/// path, which a refusal about the merged configuration (rather than about a
/// file the store read) names.
pub(crate) fn import_refusal(error: ImportError, config_file: &std::path::Path) -> Refusal {
    match error {
        ImportError::Profile(error) => profile_refusal(error),
        ImportError::AliasTaken(alias) => Refusal {
            code: ErrorCode::RepositoryAliasTaken,
            args: vec![("alias", text(alias.as_str()))],
            detail: None,
        },
        ImportError::AlreadyConfigured(alias) => Refusal {
            code: ErrorCode::RepositoryAlreadyConfigured,
            args: vec![("alias", text(alias.as_str()))],
            detail: None,
        },
        ImportError::PlanTaken(plan) => Refusal {
            code: ErrorCode::PlanIdTaken,
            args: vec![("plan", text(plan.as_str()))],
            detail: None,
        },
        ImportError::Config(error) => profile_config_refusal(error, config_file),
    }
}

/// What came of a refused export: the code, arguments and detail the catalog
/// renders. `config_file` is as for [`import_refusal`].
pub(crate) fn export_refusal(error: ExportError, config_file: &std::path::Path) -> Refusal {
    match error {
        ExportError::PlanUnknown(plan) => plan_unknown(plan.as_str()),
        ExportError::Render(ExportRefusal::InlineSecret) => Refusal {
            code: ErrorCode::InlineSecret,
            args: Vec::new(),
            detail: None,
        },
        ExportError::Render(ExportRefusal::Reference { key }) => Refusal {
            code: ErrorCode::ProfileExportReference,
            args: vec![("key", text(key))],
            detail: None,
        },
        ExportError::Render(ExportRefusal::UrlPassword { key }) => Refusal {
            code: ErrorCode::ProfileExportUrlPassword,
            args: vec![("key", text(key))],
            detail: None,
        },
        ExportError::Render(ExportRefusal::TagComma { tag }) => Refusal {
            code: ErrorCode::ProfileExportTagComma,
            args: vec![("tag", text(tag))],
            detail: None,
        },
        ExportError::Config(error) => profile_config_refusal(error, config_file),
    }
}

/// A plan the configuration does not have, or a name no plan can have.
pub(crate) fn plan_unknown(plan: &str) -> Refusal {
    Refusal {
        code: ErrorCode::PlanUnknown,
        args: vec![("plan", text(plan))],
        detail: None,
    }
}

/// A name for a repository that is not an alias: the one a person asked for,
/// or what could be made of a profile's file name.
pub(crate) fn repository_alias_invalid() -> Refusal {
    Refusal {
        code: ErrorCode::RepositoryAliasInvalid,
        args: Vec::new(),
        detail: None,
    }
}

/// A command that has no folder to keep the configuration in: the platform
/// names no home folder and no configuration folder is set.
pub(crate) fn config_folder_unknown() -> Refusal {
    Refusal {
        code: ErrorCode::ConfigFolderUnknown,
        args: Vec::new(),
        detail: None,
    }
}

/// A command whose async runtime did not start. The detail is the operating
/// system's own message, which the command line shows as it came.
pub(crate) fn runtime_unavailable(error: &std::io::Error) -> Refusal {
    Refusal {
        code: ErrorCode::RuntimeUnavailable,
        args: Vec::new(),
        detail: Some(error.to_string()),
    }
}

/// What the reader said was wrong with a profile.
fn profile_refusal(error: ProfileError) -> Refusal {
    match error {
        ProfileError::Unreadable { file } => Refusal {
            code: ErrorCode::ProfileUnreadable,
            args: vec![("file", text(file))],
            detail: None,
        },
        ProfileError::TooLarge { file, limit } => Refusal {
            code: ErrorCode::ProfileTooLarge,
            args: vec![("file", text(file)), ("limit", MsgArg::Bytes(limit))],
            detail: None,
        },
        ProfileError::Invalid {
            file,
            key,
            at,
            problem,
            detail,
        } => {
            let mut args = vec![("file", text(file))];
            args.extend(key.map(|key| ("key", text(key))));
            args.extend(line_arg(at));
            Refusal {
                code: ErrorCode::ProfileInvalid,
                args,
                detail: Some(problem_detail(&format!("{problem:?}"), &detail)),
            }
        }
        ProfileError::Unsupported { file, key, at } => {
            let mut args = vec![("file", text(file)), ("key", text(key))];
            args.extend(line_arg(at));
            Refusal {
                code: ErrorCode::ProfileUnsupported,
                args,
                detail: None,
            }
        }
    }
}

/// What the store said was wrong with the configuration an import would have
/// written: the answer a person gets for the file they know.
fn profile_config_refusal(error: ConfigError, config_file: &std::path::Path) -> Refusal {
    fn refusal(
        code: ErrorCode,
        args: Vec<(&'static str, MsgArg)>,
        detail: Option<String>,
    ) -> Refusal {
        Refusal { code, args, detail }
    }
    match error {
        ConfigError::Unavailable { file, detail } => refusal(
            ErrorCode::ConfigUnavailable,
            vec![("file", text(file))],
            Some(detail),
        ),
        ConfigError::TooLarge { file, limit } => refusal(
            ErrorCode::ConfigTooLarge,
            vec![("file", text(file)), ("limit", MsgArg::Bytes(limit))],
            None,
        ),
        ConfigError::SchemaMissing { file } => refusal(
            ErrorCode::ConfigSchemaMissing,
            vec![("file", text(file))],
            None,
        ),
        ConfigError::SchemaNewer {
            file,
            found,
            supported,
        } => refusal(
            ErrorCode::ConfigSchemaNewer,
            vec![
                ("file", text(file)),
                ("found", text(found)),
                ("supported", text(supported)),
            ],
            None,
        ),
        ConfigError::Invalid {
            file,
            key,
            at,
            problem,
            detail,
        } => {
            // The use case's own check of the merged configuration names no
            // file: it is the configuration the import would have written.
            let file = if file.is_empty() {
                config_file.display().to_string()
            } else {
                file
            };
            let mut args = vec![("file", text(file))];
            args.extend(line_arg(at));
            let what = problem_detail(&format!("{problem:?}"), &detail);
            refusal(
                ErrorCode::ConfigInvalid,
                args,
                Some(match key {
                    Some(key) => format!("{key}: {what}"),
                    None => what,
                }),
            )
        }
    }
}

/// What is wrong, as the problem's name and the parser's words when it had
/// any.
fn problem_detail(problem: &str, detail: &str) -> String {
    if detail.is_empty() {
        problem.to_owned()
    } else {
        format!("{problem}: {detail}")
    }
}

/// A notice of an import or export, as the warning the catalog renders.
pub(crate) fn profile_notice(notice: ProfileNotice) -> Notice {
    let (code, args) = match notice {
        ProfileNotice::KeyIgnored { key } => {
            (WarningCode::ProfileKeyIgnored, vec![("key", text(key))])
        }
        ProfileNotice::Prune => (WarningCode::ProfilePrune, Vec::new()),
        ProfileNotice::EnvIgnored => (WarningCode::ProfileEnvIgnored, Vec::new()),
        ProfileNotice::HooksPerPlan => (WarningCode::ProfileHooksPerPlan, Vec::new()),
        ProfileNotice::UsedProfileMissing { name } => {
            (WarningCode::ProfileUsedMissing, vec![("name", text(name))])
        }
        ProfileNotice::InlineSecret { field } => {
            (WarningCode::InlineSecret, vec![("field", text(field))])
        }
        ProfileNotice::LocationSecret { field, rclone } => (
            WarningCode::LocationSecret,
            vec![("field", text(field)), ("kind", location_kind(rclone))],
        ),
        ProfileNotice::GlobNeverMatches {
            plan,
            glob,
            absolute,
        } => glob_never_matches(&plan, &glob, absolute.as_deref()),
        ProfileNotice::TagDropped { tag } => {
            (WarningCode::ProfileTagDropped, vec![("tag", text(tag))])
        }
        ProfileNotice::OptionLeftOut { option, variable } => (
            WarningCode::ProfileOptionLeftOut,
            vec![("option", text(option)), ("variable", text(variable))],
        ),
        ProfileNotice::RetentionEmpty => (WarningCode::ProfileRetentionEmpty, Vec::new()),
        ProfileNotice::RepositorySettingKept {
            repository,
            setting,
        } => (
            WarningCode::ProfileSettingKept,
            vec![("repository", text(repository)), ("setting", text(setting))],
        ),
        ProfileNotice::KeyringCommandMissing { program } => (
            WarningCode::ProfileKeyringCommandMissing,
            vec![("program", text(program))],
        ),
        ProfileNotice::LocationsNotExported { further } => (
            WarningCode::ProfileLocationsNotExported,
            vec![(
                "count",
                MsgArg::Count(u64::try_from(further).unwrap_or(u64::MAX)),
            )],
        ),
    };
    Notice { code, args }
}

/// The running operations' cancel switches (§4.2): what the API cancels
/// through. A handle is registered when an operation starts running and
/// dropped when it ends, so only a running operation can be cancelled, once.
struct CancelRegistry {
    running: std::sync::Mutex<HashMap<OperationId, futures::channel::oneshot::Sender<()>>>,
}

/// The catalog the API asks of the daemon (§4.2): the configuration's plans,
/// with the scheduler's planned runs, and repositories, and one repository's
/// snapshots, listed live through the engine (§9: a read stores nothing and
/// takes no slot). Which repositories a row offers to create is read from
/// the operation log (decision 53).
struct ConfigCatalog {
    config: Arc<dyn ConfigStore>,
    engine: Arc<dyn Engine>,
    operations: Arc<dyn OperationStore>,
    schedules: Arc<dyn ScheduleStore>,
    /// Whether this machine reads the Wi-Fi network's name, for the notes
    /// on rules that name networks (decision 80).
    link: Arc<NameWatch>,
    /// A plan's sources as the backup walks them, which its globs are read
    /// against (#510).
    sources: Arc<dyn NormalizeSources>,
}

#[async_trait]
impl Catalog for ConfigCatalog {
    async fn plans(&self) -> Result<api::PlansDto, CatalogError> {
        let config = self.config.load().await.map_err(catalog_refusal)?;
        // A planned run that can't be read is no reason to hide the plans:
        // each scheduled one shows that it has none planned.
        let planned: HashMap<PlanId, domain::Timestamp> = match self.schedules.planned_runs().await
        {
            Ok(runs) => runs.into_iter().map(|run| (run.plan, run.at)).collect(),
            Err(error) => {
                log::warn!(
                    "the planned runs couldn't be read ({}); the plans are listed without them",
                    error.code()
                );
                HashMap::new()
            }
        };
        let mut plans = Vec::with_capacity(config.plans().len());
        for plan in config.plans() {
            let scheduled = plan.schedule.is_some();
            plans.push(api::PlanSummaryDto {
                id: plan.id.as_str().to_owned(),
                repository: plan.repository.as_str().to_owned(),
                scheduled,
                // A run kept for a plan whose schedule was just removed is
                // the scheduler's to forget, not a run.
                next_run: planned
                    .get(&plan.id)
                    .filter(|_| scheduled)
                    .map(ToString::to_string),
                warnings: never_matching_glob_warnings(plan, self.sources.as_ref())
                    .await
                    .into_iter()
                    .map(|(code, args)| api::WarningDto::new(code, args))
                    .collect(),
            });
        }
        Ok(api::PlansDto { plans })
    }

    async fn repositories(&self) -> Result<api::RepositoriesDto, CatalogError> {
        let config = self.config.load().await.map_err(catalog_refusal)?;
        let offered = self.offers(&config).await;
        // A rule that names networks is noted when the name can't be read:
        // the first listing that has one reads it once, in the background,
        // as the read may take seconds.
        let names_asked = config.repositories().values().any(|repository| {
            repository
                .location_rules()
                .into_iter()
                .flatten()
                .any(LocationRule::asks_name)
        });
        if names_asked {
            let link = Arc::clone(&self.link);
            tokio::spawn(async move { link.check_once().await });
        }
        let reads_names = self.link.reads_names();
        Ok(api::RepositoriesDto {
            repositories: config
                .repositories()
                .iter()
                .map(|(alias, repository)| {
                    let plans: Vec<&Plan> = config
                        .plans()
                        .iter()
                        .filter(|plan| &plan.repository == alias)
                        .collect();
                    listed_repository(
                        alias,
                        repository,
                        &plans,
                        reads_names,
                        offered.contains(alias),
                    )
                })
                .collect(),
        })
    }

    async fn snapshots(&self, alias: String) -> Result<api::SnapshotsDto, SnapshotsError> {
        // An alias no repository could have is as unknown as one the
        // configuration happens not to have.
        let alias = RepositoryAlias::parse(&alias).map_err(|_| SnapshotsError::Unknown)?;
        let config = self.config.load().await.map_err(|error| {
            config_unusable(error, |code, args, detail| SnapshotsError::Unavailable {
                code,
                args,
                detail,
            })
        })?;
        let repository = config.repository(&alias).ok_or(SnapshotsError::Unknown)?;
        let listing = ListSnapshots::new(
            Arc::clone(&self.engine),
            ResolveRepository::new(Arc::new(SystemSecrets::default()))
                .telling(Arc::new(log_secret_warning)),
        )
        .list(OperationId::from_uuid(Uuid::now_v7()), &alias, repository)
        .await;
        // A secret: its code and the words a command or the platform gave,
        // masked of every secret of the repository, in the log as in the
        // answer (I4).
        let refused = |refusal: SecretRefusal| {
            log::warn!(
                "the snapshots of {} could not be listed: {:?}{}",
                alias.as_str(),
                refusal.code,
                refusal
                    .detail
                    .as_deref()
                    .map_or_default(|detail| format!(" ({detail})"))
            );
            SnapshotsError::SecretRefused(refusal)
        };
        let listed = match listing {
            Ok(listed) => listed,
            Err(SnapshotError::Resolve(error)) => {
                return Err(refused(
                    secret_refusal(&error, &repository.settings, &SystemSecrets::default()).await,
                ));
            }
            Err(SnapshotError::Engine(EngineError::SecretsTooLargeToMask { field, limit })) => {
                return Err(refused(too_much_to_mask(field, limit)));
            }
            Err(error) => {
                let said = failure_redactor(&repository.settings, &SystemSecrets::default())
                    .await
                    .redact(&format!("{error:?}"));
                log::warn!(
                    "the snapshots of {} could not be listed: {said}",
                    alias.as_str()
                );
                return Err(SnapshotsError::Unavailable {
                    code: ErrorCode::EngineUnavailable,
                    args: Vec::new(),
                    detail: None,
                });
            }
        };
        let snapshots = listed
            .iter()
            .filter_map(|snapshot| {
                snapshot_dto(snapshot)
                    .map_err(|problem| {
                        log::error!(
                            "a snapshot of {} the page cannot show: {problem:?}",
                            alias.as_str()
                        );
                    })
                    .ok()
            })
            .collect();
        Ok(api::SnapshotsDto { snapshots })
    }
}

/// A configuration the lists can't read, refused as a save of it is.
fn catalog_refusal(error: ConfigError) -> CatalogError {
    config_unusable(error, |code, args, detail| CatalogError::Unavailable {
        code,
        args,
        detail,
    })
}

impl ConfigCatalog {
    /// The repositories whose rows offer to create them (decision 53). A log
    /// that can't be read offers none: the list is still shown, and Create
    /// is never offered on a guess.
    async fn offers(
        &self,
        config: &domain::planning::AgentConfig,
    ) -> std::collections::BTreeSet<RepositoryAlias> {
        match CreateOffers::new(Arc::clone(&self.operations))
            .offered(config)
            .await
        {
            Ok(offered) => offered,
            Err(error) => {
                log::warn!(
                    "the operation log could not be read, so no repository is offered to be created: {}",
                    error.code()
                );
                std::collections::BTreeSet::new()
            }
        }
    }
}

/// A listed snapshot as the page is told of it, with what the engine cut
/// from it said.
fn snapshot_dto(snapshot: &SnapshotSummary) -> Result<api::SnapshotDto, api::InvalidSnapshot> {
    api::SnapshotDto::new(
        snapshot.id.as_str().to_owned(),
        snapshot.time.to_string(),
        snapshot.hostname.clone(),
        snapshot.label.clone(),
        snapshot.tags.clone(),
        snapshot.paths.clone(),
    )
    .map(|dto| {
        dto.truncated(snapshot.paths_truncated, snapshot.tags_truncated)
            .taken_at_offset(snapshot.offset)
    })
}

/// Tests, connects and initialises repositories for the API (§4.2): each
/// request's settings become domain values, run through the use cases of
/// `app`, and every refusal becomes a typed one. Writes to the configuration
/// go one at a time, so two connections made at the same moment are both
/// kept.
struct RepositoryService {
    clock: Arc<dyn Clock>,
    engine: Arc<dyn Engine>,
    /// The network link a Test evaluates the locations' rules against.
    link: Arc<dyn NetworkLink>,
    resolve: ResolveRepository,
    config: Arc<dyn ConfigStore>,
    /// The operation log, which says whether a saved repository's row
    /// offers Create (decision 53).
    operations: Arc<dyn OperationStore>,
    /// Held across a use case's read-modify-write of the configuration, and
    /// shared with the record of an opened repository's ID.
    writes: Arc<ConfigWrites>,
}

impl RepositoryService {
    fn new(
        clock: Arc<dyn Clock>,
        engine: Arc<dyn Engine>,
        link: Arc<dyn NetworkLink>,
        resolve: ResolveRepository,
        config: Arc<dyn ConfigStore>,
        operations: Arc<dyn OperationStore>,
        writes: Arc<ConfigWrites>,
    ) -> Self {
        Self {
            clock,
            engine,
            link,
            resolve,
            config,
            operations,
            writes,
        }
    }
}

#[async_trait]
impl Repositories for RepositoryService {
    async fn test(
        &self,
        request: api::TestRepositoryDto,
    ) -> Result<api::RepositoryIdDto, RepositoriesError> {
        let settings = settings_of(request.settings)?;
        let id = match TestRepository::new(
            Arc::clone(&self.clock),
            Arc::clone(&self.engine),
            self.resolve.clone(),
        )
        .test(new_operation(), &settings)
        .await
        {
            Ok(value) => value,
            Err(error) => return Err(refusal_of(error, &settings).await),
        };
        Ok(api::RepositoryIdDto {
            id: id.as_str().to_owned(),
        })
    }

    async fn connect(
        &self,
        request: api::ConnectRepositoryDto,
    ) -> Result<api::RepositoryIdDto, RepositoriesError> {
        // The name first: a repository that can't be saved isn't tested.
        let alias =
            RepositoryAlias::parse(&request.alias).map_err(|_| RepositoriesError::AliasInvalid)?;
        let settings = settings_of(request.settings)?;
        // Asked for, this agent maintains it; otherwise nobody is asked, for
        // a repository that exists may already be maintained elsewhere (I1).
        let maintenance = if request.maintain_here {
            Maintenance::Local
        } else {
            Maintenance::None
        };
        let _writing = self.writes.lock().await;
        let id = match ConnectRepository::new(
            Arc::clone(&self.clock),
            Arc::clone(&self.engine),
            self.resolve.clone(),
            Arc::clone(&self.config),
        )
        .connect(new_operation(), alias, &settings, maintenance)
        .await
        {
            Ok(value) => value,
            Err(error) => return Err(refusal_of(error, &settings).await),
        };
        Ok(api::RepositoryIdDto {
            id: id.as_str().to_owned(),
        })
    }

    async fn init(
        &self,
        request: api::InitRepositoryDto,
    ) -> Result<api::RepositoryInitDto, RepositoriesError> {
        // The name first: a repository that was created and then can't be
        // saved is one the person has no way to find again.
        let alias = request
            .alias
            .as_deref()
            .map(RepositoryAlias::parse)
            .transpose()
            .map_err(|_| RepositoriesError::AliasInvalid)?;
        let settings = settings_of(request.settings)?;
        // Only a saving initialisation writes the configuration.
        let _writing = match alias {
            Some(_) => Some(self.writes.lock().await),
            None => None,
        };
        let initialised = match InitRepository::new(
            Arc::clone(&self.clock),
            Arc::clone(&self.engine),
            self.resolve.clone(),
            Arc::clone(&self.config),
        )
        .init(new_operation(), &settings, alias)
        .await
        {
            Ok(value) => value,
            Err(error) => return Err(refusal_of(error, &settings).await),
        };
        Ok(api::RepositoryInitDto {
            id: initialised.id.as_str().to_owned(),
            saved: initialised.saved,
        })
    }

    async fn create_saved(&self, alias: String) -> Result<api::RepositoryIdDto, RepositoriesError> {
        // A name that is no alias is one the configuration can't hold.
        let Ok(parsed) = RepositoryAlias::parse(&alias) else {
            return Err(RepositoriesError::Unknown { alias });
        };
        // Create records the new repository's ID in its entry, and whether
        // the row offers it is read under the same lock, so two Creates
        // can't both find it offered.
        let _writing = self.writes.lock().await;
        let saved = match InitRepository::new(
            Arc::clone(&self.clock),
            Arc::clone(&self.engine),
            self.resolve.clone(),
            Arc::clone(&self.config),
        )
        .init_saved(
            new_operation(),
            &parsed,
            &CreateOffers::new(Arc::clone(&self.operations)),
        )
        .await
        {
            Ok(saved) => saved,
            // Before the entry is found and its row known to offer Create,
            // nothing ran: the configuration can't be read, it doesn't hold
            // the alias, or the operation log can't be read.
            Err(SavedEntryError::Config(error)) => return Err(config_refusal(error)),
            Err(SavedEntryError::Unknown(_)) => return Err(RepositoriesError::Unknown { alias }),
            // The log's own words say what it couldn't do, as maintenance
            // tells it.
            Err(SavedEntryError::Log(
                app::ports::StoreError::Unavailable { detail }
                | app::ports::StoreError::Corrupt { detail },
            )) => {
                return Err(unavailable(
                    ErrorCode::OperationFailed,
                    Vec::new(),
                    Some(detail),
                ));
            }
        };
        match saved.created {
            Ok(id) => Ok(api::RepositoryIdDto {
                id: id.as_str().to_owned(),
            }),
            Err(error) => Err(refusal_of(error, &saved.settings).await),
        }
    }

    async fn test_saved(&self, alias: String) -> Result<api::LocationsTestDto, RepositoriesError> {
        // A name that is no alias is one the configuration can't hold.
        let Ok(parsed) = RepositoryAlias::parse(&alias) else {
            return Err(RepositoriesError::Unknown { alias });
        };
        let config = self.config.load().await.map_err(config_refusal)?;
        let Some(entry) = config.repositories().get(&parsed).cloned() else {
            return Err(RepositoriesError::Unknown { alias });
        };
        // Probes go through the daemon's own engine, which hands a probe
        // straight to a worker; the log hears what a job's choice tells it.
        let saved = app::ports::SavedEntry {
            alias: parsed,
            entry,
        };
        let tested = ChooseLocation::new(
            Arc::clone(&self.engine),
            Arc::clone(&self.clock),
            Arc::clone(&self.link),
            self.resolve.clone(),
            Arc::new(log_location),
        )
        .test_each(new_operation(), &saved)
        .await;
        match tested {
            Ok(record) => Ok(api::LocationsTestDto {
                locations: locations_tried(&record),
            }),
            Err(error) => Err(refusal_of(error, &saved.entry.settings).await),
        }
    }
}

/// Every location of a choice or a Test, as the wire carries it: by
/// position, the address masked as recorded, and the outcome's code.
fn locations_tried(record: &LocationsRecord) -> Vec<api::LocationTriedDto> {
    record
        .locations
        .iter()
        .map(|tried| api::LocationTriedDto {
            position: tried.position,
            address: tried.address.clone(),
            outcome: tried.outcome.code().to_owned(),
            source: tried.source.clone(),
        })
        .collect()
}

/// A fresh operation ID: a UUIDv7, as every operation of the daemon has.
fn new_operation() -> OperationId {
    OperationId::from_uuid(Uuid::now_v7())
}

/// A request's settings as domain values, read as the configuration reader
/// reads the same keys: a password file is an absolute path, and a keyring
/// entry is `service/name`. A typed password and a location (hot ones too)
/// are not empty. What it can't take is an invalid body, except a setting
/// that names a path on this machine by a relative one, which is refused at
/// that setting.
fn settings_of(dto: api::RepositorySettingsDto) -> Result<RepositorySettings, RepositoriesError> {
    let empty_password =
        matches!(&dto.password, api::PasswordDto::Inline { value } if value.is_empty());
    if empty_password || dto.repository.is_empty() || dto.repo_hot.as_deref() == Some("") {
        return Err(RepositoriesError::InvalidBody);
    }
    let password = match dto.password {
        api::PasswordDto::Inline { value } => SecretRef::Inline(SecretValue::new(value)),
        api::PasswordDto::File { path } if path.starts_with('/') => SecretRef::File {
            path,
            read: FileRead::FirstLine,
        },
        api::PasswordDto::File { path } if path.is_empty() => {
            return Err(RepositoriesError::InvalidBody);
        }
        api::PasswordDto::File { .. } => return Err(path_relative(Field::Password)),
        api::PasswordDto::Keyring { entry } => SecretRef::Keyring(
            KeyringEntry::parse(&entry).map_err(|_| RepositoriesError::InvalidBody)?,
        ),
        api::PasswordDto::Env { name } => SecretRef::Env(name),
    };
    let settings = RepositorySettings {
        repository: dto.repository,
        repo_hot: dto.repo_hot,
        password,
        options: dto.options,
        options_hot: dto.options_hot,
        options_cold: dto.options_cold,
    };
    // What the configuration would refuse when it loads next is not saved,
    // nor tested: a request can't take more than a job does. A value over
    // its cap is refused for that first, as the configuration refuses it.
    settings
        .check_sizes()
        .map_err(|_| RepositoriesError::InvalidBody)?;
    match settings.relative_path() {
        Some(field) => Err(path_relative(match field {
            RepositoryField::Repository => Field::Repository,
            RepositoryField::RepoHot => Field::RepoHot,
            // A request has no password command; the field is the password.
            RepositoryField::PasswordCommand => Field::Password,
            RepositoryField::Option(name) => Field::Option(name),
            RepositoryField::OptionHot(name) => Field::OptionHot(name),
            RepositoryField::OptionCold(name) => Field::OptionCold(name),
        })),
        None => Ok(settings),
    }
}

/// The refusal of a relative path at `field`, named as the configuration
/// names it.
fn path_relative(field: Field) -> RepositoriesError {
    RepositoriesError::PathRelative {
        field: field.name(),
    }
}

/// Why a repository use case failed, as the typed refusal the route answers
/// with. `settings` are the request's: a location the refusal names is shown
/// redacted.
async fn refusal_of(error: RepositoryError, settings: &RepositorySettings) -> RepositoriesError {
    match error {
        RepositoryError::Exists => exists(settings),
        RepositoryError::CreateNotOffered => RepositoriesError::CreateNotOffered,
        RepositoryError::AliasTaken(alias) => RepositoriesError::AliasTaken {
            alias: alias.as_str().to_owned(),
        },
        RepositoryError::AlreadyConfigured(alias) => RepositoriesError::AlreadyConfigured {
            alias: alias.as_str().to_owned(),
        },
        RepositoryError::Resolve(error) => RepositoriesError::SecretRefused(
            secret_refusal(&error, settings, &SystemSecrets::default()).await,
        ),
        RepositoryError::Engine(EngineError::Unavailable { detail }) => {
            unavailable(ErrorCode::EngineUnavailable, Vec::new(), Some(detail))
        }
        RepositoryError::Engine(EngineError::StartTimedOut) => {
            unavailable(ErrorCode::EngineStartTimedOut, Vec::new(), None)
        }
        RepositoryError::Engine(EngineError::SecretsTooLargeToMask { field, limit }) => {
            RepositoriesError::SecretRefused(too_much_to_mask(field, limit))
        }
        RepositoryError::Failed(reason) => failed_refusal(reason, settings),
        RepositoryError::Config(error) => config_refusal(error),
        RepositoryError::TimedOut => unavailable(ErrorCode::RepositoryTimedOut, Vec::new(), None),
    }
}

/// Secrets that come to more than a job masks, as the secret refusal the
/// routes answer with: the cap, and the setting where they passed it, when
/// one is known (§10.2). Neither is a secret or tells one (I4).
pub(crate) fn too_much_to_mask(field: Option<String>, limit: usize) -> SecretRefusal {
    let mut args = vec![(
        "limit",
        MsgArg::Bytes(u64::try_from(limit).unwrap_or(u64::MAX)),
    )];
    args.extend(field.map(|field| ("field", MsgArg::Text(field))));
    SecretRefusal {
        code: ErrorCode::SecretsTooLargeToMask,
        args,
        detail: None,
    }
}

fn unavailable(
    code: ErrorCode,
    args: Vec<(&'static str, MsgArg)>,
    detail: Option<String>,
) -> RepositoriesError {
    RepositoriesError::Unavailable { code, args, detail }
}

fn exists(settings: &RepositorySettings) -> RepositoriesError {
    RepositoriesError::Exists {
        location: redacted_location(settings, &settings.repository),
    }
}

/// Says in the daemon's log what reading a set of secrets warned of: a file
/// other users can read, by its path, masked of the secrets read with it,
/// and its mode, never the secret (#386, I4). Each job, snapshot listing
/// and notification that reads secrets tells it; a re-read that only masks
/// a refusal doesn't (§10.2).
fn log_secret_warning(warning: &SecretWarning) {
    match warning {
        SecretWarning::FileReadableByOthers { path, mode } => log::warn!(
            "{:?}: other users of this machine can read the secret file {path} \
             (permissions {mode:04o}); run `chmod 600` on it so that only its \
             owner can read it",
            WarningCode::SecretFileReadable
        ),
    }
}

/// A secret that couldn't be resolved, as the typed refusal of its cause
/// ([`ResolveError::refusal`]). What a platform or a command said is masked
/// of every secret of the repository first (I4), read again through
/// `resolver`. A refusal that shows nothing a source said reads nothing: a
/// location with too many credentials, or a reference that would reach a
/// process's arguments, is refused before any secret is read (§10.2).
async fn secret_refusal(
    error: &ResolveError,
    settings: &RepositorySettings,
    resolver: &dyn SecretResolver,
) -> SecretRefusal {
    refusal_with(error, &refusal_redactor(error, settings, resolver).await)
}

/// [`secret_refusal`], with what `redactor` masks.
fn refusal_with(error: &ResolveError, redactor: &Redactor) -> SecretRefusal {
    match error.refusal(redactor) {
        Ok(refusal) => {
            let (code, args, detail) = app::announce::refused(&error.field.name(), &refusal);
            SecretRefusal { code, args, detail }
        }
        Err(limit) => too_much_to_mask(Some(error.field.name()), limit),
    }
}

/// What masks the texts of `error`'s refusal: only a secret that couldn't be
/// read has any, where it was looked for and what was said, so only then are
/// the repository's secrets read again ([`failure_redactor`]).
async fn refusal_redactor(
    error: &ResolveError,
    settings: &RepositorySettings,
    resolver: &dyn SecretResolver,
) -> Redactor {
    match error.cause {
        Cause::Unresolved(_) => failure_redactor(settings, resolver).await,
        Cause::Invalid(_)
        | Cause::ExpansionTooLong { .. }
        | Cause::ReferenceInProcessArguments
        | Cause::TooManyCredentials { .. }
        | Cause::TooMuchToMask { .. }
        | Cause::RelativePath => Redactor::new([]),
    }
}

/// How long a secret is read again for a redactor: a source that does not
/// answer in this time adds nothing, and a failure doesn't wait for it.
const REREAD_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

/// A secret read again, for a redactor: `None` when it can't be read, since
/// what can't be read can't show in what a failure says.
async fn read_again(resolver: &dyn SecretResolver, secret: &SecretRef) -> Option<SecretValue> {
    tokio::time::timeout(REREAD_LIMIT, resolver.resolve(secret))
        .await
        .ok()?
        .ok()
        .map(|found| found.value)
}

/// A redactor for what a failure of `settings` says: its inline password and
/// credential options, and what the references of its settings, and a
/// password read from the environment or a file, resolve to through
/// `resolver`. A failed resolution stops at the first fault, so the other
/// secrets of the repository were not read yet: they are read again here, so
/// that a command that prints one of them in its error doesn't show it. A
/// command is not run a second time. Once what was read again passes what a
/// redactor masks, nothing more is read and the redactor masks every text
/// whole, since a secret not read could be in it (I4).
async fn failure_redactor(
    settings: &RepositorySettings,
    resolver: &dyn SecretResolver,
) -> Redactor {
    let mut resolved = Vec::new();
    let mut masked = MaskedSecrets::default();
    if matches!(
        settings.password,
        SecretRef::Env(_) | SecretRef::File { .. }
    ) && let Some(value) = read_again(resolver, &settings.password).await
    {
        if masked.secret(&value).is_err() {
            return Redactor::masking_everything();
        }
        resolved.push(value);
    }
    let texts = std::iter::once(&settings.repository)
        .chain(&settings.repo_hot)
        .chain(settings.options.values())
        .chain(settings.options_hot.values())
        .chain(settings.options_cold.values());
    // Each distinct reference once, as the resolution reads it.
    let mut references = std::collections::BTreeSet::new();
    for text in texts {
        if let Ok(template) = Template::parse(text) {
            for part in template.parts() {
                if let Part::Reference(reference) = part {
                    references.insert(reference.clone());
                }
            }
        }
    }
    for reference in references {
        if let Some(value) = read_again(resolver, &reference.secret_ref()).await {
            if masked.secret(&value).is_err() {
                return Redactor::masking_everything();
            }
            resolved.push(value);
        }
    }
    let none = SecretValue::new("");
    Redactor::for_repository(RepositorySecrets {
        repository: &settings.repository,
        repo_hot: settings.repo_hot.as_deref(),
        password: settings.password.inline().unwrap_or(&none),
        options: &settings.options,
        options_hot: &settings.options_hot,
        options_cold: &settings.options_cold,
        resolved: &resolved,
    })
}

/// A job that ran and failed: the engine's own words for a failure it
/// reports, a typed code for the rest. The engine redacts what it says.
fn failed_refusal(reason: FailureReason, settings: &RepositorySettings) -> RepositoriesError {
    use FailureReason as F;
    match reason {
        F::EngineFailed { detail } => RepositoriesError::OperationFailed { detail },
        F::EngineUnavailable { detail } | F::JobRejected { detail } => {
            unavailable(ErrorCode::EngineUnavailable, Vec::new(), Some(detail))
        }
        F::EngineExited { exit_code } => unavailable(
            ErrorCode::EngineExited,
            exit_code
                .map(|code| ("exit_code", text(code)))
                .into_iter()
                .collect(),
            None,
        ),
        F::ProtocolViolated { transition } => unavailable(
            ErrorCode::ProtocolViolated,
            vec![("transition", text(format!("{transition:?}")))],
            None,
        ),
        F::DeadlinePassed => unavailable(ErrorCode::DeadlinePassed, Vec::new(), None),
        F::EngineStartTimedOut => unavailable(ErrorCode::EngineStartTimedOut, Vec::new(), None),
        F::EngineExitTimedOut => unavailable(ErrorCode::EngineExitTimedOut, Vec::new(), None),
        // Only a backup's before-hook stops a job this way: a repository job
        // runs no hooks. It is still a failure, in the catalog's plainest words.
        F::HookFailed { .. } => unavailable(ErrorCode::OperationFailed, Vec::new(), None),
        F::RepositoryExists => exists(settings),
        F::RepositoryReplaced => unavailable(ErrorCode::RepositoryReplaced, Vec::new(), None),
        F::RepositoryNotFound => RepositoriesError::NotFound,
        F::CredentialsRefused => unavailable(ErrorCode::CredentialsRefused, Vec::new(), None),
        F::SecretsTooLargeToMask { field, limit } => {
            RepositoriesError::SecretRefused(too_much_to_mask(field, limit))
        }
        F::DaemonStopped => unavailable(ErrorCode::DaemonStopped, Vec::new(), None),
        F::NoLocationUsable => unavailable(ErrorCode::NoLocationUsable, Vec::new(), None),
        F::NoLocationRuleHolds => unavailable(ErrorCode::NoLocationRuleHolds, Vec::new(), None),
        F::PasswordNotAccepted => unavailable(ErrorCode::PasswordNotAccepted, Vec::new(), None),
        // Only a run the schedule started records a refusal (§5): a
        // repository job is refused before it runs, as `secret_refusal` says.
        F::SecretRefused { field, refusal } => {
            let (code, args, detail) = app::announce::refused(&field, &refusal);
            RepositoriesError::SecretRefused(SecretRefusal { code, args, detail })
        }
    }
}

/// A configuration that couldn't be read or written, refused by a repository
/// route.
fn config_refusal(error: ConfigError) -> RepositoriesError {
    config_unusable(error, unavailable)
}

/// A configuration that couldn't be read or written, as the code and the
/// arguments its message declares, which `unavailable` makes a route's
/// refusal of: every route that reads the file says the same of it (#347).
/// The store never puts a value in what it reports (`ConfigError`), so its
/// words can be shown.
fn config_unusable<R>(
    error: ConfigError,
    unavailable: impl FnOnce(ErrorCode, Vec<(&'static str, MsgArg)>, Option<String>) -> R,
) -> R {
    match error {
        ConfigError::Unavailable { file, detail } => unavailable(
            ErrorCode::ConfigUnavailable,
            vec![("file", text(file))],
            Some(detail),
        ),
        ConfigError::TooLarge { file, limit } => unavailable(
            ErrorCode::ConfigTooLarge,
            vec![("file", text(file)), ("limit", MsgArg::Bytes(limit))],
            None,
        ),
        ConfigError::SchemaMissing { file } => unavailable(
            ErrorCode::ConfigSchemaMissing,
            vec![("file", text(file))],
            None,
        ),
        ConfigError::SchemaNewer {
            file,
            found,
            supported,
        } => unavailable(
            ErrorCode::ConfigSchemaNewer,
            vec![
                ("file", text(file)),
                ("found", text(found)),
                ("supported", text(supported)),
            ],
            None,
        ),
        ConfigError::Invalid {
            file,
            key,
            at,
            detail,
            ..
        } => {
            let mut args = vec![("file", text(file))];
            if let Some(at) = at {
                args.push(("line", text(at.line)));
            }
            let detail = match (key, detail.is_empty()) {
                (Some(key), false) => Some(format!("{key}: {detail}")),
                (Some(key), true) => Some(key),
                (None, false) => Some(detail),
                (None, true) => None,
            };
            unavailable(ErrorCode::ConfigInvalid, args, detail)
        }
    }
}

/// A configured entry as the repositories list shows it: its row, with the
/// secrets each further location writes as text by its position; for an
/// entry reached at several locations, each location in order with its
/// rule; a warning for each location that sends its credentials where an
/// impostor could read them, by position, cause and whether it has a rule,
/// never the address; and the notes about what the locations' rules mean
/// for `plans`, the plans that back up to it (decision 80). The warnings
/// and notes of a repository of one location say so, for its row lists no
/// locations.
fn listed_repository(
    alias: &RepositoryAlias,
    repository: &RepositoryConfig,
    plans: &[&Plan],
    reads_names: bool,
    create_offered: bool,
) -> api::RepositoryDto {
    let mut row = repository_row(
        alias,
        &repository.settings,
        repository.maintenance,
        create_offered,
    );
    let dto = |code, args: Vec<(&'static str, MsgArg)>| api::WarningDto {
        code,
        args: args
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    };
    // The row holds the first location's secrets already.
    row.warnings.extend(
        inline_secret_warnings(repository)
            .into_iter()
            .filter(|(position, _, _)| *position > 1)
            .map(|(_, code, args)| dto(code, args)),
    );
    row.warnings
        .extend(repository.location_warnings().into_iter().map(|warning| {
            dto(
                api::WarningCode::LocationPlainHttpCredentials,
                exposed_args(repository, warning),
            )
        }));
    let locations = || ("locations".to_owned(), text(repository.locations_code()));
    row.warnings.extend(
        repository
            .rule_notes(plans, reads_names)
            .into_iter()
            .map(|note| {
                let (code, args) = match note {
                    RuleNote::EveryLocationRuled => {
                        (api::WarningCode::LocationEveryRuled, vec![locations()])
                    }
                    RuleNote::LastRuledWithoutRetry { plan } => (
                        api::WarningCode::LocationLastRuledNoRetry,
                        vec![("plan".to_owned(), text(plan.as_str())), locations()],
                    ),
                    RuleNote::NameUnreadable { position } => (
                        api::WarningCode::LocationRuleNameUnreadable,
                        vec![("position".to_owned(), text(position)), locations()],
                    ),
                };
                api::WarningDto {
                    code,
                    args: args.into_iter().collect(),
                }
            }),
    );
    if repository.has_several_locations() {
        let first = Location {
            when: repository.locations.first_when.clone(),
            ..Location::of(&repository.settings)
        };
        row.locations = std::iter::once(first)
            .chain(repository.locations.further.iter().cloned())
            .zip(1u8..)
            .map(|(location, position)| listed_location(&location, position))
            .collect();
    }
    row
}

/// One location of an entry as the list shows it: its address masked, and
/// each option by name, with its value only when that is references alone.
fn listed_location(location: &Location, position: u8) -> api::LocationDto {
    let options = [
        ("options", &location.options),
        ("options-hot", &location.options_hot),
        ("options-cold", &location.options_cold),
    ]
    .into_iter()
    .flat_map(|(set, options)| {
        options
            .iter()
            .map(move |(name, value)| api::LocationOptionDto {
                set: set.to_owned(),
                name: name.clone(),
                reference: domain::secrets::only_references(value).then(|| value.clone()),
            })
    })
    .collect();
    api::LocationDto {
        position,
        repository: location.masked_address(),
        repo_hot: location.masked_repo_hot(),
        options,
        when: location
            .when
            .iter()
            .flat_map(LocationRule::alternatives)
            .map(|alternative| {
                let names = |filter: Option<&NameFilter>| {
                    filter.map_or_default(|filter| {
                        filter
                            .names()
                            .iter()
                            .map(|name| name.as_str().to_owned())
                            .collect()
                    })
                };
                let filter = alternative.names();
                api::LocationAlternativeDto {
                    network: alternative.kind().as_str().to_owned(),
                    only: names(filter.filter(|filter| matches!(filter, NameFilter::Only(_)))),
                    except: names(filter.filter(|filter| matches!(filter, NameFilter::Except(_)))),
                }
            })
            .collect(),
    }
}

/// One configured repository as a list shows it: its locations masked, and
/// where its password comes from, never the password (I4). `create_offered`
/// is whether its row offers to create it (decision 53).
fn repository_row(
    alias: &RepositoryAlias,
    settings: &RepositorySettings,
    maintenance: domain::planning::Maintenance,
    create_offered: bool,
) -> api::RepositoryDto {
    api::RepositoryDto {
        alias: alias.as_str().to_owned(),
        maintained_here: domain::maintenance::Ownership::owned_here(maintenance),
        create_offered,
        repository: redacted_location(settings, &settings.repository),
        repo_hot: settings
            .repo_hot
            .as_deref()
            .map(|hot| redacted_location(settings, hot)),
        password: match &settings.password {
            SecretRef::Inline(_) => api::PasswordSourceDto::Inline,
            SecretRef::File { path, .. } => api::PasswordSourceDto::File { path: path.clone() },
            SecretRef::Keyring(entry) => api::PasswordSourceDto::Keyring {
                entry: entry.to_string(),
            },
            SecretRef::Env(name) => api::PasswordSourceDto::Env { name: name.clone() },
            // The program only: its arguments may hold the secret itself.
            SecretRef::Command(command) => api::PasswordSourceDto::Command {
                program: command.program().to_owned(),
            },
        },
        warnings: settings
            .inline_secrets()
            .iter()
            .map(|secret| {
                let (code, args) = inline_secret_warning(settings, secret, 1);
                api::WarningDto {
                    code,
                    args: args
                        .into_iter()
                        .map(|(name, value)| (name.to_owned(), value))
                        .collect(),
                }
            })
            .collect(),
        locations: Vec::new(),
    }
}

/// A location of `settings`, as it may be shown: with the password a URL
/// holds masked. The typed password is not masked: it isn't in a location,
/// and a location that happens to spell it is still a location.
fn redacted_location(settings: &RepositorySettings, text: &str) -> String {
    Redactor::for_repository(RepositorySecrets {
        repository: &settings.repository,
        repo_hot: settings.repo_hot.as_deref(),
        password: &SecretValue::new(""),
        options: &settings.options,
        options_hot: &settings.options_hot,
        options_cold: &settings.options_cold,
        resolved: &[],
    })
    .redact(text)
}

impl CancelRegistry {
    fn new() -> Self {
        Self {
            running: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The map, recovered even from a poisoned lock: the alternative —
    /// handing out a receiver whose sender is gone — would cancel every
    /// operation that starts next.
    fn running(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<OperationId, futures::channel::oneshot::Sender<()>>>
    {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers `id` as running, returning the future its run ends on when
    /// the API cancels it.
    fn register(&self, id: OperationId) -> futures::channel::oneshot::Receiver<()> {
        let (sender, receiver) = futures::channel::oneshot::channel();
        self.running().insert(id, sender);
        receiver
    }

    /// Cancels `id`: `true` when it was running and is now cancelled, `false`
    /// when nothing by that ID is running, so there is nothing to cancel.
    /// A second cancel of the same operation is `false`: the one handle is
    /// spent.
    fn cancel(&self, id: &OperationId) -> bool {
        self.running()
            .remove(id)
            .is_some_and(|sender| sender.send(()).is_ok())
    }

    /// The operation ended, cancelled or not: its handle, fired or spent, is
    /// dropped, so a later cancel is a typed refusal.
    fn finish(&self, id: &OperationId) {
        let _ = self.running().remove(id);
    }
}

/// What the daemon logs, for the tests that look at the log: the warnings it
/// gives (`lines`) and what it says at info level (`infos`). `log` takes one
/// logger for a process, so the tests of every module that reads the log
/// share this one, and each reads only the lines about what it started.
#[cfg(test)]
mod captured_log {
    use std::sync::{Mutex, PoisonError};

    struct Captured;

    static LOGGED: Mutex<Vec<(log::Level, String)>> = Mutex::new(Vec::new());

    impl log::Log for Captured {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }
        fn log(&self, record: &log::Record<'_>) {
            LOGGED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push((record.level(), record.args().to_string()));
        }
        fn flush(&self) {}
    }

    /// Starts capturing, before the code that logs runs.
    pub(super) fn capture() {
        let _ = log::set_logger(&Captured);
        log::set_max_level(log::LevelFilter::Info);
    }

    /// Every line logged so far at warning level or above, with its level.
    pub(super) fn lines() -> Vec<(log::Level, String)> {
        LOGGED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(level, _)| *level <= log::Level::Warn)
            .cloned()
            .collect()
    }

    /// Every line logged so far at info level.
    pub(super) fn infos() -> Vec<String> {
        LOGGED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(level, _)| *level == log::Level::Info)
            .map(|(_, line)| line.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::assert_matches;

    use super::*;
    use app::ports::{HookContext, HookOutcome, HookStatus, JobControl, ProgressEvent};

    fn listed(paths_truncated: bool, tags_truncated: bool) -> SnapshotSummary {
        SnapshotSummary {
            id: domain::SnapshotId::parse(&"ab".repeat(32)).expect("a snapshot ID"),
            time: domain::Timestamp::from_second(1_800_000_000).expect("a time"),
            offset: domain::Offset::UTC,
            hostname: "demo-host".to_owned(),
            label: "home".to_owned(),
            tags: vec!["demo".to_owned()],
            paths: vec!["/home/demo".to_owned()],
            paths_truncated,
            tags_truncated,
        }
    }

    #[test]
    fn a_listed_snapshot_tells_the_page_what_was_cut_from_it() {
        let dto = snapshot_dto(&listed(true, false)).expect("a valid snapshot");
        assert!(dto.paths_truncated && !dto.tags_truncated);
        let dto = snapshot_dto(&listed(false, true)).expect("a valid snapshot");
        assert!(!dto.paths_truncated && dto.tags_truncated);
        let dto = snapshot_dto(&listed(false, false)).expect("a valid snapshot");
        assert!(!dto.paths_truncated && !dto.tags_truncated);
        assert_eq!(dto.label, "home");
    }

    #[test]
    fn a_listed_snapshot_tells_the_page_the_offset_of_the_clock_that_took_it() {
        let chicago = domain::Offset::from_seconds(-6 * 3600).expect("an offset");
        let dto = snapshot_dto(&SnapshotSummary {
            offset: chicago,
            ..listed(false, false)
        })
        .expect("a valid snapshot");
        assert_eq!(dto.offset, Some(api::ClockOffset::new(chicago)));
        assert_eq!(dto.time, "2027-01-15T08:00:00Z", "the instant, in UTC");
    }

    /// A succeeded operation of `kind` as the log keeps it, with `result`
    /// and the alias of its repository.
    fn logged(
        kind: OperationKind,
        result: Option<EngineResult>,
        alias: Option<&str>,
    ) -> OperationRecord {
        let at =
            |second: i64| domain::Timestamp::from_second(1_800_000_000 + second).expect("a time");
        OperationRecord {
            id: OperationId::from_uuid(Uuid::from_u128(0x0192)),
            kind,
            trigger: Trigger::Manual,
            target: domain::execution::Target {
                plan: None,
                repository: Some(demo_repository()),
            },
            status: OperationStatus::Succeeded,
            queued_at: at(0),
            started_at: Some(at(10)),
            finished_at: Some(at(70)),
            warning_count: 0,
            warning_kinds: Vec::new(),
            result,
            repository_alias: alias.map(|alias| RepositoryAlias::parse(alias).expect("an alias")),
            locations: None,
        }
    }

    fn backed_up(bytes_processed: Option<u64>) -> EngineResult {
        EngineResult::Backup(app::ports::BackupSummary {
            repository: demo_repository(),
            snapshot: domain::SnapshotId::parse(&"cd".repeat(32)).expect("a snapshot ID"),
            parent: None,
            files_new: 1,
            files_changed: 2,
            files_unmodified: 3,
            bytes_processed,
            data_added: 4096,
            data_added_packed: 1024,
            error_count: 0,
        })
    }

    #[test]
    fn a_maintenance_row_names_its_repository_by_the_alias_it_ran_under() {
        let check = logged(OperationKind::Check, None, Some("demo"));
        let dto = summary(&check).expect("a row the page can show");
        assert_eq!(dto.repository.as_deref(), Some("demo"));
        // A row logged before the alias was kept names none.
        let old = logged(OperationKind::Check, None, None);
        assert_eq!(summary(&old).expect("a row").repository, None);
    }

    /// The page names an operation's warnings by their codes, each once,
    /// as the log keeps their kinds (#508).
    #[test]
    fn a_row_names_its_warnings_by_code() {
        let mut warned = logged(OperationKind::Backup, None, None);
        warned.status = OperationStatus::SucceededWithWarnings;
        warned.warning_count = 3;
        warned.warning_kinds = vec![WarningKind::SourceUnreadable, WarningKind::CheckWarning];
        assert_eq!(
            summary(&warned)
                .expect("a row the page can show")
                .warning_codes,
            [WarningCode::SourceUnreadable, WarningCode::CheckWarning]
        );
    }

    #[test]
    fn a_backup_row_carries_its_snapshot_and_the_bytes_it_went_through() {
        let backup = logged(OperationKind::Backup, Some(backed_up(Some(65_536))), None);
        let dto = summary(&backup).expect("a row the page can show");
        assert_eq!(
            dto.result,
            Some(api::OperationResultDto::Backup {
                files: 6,
                bytes: 4096,
                duration_ms: 60_000,
                snapshot: Some("cd".repeat(32)),
                bytes_processed: Some(65_536),
            })
        );
        // A result logged before the bytes were kept shows none, not zero.
        let old = logged(OperationKind::Backup, Some(backed_up(None)), None);
        let Some(api::OperationResultDto::Backup {
            bytes_processed, ..
        }) = summary(&old).expect("a row").result
        else {
            panic!("a backup's result");
        };
        assert_eq!(bytes_processed, None);
    }

    #[test]
    fn a_backups_file_count_stops_at_the_largest_number_it_can_hold() {
        // The engine's counts are its own: three that overflow when added
        // are shown as the most a count holds, never a panic of the read.
        let EngineResult::Backup(mut huge) = backed_up(None) else {
            panic!("a backup's result");
        };
        huge.files_new = u64::MAX;
        let backup = logged(
            OperationKind::Backup,
            Some(EngineResult::Backup(huge)),
            None,
        );
        let Some(api::OperationResultDto::Backup { files, .. }) =
            summary(&backup).expect("a row").result
        else {
            panic!("a backup's result");
        };
        assert_eq!(files, u64::MAX);
    }

    /// A failed row says why, in the engine's own words, which the engine
    /// masked; a row that didn't fail has no failure.
    #[test]
    fn a_failed_operations_row_carries_its_failure_in_the_engines_words() {
        let mut backup = logged(OperationKind::Backup, None, None);
        backup.status = OperationStatus::Failed {
            reason: FailureReason::EngineFailed {
                detail: "no key opens the repository".to_owned(),
            },
        };
        assert_eq!(
            summary(&backup).expect("a row the page can show").failure,
            Some(api::ErrorDto::new(
                ErrorCode::OperationFailed,
                [],
                Some("no key opens the repository".to_owned())
            ))
        );
        let succeeded = logged(OperationKind::Backup, Some(backed_up(None)), None);
        assert_eq!(summary(&succeeded).expect("a row").failure, None);
    }

    /// `/api/v1` is published (§12.1): the failure is a field added beside
    /// `failure-code`, which keeps its name and value, and a row that didn't
    /// fail still says `failure-code: null` and has no `failure`.
    #[test]
    fn a_failed_operations_row_keeps_its_failure_code_beside_the_failure() {
        let mut backup = logged(OperationKind::Backup, None, None);
        backup.status = OperationStatus::Failed {
            reason: FailureReason::EngineExited { exit_code: Some(3) },
        };
        let row = summary(&backup).expect("a row the page can show");
        let wire = serde_json::to_value(&row).expect("a row is JSON");
        assert_eq!(
            wire.get("failure-code"),
            Some(&serde_json::json!("engine-exited")),
            "{wire}"
        );
        assert_eq!(
            wire.get("failure"),
            Some(&serde_json::json!({
                "code": "engine-exited",
                "args": {"exit_code": {"type": "text", "value": "3"}},
            })),
            "{wire}"
        );
        let succeeded = logged(OperationKind::Backup, Some(backed_up(None)), None);
        let wire = serde_json::to_value(summary(&succeeded).expect("a row")).expect("JSON");
        assert_eq!(wire.get("failure-code"), Some(&serde_json::Value::Null));
        assert_eq!(wire.get("failure"), None, "{wire}");
    }

    fn demo_settings(password: SecretRef) -> RepositorySettings {
        RepositorySettings {
            repository: "s3:s3.example.net/demo".to_owned(),
            repo_hot: None,
            password,
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        }
    }

    fn field_warning(field: &str) -> api::WarningDto {
        api::WarningDto {
            code: WarningCode::ConfigInlineSecret,
            args: [("field".to_owned(), MsgArg::Text(field.to_owned()))].into(),
        }
    }

    #[test]
    fn a_listed_repository_with_secrets_in_the_config_carries_a_warning_for_each_without_the_value()
    {
        let (password, key) = ("not-a-real-password", "not-a-real-key");
        let mut settings = demo_settings(SecretRef::Inline(SecretValue::new(password)));
        settings
            .options_cold
            .insert("secret_access_key".to_owned(), key.to_owned());
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let row = repository_row(
            &alias,
            &settings,
            domain::planning::Maintenance::Local,
            false,
        );
        assert_eq!(
            row.warnings,
            [
                field_warning("password"),
                field_warning("options-cold.secret_access_key")
            ]
        );
        let wire = serde_json::to_string(&row).expect("a row is JSON");
        assert!(
            !wire.contains(password) && !wire.contains(key),
            "the row repeats a secret: {wire}"
        );
    }

    #[test]
    fn a_listed_repository_whose_secrets_are_read_from_elsewhere_has_no_warning() {
        let mut settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        settings
            .options
            .insert("secret_access_key".to_owned(), "${DEMO_KEY}".to_owned());
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        assert_eq!(
            repository_row(
                &alias,
                &settings,
                domain::planning::Maintenance::Local,
                false
            )
            .warnings,
            []
        );
    }

    #[test]
    fn a_listed_repository_with_a_password_in_its_location_is_warned_of_by_the_setting_never_the_password()
     {
        let (cold, hot) = ("CANARY-cold-password", "CANARY-hot-password");
        let mut settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        settings.repository = format!("rest:https://demo:{cold}@backup.example.com/nas");
        settings.repo_hot = Some(format!("rest:https://demo:{hot}@hot.example.com/nas"));
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let row = repository_row(
            &alias,
            &settings,
            domain::planning::Maintenance::Local,
            false,
        );
        let location = |field: &str| api::WarningDto {
            code: WarningCode::LocationSecret,
            args: [
                ("field".to_owned(), MsgArg::Text(field.to_owned())),
                ("kind".to_owned(), MsgArg::Text("url".to_owned())),
            ]
            .into(),
        };
        assert_eq!(row.warnings, [location("repository"), location("repo-hot")]);
        let wire = serde_json::to_string(&row).expect("a row is JSON");
        let shown = row_warnings(&row);
        // The row shows the location masked (`demo:***@`), but never the
        // password; the warning names only the setting, not the user either.
        assert!(
            !wire.contains(cold) && !wire.contains(hot),
            "the row repeats a password: {wire}"
        );
        for said in &shown {
            assert!(
                !said.contains(cold) && !said.contains(hot) && !said.contains("demo"),
                "a warning repeats the credentials: {said}"
            );
        }
        assert!(
            shown[0].contains("The address in the repository setting holds a password as text")
                && shown[1]
                    .contains("The address in the repo-hot setting holds a password as text"),
            "{shown:?}"
        );
    }

    /// rclone is handed the location in its arguments, where a `${…}`
    /// reference is refused (§10.2), so the warning sends the credential to
    /// rclone's own configuration file instead.
    #[test]
    fn a_listed_repository_with_a_credential_in_its_rclone_location_is_sent_to_rclones_configuration()
     {
        let pass = "CANARY-rclone-pass";
        let mut settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        settings.repository = format!("rclone::sftp,host=nas.example.com,pass={pass}:/backups");
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let row = repository_row(
            &alias,
            &settings,
            domain::planning::Maintenance::Local,
            false,
        );
        assert_eq!(
            row.warnings,
            [api::WarningDto {
                code: WarningCode::LocationSecret,
                args: [
                    ("field".to_owned(), MsgArg::Text("repository".to_owned())),
                    ("kind".to_owned(), MsgArg::Text("rclone".to_owned())),
                ]
                .into(),
            }]
        );
        let shown = row_warnings(&row);
        assert!(
            shown[0].contains("rclone's own configuration file")
                && shown[0].contains("rclone:remote:path"),
            "{shown:?}"
        );
        assert!(
            !shown[0].contains("${") && !shown[0].contains(pass),
            "{shown:?}"
        );
    }

    #[test]
    fn a_listed_repository_shows_its_rclone_locations_without_the_credentials_written_in_them() {
        let mut settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        settings.repository =
            "rclone::sftp,host=nas.example.com,pass='CANARY-rclone-cold,x:y':/backups".to_owned();
        settings.repo_hot =
            Some("rclone::b2,account=demo,key=CANARY-rclone-hot:demo-bucket".to_owned());
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let row = repository_row(
            &alias,
            &settings,
            domain::planning::Maintenance::Local,
            false,
        );
        assert_eq!(
            row.repository,
            "rclone::sftp,host=nas.example.com,pass=***:/backups"
        );
        assert_eq!(
            row.repo_hot.as_deref(),
            Some("rclone::b2,account=demo,key=***:demo-bucket")
        );
        let wire = serde_json::to_string(&row).expect("a row is JSON");
        assert!(
            !wire.contains("CANARY") && !wire.contains("x:y"),
            "the row repeats a credential: {wire}"
        );
    }

    /// What a listed repository's warnings say, in English.
    fn row_warnings(row: &api::RepositoryDto) -> Vec<String> {
        row.warnings
            .iter()
            .map(|warning| {
                let args: Vec<(&str, MsgArg)> = warning
                    .args
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.clone()))
                    .collect();
                english()
                    .warning(warning.code, &args, None)
                    .text()
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn a_listed_repository_whose_location_has_no_password_written_in_it_has_no_warning() {
        let mut settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        for location in [
            "rest:https://demo@backup.example.com/nas",
            "rest:https://demo:${DEMO_REST_PASSWORD}@backup.example.com/nas",
        ] {
            settings.repository = location.to_owned();
            let alias = RepositoryAlias::parse("demo").expect("an alias");
            assert_eq!(
                repository_row(
                    &alias,
                    &settings,
                    domain::planning::Maintenance::Local,
                    false
                )
                .warnings,
                [],
                "{location}"
            );
        }
    }

    #[test]
    fn a_listed_repository_says_whether_this_agent_maintains_it() {
        let settings = demo_settings(SecretRef::Env("DEMO_PASSWORD".to_owned()));
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        for (maintenance, here) in [
            (domain::planning::Maintenance::Local, true),
            (domain::planning::Maintenance::Hub, false),
            (domain::planning::Maintenance::None, false),
        ] {
            let row = repository_row(&alias, &settings, maintenance, false);
            assert_eq!(row.maintained_here, here, "{maintenance:?}");
            let wire = serde_json::to_value(&row).expect("a row is JSON");
            assert_eq!(
                wire["maintained-here"],
                serde_json::Value::Bool(here),
                "{maintenance:?}"
            );
        }
    }

    /// An entry `nas` reached at `rest:http://lan.example.lan/r` with a user
    /// and password written in it and a key by reference, then at
    /// `/srv/public` with a key written in the file.
    fn two_locations() -> RepositoryConfig {
        let mut first = demo_settings(SecretRef::Env("NAS_PASSWORD".to_owned()));
        first.repository = "rest:http://lan:CANARY-url-pass@lan.example.lan/r".to_owned();
        first
            .options
            .insert("application_key".to_owned(), "${LAN_KEY}".to_owned());
        RepositoryConfig {
            locations: domain::planning::Locations {
                first_when: None,
                further: vec![Location {
                    repository: "/srv/public".to_owned(),
                    options_cold: [(
                        "secret_access_key".to_owned(),
                        "CANARY-written-key".to_owned(),
                    )]
                    .into(),
                    ..Location::default()
                }],
            },
            id: None,
            settings: first,
            maintenance: Maintenance::Local,
            group_by: Default::default(),
            hooks: Default::default(),
        }
    }

    #[test]
    fn a_listed_repository_lists_its_locations_in_order_with_references_and_never_a_written_secret()
    {
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let row = listed_repository(&alias, &two_locations(), &[], true, false);
        let option = |set: &str, name: &str, reference: Option<&str>| api::LocationOptionDto {
            set: set.to_owned(),
            name: name.to_owned(),
            reference: reference.map(str::to_owned),
        };
        assert_eq!(
            row.locations,
            vec![
                api::LocationDto {
                    position: 1,
                    repository: "rest:http://lan:***@lan.example.lan/r".to_owned(),
                    repo_hot: None,
                    options: vec![option("options", "application_key", Some("${LAN_KEY}"))],
                    when: Vec::new(),
                },
                api::LocationDto {
                    position: 2,
                    repository: "/srv/public".to_owned(),
                    repo_hot: None,
                    options: vec![option("options-cold", "secret_access_key", None)],
                    when: Vec::new(),
                },
            ]
        );
        let wire = serde_json::to_string(&row).expect("a row is JSON");
        assert!(!wire.contains("CANARY"), "the row shows a secret: {wire}");
    }

    #[test]
    fn a_listed_repository_warns_of_a_plain_http_location_and_says_whether_it_has_a_rule() {
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let warning = |rule: &str| api::WarningDto {
            code: WarningCode::LocationPlainHttpCredentials,
            args: [
                ("position".to_owned(), MsgArg::Text("1".to_owned())),
                ("cause".to_owned(), MsgArg::Text("plain-http".to_owned())),
                ("rule".to_owned(), MsgArg::Text(rule.to_owned())),
                ("locations".to_owned(), MsgArg::Text("several".to_owned())),
            ]
            .into(),
        };
        let mut entry = two_locations();
        let row = listed_repository(&alias, &entry, &[], true, false);
        assert!(
            row.warnings.contains(&warning("none")),
            "{:?}",
            row.warnings
        );
        entry.locations.first_when = Some(
            LocationRule::new(vec![domain::planning::NetworkKind::Wired.into()]).expect("a rule"),
        );
        let row = listed_repository(&alias, &entry, &[], true, false);
        assert!(
            row.warnings.contains(&warning("network")),
            "{:?}",
            row.warnings
        );
    }

    #[test]
    fn a_listed_repository_shows_each_locations_rule_and_notes_what_the_rules_mean() {
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let mut entry = two_locations();
        let wired =
            LocationRule::new(vec![domain::planning::NetworkKind::Wired.into()]).expect("a rule");
        entry.locations.first_when = Some(wired.clone());
        entry.locations.further[0].when = Some(
            LocationRule::new(vec![
                domain::planning::NetworkKind::WiFi.into(),
                domain::planning::NetworkKind::Wired.into(),
            ])
            .expect("a rule"),
        );
        let mut plan = Plan::new(
            PlanId::parse("home").expect("a plan ID"),
            alias.clone(),
            Vec::new(),
        );
        plan.schedule = Some(domain::planning::Schedule::new(
            domain::planning::Interval::parse("1d").expect("an interval"),
        ));
        let row = listed_repository(&alias, &entry, &[&plan], true, false);
        let when: Vec<Vec<&str>> = row
            .locations
            .iter()
            .map(|location| {
                location
                    .when
                    .iter()
                    .map(|kind| kind.network.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(when, vec![vec!["wired"], vec!["wifi", "wired"]]);
        let note = |code, args: &[(&str, &str)]| api::WarningDto {
            code,
            args: args
                .iter()
                .map(|(name, value)| ((*name).to_owned(), MsgArg::Text((*value).to_owned())))
                .collect(),
        };
        for expected in [
            note(WarningCode::LocationEveryRuled, &[("locations", "several")]),
            note(
                WarningCode::LocationLastRuledNoRetry,
                &[("plan", "home"), ("locations", "several")],
            ),
        ] {
            assert!(
                row.warnings.contains(&expected),
                "{expected:?} in {:?}",
                row.warnings
            );
        }
        assert!(
            listed_repository(&alias, &entry, &[], true, false)
                .warnings
                .iter()
                .all(|warning| warning.code != WarningCode::LocationLastRuledNoRetry),
            "no plan: no note about retries"
        );
    }

    #[test]
    fn a_listed_rule_names_its_networks_and_is_noted_where_names_cant_be_read() {
        use domain::planning::{Alternative, NetworkKind, NetworkName};
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let names = |names: &[&str]| {
            names
                .iter()
                .map(|name| NetworkName::parse(name).expect("a name"))
                .collect()
        };
        let wifi = |filter| {
            LocationRule::new(vec![
                Alternative::new(NetworkKind::WiFi, filter).expect("an alternative"),
            ])
            .expect("a rule")
        };
        let mut entry = two_locations();
        entry.locations.first_when = Some(wifi(Some(NameFilter::Only(names(&["Office", "Lab"])))));
        entry.locations.further[0].when = Some(wifi(Some(NameFilter::Except(names(&["Guest"])))));
        let row = listed_repository(&alias, &entry, &[], false, false);
        let when: Vec<(Vec<String>, Vec<String>)> = row
            .locations
            .iter()
            .flat_map(|location| location.when.iter())
            .map(|alternative| (alternative.only.clone(), alternative.except.clone()))
            .collect();
        let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        assert_eq!(
            when,
            vec![
                (owned(&["Office", "Lab"]), Vec::new()),
                (Vec::new(), owned(&["Guest"])),
            ]
        );
        let unreadable: Vec<&api::WarningDto> = row
            .warnings
            .iter()
            .filter(|warning| warning.code == WarningCode::LocationRuleNameUnreadable)
            .collect();
        assert_eq!(
            unreadable
                .iter()
                .map(|warning| warning.args.get("position"))
                .collect::<Vec<_>>(),
            [
                Some(&MsgArg::Text("1".to_owned())),
                Some(&MsgArg::Text("2".to_owned())),
            ]
        );
        assert!(
            listed_repository(&alias, &entry, &[], true, false)
                .warnings
                .iter()
                .all(|warning| warning.code != WarningCode::LocationRuleNameUnreadable),
            "a machine that reads the name: no note"
        );
        let wire = serde_json::to_value(&row).expect("a row is JSON");
        assert_eq!(
            wire.pointer("/locations/0/when/0/except"),
            None,
            "no empty list on the wire: {wire}"
        );
    }

    #[test]
    fn a_listed_repository_of_one_location_lists_no_locations() {
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let mut entry = two_locations();
        entry.locations = Default::default();
        let row = listed_repository(&alias, &entry, &[], true, false);
        assert_eq!(row.locations, Vec::new());
        let wire = serde_json::to_value(&row).expect("a row is JSON");
        assert_eq!(wire.get("locations"), None, "{wire}");
    }

    /// The page lists no locations for a repository of one, so neither its
    /// warning nor its notes name a location by its position or a rule the
    /// page doesn't show (#473), and the warning is given with no rule as
    /// with one (#506).
    #[test]
    fn a_listed_repository_of_one_location_is_warned_and_noted_as_a_repository() {
        use domain::planning::{Alternative, NetworkKind, NetworkName};
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let mut entry = two_locations();
        entry.locations = Default::default();
        let row = listed_repository(&alias, &entry, &[], true, false);
        let exposed = api::WarningDto {
            code: WarningCode::LocationPlainHttpCredentials,
            args: [
                ("position".to_owned(), MsgArg::Text("1".to_owned())),
                ("cause".to_owned(), MsgArg::Text("plain-http".to_owned())),
                ("rule".to_owned(), MsgArg::Text("none".to_owned())),
                ("locations".to_owned(), MsgArg::Text("one".to_owned())),
            ]
            .into(),
        };
        assert!(row.warnings.contains(&exposed), "{:?}", row.warnings);

        let office = NetworkName::parse("Office").expect("a name");
        entry.locations.first_when = Some(
            LocationRule::new(vec![
                Alternative::new(NetworkKind::WiFi, Some(NameFilter::Only(vec![office])))
                    .expect("an alternative"),
            ])
            .expect("a rule"),
        );
        let mut plan = Plan::new(
            PlanId::parse("home").expect("a plan ID"),
            alias.clone(),
            Vec::new(),
        );
        plan.schedule = Some(domain::planning::Schedule::new(
            domain::planning::Interval::parse("1d").expect("an interval"),
        ));
        let row = listed_repository(&alias, &entry, &[&plan], false, false);
        for code in [
            WarningCode::LocationPlainHttpCredentials,
            WarningCode::LocationEveryRuled,
            WarningCode::LocationLastRuledNoRetry,
            WarningCode::LocationRuleNameUnreadable,
        ] {
            let warning = row
                .warnings
                .iter()
                .find(|warning| warning.code == code)
                .unwrap_or_else(|| panic!("no {code:?} in {:?}", row.warnings));
            assert_eq!(
                warning.args.get("locations"),
                Some(&MsgArg::Text("one".to_owned())),
                "{code:?}"
            );
        }
    }

    /// The log names a run by what started it, in words (#505).
    #[test]
    fn a_run_is_named_in_the_log_by_what_started_it_in_words() {
        for (trigger, words) in [
            (Trigger::Schedule, "the scheduled run"),
            (Trigger::Manual, "the manual run"),
            (Trigger::CatchUp, "the catch-up run"),
            (Trigger::Retry, "the retry"),
            (Trigger::Hub, "the hub's run"),
            (Trigger::Mcp, "the MCP client's run"),
        ] {
            assert_eq!(run_named(trigger), words, "{trigger:?}");
        }
    }

    /// A secret written as text in a further location is pointed out as one
    /// in the first is, by the setting and the location's position, never
    /// the secret (#505).
    #[test]
    fn a_listed_repository_points_out_a_secret_written_in_a_further_location() {
        let alias = RepositoryAlias::parse("nas").expect("an alias");
        let mut entry = two_locations();
        entry.locations.further[0].repo_hot =
            Some("rest:https://demo:CANARY-hot-pass@hot.example.lan/r".to_owned());
        let row = listed_repository(&alias, &entry, &[], true, false);
        let warning = |code, args: &[(&str, &str)]| api::WarningDto {
            code,
            args: args
                .iter()
                .map(|(name, value)| ((*name).to_owned(), MsgArg::Text((*value).to_owned())))
                .collect(),
        };
        for expected in [
            warning(
                WarningCode::LocationSecret,
                &[("field", "repository"), ("kind", "url")],
            ),
            warning(
                WarningCode::LocationSecret,
                &[("field", "repo-hot"), ("kind", "url"), ("position", "2")],
            ),
            warning(
                WarningCode::ConfigInlineSecret,
                &[
                    ("field", "options-cold.secret_access_key"),
                    ("position", "2"),
                ],
            ),
        ] {
            assert!(
                row.warnings.contains(&expected),
                "{expected:?} in {:?}",
                row.warnings
            );
        }
        let shown = format!("{row:?}");
        for secret in ["CANARY-url-pass", "CANARY-written-key", "CANARY-hot-pass"] {
            assert!(!shown.contains(secret), "{shown}");
        }
    }

    /// What a repository of one location resolves to is said of the
    /// repository, never of "location 1" (#473).
    #[test]
    fn the_log_says_a_repository_of_one_location_exposes_its_credentials_naming_no_location() {
        captured_log::capture();
        log_location(LocationReport::ExposedCredentials {
            alias: RepositoryAlias::parse("solo-exposed").expect("an alias"),
            position: None,
            cause: domain::planning::ExposedCredentials::PlainHttp,
        });
        let said: Vec<_> = captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains("solo-exposed"))
            .collect();
        let [(level, line)] = said.as_slice() else {
            panic!("one line names the repository: {said:?}");
        };
        assert_eq!(*level, log::Level::Warn);
        assert!(
            line.contains(
                "repository solo-exposed: this repository sends its credentials where whoever answers at its address can read them (plain-http)"
            ),
            "{line}"
        );
        assert!(!line.contains("location"), "{line}");
    }

    #[test]
    fn an_operations_summary_carries_the_locations_its_choice_saw() {
        use domain::execution::locations::LocationTried;
        let tried = |position, address: &str, outcome| LocationTried {
            position,
            address: address.to_owned(),
            outcome,
            source: None,
        };
        let record = LocationsRecord {
            locations: vec![
                tried(1, "/srv/lan", LocationOutcome::Silent),
                tried(2, "/srv/public", LocationOutcome::Used),
            ],
        };
        let summary = api::OperationSummaryDto::new(
            "018f0000-0000-7000-8000-000000000001".to_owned(),
            "home".to_owned(),
            "backup".to_owned(),
            "succeeded".to_owned(),
            "2026-11-12T10:00:00Z".to_owned(),
            None,
            None,
            Vec::new(),
            None,
            None,
            None,
        )
        .expect("a summary")
        .with_locations(locations_tried(&record));
        assert_eq!(
            summary.used_location().map(|(used, count)| (
                used.position,
                used.address.as_str(),
                count
            )),
            Some((2, "/srv/public", 2))
        );
        let wire = serde_json::to_value(&summary).expect("a summary is JSON");
        assert_eq!(
            wire["locations"],
            serde_json::json!([
                {"position": 1, "address": "/srv/lan", "outcome": "silent"},
                {"position": 2, "address": "/srv/public", "outcome": "used"},
            ])
        );
    }

    /// Empty scratch folders the daemon is pointed at: no configuration has
    /// ever been written to the first.
    fn own_folders(scratch: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let config = scratch.join("config");
        let data = scratch.join("data");
        std::fs::create_dir_all(&config).expect("the config folder");
        std::fs::create_dir_all(&data).expect("the data folder");
        (config, data)
    }

    /// A backup that was running when the daemon that ran it stopped.
    fn running_backup(id: OperationId) -> domain::execution::Operation {
        let mut backup = domain::execution::Operation::queue(
            id,
            OperationKind::Backup,
            Trigger::Manual,
            domain::execution::Target {
                plan: None,
                repository: None,
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        );
        backup
            .start(domain::Timestamp::from_second(1_800_000_001).expect("a time"))
            .expect("the backup starts");
        backup
    }

    /// A disk that never answers, as a mount whose server is down.
    struct Stalled;

    #[async_trait]
    impl NormalizeSources for Stalled {
        async fn normalized(
            &self,
            _: &[domain::planning::Source],
        ) -> Result<Vec<domain::planning::Source>, app::ports::SourcesError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_look_at_the_sources_that_never_answers_is_given_up() {
        // One look to walk the sources, then one per source for the holder.
        let limit = std::time::Duration::from_millis(50);
        let mut plan = Plan::new(
            domain::PlanId::parse("home").expect("an ID"),
            domain::RepositoryAlias::parse("nas").expect("an alias"),
            ["/home/user", "/srv/disk"]
                .map(|source| domain::planning::Source::parse(source).expect("a source"))
                .to_vec(),
        );
        plan.filter.globs = vec!["!docker/app/cache".to_owned()];
        let said = tokio::time::timeout(
            limit * 3 + std::time::Duration::from_millis(200),
            never_matching_glob_warnings(&plan, &WithinLimit(Stalled, limit)),
        )
        .await
        .expect("the look is given up");
        assert_eq!(
            said,
            [glob_never_matches("home", "!docker/app/cache", None)]
        );
    }

    #[tokio::test]
    async fn the_daemon_starts_with_no_configuration() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        // An agent that was never configured has nothing to do, and starts.
        let state = daemon_at(Some(config), Some(data), None).await;
        assert!(
            state.is_ok(),
            "an empty configuration serves: {:?}",
            state.err().map(|refused| refused.message)
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_workers_cache_folder_is_made_for_the_daemons_user_alone() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().expect("a scratch dir");
        let dir = scratch.path().join("cache").join("ladderback");
        assert_eq!(workers_cache(Some(dir.clone())), Some(dir.clone()));
        let mode = std::fs::metadata(&dir)
            .expect("the folder is made")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }

    #[test]
    fn a_cache_folder_that_cannot_be_made_is_said_once_and_left_out() {
        captured_log::capture();
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let file = scratch.path().join("a-file");
        std::fs::write(&file, b"").expect("the file in the way");
        let dir = file.join("cache");
        assert_eq!(workers_cache(Some(dir.clone())), None);
        let shown = dir.display().to_string();
        let said: Vec<_> = captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains(&shown))
            .collect();
        let [(level, line)] = said.as_slice() else {
            panic!("one line names the folder: {said:?}");
        };
        assert_eq!(*level, log::Level::Warn);
        assert!(line.contains("without a cache"), "{line}");
    }

    /// A folder that is there but can't be written to (another user's, or
    /// read-only) is found once at start, not by every job.
    #[cfg(unix)]
    #[test]
    fn a_cache_folder_that_cannot_be_written_to_is_said_once_with_the_reason_and_left_out() {
        use std::os::unix::fs::PermissionsExt;

        captured_log::capture();
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let dir = scratch.path().join("ladderback");
        std::fs::create_dir(&dir).expect("the cache folder");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500))
            .expect("a read-only cache folder");
        assert_eq!(workers_cache(Some(dir.clone())), None);
        let shown = dir.display().to_string();
        let said: Vec<_> = captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains(&shown))
            .collect();
        let [(level, line)] = said.as_slice() else {
            panic!("one line names the folder: {said:?}");
        };
        assert_eq!(*level, log::Level::Warn);
        assert!(line.contains("can't be written to"), "{line}");
        // EACCES, the OS's reason, as the OS words it.
        let reason = std::io::Error::from_raw_os_error(13).to_string();
        assert!(line.contains(&reason), "{line}");
    }

    /// A secret read from a file other users can read is said in the
    /// daemon's log, by the file's path masked of the secrets read with it
    /// and its mode, never the secret (#386, I4).
    #[cfg(unix)]
    #[tokio::test]
    async fn a_secret_file_others_can_read_is_logged_by_its_masked_path_and_mode() {
        use std::os::unix::fs::PermissionsExt;

        captured_log::capture();
        let scratch = tempfile::tempdir().expect("a scratch dir");
        // The folder is named as the secret is, by chance.
        let folder = scratch.path().join("demo-secret-value");
        std::fs::create_dir(&folder).expect("the folder");
        let path = folder.join("nas-password");
        std::fs::write(&path, b"demo-secret-value\n").expect("the password file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("the file's mode");
        let scratch_path = scratch.path().to_str().expect("a scratch path is text");
        let resolved = ResolveRepository::new(Arc::new(SystemSecrets::default()))
            .telling(Arc::new(log_secret_warning))
            .resolve(&demo_settings(SecretRef::File {
                path: path.to_str().expect("a scratch path is text").to_owned(),
                read: FileRead::FirstLine,
            }))
            .await
            .expect("the file is read");
        assert_eq!(
            resolved.access.password,
            SecretValue::new("demo-secret-value")
        );
        let shown = format!("{scratch_path}/***/nas-password");
        let said: Vec<_> = captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains(&shown))
            .collect();
        let [(level, line)] = said.as_slice() else {
            panic!("one line names the file: {said:?}");
        };
        assert_eq!(*level, log::Level::Warn);
        assert!(line.contains("SecretFileReadable"), "{line}");
        assert!(line.contains("0644"), "{line}");
        assert!(
            !captured_log::lines()
                .iter()
                .any(|(_, line)| line.contains("demo-secret-value")),
            "the secret is in the log"
        );
    }

    #[test]
    fn without_a_cache_folder_the_workers_run_without_one() {
        captured_log::capture();
        assert_eq!(workers_cache(None), None);
        assert!(
            captured_log::lines()
                .iter()
                .any(|(level, line)| *level == log::Level::Warn
                    && line.contains("LADDERBACK_CACHE_DIR")),
            "the log says how to give the workers one"
        );
    }

    #[tokio::test]
    async fn the_daemon_ends_what_a_crash_left_unfinished_before_it_hands_over_its_state() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let database = data.join("ladderback.db");
        // The log an earlier run left behind: a backup that was running when
        // that run stopped.
        let earlier = SqliteStore::open(&database).await.expect("the log opens");
        let id = OperationId::from_uuid(Uuid::from_u128(1));
        earlier
            .save(&running_backup(id), None)
            .await
            .expect("the running backup is saved");
        earlier.close().await;

        let daemon = daemon_at(Some(config), Some(data), None)
            .await
            .map_err(|refused| refused.message)
            .expect("the daemon starts");
        drop(daemon);

        let log = SqliteStore::open(&database)
            .await
            .expect("the log opens again");
        let detail = log
            .get(id)
            .await
            .expect("the log reads")
            .expect("the backup is still in the log");
        assert_eq!(
            detail.record.status,
            OperationStatus::Failed {
                reason: FailureReason::DaemonStopped
            }
        );
    }

    /// Leaves at `database` a log that shows `id` as a running backup and
    /// refuses every change to its operations, as a failing disk would.
    async fn a_log_that_refuses_changes(database: &std::path::Path, id: OperationId) {
        use sqlx::{ConnectOptions, Connection};
        let earlier = SqliteStore::open(database).await.expect("the log opens");
        earlier
            .save(&running_backup(id), None)
            .await
            .expect("the running backup is saved");
        earlier.close().await;
        let mut raw = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(database)
            .connect()
            .await
            .expect("a raw connection to the log");
        sqlx::query(
            "CREATE TRIGGER refuse_changes BEFORE UPDATE ON operations
             BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&mut raw)
        .await
        .expect("the log is made to refuse changes");
        raw.close().await.expect("the connection closes");
    }

    /// Fails unless the operation log at `database` is closed: SQLite removes
    /// its `-wal` and `-shm` files when the last connection to it closes.
    fn assert_closed(database: &std::path::Path) {
        for suffix in ["-wal", "-shm"] {
            let mut file = database.as_os_str().to_owned();
            file.push(suffix);
            let file = std::path::PathBuf::from(file);
            assert!(
                !file.exists(),
                "{} outlived the daemon: the operation log is still open",
                file.display()
            );
        }
    }

    #[tokio::test]
    async fn a_log_that_cannot_be_recovered_stops_the_start_and_keeps_what_it_held() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let database = data.join("ladderback.db");
        let id = OperationId::from_uuid(Uuid::from_u128(1));
        a_log_that_refuses_changes(&database, id).await;

        let refused = daemon_at(Some(config), Some(data), None)
            .await
            .err()
            .expect("a log that cannot be changed refuses the start");
        assert_eq!(refused.message, msg::CLI_CANNOT_RECOVER_OPERATIONS);

        let log = SqliteStore::open(&database)
            .await
            .expect("the log opens again");
        let detail = log
            .get(id)
            .await
            .expect("the log reads")
            .expect("the backup is still in the log");
        assert_eq!(
            detail.record.status,
            OperationStatus::Running,
            "the log is as it was"
        );
    }

    #[tokio::test]
    async fn a_start_refused_once_the_log_is_open_closes_the_log_before_it_returns() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let database = data.join("ladderback.db");
        a_log_that_refuses_changes(&database, OperationId::from_uuid(Uuid::from_u128(1))).await;

        let refused = daemon_at(Some(config), Some(data), None)
            .await
            .err()
            .expect("a log that cannot be changed refuses the start");
        assert_eq!(refused.message, msg::CLI_CANNOT_RECOVER_OPERATIONS);
        assert_closed(&database);
    }

    #[tokio::test]
    async fn a_queued_row_that_cannot_be_read_is_logged_by_its_code_alone() {
        use sqlx::{ConnectOptions, Connection};
        super::captured_log::capture();
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let database = data.join("ladderback.db");
        let id = OperationId::from_uuid(Uuid::from_u128(0x274));
        let earlier = SqliteStore::open(&database).await.expect("the log opens");
        let queued = domain::execution::Operation::queue(
            id,
            OperationKind::Backup,
            Trigger::Manual,
            domain::execution::Target {
                plan: None,
                repository: None,
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        );
        earlier
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");
        earlier.close().await;
        // A value the row can't hold, which the store's own account of the
        // row repeats: it is the row's, so nobody has checked it (I4).
        let canary = "canary-7f3a-not-a-trigger";
        let mut raw = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&database)
            .connect()
            .await
            .expect("a raw connection to the log");
        sqlx::query("UPDATE operations SET trigger = ?")
            .bind(canary)
            .execute(&mut raw)
            .await
            .expect("the row is changed");
        raw.close().await.expect("the connection closes");

        let daemon = daemon_at(Some(config), Some(data), None)
            .await
            .map_err(|refused| refused.message)
            .expect("an unreadable queued row doesn't refuse the start");
        drop(daemon);

        let said: Vec<_> = super::captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains("in the operation log can't be read"))
            .collect();
        assert!(
            said.iter()
                .any(|(level, line)| *level == log::Level::Warn && line.contains("store-corrupt")),
            "the row is logged with the store's code: {said:?}"
        );
        assert!(
            said.iter().all(|(_, line)| !line.contains(canary)),
            "the row's own text never reaches the log: {said:?}"
        );
    }

    #[tokio::test]
    async fn a_close_that_ends_within_its_bound_is_waited_for_and_says_nothing() {
        super::captured_log::capture();
        let database = std::path::Path::new("/srv/demo/closes-in-time/ladderback.db");
        let (closed, mut close) = futures::channel::oneshot::channel::<()>();
        let closing = async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let _ = closed.send(());
        };
        close_within(closing, database, std::time::Duration::from_secs(5)).await;
        assert_eq!(
            close.try_recv(),
            Ok(Some(())),
            "the close ran to its end before close_within returned"
        );
        let said: Vec<_> = super::captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains("closes-in-time"))
            .collect();
        assert!(said.is_empty(), "a close in time logs nothing: {said:?}");
    }

    #[tokio::test]
    async fn a_close_that_outlasts_its_bound_is_given_up_with_a_warning() {
        super::captured_log::capture();
        let database = std::path::Path::new("/srv/demo/never-closes/ladderback.db");
        let bound = std::time::Duration::from_millis(50);
        let began = std::time::Instant::now();
        // Bounded itself, so a close_within that waited for ever fails here
        // rather than at the test runner's limit.
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            close_within(std::future::pending(), database, bound),
        )
        .await
        .expect("close_within gives up once its bound has run out");
        assert!(
            began.elapsed() >= bound,
            "it waited the bound out first, not {:?}",
            began.elapsed()
        );
        let said: Vec<_> = super::captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains("/srv/demo/never-closes/ladderback.db"))
            .collect();
        assert!(
            said.len() == 1 && said.iter().all(|(level, _)| *level == log::Level::Warn),
            "one warning names the log it gave up on: {said:?}"
        );
    }

    #[test]
    fn each_recording_is_logged_at_its_level_naming_only_the_alias_the_id_and_the_reason() {
        super::captured_log::capture();
        let id = "c".repeat(64);
        let parsed = || domain::RepositoryId::parse(&id).expect("a repository ID");
        let alias = |alias: &str| RepositoryAlias::parse(alias).expect("an alias");
        let cases = [
            (
                Recording::Recorded {
                    alias: alias("logged-recorded"),
                    id: parsed(),
                },
                log::Level::Info,
                format!(
                    "recorded the repository ID {id} in the entry logged-recorded of the configuration: a job opened it"
                ),
            ),
            (
                Recording::NotRecorded {
                    alias: alias("logged-changed"),
                    id: parsed(),
                    why: domain::planning::NotRecorded::Changed,
                },
                log::Level::Info,
                format!(
                    "did not record the repository ID {id} in the entry logged-changed of the configuration (Changed); the next job that opens it tries again"
                ),
            ),
            (
                Recording::NotRecorded {
                    alias: alias("logged-same"),
                    id: parsed(),
                    why: domain::planning::NotRecorded::SameRepository {
                        other: alias("logged-other"),
                    },
                },
                log::Level::Warn,
                format!(
                    "did not record the repository ID {id} in the entry logged-same of the configuration: the entry logged-other records it, so the two entries are one repository; remove one of them"
                ),
            ),
            (
                Recording::NotInPlace {
                    alias: alias("logged-form"),
                    id: parsed(),
                    why: app::ports::NotInPlace::Form,
                },
                log::Level::Warn,
                format!(
                    "did not record the repository ID {id} in the entry logged-form of the configuration: it can't be added without changing something else in the file (Form); the next job that opens it tries again"
                ),
            ),
            (
                Recording::Busy {
                    alias: alias("logged-busy"),
                    id: parsed(),
                },
                log::Level::Info,
                format!(
                    "did not record the repository ID {id} in the entry logged-busy of the configuration: another change to it was being saved; the next job that opens it tries again"
                ),
            ),
            (
                Recording::Failed {
                    alias: alias("logged-failed"),
                    id: parsed(),
                    error: ConfigError::Unavailable {
                        file: "/srv/demo/config.toml".to_owned(),
                        detail: "read-only file system".to_owned(),
                    },
                },
                log::Level::Warn,
                format!(
                    "did not record the repository ID {id} in the entry logged-failed of the configuration (Unavailable {{ file: \"/srv/demo/config.toml\", detail: \"read-only file system\" }}); the next job that opens it tries again"
                ),
            ),
        ];
        for (recording, level, line) in cases {
            let entry = match &recording {
                Recording::Recorded { alias, .. }
                | Recording::NotRecorded { alias, .. }
                | Recording::NotInPlace { alias, .. }
                | Recording::Busy { alias, .. }
                | Recording::Failed { alias, .. } => format!("entry {} ", alias.as_str()),
            };
            log_recording(recording);
            let said: Vec<_> = super::captured_log::lines()
                .into_iter()
                .chain(
                    super::captured_log::infos()
                        .into_iter()
                        .map(|line| (log::Level::Info, line)),
                )
                .filter(|(_, said)| said.contains(&entry))
                .collect();
            assert_eq!(said, vec![(level, line)]);
        }
    }

    #[tokio::test]
    async fn a_data_folder_another_process_holds_is_refused_before_its_log_is_opened() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        // Held as another process holds it, before any log exists.
        let held = DataDirLock::acquire(&data).expect("the folder is free");
        let refused = daemon_at(Some(config), Some(data.clone()), None)
            .await
            .err()
            .expect("a data folder another process holds refuses");
        assert_eq!(refused.message, msg::CLI_DATA_DIR_IN_USE);
        assert!(
            !data.join("ladderback.db").exists(),
            "a refused daemon never opens the log"
        );
        drop(held);
    }

    #[tokio::test]
    async fn a_newer_schema_is_a_typed_refusal() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        std::fs::write(config.join("config.toml"), "schema = 999999\n").expect("the config file");
        let refused = daemon_at(Some(config), Some(data), None)
            .await
            .err()
            .expect("a newer schema refuses to start");
        assert_eq!(refused.message, msg::CLI_CONFIG_SCHEMA_NEWER);
        assert!(
            refused
                .args
                .iter()
                .any(|(name, value)| *name == "found" && *value == text(999_999u64))
        );
    }

    #[tokio::test]
    async fn a_second_daemon_on_a_data_folder_is_refused_naming_the_first() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let first = daemon_at(Some(config.clone()), Some(data.clone()), None)
            .await
            .map_err(|refused| refused.message)
            .expect("the first daemon starts");
        let refused = daemon_at(Some(config), Some(data), None)
            .await
            .err()
            .expect("a data folder another daemon holds refuses");
        assert_eq!(refused.message, msg::CLI_DATA_DIR_IN_USE);
        assert!(
            refused
                .args
                .iter()
                .any(|(name, value)| *name == "pid" && *value == text(std::process::id())),
            "the refusal names the process that holds the folder"
        );
        drop(first);
    }

    #[tokio::test]
    async fn a_daemon_refused_the_data_folder_has_not_read_the_configuration() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let first = daemon_at(Some(config.clone()), Some(data.clone()), None)
            .await
            .map_err(|refused| refused.message)
            .expect("the first daemon starts");
        // A configuration only a newer build reads: a daemon that read it
        // would refuse it for that, and one that migrated an older file would
        // rewrite what the first daemon runs on.
        std::fs::write(config.join("config.toml"), "schema = 999999\n").expect("the config file");
        let refused = daemon_at(Some(config), Some(data), None)
            .await
            .err()
            .expect("a data folder another daemon holds refuses");
        assert_eq!(refused.message, msg::CLI_DATA_DIR_IN_USE);
        drop(first);
    }

    #[tokio::test]
    async fn a_data_folder_is_free_again_once_its_daemon_is_gone() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config, data) = own_folders(scratch.path());
        let first = daemon_at(Some(config.clone()), Some(data.clone()), None)
            .await
            .map_err(|refused| refused.message)
            .expect("the first daemon starts");
        drop(first);
        let second = daemon_at(Some(config), Some(data), None).await;
        assert!(
            second.is_ok(),
            "a folder nobody holds is served: {:?}",
            second.err().map(|refused| refused.message)
        );
    }

    #[test]
    fn a_lock_that_cannot_be_taken_is_refused_by_what_is_known_of_it() {
        let folder = std::path::Path::new("/srv/demo/data");
        let named = lock_refused(folder, LockError::InUse { holder: Some(4321) });
        assert_eq!(named.message, msg::CLI_DATA_DIR_IN_USE);
        assert_eq!(
            named.args,
            vec![("path", text("/srv/demo/data")), ("pid", text(4321))]
        );
        let unnamed = lock_refused(folder, LockError::InUse { holder: None });
        assert_eq!(unnamed.message, msg::CLI_DATA_DIR_IN_USE_UNNAMED);
        assert_eq!(unnamed.args, vec![("path", text("/srv/demo/data"))]);
        let unavailable = lock_refused(
            folder,
            LockError::Unavailable {
                detail: "Permission denied (os error 13)".to_owned(),
            },
        );
        assert_eq!(unavailable.message, msg::CLI_CANNOT_LOCK_DATA_DIR);
        assert_eq!(
            unavailable.args,
            vec![
                ("path", text("/srv/demo/data/ladderback.lock")),
                ("detail", text("Permission denied (os error 13)"))
            ]
        );
    }

    #[test]
    fn a_log_that_cannot_be_recovered_is_refused_with_the_stores_own_words() {
        for error in [
            app::ports::StoreError::Unavailable {
                detail: "disk I/O error".to_owned(),
            },
            app::ports::StoreError::Corrupt {
                detail: "operation 5: unknown status `bogus`".to_owned(),
            },
        ] {
            let (app::ports::StoreError::Unavailable { detail }
            | app::ports::StoreError::Corrupt { detail }) = &error;
            let refused =
                recovery_refused(std::path::Path::new("/srv/demo/data/ladderback.db"), &error);
            assert_eq!(refused.message, msg::CLI_CANNOT_RECOVER_OPERATIONS);
            assert_eq!(
                refused.args,
                vec![
                    ("path", text("/srv/demo/data/ladderback.db")),
                    ("detail", text(detail))
                ]
            );
        }
    }

    #[tokio::test]
    async fn a_running_operation_is_cancelled_once() {
        let registry = CancelRegistry::new();
        let id = OperationId::from_uuid(Uuid::new_v4());
        let cancel = registry.register(id);
        assert!(registry.cancel(&id), "the running operation cancels");
        let sleep = tokio::time::sleep(std::time::Duration::from_secs(5));
        tokio::pin!(sleep);
        tokio::select! {
            _ = cancel => {}
            _ = &mut sleep => panic!("cancelling fired the run's cancel future"),
        }
        assert!(!registry.cancel(&id), "one handle, one cancel");
        registry.finish(&id);
        assert!(
            !registry.cancel(&id),
            "a finished operation no longer cancels"
        );
    }

    /// A hook runner that reports a run has entered its before-hooks and
    /// parks there until the test opens the gate.
    struct GateHooks {
        entered: std::sync::Arc<tokio::sync::watch::Sender<u32>>,
        open: std::sync::Arc<tokio::sync::Mutex<tokio::sync::watch::Receiver<bool>>>,
    }

    impl GateHooks {
        fn new() -> (
            Self,
            tokio::sync::watch::Receiver<u32>,
            tokio::sync::watch::Sender<bool>,
        ) {
            let (entered_tx, entered_rx) = tokio::sync::watch::channel(0);
            let (open_tx, open_rx) = tokio::sync::watch::channel(false);
            let open_gate = open_tx.clone();
            let hooks = Self {
                entered: std::sync::Arc::new(entered_tx),
                open: std::sync::Arc::new(tokio::sync::Mutex::new(open_rx)),
            };
            (hooks, entered_rx, open_gate)
        }
    }

    #[async_trait]
    impl HookRunner for GateHooks {
        async fn run(
            &self,
            _entry: &domain::planning::HookEntry,
            _context: &HookContext,
        ) -> HookOutcome {
            self.entered.send_modify(|seen| *seen += 1);
            let mut open = self.open.lock().await;
            while !*open.borrow_and_update() {
                open.changed().await.expect("the gate stays open");
            }
            HookOutcome {
                status: HookStatus::Succeeded,
                stdout: Default::default(),
                stderr: Default::default(),
            }
        }
    }

    /// An engine whose jobs report nothing: the run parks, as a real backup
    /// does while it works, and stopping it is a no-op.
    struct ParkedEngine;

    #[async_trait]
    impl Engine for ParkedEngine {
        async fn run(&self, _op: OperationId, _job: Job) -> Result<RunningJob, EngineError> {
            Ok(RunningJob {
                events: futures::stream::pending().boxed(),
                control: Box::new(Parked),
            })
        }
    }

    struct Parked;

    #[async_trait]
    impl JobControl for Parked {
        async fn stop(&mut self) {}
    }

    /// A runner over scratch folders with one plan, "home", whose agent-level
    /// before-hook is the gate; its engine parks. Also what the gate says and
    /// opens, and the scratch folder, which must outlive the runner.
    async fn runner_at_a_gate() -> (
        PlanRunner,
        tokio::sync::watch::Receiver<u32>,
        tokio::sync::watch::Sender<bool>,
        tempfile::TempDir,
    ) {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config_dir, data_dir) = own_folders(scratch.path());
        let source = scratch.path().join("source");
        std::fs::create_dir_all(&source).expect("the source folder");
        let alias = domain::RepositoryAlias::parse("demo").expect("a repository alias");
        let settings = domain::planning::RepositorySettings {
            repository: scratch.path().join("repo").display().to_string(),
            repo_hot: None,
            password: domain::secrets::SecretRef::Inline(domain::secrets::SecretValue::new(
                "demo-password",
            )),
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        let hooks = domain::planning::Hooks {
            run_before: vec![domain::planning::HookEntry::parse("true").expect("a hook")],
            ..Default::default()
        };
        let config = domain::planning::AgentConfig::new(
            domain::planning::AgentSettings::default(),
            hooks,
            std::collections::BTreeMap::from([(
                alias.clone(),
                domain::planning::RepositoryConfig {
                    locations: Default::default(),
                    id: None,
                    settings,
                    // This agent maintains it, so a preview and a run reach
                    // the engine.
                    maintenance: domain::planning::Maintenance::Local,
                    group_by: Default::default(),
                    hooks: Default::default(),
                },
            )]),
            vec![domain::planning::Plan::new(
                domain::PlanId::parse("home").expect("a plan ID"),
                alias,
                vec![
                    domain::planning::Source::parse(source.display().to_string().as_str())
                        .expect("a source"),
                ],
            )],
        )
        .expect("a valid configuration");
        let (hooks, entered, open_gate) = GateHooks::new();
        let config_store = Arc::new(FileConfigStore::new(
            config_dir.join("config.toml"),
            Arc::new(SystemClock),
        ));
        app::ports::ConfigStore::save(&*config_store, &config)
            .await
            .expect("the config saves");
        let runner = PlanRunner {
            config: config_store,
            resolve: Arc::new(ResolveRepository::new(Arc::new(SystemSecrets::default()))),
            agent_id: agent_id::load_or_create(&data_dir).expect("an agent ID"),
            engine: Arc::new(ParkedEngine),
            locations: None,
            retention: Arc::new(engine::RusticRetention),
            store: Arc::new(
                SqliteStore::open(&data_dir.join("ladderback.db"))
                    .await
                    .expect("the store opens"),
            ),
            registry: Arc::new(CancelRegistry::new()),
            hooks: Arc::new(hooks),
            events: Arc::new(BroadcastEventBus::new()),
            queue: Arc::new(RunQueue::new()),
            ends: app::retry::run_ends().0,
        };
        (runner, entered, open_gate, scratch)
    }

    /// A cancel that arrives the instant the route answers finds the
    /// operation, not an "already ended" refusal (§4.2): the cancel switch
    /// is parked before the run starts, while the run is still in its
    /// before-hooks.
    #[tokio::test]
    async fn a_cancel_in_the_start_window_stops_the_operation() {
        let (runner, mut entered, open_gate, _scratch) = runner_at_a_gate().await;
        let id = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");
        // The run is in its before-hooks now, parked at the gate and not yet
        // at the engine, so the switch must already be parked for a cancel.
        entered.changed().await.expect("the run reaches its hooks");
        // The cancel is answered while the gate is still closed: a switch
        // parked any later than the route's answer is a refusal here.
        let cancel = runner.cancel(id).await;
        open_gate.send(true).expect("the gate opens");
        cancel.expect("a cancel in the start window is accepted");
    }

    /// A backup started while another runs waits in the queue for its turn
    /// (§5, the concurrency limit, 1 here): it is in the log as queued, and
    /// nothing of it runs, its hooks included, until the running one ends.
    #[tokio::test]
    async fn a_backup_started_while_another_runs_waits_for_its_turn() {
        let (runner, mut entered, open_gate, _scratch) = runner_at_a_gate().await;
        let first = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");
        entered
            .changed()
            .await
            .expect("the first run reaches its hooks");
        let second = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts again");
        let second_id = operation_of(&second).expect("an operation ID");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), entered.changed())
                .await
                .is_err(),
            "the second run reached its hooks while the first one ran"
        );
        assert_eq!(status_of(&runner, second_id).await, OperationStatus::Queued);

        open_gate.send(true).expect("the gate opens");
        runner
            .cancel(first)
            .await
            .expect("the first run is cancelled");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            entered.wait_for(|entries| *entries >= 2),
        )
        .await
        .expect("the second run takes its turn once the first one ends")
        .expect("the gate stays open");
        runner
            .cancel(second)
            .await
            .expect("the second run is cancelled");
    }

    /// The end the runner's next backup says, within a few seconds.
    async fn said_end(ends: &app::retry::RunEnds) -> Option<app::retry::RunEnd> {
        tokio::time::timeout(std::time::Duration::from_secs(5), ends.next())
            .await
            .expect("the run says how it ended")
    }

    /// A backup's run says how it ended, so the scheduler can retry a
    /// failed one (§5): here one started by hand, which a person cancels.
    #[tokio::test]
    async fn a_backup_started_now_says_how_it_ended() {
        let (mut runner, mut entered, open_gate, _scratch) = runner_at_a_gate().await;
        let (sender, ends) = app::retry::run_ends();
        runner.ends = sender;
        let started = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");
        let operation = operation_of(&started).expect("an operation ID");
        entered.changed().await.expect("the run reaches its hooks");
        open_gate.send(true).expect("the gate opens");
        runner.cancel(started).await.expect("the run is cancelled");
        assert_eq!(
            said_end(&ends).await,
            Some(app::retry::RunEnd {
                operation,
                plan: domain::PlanId::parse("home").expect("a plan ID"),
                trigger: Trigger::Manual,
                outcome: app::retry::RunOutcome::Cancelled,
            })
        );
    }

    /// The same for a backup the queue held from before the start.
    #[tokio::test]
    async fn a_resumed_backup_says_how_it_ended() {
        let (mut runner, mut entered, open_gate, _scratch) = runner_at_a_gate().await;
        let (sender, ends) = app::retry::run_ends();
        runner.ends = sender;
        let operation = OperationId::from_uuid(Uuid::from_u128(0x7275));
        let queued = domain::execution::Operation::queue(
            operation,
            OperationKind::Backup,
            Trigger::Retry,
            domain::execution::Target {
                plan: Some(domain::PlanId::parse("home").expect("a plan ID")),
                repository: None,
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        );
        runner
            .store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");
        PlanRuns::resume(&runner, queued).await;
        entered.changed().await.expect("the run reaches its hooks");
        open_gate.send(true).expect("the gate opens");
        runner
            .cancel(operation.as_uuid().hyphenated().to_string())
            .await
            .expect("the run is cancelled");
        assert_eq!(
            said_end(&ends).await,
            Some(app::retry::RunEnd {
                operation,
                plan: domain::PlanId::parse("home").expect("a plan ID"),
                trigger: Trigger::Retry,
                outcome: app::retry::RunOutcome::Cancelled,
            })
        );
    }

    /// A runner as [`runner_at_a_gate`] makes it, whose log holds a backup of
    /// "home" queued under `id`, over a configuration that can no longer be
    /// read. Also the queued backup, and the scratch folder.
    async fn a_queued_backup_over_a_spoiled_configuration(
        id: OperationId,
    ) -> (PlanRunner, domain::execution::Operation, tempfile::TempDir) {
        let (runner, _entered, _gate, scratch) = runner_at_a_gate().await;
        let queued = domain::execution::Operation::queue(
            id,
            OperationKind::Backup,
            Trigger::Manual,
            domain::execution::Target {
                plan: Some(domain::PlanId::parse("home").expect("a plan ID")),
                repository: None,
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        );
        runner
            .store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");
        std::fs::write(
            scratch.path().join("config").join("config.toml"),
            "not = [toml",
        )
        .expect("the configuration is spoiled");
        (runner, queued, scratch)
    }

    async fn status_of(runner: &PlanRunner, id: OperationId) -> OperationStatus {
        runner
            .store
            .get(id)
            .await
            .expect("the log reads")
            .expect("the backup is in the log")
            .record
            .status
    }

    /// A queued backup whose job can't be built at the restart ends as
    /// failed, and the log says why (decision 73): left queued, it would
    /// show as queued with nothing to run it until the next start. Here the
    /// configuration becomes unreadable between the background task's read
    /// and the runner's own.
    #[tokio::test]
    async fn a_queued_backup_ends_as_daemon_stopped_when_the_configuration_cannot_be_read() {
        super::captured_log::capture();
        let id = OperationId::from_uuid(Uuid::from_u128(0x2741));
        let (runner, queued, _scratch) = a_queued_backup_over_a_spoiled_configuration(id).await;

        PlanRuns::resume(&runner, queued).await;

        assert_eq!(
            status_of(&runner, id).await,
            OperationStatus::Failed {
                reason: FailureReason::DaemonStopped
            }
        );
        let hyphenated = id.as_uuid().hyphenated().to_string();
        let said = super::captured_log::lines();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains(&hyphenated)
                && line.contains("the configuration can't be read")
                && line.contains("(config-invalid)")),
            "the log says why the backup ended: {said:?}"
        );
    }

    /// The same when the background task's own read of the configuration
    /// fails: no job can be built, so every queued backup ends as failed,
    /// and the log says why.
    #[tokio::test]
    async fn the_queue_ends_as_daemon_stopped_when_the_background_task_cannot_read_the_configuration()
     {
        super::captured_log::capture();
        let id = OperationId::from_uuid(Uuid::from_u128(0x2742));
        let (runner, queued, _scratch) = a_queued_backup_over_a_spoiled_configuration(id).await;

        super::resume_queued(
            vec![OperationRecord::of(&queued, None)],
            runner.config.as_ref(),
            Arc::clone(&runner.store),
            Arc::clone(&runner.events),
            &runner,
        )
        .await;

        assert_eq!(
            status_of(&runner, id).await,
            OperationStatus::Failed {
                reason: FailureReason::DaemonStopped
            }
        );
        let said = super::captured_log::lines();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains("the configuration can't be read, so any operation queued")),
            "the log says why the queue ended: {said:?}"
        );
        let hyphenated = id.as_uuid().hyphenated().to_string();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains(&hyphenated)
                && line.contains("ConfigUnreadable")),
            "the log names the backup that ended, and why: {said:?}"
        );
    }

    /// A queued backup whose plan now backs up to another repository than
    /// the one it was queued for isn't run there under the old record: it
    /// ends as failed, and the log says why (decision 73).
    #[tokio::test]
    async fn a_queued_backup_whose_plan_moved_to_another_repository_ends_as_daemon_stopped() {
        super::captured_log::capture();
        let id = OperationId::from_uuid(Uuid::from_u128(0x2743));
        let (runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let queued = domain::execution::Operation::queue(
            id,
            OperationKind::Backup,
            Trigger::Schedule,
            domain::execution::Target {
                plan: Some(domain::PlanId::parse("home").expect("a plan ID")),
                repository: Some(
                    domain::RepositoryId::parse(&"b".repeat(64)).expect("a repository ID"),
                ),
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        );
        runner
            .store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");

        PlanRuns::resume(&runner, queued).await;

        assert_eq!(
            status_of(&runner, id).await,
            OperationStatus::Failed {
                reason: FailureReason::DaemonStopped
            }
        );
        let hyphenated = id.as_uuid().hyphenated().to_string();
        let said = super::captured_log::lines();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains(&hyphenated)
                && line.contains("another repository")),
            "the log says why the backup ended: {said:?}"
        );
    }

    /// An engine for a repository that holds no snapshots: it names the
    /// repository when asked to identify it, and lists nothing for any other
    /// job.
    struct Empty;

    #[async_trait]
    impl Engine for Empty {
        async fn run(&self, _op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let result = match job {
                Job::Identify(_) => EngineResult::Repository {
                    id: domain::RepositoryId::parse(&"a".repeat(64)).expect("a repository ID"),
                },
                _ => EngineResult::Snapshots(Vec::new()),
            };
            Ok(RunningJob {
                events: futures::stream::iter([EngineEvent::Succeeded(result)]).boxed(),
                control: Box::new(Parked),
            })
        }
    }

    /// A start, a preview and a read of the groups name a repository by its
    /// alias: one the configuration doesn't hold and one no alias could be
    /// are both unknown, and the engine is never asked.
    #[tokio::test]
    async fn a_maintenance_request_for_a_repository_that_is_not_there_is_unknown() {
        use http_adapter::MaintenanceRequest as Request;
        let (runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        for repository in ["missing", "Not An Alias"] {
            for request in [Request::Forget, Request::Prune, Request::Check] {
                assert_eq!(
                    runner.preview(repository.to_owned(), request.clone()).await,
                    Err(MaintenanceRefusal::UnknownRepository),
                    "a preview of {request:?} on {repository}"
                );
                assert_eq!(
                    runner
                        .start_maintenance(repository.to_owned(), request.clone(), None)
                        .await,
                    Err(MaintenanceRefusal::UnknownRepository),
                    "a start of {request:?} on {repository}"
                );
            }
            assert_eq!(
                runner.groups(repository.to_owned()).await,
                Err(MaintenanceRefusal::UnknownRepository),
                "the groups of {repository}"
            );
        }
    }

    /// A check says nothing of the repository but that it is configured, so
    /// its preview needs no engine: this runner's parks every job.
    #[tokio::test]
    async fn a_check_preview_is_told_without_the_engine() {
        let (runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        assert_eq!(
            runner
                .preview("demo".to_owned(), http_adapter::MaintenanceRequest::Check)
                .await,
            Ok(api::MaintenancePreviewDto::Check { read_data: false })
        );
    }

    /// A selection is the text a preview named it by; anything else is
    /// refused before the repository is read: this runner's engine parks.
    #[tokio::test]
    async fn a_selection_that_is_not_one_is_refused_before_anything_is_read() {
        let (runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        assert_eq!(
            runner
                .start_maintenance(
                    "demo".to_owned(),
                    http_adapter::MaintenanceRequest::Forget,
                    Some("not-a-selection".to_owned()),
                )
                .await,
            Err(MaintenanceRefusal::InvalidBody)
        );
    }

    /// An adoption into a plan no plan could be called is unknown before the
    /// repository is read.
    #[tokio::test]
    async fn an_adoption_into_a_plan_that_cannot_be_named_is_refused_before_anything_is_read() {
        let (runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        for into in ["Not A Plan", ""] {
            assert_eq!(
                runner
                    .start_maintenance(
                        "demo".to_owned(),
                        http_adapter::MaintenanceRequest::Adopt {
                            into: into.to_owned(),
                            from: api::AdoptionFromDto::Unmanaged,
                        },
                        None,
                    )
                    .await,
                Err(MaintenanceRefusal::UnknownPlan {
                    plan: into.to_owned()
                })
            );
        }
    }

    /// Every plan of the repository is listed, the ones with no snapshots
    /// too, with the repository read live through the engine.
    #[tokio::test]
    async fn the_groups_of_a_repository_with_no_snapshots_still_list_every_plan() {
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(Empty);
        assert_eq!(
            runner.groups("demo".to_owned()).await,
            Ok(api::SnapshotGroupsDto {
                plans: vec![api::PlanGroupDto {
                    plan: "home".to_owned(),
                    count: 0,
                    snapshots: Vec::new(),
                }],
                orphaned: Vec::new(),
                unmanaged: Vec::new(),
                warnings: Vec::new(),
            })
        );
    }

    /// One repository under two names never reaches the history view (I6,
    /// §7.3): a file with a second entry at the first one's location is not a
    /// configuration, so neither name's groups are read, and no plan of the
    /// one can show as orphaned history adoptable into the other. The refusal
    /// is the mistake's, as a save's is (#347).
    #[tokio::test]
    async fn a_repository_under_two_names_never_reaches_the_history_view() {
        let (mut runner, _entered, _gate, scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(Empty);
        let location = scratch.path().join("repo").display().to_string();
        let (config_dir, _) = own_folders(scratch.path());
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "schema = 1\n\n\
                 [repositories.demo]\nrepository = {location:?}\npassword = \"demo-password\"\n\
                 maintenance = \"local\"\n\n\
                 [repositories.other]\nrepository = {location:?}\npassword = \"demo-password\"\n\n\
                 [[plans]]\nid = \"home\"\nrepository = \"demo\"\nsources = [\"/home/demo\"]\n"
            ),
        )
        .expect("the config file");
        for alias in ["demo", "other"] {
            let refused = runner.groups(alias.to_owned()).await;
            assert_matches!(
                refused,
                Err(MaintenanceRefusal::Unavailable {
                    code: ErrorCode::ConfigInvalid,
                    ..
                }),
                "the groups of {alias}: {refused:?}"
            );
        }
    }

    /// An engine for a repository that holds no snapshots, as [`Empty`], that
    /// also tells which repository ID each job names as the one it must open.
    /// Like the real engine, it identifies a repository as the ID the job
    /// named, when the job names one.
    struct Named(tokio::sync::mpsc::UnboundedSender<(String, Option<domain::RepositoryId>)>);

    #[async_trait]
    impl Engine for Named {
        async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let named = job.target().repository;
            let _ = self.0.send((format!("{:?}", job.kind()), named.clone()));
            match (&job, named) {
                (Job::Identify(_), Some(id)) => Ok(RunningJob {
                    events: futures::stream::iter([EngineEvent::Succeeded(
                        EngineResult::Repository { id },
                    )])
                    .boxed(),
                    control: Box::new(Parked),
                }),
                _ => Empty.run(op, job).await,
            }
        }
    }

    /// A repository ID the configuration recorded goes with every job that
    /// opens the repository: the listing, the reads behind the history view
    /// and a backup. The engine refuses a location whose repository has
    /// another ID (§7.3), so a location that now holds another repository is
    /// never read or written under the entry's name.
    #[tokio::test]
    async fn every_job_names_the_repository_id_the_configuration_recorded() {
        let (mut runner, _entered, open_gate, scratch) = runner_at_a_gate().await;
        let (seen, mut jobs) = tokio::sync::mpsc::unbounded_channel();
        runner.engine = Arc::new(Named(seen));
        let recorded = "07".repeat(32);
        let location = scratch.path().join("repo").display().to_string();
        let source = scratch.path().join("source").display().to_string();
        let (config_dir, _) = own_folders(scratch.path());
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "schema = 1\n\n\
                 [repositories.demo]\nrepository = {location:?}\n\
                 repository-id = {recorded:?}\npassword = \"demo-password\"\n\n\
                 [[plans]]\nid = \"home\"\nrepository = \"demo\"\nsources = [{source:?}]\n"
            ),
        )
        .expect("the config file");
        let expected = Some(domain::RepositoryId::parse(&recorded).expect("a repository ID"));
        let catalog = ConfigCatalog {
            config: Arc::clone(&runner.config),
            engine: Arc::clone(&runner.engine),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        catalog
            .snapshots("demo".to_owned())
            .await
            .expect("the listing");
        runner.groups("demo".to_owned()).await.expect("the groups");
        open_gate.send(true).expect("the gate opens");
        runner
            .start_backup("home".to_owned())
            .await
            .expect("the plan starts");
        let mut kinds = Vec::new();
        while !kinds.iter().any(|kind| kind == "Backup") {
            let (kind, id) = tokio::time::timeout(std::time::Duration::from_secs(5), jobs.recv())
                .await
                .expect("the engine is asked")
                .expect("a job");
            assert_eq!(id, expected, "the {kind} job");
            kinds.push(kind);
        }
        assert!(kinds.contains(&"Ls".to_owned()), "the jobs: {kinds:?}");
    }

    /// An engine for a repository that holds no snapshots, as [`Empty`], that
    /// also tells which configuration entry each job was made from.
    struct Saved(tokio::sync::mpsc::UnboundedSender<(String, Option<app::ports::SavedEntry>)>);

    #[async_trait]
    impl Engine for Saved {
        async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let entry = job.access().and_then(|access| access.entry.clone());
            let _ = self.0.send((format!("{:?}", job.kind()), entry));
            Empty.run(op, job).await
        }
    }

    /// Every job that opens a saved repository carries the entry it was made
    /// from, as it was read: the listing, the reads behind the history view
    /// and a backup. The daemon records in that entry the ID of the
    /// repository the job opened, when it records none (§6.1, decision 57).
    #[tokio::test]
    async fn every_job_carries_the_entry_it_was_made_from() {
        let (mut runner, _entered, open_gate, scratch) = runner_at_a_gate().await;
        let (seen, mut jobs) = tokio::sync::mpsc::unbounded_channel();
        runner.engine = Arc::new(Saved(seen));
        let location = scratch.path().join("repo").display().to_string();
        let source = scratch.path().join("source").display().to_string();
        let (config_dir, _) = own_folders(scratch.path());
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "schema = 1\n\n\
                 [repositories.demo]\nrepository = {location:?}\npassword = \"demo-password\"\n\
                 maintenance = \"local\"\n\n\
                 [[plans]]\nid = \"home\"\nrepository = \"demo\"\nsources = [{source:?}]\n"
            ),
        )
        .expect("the config file");
        let alias = RepositoryAlias::parse("demo").expect("an alias");
        let expected = Some(app::ports::SavedEntry {
            entry: runner
                .config
                .load()
                .await
                .expect("the configuration")
                .repository(&alias)
                .cloned()
                .expect("the entry"),
            alias,
        });
        let catalog = ConfigCatalog {
            config: Arc::clone(&runner.config),
            engine: Arc::clone(&runner.engine),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        catalog
            .snapshots("demo".to_owned())
            .await
            .expect("the listing");
        runner.groups("demo".to_owned()).await.expect("the groups");
        open_gate.send(true).expect("the gate opens");
        runner
            .start_backup("home".to_owned())
            .await
            .expect("the plan starts");
        let mut kinds = Vec::new();
        while !kinds.iter().any(|kind| kind == "Backup") {
            let (kind, entry) =
                tokio::time::timeout(std::time::Duration::from_secs(5), jobs.recv())
                    .await
                    .expect("the engine is asked")
                    .expect("a job");
            assert_eq!(entry, expected, "the {kind} job");
            kinds.push(kind);
        }
        assert!(kinds.contains(&"Ls".to_owned()), "the jobs: {kinds:?}");
    }

    /// An engine for a repository that holds no snapshots, whose every job
    /// opens the repository `.0` and says so when it starts.
    struct Opens(domain::RepositoryId);

    #[async_trait]
    impl Engine for Opens {
        async fn run(&self, _op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let result = match job {
                Job::Identify(_) => EngineResult::Repository { id: self.0.clone() },
                _ => EngineResult::Snapshots(Vec::new()),
            };
            Ok(RunningJob {
                events: futures::stream::iter([
                    EngineEvent::Started {
                        index_read_at: domain::Timestamp::from_second(1_800_000_000)
                            .expect("a time"),
                        repository: Some(self.0.clone()),
                    },
                    EngineEvent::Succeeded(result),
                ])
                .boxed(),
                control: Box::new(Parked),
            })
        }
    }

    /// The daemon's engine records, through the configuration file, the ID
    /// of the repository a job opened in the entry the job was made from:
    /// the file gains that one line, and every other byte is as written
    /// (§6.1, decision 57).
    #[tokio::test]
    async fn a_job_that_opens_an_entry_without_an_id_records_it_in_the_file() {
        let scratch = tempfile::tempdir().expect("a scratch dir");
        let (config_dir, _) = own_folders(scratch.path());
        let file = config_dir.join("config.toml");
        let location = scratch.path().join("repo").display().to_string();
        let source = scratch.path().join("source").display().to_string();
        let entry = format!(
            "schema = 1\n\n\
             [repositories.demo]\nrepository = {location:?}\npassword = \"demo-password\"\n\
             maintenance = \"local\"\n"
        );
        let plans =
            format!("\n[[plans]]\nid = \"home\"\nrepository = \"demo\"\nsources = [{source:?}]\n");
        std::fs::write(&file, format!("{entry}{plans}")).expect("the config file");
        let id = "07".repeat(32);
        let config: Arc<dyn ConfigStore> =
            Arc::new(FileConfigStore::new(file.clone(), Arc::new(SystemClock)));
        let told = Arc::new(std::sync::Mutex::new(Vec::new()));
        let report = {
            let told = Arc::clone(&told);
            move |recording: Recording| {
                told.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(recording);
            }
        };
        let catalog = ConfigCatalog {
            config: Arc::clone(&config),
            engine: Arc::new(app::repositories::RecordOpened::new(
                Arc::new(Opens(
                    domain::RepositoryId::parse(&id).expect("a repository ID"),
                )),
                config,
                Arc::new(app::repositories::ConfigWrites::default()),
                Arc::new(report),
            )),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        catalog
            .snapshots("demo".to_owned())
            .await
            .expect("the listing");
        assert_eq!(
            std::fs::read_to_string(&file).expect("the config file"),
            format!("{entry}repository-id = {id:?}\n{plans}")
        );
        assert_eq!(
            *told
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![Recording::Recorded {
                alias: RepositoryAlias::parse("demo").expect("an alias"),
                id: domain::RepositoryId::parse(&id).expect("a repository ID"),
            }]
        );
    }

    /// A start that carries a selection is held to it (I6): what a forget
    /// would remove is read again, and a repository that no longer matches
    /// what the review named refuses before anything is removed.
    #[tokio::test]
    async fn a_forget_held_to_a_selection_the_repository_no_longer_matches_is_refused() {
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(Empty);
        let reviewed = runner
            .preview("demo".to_owned(), http_adapter::MaintenanceRequest::Forget)
            .await
            .expect("a preview of the empty repository");
        let api::MaintenancePreviewDto::Forget { selection, .. } = reviewed else {
            panic!("a forget is previewed as a forget");
        };
        let other = if selection == "0123456789abcdef" {
            "fedcba9876543210"
        } else {
            "0123456789abcdef"
        };
        assert_eq!(
            runner
                .start_maintenance(
                    "demo".to_owned(),
                    http_adapter::MaintenanceRequest::Forget,
                    Some(other.to_owned()),
                )
                .await,
            Err(MaintenanceRefusal::SelectionChanged)
        );
    }

    /// A client that cancels the instant it has the answer to its start is
    /// not told the operation is unknown or not running: the operation is
    /// recorded and its cancel switch parked before the route answers, and
    /// the run it cancels ends cancelled.
    #[tokio::test]
    async fn a_cancel_right_after_the_answer_is_accepted_and_ends_the_operation_cancelled() {
        let (runner, _entered, open_gate, _scratch) = runner_at_a_gate().await;
        open_gate.send(true).expect("the gate opens");
        let id = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");
        runner
            .cancel(id.clone())
            .await
            .expect("a cancel straight after the answer is accepted");
        let operation = operation_of(&id).expect("the answer is an operation ID");
        assert_eq!(ended(&runner, operation).await, OperationStatus::Cancelled);
    }

    /// The status the operation ends with, once it has.
    async fn ended(runner: &PlanRunner, operation: OperationId) -> OperationStatus {
        let ended = async {
            loop {
                let detail = GetOperation::new(Arc::clone(&runner.store))
                    .get(operation)
                    .await
                    .expect("the log reads");
                if let Some(detail) = detail
                    && detail.record.status.is_finished()
                {
                    return detail.record.status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), ended)
            .await
            .expect("the cancelled operation ends")
    }

    /// An operation log whose writes wait until the test opens it.
    struct GatedSaves {
        inner: Arc<dyn OperationStore>,
        open: tokio::sync::watch::Receiver<bool>,
    }

    #[async_trait]
    impl OperationStore for GatedSaves {
        async fn recover(
            &self,
            reason: &FailureReason,
        ) -> Result<app::ports::Recovered, app::ports::StoreError> {
            self.inner.recover(reason).await
        }

        async fn save(
            &self,
            operation: &domain::execution::Operation,
            result: Option<&app::ports::EngineResult>,
        ) -> Result<(), app::ports::StoreError> {
            let mut open = self.open.clone();
            while !*open.borrow_and_update() {
                open.changed().await.expect("the gate stays open");
            }
            self.inner.save(operation, result).await
        }

        async fn try_start(
            &self,
            operation: &domain::execution::Operation,
        ) -> Result<app::ports::Start, app::ports::StoreError> {
            self.inner.try_start(operation).await
        }

        async fn get(
            &self,
            id: OperationId,
        ) -> Result<Option<app::ports::OperationDetail>, app::ports::StoreError> {
            self.inner.get(id).await
        }

        async fn list(
            &self,
            query: &app::ports::HistoryQuery,
        ) -> Result<app::ports::HistoryPage, app::ports::StoreError> {
            self.inner.list(query).await
        }

        async fn ever_succeeded(
            &self,
            plan: &domain::PlanId,
        ) -> Result<bool, app::ports::StoreError> {
            self.inner.ever_succeeded(plan).await
        }
    }

    /// A client that gives up on its start while the log is still being
    /// written (the connection drops, and the handler with it) leaves a run
    /// that goes on: nothing is left queued and never run, or registered and
    /// never cancellable.
    #[tokio::test]
    async fn a_start_whose_client_gave_up_still_runs_and_still_cancels() {
        let (mut runner, _entered, open_hooks, _scratch) = runner_at_a_gate().await;
        open_hooks.send(true).expect("the gate opens");
        let (open_log, log_is_open) = tokio::sync::watch::channel(false);
        runner.store = Arc::new(GatedSaves {
            inner: Arc::clone(&runner.store),
            open: log_is_open,
        });
        let answered = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            runner.start_backup("home".into()),
        )
        .await;
        assert!(answered.is_err(), "the start waits for the log");
        open_log.send(true).expect("the log opens");
        let operation = *runner
            .registry
            .running()
            .keys()
            .next()
            .expect("the start parked a cancel switch");
        let id = operation.as_uuid().hyphenated().to_string();
        let accepted = async {
            loop {
                match runner.cancel(id.clone()).await {
                    Err(http_adapter::CancelError::Unknown) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    other => return other,
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), accepted)
            .await
            .expect("the operation ends up in the log")
            .expect("the operation that was started is cancellable");
        assert_eq!(ended(&runner, operation).await, OperationStatus::Cancelled);
    }

    /// An engine whose jobs play `events`, whatever the jobs are.
    struct Answers(Vec<EngineEvent>);

    #[async_trait]
    impl Engine for Answers {
        async fn run(&self, _op: OperationId, _job: Job) -> Result<RunningJob, EngineError> {
            Ok(RunningJob {
                events: futures::stream::iter(self.0.clone()).boxed(),
                control: Box::new(Parked),
            })
        }
    }

    fn started() -> EngineEvent {
        EngineEvent::Started {
            index_read_at: domain::Timestamp::from_second(1_800_000_000).expect("a time"),
            repository: None,
        }
    }

    /// The demo repository's ID: what [`Completes`] reports for a read of it,
    /// and what the tests that look at its slot in the log name.
    fn demo_repository() -> domain::RepositoryId {
        domain::RepositoryId::parse(&"ab".repeat(32)).expect("a repository ID")
    }

    /// An engine whose jobs start and succeed at once, each with what its
    /// kind reports: the ID of the repository a read opens, the outcome of
    /// a check.
    struct Completes;

    #[async_trait]
    impl Engine for Completes {
        async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let repository = demo_repository();
            let result = match &job {
                Job::Identify(_) => EngineResult::Repository { id: repository },
                Job::Check(_) => EngineResult::Check(app::ports::CheckSummary {
                    repository,
                    errors: 0,
                    warnings: 0,
                }),
                _ => EngineResult::Fake { steps_completed: 1 },
            };
            Answers(vec![started(), EngineEvent::Succeeded(result)])
                .run(op, job)
                .await
        }
    }

    /// An operation log that takes every write at once but the one it holds,
    /// an operation's start or its end, and holds that one until the test
    /// lets it through.
    struct Held {
        inner: Arc<dyn OperationStore>,
        holds: fn(&OperationStatus) -> bool,
        arrived: tokio::sync::Notify,
        open: tokio::sync::watch::Sender<bool>,
    }

    impl Held {
        /// Over `inner`, holding the write of an operation's end.
        fn end_over(inner: Arc<dyn OperationStore>) -> Self {
            Self::over(inner, OperationStatus::is_finished)
        }

        /// Over `inner`, holding the write that says an operation is running.
        fn start_over(inner: Arc<dyn OperationStore>) -> Self {
            Self::over(inner, |status| *status == OperationStatus::Running)
        }

        fn over(inner: Arc<dyn OperationStore>, holds: fn(&OperationStatus) -> bool) -> Self {
            Self {
                inner,
                holds,
                arrived: tokio::sync::Notify::new(),
                open: tokio::sync::watch::channel(false).0,
            }
        }

        /// Waits until a run has handed the log the write it holds.
        async fn arrived(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.arrived.notified())
                .await
                .expect("the run reached the write the log holds");
        }

        fn let_it_through(&self) {
            self.open.send_replace(true);
        }
    }

    #[async_trait]
    impl OperationStore for Held {
        async fn recover(
            &self,
            reason: &FailureReason,
        ) -> Result<app::ports::Recovered, app::ports::StoreError> {
            self.inner.recover(reason).await
        }

        async fn save(
            &self,
            operation: &domain::execution::Operation,
            result: Option<&app::ports::EngineResult>,
        ) -> Result<(), app::ports::StoreError> {
            if (self.holds)(operation.status()) {
                self.arrived.notify_one();
                let _ = self.open.subscribe().wait_for(|open| *open).await;
            }
            self.inner.save(operation, result).await
        }

        async fn try_start(
            &self,
            operation: &domain::execution::Operation,
        ) -> Result<app::ports::Start, app::ports::StoreError> {
            self.inner.try_start(operation).await
        }

        async fn get(
            &self,
            id: OperationId,
        ) -> Result<Option<app::ports::OperationDetail>, app::ports::StoreError> {
            self.inner.get(id).await
        }

        async fn list(
            &self,
            query: &app::ports::HistoryQuery,
        ) -> Result<app::ports::HistoryPage, app::ports::StoreError> {
            self.inner.list(query).await
        }

        async fn ever_succeeded(
            &self,
            plan: &domain::PlanId,
        ) -> Result<bool, app::ports::StoreError> {
            self.inner.ever_succeeded(plan).await
        }
    }

    /// What a page that listens to the daemon hears.
    type Heard = futures::stream::BoxStream<'static, UiEvent>;

    /// Runs the daemon's engine and bus over `inner`'s jobs and `log` as the
    /// runner's own, and returns what a page that listens to the bus hears.
    async fn listen(
        runner: &mut PlanRunner,
        inner: impl Engine + 'static,
        log: Arc<dyn OperationStore>,
    ) -> Heard {
        let bus = Arc::new(BroadcastEventBus::new());
        let heard = bus.subscribe().await;
        runner.engine = Arc::new(EventEngine {
            inner: Arc::new(inner),
            bus: bus.clone(),
        });
        runner.events = bus;
        runner.store = log;
        heard
    }

    fn is_end(news: &UiEvent) -> bool {
        matches!(
            news,
            UiEvent::OperationSucceeded { .. } | UiEvent::OperationFailed { .. }
        )
    }

    /// How operations have ended, as far as the page has heard, without
    /// waiting for any more.
    fn ends_heard(heard: &mut Heard) -> Vec<UiEvent> {
        let mut ends = Vec::new();
        while let Some(Some(news)) = futures::FutureExt::now_or_never(heard.next()) {
            if is_end(&news) {
                ends.push(news);
            }
        }
        ends
    }

    /// Waits for the page to hear how an operation ended.
    async fn next_end(heard: &mut Heard) -> UiEvent {
        let end = async {
            while let Some(news) = heard.next().await {
                if is_end(&news) {
                    return news;
                }
            }
            panic!("the page's news ended");
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), end)
            .await
            .expect("the page hears how the operation ended")
    }

    /// The history as a page reads it, once it has heard the end.
    async fn history_of(runner: &PlanRunner, id: &str) -> app::ports::OperationDetail {
        GetOperation::new(Arc::clone(&runner.store))
            .get(operation_of(id).expect("an operation ID"))
            .await
            .expect("the log reads")
            .expect("the operation is in the log")
    }

    /// A page that reads the history on hearing a backup ended finds the end
    /// there (#134): the daemon announces it once the log has the write, not
    /// when the engine says it is done.
    #[tokio::test]
    async fn a_backup_is_announced_to_the_pages_once_the_log_has_its_end() {
        let (mut runner, _entered, open_hooks, _scratch) = runner_at_a_gate().await;
        open_hooks.send(true).expect("the gate opens");
        let log = Arc::new(Held::end_over(Arc::clone(&runner.store)));
        let mut heard = listen(&mut runner, Completes, log.clone()).await;
        let id = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");

        log.arrived().await;
        assert_eq!(
            ends_heard(&mut heard),
            [],
            "the page heard the end before the log had it"
        );

        log.let_it_through();
        let end = next_end(&mut heard).await;
        assert_matches!(&end, UiEvent::OperationSucceeded { operation, .. } if *operation == id);
        assert_eq!(
            history_of(&runner, &id).await.record.status,
            OperationStatus::Succeeded
        );
    }

    /// The same for maintenance: the reads that come before it (the
    /// repository's identity) are not how it ended, so the page hears of one
    /// end, once the log has it.
    #[tokio::test]
    async fn a_check_is_announced_to_the_pages_once_and_once_the_log_has_its_end() {
        let (mut runner, _entered, _open_hooks, _scratch) = runner_at_a_gate().await;
        let log = Arc::new(Held::end_over(Arc::clone(&runner.store)));
        let mut heard = listen(&mut runner, Completes, log.clone()).await;
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the check starts");

        log.arrived().await;
        assert_eq!(
            ends_heard(&mut heard),
            [],
            "the page heard an end before the log had it"
        );

        log.let_it_through();
        let end = next_end(&mut heard).await;
        assert_matches!(&end, UiEvent::OperationSucceeded { operation, .. } if *operation == id);
        assert_eq!(
            history_of(&runner, &id).await.record.status,
            OperationStatus::Succeeded
        );
        assert_eq!(ends_heard(&mut heard), [], "the end was announced twice");
    }

    /// What the page has heard so far that tells of a start, without waiting
    /// for any more.
    fn starts_heard(heard: &mut Heard) -> Vec<UiEvent> {
        let mut starts = Vec::new();
        while let Some(Some(news)) = futures::FutureExt::now_or_never(heard.next()) {
            if matches!(news, UiEvent::OperationStarted { .. }) {
                starts.push(news);
            }
        }
        starts
    }

    /// A page that reads the history on hearing a backup started finds it
    /// running there (#305): the daemon announces the start once the log
    /// has the write that says so, not when the engine says it started. A
    /// page that heard it earlier and read a history that still had the
    /// backup queued, its events stream opening only after the news, never
    /// learned it ran.
    #[tokio::test]
    async fn a_backup_is_announced_to_the_pages_as_started_once_the_log_has_it_running() {
        let (mut runner, _entered, open_hooks, _scratch) = runner_at_a_gate().await;
        open_hooks.send(true).expect("the gate opens");
        let log = Arc::new(Held::start_over(Arc::clone(&runner.store)));
        let mut heard = listen(&mut runner, Completes, log.clone()).await;
        let id = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");

        log.arrived().await;
        assert_eq!(
            starts_heard(&mut heard),
            [],
            "the page heard the start before the log had it"
        );
        assert_eq!(
            history_of(&runner, &id).await.record.status,
            OperationStatus::Queued
        );

        log.let_it_through();
        let start = tokio::time::timeout(std::time::Duration::from_secs(10), heard.next())
            .await
            .expect("the page hears the start once the log has it");
        assert_matches!(
            &start,
            Some(UiEvent::OperationStarted { operation, .. }) if *operation == id
        );
        assert_ne!(
            history_of(&runner, &id).await.record.status,
            OperationStatus::Queued,
            "the page heard the start, and the log has it"
        );
    }

    /// A maintenance run is told to have started once, when it has: not by
    /// the reads that come before it (the repository's identity), which
    /// name the run's operation before the log has it at all.
    #[tokio::test]
    async fn a_check_is_announced_to_the_pages_as_started_once() {
        let (mut runner, _entered, _open_hooks, _scratch) = runner_at_a_gate().await;
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, Completes, log).await;
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the check starts");

        let mut news = Vec::new();
        let end = async {
            while let Some(event) = heard.next().await {
                let ended = is_end(&event);
                news.push(event);
                if ended {
                    return;
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), end)
            .await
            .expect("the page hears how the check ended");
        let starts: Vec<&UiEvent> = news
            .iter()
            .filter(|news| matches!(news, UiEvent::OperationStarted { .. }))
            .collect();
        assert_matches!(
            starts.as_slice(),
            [UiEvent::OperationStarted { operation, .. }] if *operation == id,
            "the page hears one start, the run's: {news:?}"
        );
    }

    /// The engine's own start and end are no news to a page: whoever records
    /// the operation announces them, once the log has them. The runner still
    /// sees every event.
    #[tokio::test]
    async fn the_engine_tells_the_pages_neither_how_a_job_started_nor_how_it_ended() {
        let ends = [
            EngineEvent::Succeeded(EngineResult::Fake { steps_completed: 1 }),
            EngineEvent::Failed(FailureReason::EngineFailed {
                detail: "repository not found".to_owned(),
            }),
        ];
        for end in ends {
            let bus = Arc::new(BroadcastEventBus::new());
            let mut heard = bus.subscribe().await;
            let engine = EventEngine {
                inner: Arc::new(Answers(vec![started(), end.clone()])),
                bus,
            };
            let job = Job::Fake(app::ports::FakeJob {
                steps: 1,
                step_delay: std::time::Duration::ZERO,
                warnings: Vec::new(),
                end: app::ports::FakeEnd::Succeed,
                deadline_budget: None,
            });
            let operation = OperationId::from_uuid(Uuid::from_u128(7));
            let running = engine
                .run(operation, job)
                .await
                .expect("the engine takes the job");
            let reported: Vec<EngineEvent> = running.events.collect().await;
            assert_eq!(reported, [started(), end.clone()], "the runner's events");

            let mut news = Vec::new();
            while let Some(Some(event)) = futures::FutureExt::now_or_never(heard.next()) {
                news.push(event);
            }
            assert_eq!(news, [], "the page's news of {end:?}");
        }
    }

    /// An engine that starts every job, tells of a progress bar beginning and
    /// of a warning, then answers it as [`Empty`] does, and counts the jobs
    /// it was asked.
    struct TellsThenEmpty(Arc<std::sync::atomic::AtomicUsize>);

    #[async_trait]
    impl Engine for TellsThenEmpty {
        async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let empty = Empty.run(op, job).await?;
            let news = [
                started(),
                EngineEvent::Progress(ProgressEvent::Begin {
                    key: 1,
                    kind: app::ports::ProgressKind::Counter,
                    step: Some(app::ports::OperationStep::ReadingIndex),
                }),
                EngineEvent::Warning(domain::execution::Warning {
                    kind: WarningKind::SourceUnreadable,
                    item: Some("/home/demo/locked".to_owned()),
                    during: "reading".to_owned(),
                    detail: "permission denied".to_owned(),
                }),
            ];
            Ok(RunningJob {
                events: futures::stream::iter(news).chain(empty.events).boxed(),
                control: empty.control,
            })
        }
    }

    /// All the page has heard so far, without waiting for any more.
    fn heard_now(heard: &mut Heard) -> Vec<UiEvent> {
        let mut news = Vec::new();
        while let Some(Some(one)) = futures::FutureExt::now_or_never(heard.next()) {
            news.push(one);
        }
        news
    }

    /// A read that is no operation tells a page nothing (#308): the snapshot
    /// listing, the groups and the previews of a forget and a prune each
    /// start a job, and none of them is a row in the history. A page told of
    /// their starts would count them running for ever, and their progress
    /// and warnings belong to no row it shows.
    #[tokio::test]
    async fn reads_that_are_no_operation_tell_the_pages_nothing() {
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, TellsThenEmpty(Arc::clone(&asked)), log).await;
        let catalog = ConfigCatalog {
            config: Arc::clone(&runner.config),
            engine: Arc::clone(&runner.engine),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        let mut jobs = 0;
        let mut nothing_from = |read: &str, heard: &mut Heard| {
            let now = asked.load(std::sync::atomic::Ordering::SeqCst);
            assert!(now > jobs, "the {read} reached the engine");
            jobs = now;
            assert_eq!(heard_now(heard), [], "the {read}");
        };

        catalog
            .snapshots("demo".to_owned())
            .await
            .expect("the listing");
        nothing_from("listing", &mut heard);
        runner.groups("demo".to_owned()).await.expect("the groups");
        nothing_from("groups", &mut heard);
        for request in [
            http_adapter::MaintenanceRequest::Forget,
            http_adapter::MaintenanceRequest::Prune,
        ] {
            // What a preview finds is not the point: that it read is.
            let _ = runner.preview("demo".to_owned(), request.clone()).await;
            nothing_from(&format!("preview of {request:?}"), &mut heard);
        }
    }

    /// Which of the engine's jobs tell the pages their progress and warnings
    /// (#308): an operation's, which the history has a row for, and none of
    /// the reads', nor the initialising of a repository, which answers its
    /// request and has no row.
    #[tokio::test]
    async fn only_an_operations_job_tells_the_pages_its_progress_and_warnings() {
        let bus = Arc::new(BroadcastEventBus::new());
        let mut heard = bus.subscribe().await;
        let engine = EventEngine {
            inner: Arc::new(TellsThenEmpty(Arc::default())),
            bus,
        };
        let access = app::ports::RepositoryAccess {
            repository: "/srv/demo/repo".to_owned(),
            repo_hot: None,
            password: domain::secrets::SecretValue::new("demo-password"),
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
            secrets: Vec::new(),
            id: None,
            entry: None,
        };
        let plan = app::ports::PruneJob {
            repository: access.clone(),
            keep_delete: domain::execution::deadline::KeepDelete::MIN,
        };
        let tags = PlanTags::new(
            domain::PlanId::try_from("home".to_owned()).expect("a plan ID"),
            domain::AgentId::from_uuid(uuid::Uuid::from_u128(1)),
        );
        let operations = [
            Job::Fake(app::ports::FakeJob {
                steps: 0,
                step_delay: std::time::Duration::ZERO,
                warnings: Vec::new(),
                end: app::ports::FakeEnd::Succeed,
                deadline_budget: None,
            }),
            Job::Backup(BackupJob {
                repository: access.clone(),
                sources: vec!["/home/demo".to_owned()],
                tags: tags.clone(),
                extra_tags: Vec::new(),
                label: None,
                host: None,
                filter: Default::default(),
                deadline_budget: domain::SignedDuration::from_hours(1),
            }),
            Job::Forget(app::ports::ForgetJob {
                repository: access.clone(),
                ids: Vec::new(),
            }),
            Job::Rewrite(app::ports::RewriteJob {
                repository: access.clone(),
                ids: Vec::new(),
                tags,
            }),
            Job::Prune(plan.clone()),
            Job::Check(app::ports::CheckJob {
                repository: access.clone(),
                read_data: false,
            }),
        ];
        let reads = [
            Job::Identify(access.clone()),
            Job::Init(access.clone()),
            Job::Ls(access.clone()),
            Job::PrunePlan(plan),
        ];
        for (job, operation) in operations
            .into_iter()
            .map(|job| (job, true))
            .chain(reads.into_iter().map(|job| (job, false)))
        {
            let said = format!("{job:?}");
            let running = engine
                .run(new_operation(), job)
                .await
                .expect("the job runs");
            running.events.collect::<Vec<_>>().await;
            let news = heard_now(&mut heard);
            if operation {
                assert_matches!(
                    news.as_slice(),
                    [
                        UiEvent::OperationProgress { .. },
                        UiEvent::OperationWarning { .. }
                    ],
                    "{said}: {news:?}"
                );
            } else {
                assert_eq!(news, [], "{said}");
            }
        }
    }

    /// An operation log that refuses every save, in the words a database
    /// uses for what it refuses, and counts them. Starts and reads go to the
    /// log behind it.
    pub(super) struct RefusingSaves {
        pub(super) inner: Arc<dyn OperationStore>,
        pub(super) refused: Arc<std::sync::atomic::AtomicUsize>,
    }

    /// What a refused save says for itself: none of it may reach the daemon's
    /// log.
    pub(super) const REFUSAL: &str = "canary: the disk of /srv/secret-folder is full";

    #[async_trait]
    impl OperationStore for RefusingSaves {
        async fn recover(
            &self,
            reason: &FailureReason,
        ) -> Result<app::ports::Recovered, app::ports::StoreError> {
            self.inner.recover(reason).await
        }

        async fn save(
            &self,
            _operation: &domain::execution::Operation,
            _result: Option<&app::ports::EngineResult>,
        ) -> Result<(), app::ports::StoreError> {
            self.refused
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(app::ports::StoreError::Unavailable {
                detail: REFUSAL.to_owned(),
            })
        }

        async fn try_start(
            &self,
            operation: &domain::execution::Operation,
        ) -> Result<app::ports::Start, app::ports::StoreError> {
            self.inner.try_start(operation).await
        }

        async fn get(
            &self,
            id: OperationId,
        ) -> Result<Option<app::ports::OperationDetail>, app::ports::StoreError> {
            self.inner.get(id).await
        }

        async fn list(
            &self,
            query: &app::ports::HistoryQuery,
        ) -> Result<app::ports::HistoryPage, app::ports::StoreError> {
            self.inner.list(query).await
        }

        async fn ever_succeeded(
            &self,
            plan: &domain::PlanId,
        ) -> Result<bool, app::ports::StoreError> {
            self.inner.ever_succeeded(plan).await
        }
    }

    /// An engine that answers the identify a maintenance run begins with and
    /// parks the job itself, as a real check does while it works.
    struct IdentifiesThenParks;

    #[async_trait]
    impl Engine for IdentifiesThenParks {
        async fn run(&self, op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            match job {
                Job::Identify(_) => Completes.run(op, job).await,
                _ => ParkedEngine.run(op, job).await,
            }
        }
    }

    /// A client that cancels the instant it has the answer to a maintenance
    /// start is not told the operation is unknown or not running: the run
    /// starts on a task of its own, and that task parks the run's cancel
    /// switch before the route answers (§4.2). The run it cancels ends
    /// cancelled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancel_right_after_a_maintenance_start_is_accepted_and_ends_the_run_cancelled() {
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(IdentifiesThenParks);
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the check starts");
        runner
            .cancel(id.clone())
            .await
            .expect("a cancel straight after the answer is accepted");
        let operation = operation_of(&id).expect("the answer is an operation ID");
        assert_eq!(ended(&runner, operation).await, OperationStatus::Cancelled);
    }

    /// The lines the daemon logged about `operation` once its run had said
    /// what the log refused it: one for each of the `refused` writes, so the
    /// wait ends when there are that many. A run that never says gives up
    /// after a few seconds, and the lines as they stand tell what is missing.
    async fn logged_about_the_run(
        operation: &str,
        refused: &std::sync::atomic::AtomicUsize,
    ) -> Vec<(log::Level, String)> {
        let about = || {
            captured_log::lines()
                .into_iter()
                .filter(|(_, line)| line.contains(operation))
                .collect::<Vec<_>>()
        };
        let said = async {
            loop {
                let lines = about();
                let refused = refused.load(std::sync::atomic::Ordering::SeqCst);
                if refused > 0 && lines.len() >= refused {
                    return lines;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), said)
            .await
            .unwrap_or_else(|_| about())
    }

    /// Each refused write is said once, at warning level, with the code of
    /// the failure and none of its words (I4).
    fn assert_each_refusal_is_said(lines: &[(log::Level, String)], refused: usize) {
        assert!(
            refused > 0,
            "the log refused nothing, so the test shows nothing"
        );
        assert_eq!(
            lines.len(),
            refused,
            "one line for each refused write: {lines:?}"
        );
        for (level, line) in lines {
            assert_eq!(*level, log::Level::Warn, "{line}");
            assert!(line.contains("store-unavailable"), "{line}");
        }
        let everything = captured_log::lines();
        assert!(
            everything.iter().all(|(_, line)| !line.contains("canary")),
            "a line repeats what the log said: {everything:?}"
        );
    }

    /// A backup goes on without a log that refuses its writes, and the
    /// daemon says so in its own log: a log that has stopped taking writes
    /// is otherwise invisible to the person running it (#135).
    #[tokio::test]
    async fn a_write_the_log_refuses_during_a_backup_is_logged_once_with_its_code_and_never_its_words()
     {
        captured_log::capture();
        let (mut runner, _entered, open_hooks, _scratch) = runner_at_a_gate().await;
        open_hooks.send(true).expect("the gate opens");
        let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runner.store = Arc::new(RefusingSaves {
            inner: Arc::clone(&runner.store),
            refused: Arc::clone(&refused),
        });
        let id = runner
            .start_backup("home".into())
            .await
            .expect("a backup starts without a log that takes its writes");
        // The engine parks the run, as a real one is parked while it works,
        // so a cancel is what ends it.
        let operation = operation_of(&id).expect("an operation ID");
        assert!(
            runner.registry.cancel(&operation),
            "the run has a switch to fire"
        );
        let lines = logged_about_the_run(&id, &refused).await;
        assert_each_refusal_is_said(&lines, refused.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The same for a maintenance run, whose last write is the one the log
    /// refuses here: its start was taken, so the run goes on to the end.
    #[tokio::test]
    async fn a_write_the_log_refuses_during_maintenance_is_logged_once_with_its_code_and_never_its_words()
     {
        captured_log::capture();
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(Completes);
        let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        runner.store = Arc::new(RefusingSaves {
            inner: Arc::clone(&runner.store),
            refused: Arc::clone(&refused),
        });
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("a check starts");
        let lines = logged_about_the_run(&id, &refused).await;
        assert_each_refusal_is_said(&lines, refused.load(std::sync::atomic::Ordering::SeqCst));
    }

    /// The lines `log_ended` logs about a run that `n` names, which ended
    /// after the log refused it the `refused` writes.
    fn logged_when_a_run_ends(
        n: u128,
        refused: Vec<app::ports::StoreError>,
    ) -> Vec<(log::Level, String)> {
        captured_log::capture();
        let id = OperationId::from_uuid(Uuid::from_u128(n));
        log_ended(&Finished {
            operation: running_backup(id),
            result: None,
            recording_errors: refused,
        });
        let id = id.as_uuid().hyphenated().to_string();
        captured_log::lines()
            .into_iter()
            .filter(|(_, line)| line.contains(&id))
            .collect()
    }

    /// A log that took every write of a run has nothing to be warned of.
    #[test]
    fn a_run_the_log_took_in_full_logs_no_warning() {
        assert_eq!(logged_when_a_run_ends(0x7001, Vec::new()), []);
    }

    /// Each refused write is one warning, named by the code of its kind of
    /// failure, and none of the failure's words reaches the log (I4).
    #[test]
    fn each_write_the_log_refused_is_one_warning_with_the_code_of_its_failure() {
        let words = "canary: the disk of /srv/secret-folder is full";
        let lines = logged_when_a_run_ends(
            0x7002,
            vec![
                app::ports::StoreError::Unavailable {
                    detail: words.to_owned(),
                },
                app::ports::StoreError::Corrupt {
                    detail: words.to_owned(),
                },
            ],
        );
        let [(first_level, first), (second_level, second)] = lines.as_slice() else {
            panic!("one line for each refused write: {lines:?}");
        };
        assert_eq!(*first_level, log::Level::Warn, "{first}");
        assert_eq!(*second_level, log::Level::Warn, "{second}");
        assert!(first.contains("store-unavailable"), "{first}");
        assert!(second.contains("store-corrupt"), "{second}");
        assert!(
            lines.iter().all(|(_, line)| !line.contains("canary")),
            "a line repeats what the log said: {lines:?}"
        );
    }

    /// Within a few seconds the repository takes another maintenance
    /// operation: whatever ran on it has ended, or was never recorded.
    async fn assert_the_repository_takes_another_check(runner: &PlanRunner, what: &str) {
        let now = domain::Timestamp::from_second(1_800_000_100).expect("a timestamp");
        let mut check = domain::execution::Operation::queue(
            OperationId::from_uuid(Uuid::now_v7()),
            OperationKind::Check,
            Trigger::Manual,
            domain::execution::Target {
                plan: None,
                repository: Some(demo_repository()),
            },
            now,
        );
        check.start(now).expect("a queued operation starts");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match runner.store.try_start(&check).await {
                Ok(app::ports::Start::Started) => return,
                Ok(app::ports::Start::Refused { running }) => {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "{what}: the repository is still held by {}, which nothing ends",
                        running.as_uuid().hyphenated()
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
                Err(error) => panic!("{what}: the log refused the check's start: {error}"),
            }
        }
    }

    /// Where a test cuts a maintenance start short, each reached whatever the
    /// threads do. The route's own future waits at two points, its reads and
    /// the one-shot channel the task that starts the run answers on; the cut
    /// at the channel is made twice, before the log has committed the start
    /// and after.
    #[derive(Clone, Copy, Debug)]
    enum StartCut {
        /// While it reads the repository, before anything is spawned or
        /// stored: the reads take no slot.
        DuringItsReads,
        /// Once the start has reached the operation log, whose write of it
        /// can't commit: another connection holds the database's write lock.
        BeforeTheLogStoresIt,
        /// After the log has committed the start, before the route has heard
        /// so.
        AfterTheLogStoredIt,
    }

    impl std::fmt::Display for StartCut {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(match self {
                Self::DuringItsReads => "during its reads",
                Self::BeforeTheLogStoresIt => "before the log stored it",
                Self::AfterTheLogStoredIt => "after the log stored it",
            })
        }
    }

    /// An engine that says when a read began and never answers it.
    #[derive(Default)]
    struct HeldRead {
        reading: tokio::sync::Notify,
    }

    impl HeldRead {
        /// Waits until a read has begun.
        async fn read_begun(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.reading.notified())
                .await
                .expect("the start reads the repository");
        }
    }

    #[async_trait]
    impl Engine for HeldRead {
        async fn run(&self, _op: OperationId, _job: Job) -> Result<RunningJob, EngineError> {
            self.reading.notify_one();
            futures::future::pending().await
        }
    }

    /// Starts a check of the demo repository on `runner`, whose log is the
    /// database at `database`, and drops the route at `cut`, as a client
    /// that hangs up drops it. A route that answers first fails the test, as
    /// does a start cut at the channel that the log then never stored: the
    /// route took it with it.
    async fn cut_a_start_short(runner: &mut PlanRunner, database: &std::path::Path, cut: StartCut) {
        use sqlx::{ConnectOptions, Connection};
        let check = || http_adapter::MaintenanceRequest::Check;
        match cut {
            StartCut::DuringItsReads => {
                let engine = Arc::new(HeldRead::default());
                runner.engine = engine.clone();
                let route = runner.start_maintenance("demo".into(), check(), None);
                tokio::select! {
                    answered = route => panic!("the route answered during its reads: {answered:?}"),
                    () = engine.read_begun() => {}
                }
            }
            StartCut::BeforeTheLogStoresIt => {
                runner.engine = Arc::new(Completes);
                let log = Arc::new(HeldAnswer::over(Arc::clone(&runner.store)));
                log.let_the_answer_through();
                runner.store = log.clone();
                let mut lock = sqlx::sqlite::SqliteConnectOptions::new()
                    .filename(database)
                    .connect()
                    .await
                    .expect("a connection of the test's own to the log");
                sqlx::query("BEGIN IMMEDIATE")
                    .execute(&mut lock)
                    .await
                    .expect("the test takes the write lock");
                let route = runner.start_maintenance("demo".into(), check(), None);
                tokio::select! {
                    answered = route => panic!("the route answered before the log stored the start: {answered:?}"),
                    () = log.start_reached() => {}
                }
                sqlx::query("ROLLBACK")
                    .execute(&mut lock)
                    .await
                    .expect("the test gives the write lock back");
                lock.close().await.expect("the test's connection closes");
                // The start goes on without the route: the log stores it
                // before anything else asks for the repository.
                tokio::time::timeout(std::time::Duration::from_secs(10), log.stored.notified())
                    .await
                    .expect("the log never answered the start: dropping the route cut it");
                log.assert_it_started();
            }
            StartCut::AfterTheLogStoredIt => {
                runner.engine = Arc::new(Completes);
                let log = Arc::new(HeldAnswer::over(Arc::clone(&runner.store)));
                runner.store = log.clone();
                let route = runner.start_maintenance("demo".into(), check(), None);
                tokio::select! {
                    answered = route => panic!("the route answered before the log did: {answered:?}"),
                    () = log.start_stored() => {}
                }
                log.assert_it_started();
                log.let_the_answer_through();
            }
        }
    }

    /// A client that hangs up in the middle of its request drops the handler
    /// wherever it is waiting. A maintenance start dropped there must not
    /// leave the repository held by a row nothing ends (I7): once SQLite has
    /// committed the start, only the run itself ends it, and a handler that
    /// was dropped before it learned of the commit has no one left to run it
    /// (#135). Each [`StartCut`] lands at its point whatever the threads do,
    /// the start cut at the channel is stored and then ended by its run, and
    /// the start that nothing cuts is answered with its run.
    ///
    /// This proves the daemon's level: wherever the route is dropped, the
    /// start is left whole or never made. The cuts are the route's await
    /// points as it has them; one it gains needs a cut of its own. That the
    /// log's own call, cut after its commit, leaves the repository held,
    /// which is why the start runs on a task the route can't drop, the store
    /// proves at its level
    /// (`pool_tests::a_start_cut_after_its_commit_holds_the_repository`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_maintenance_start_cut_short_at_any_await_point_leaves_the_repository_free() {
        for cut in [
            StartCut::DuringItsReads,
            StartCut::BeforeTheLogStoresIt,
            StartCut::AfterTheLogStoredIt,
        ] {
            let (mut runner, _entered, _gate, scratch) = runner_at_a_gate().await;
            let (_, data) = own_folders(scratch.path());
            cut_a_start_short(&mut runner, &data.join("ladderback.db"), cut).await;
            assert_the_repository_takes_another_check(&runner, &format!("after a start cut {cut}"))
                .await;
        }
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        runner.engine = Arc::new(Completes);
        let answer = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await;
        assert!(
            answer.is_ok(),
            "the start that nothing cut was refused: {answer:?}"
        );
        assert_the_repository_takes_another_check(&runner, "after a start nothing cut").await;
    }

    /// An operation log that says when a start reached it, stores the start
    /// and then keeps its answer until the test lets it through: the moment a
    /// client that hangs up cuts the route, after the log has the start and
    /// before the route has learned so. It keeps what it decided for the
    /// first start.
    struct HeldAnswer {
        inner: Arc<dyn OperationStore>,
        reached: tokio::sync::Notify,
        stored: tokio::sync::Notify,
        first: std::sync::OnceLock<Result<app::ports::Start, app::ports::StoreError>>,
        open: tokio::sync::watch::Sender<bool>,
    }

    impl HeldAnswer {
        fn over(inner: Arc<dyn OperationStore>) -> Self {
            Self {
                inner,
                reached: tokio::sync::Notify::new(),
                stored: tokio::sync::Notify::new(),
                first: std::sync::OnceLock::new(),
                open: tokio::sync::watch::channel(false).0,
            }
        }

        /// Waits until a start has reached the log, which may not have
        /// stored it yet.
        async fn start_reached(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.reached.notified())
                .await
                .expect("the start reached the log");
        }

        /// Waits until the log has stored a start.
        async fn start_stored(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.stored.notified())
                .await
                .expect("the log stored the start");
        }

        /// The log took the first start, as running: it holds the
        /// repository.
        fn assert_it_started(&self) {
            assert_eq!(
                self.first.get(),
                Some(&Ok(app::ports::Start::Started)),
                "the log took the start"
            );
        }

        fn let_the_answer_through(&self) {
            self.open.send_replace(true);
        }
    }

    #[async_trait]
    impl OperationStore for HeldAnswer {
        async fn recover(
            &self,
            reason: &FailureReason,
        ) -> Result<app::ports::Recovered, app::ports::StoreError> {
            self.inner.recover(reason).await
        }

        async fn save(
            &self,
            operation: &domain::execution::Operation,
            result: Option<&app::ports::EngineResult>,
        ) -> Result<(), app::ports::StoreError> {
            self.inner.save(operation, result).await
        }

        async fn try_start(
            &self,
            operation: &domain::execution::Operation,
        ) -> Result<app::ports::Start, app::ports::StoreError> {
            self.reached.notify_one();
            let decided = self.inner.try_start(operation).await;
            let _ = self.first.set(decided.clone());
            self.stored.notify_one();
            let _ = self.open.subscribe().wait_for(|open| *open).await;
            decided
        }

        async fn get(
            &self,
            id: OperationId,
        ) -> Result<Option<app::ports::OperationDetail>, app::ports::StoreError> {
            self.inner.get(id).await
        }

        async fn list(
            &self,
            query: &app::ports::HistoryQuery,
        ) -> Result<app::ports::HistoryPage, app::ports::StoreError> {
            self.inner.list(query).await
        }

        async fn ever_succeeded(
            &self,
            plan: &domain::PlanId,
        ) -> Result<bool, app::ports::StoreError> {
            self.inner.ever_succeeded(plan).await
        }
    }

    /// How operations have ended as far as the page has heard, once the task
    /// that runs them is done. That task holds a clone of the cancel registry
    /// until it has told the pages and the log how the run ended, so all the
    /// news is in when the clone is dropped.
    async fn ends_heard_when_done(runner: &PlanRunner, heard: &mut Heard) -> Vec<UiEvent> {
        let done = async {
            while Arc::strong_count(&runner.registry) > 1 {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), done)
            .await
            .expect("the run is done");
        ends_heard(heard)
    }

    /// The daemon said once, at info level, that the run of `operation`
    /// ended, and how. The run is done, so the line is in the log already.
    fn assert_said_once_that_it_ended(operation: &str, how: &str) {
        let said: Vec<String> = captured_log::infos()
            .into_iter()
            .filter(|line| line.contains(operation) && line.contains(" ended: "))
            .collect();
        let [line] = said.as_slice() else {
            panic!("one line says the run ended: {said:?}");
        };
        assert!(line.ends_with(how), "{line}");
    }

    /// A maintenance run that ends is told to the pages once, by the run,
    /// and said once in the daemon's log, by the task that ran it, however
    /// fast the route answered.
    #[tokio::test]
    async fn a_maintenance_run_that_ends_is_announced_once_and_logged_once() {
        captured_log::capture();
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, Completes, log).await;
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the check starts");

        let ends = ends_heard_when_done(&runner, &mut heard).await;
        assert_matches!(
            ends.as_slice(),
            [UiEvent::OperationSucceeded { operation, .. }] if *operation == id,
            "the page hears one end, the run's: {ends:?}"
        );
        assert_said_once_that_it_ended(&id, "Succeeded");
    }

    /// The same for a backup, which the daemon runs on a task of its own too.
    #[tokio::test]
    async fn a_backup_that_ends_is_announced_once_and_logged_once() {
        captured_log::capture();
        let (mut runner, _entered, open_hooks, _scratch) = runner_at_a_gate().await;
        open_hooks.send(true).expect("the gate opens");
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, Completes, log).await;
        let id = runner
            .start_backup("home".into())
            .await
            .expect("the plan starts");

        let ends = ends_heard_when_done(&runner, &mut heard).await;
        assert_matches!(
            ends.as_slice(),
            [UiEvent::OperationSucceeded { operation, .. }] if *operation == id,
            "the page hears one end, the run's: {ends:?}"
        );
        assert_said_once_that_it_ended(&id, "Succeeded");
    }

    /// A maintenance run cancelled after its start is announced to no page
    /// (the page that cancelled it reads the history until it settles), and
    /// the daemon's log says once that it ended cancelled.
    #[tokio::test]
    async fn a_maintenance_run_cancelled_after_its_start_is_announced_to_no_page_and_logged_once() {
        captured_log::capture();
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, IdentifiesThenParks, log).await;
        let id = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the check starts");
        runner
            .cancel(id.clone())
            .await
            .expect("a cancel straight after the answer is accepted");

        assert_eq!(
            ends_heard_when_done(&runner, &mut heard).await,
            [],
            "the page was told how a cancelled run ended"
        );
        let operation = operation_of(&id).expect("the answer is an operation ID");
        assert_eq!(ended(&runner, operation).await, OperationStatus::Cancelled);
        assert_said_once_that_it_ended(&id, "Cancelled");
    }

    /// A maintenance start refused because a run holds the repository began
    /// no operation, so no page is told how one ended; the run that holds
    /// the repository goes on undisturbed, until its cancel.
    #[tokio::test]
    async fn a_maintenance_start_refused_for_a_busy_repository_is_announced_to_no_page() {
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let log = Arc::clone(&runner.store);
        let mut heard = listen(&mut runner, IdentifiesThenParks, log).await;
        let running = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await
            .expect("the first check starts");
        let refused = runner
            .start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None)
            .await;
        assert_eq!(
            refused,
            Err(MaintenanceRefusal::Busy {
                running: running.clone()
            })
        );
        assert_eq!(
            ends_heard(&mut heard),
            [],
            "the refused start was announced, or the run it was refused for ended"
        );
        runner
            .cancel(running)
            .await
            .expect("the run that holds the repository is cancelled");

        assert_eq!(
            ends_heard_when_done(&runner, &mut heard).await,
            [],
            "the page was told how a run ended: the refused one began none, and the one cancelled is no news"
        );
    }

    /// A client that hangs up after the log has stored its maintenance start
    /// has no ID to ask for the run by, but the run goes on, and the pages
    /// are told how it ended, once, and the log says so: the task that
    /// started it ends it, not the route that was dropped.
    #[tokio::test]
    async fn a_maintenance_run_whose_client_hung_up_is_still_announced_once_and_logged_once() {
        captured_log::capture();
        let (mut runner, _entered, _gate, _scratch) = runner_at_a_gate().await;
        let log = Arc::new(HeldAnswer::over(Arc::clone(&runner.store)));
        let mut heard = listen(&mut runner, Completes, log.clone()).await;
        let route =
            runner.start_maintenance("demo".into(), http_adapter::MaintenanceRequest::Check, None);
        tokio::select! {
            answered = route => panic!("the route answered before the log did: {answered:?}"),
            () = log.start_stored() => {}
        }
        log.let_the_answer_through();

        let ends = ends_heard_when_done(&runner, &mut heard).await;
        let [UiEvent::OperationSucceeded { operation, .. }] = ends.as_slice() else {
            panic!("the page hears how the run ended, once: {ends:?}");
        };
        assert_eq!(
            history_of(&runner, operation).await.record.status,
            OperationStatus::Succeeded
        );
        assert_said_once_that_it_ended(operation, "Succeeded");
    }

    fn english() -> l10n::Localizer {
        l10n::Localizer::new(&[])
    }

    fn said(refusal: &Refusal) -> String {
        let shown = english().error(refusal.code, &refusal.args, refusal.detail.as_deref());
        assert!(shown.problems().is_empty(), "{:?}", shown.problems());
        shown.into_text()
    }

    #[test]
    fn an_event_told_while_the_configuration_cant_be_read_is_logged_by_its_code() {
        let line = report_line(
            &english(),
            &Report::ConfigUnavailable(ConfigError::Unavailable {
                file: "/home/demo/.config/ladderback/config.toml".to_owned(),
                detail: "permission denied".to_owned(),
            }),
        );
        for expected in [
            "ConfigUnavailable",
            "/home/demo/.config/ladderback/config.toml",
            "permission denied",
        ] {
            assert!(line.contains(expected), "{expected} in {line}");
        }
    }

    /// The notifier says an unusable configuration by the code, the arguments
    /// and the detail every route refuses it with, whichever way it can't be
    /// used.
    #[test]
    fn each_way_a_configuration_cant_be_used_is_logged_as_the_routes_refuse_it() {
        for (error, code, args, detail) in
            super::repository_service_tests::unusable_configurations()
        {
            let message = english().error(code, &args, detail);
            assert_eq!(
                report_line(&english(), &Report::ConfigUnavailable(error.clone())),
                format!("no notification was sent for an event: {code:?}: {message}"),
                "{error:?}"
            );
        }
    }

    #[test]
    fn a_notification_not_delivered_is_logged_by_its_code() {
        use domain::notifications::{ChannelName, EventKind, RuleName};
        let line = report_line(
            &english(),
            &Report::Undelivered(app::notifications::Undelivered {
                event: EventKind::BackupFailed,
                rule: RuleName::parse("failures").expect("a rule name"),
                channel: ChannelName::parse("ops").expect("a channel name"),
                reason: app::notifications::Unsent::Delivery(app::ports::NotifyError::TimedOut),
            }),
        );
        for expected in [
            "NotificationUndelivered",
            "backup.failed",
            "failures",
            "ops",
            "did not answer in time",
        ] {
            assert!(line.contains(expected), "{expected} in {line}");
        }
    }

    fn invalid(key: Option<&str>, at: Option<(usize, usize)>) -> ImportError {
        ImportError::Profile(ProfileError::Invalid {
            file: "nas.toml".to_owned(),
            key: key.map(str::to_owned),
            at: at.map(|(line, column)| app::ports::Position { line, column }),
            problem: app::ports::ProfileProblem::UnknownKey,
            detail: String::new(),
        })
    }

    #[test]
    fn an_invalid_profile_names_its_file_its_key_and_its_line() {
        let refusal = import_refusal(
            invalid(Some("forget.keep-lots"), Some((7, 3))),
            std::path::Path::new("/cfg/config.toml"),
        );
        assert_eq!(refusal.code, ErrorCode::ProfileInvalid);
        let said = said(&refusal);
        for part in ["nas.toml", "forget.keep-lots", "line 7", "UnknownKey"] {
            assert!(said.contains(part), "{part}: {said}");
        }
    }

    #[test]
    fn an_invalid_profile_with_no_place_does_not_say_none() {
        let refusal = import_refusal(invalid(None, None), std::path::Path::new("/cfg/c.toml"));
        assert!(refusal.args.iter().all(|(name, _)| *name == "file"));
        let said = said(&refusal);
        assert!(said.contains("nas.toml"), "{said}");
        assert!(!said.contains("none") && !said.contains("line"), "{said}");
    }

    #[test]
    fn an_unsupported_setting_names_the_key_and_the_line_when_known() {
        let unsupported = |at| {
            ImportError::Profile(ProfileError::Unsupported {
                file: "nas.toml".to_owned(),
                key: "forget.keep-tags".to_owned(),
                at,
            })
        };
        let path = std::path::Path::new("/cfg/c.toml");
        let placed = said(&import_refusal(
            unsupported(Some(app::ports::Position {
                line: 12,
                column: 1,
            })),
            path,
        ));
        assert!(placed.contains("forget.keep-tags") && placed.contains("line 12"));
        let unplaced = said(&import_refusal(unsupported(None), path));
        assert!(unplaced.contains("forget.keep-tags") && !unplaced.contains("line"));
    }

    #[test]
    fn a_profile_that_cannot_be_read_names_the_file_and_the_limit() {
        let path = std::path::Path::new("/cfg/c.toml");
        let unreadable = import_refusal(
            ImportError::Profile(ProfileError::Unreadable {
                file: "missing.toml".to_owned(),
            }),
            path,
        );
        assert_eq!(unreadable.code, ErrorCode::ProfileUnreadable);
        assert!(said(&unreadable).contains("missing.toml"));
        let large = import_refusal(
            ImportError::Profile(ProfileError::TooLarge {
                file: "big.toml".to_owned(),
                limit: 1 << 20,
            }),
            path,
        );
        assert_eq!(large.code, ErrorCode::ProfileTooLarge);
        let said = said(&large);
        assert!(
            said.contains("big.toml") && said.contains("1 MiB"),
            "{said}"
        );
    }

    #[test]
    fn an_export_that_cannot_be_made_says_which_plan_or_which_setting() {
        let path = std::path::Path::new("/cfg/c.toml");
        let unknown = export_refusal(
            ExportError::PlanUnknown(PlanId::parse("ghost").expect("a slug")),
            path,
        );
        assert_eq!(unknown.code, ErrorCode::PlanUnknown);
        assert!(said(&unknown).contains("ghost"));
        let inline = export_refusal(ExportError::Render(ExportRefusal::InlineSecret), path);
        assert_eq!(inline.code, ErrorCode::InlineSecret);
        assert!(said(&inline).contains("password"));
        let reference = export_refusal(
            ExportError::Render(ExportRefusal::Reference {
                key: "repository".to_owned(),
            }),
            path,
        );
        assert_eq!(reference.code, ErrorCode::ProfileExportReference);
        assert!(said(&reference).contains("repository"));
        let url = export_refusal(
            ExportError::Render(ExportRefusal::UrlPassword {
                key: "repo-hot".to_owned(),
            }),
            path,
        );
        assert_eq!(url.code, ErrorCode::ProfileExportUrlPassword);
        assert!(said(&url).contains("repo-hot"));
        let tag = export_refusal(
            ExportError::Render(ExportRefusal::TagComma {
                tag: "a,b".to_owned(),
            }),
            path,
        );
        assert_eq!(tag.code, ErrorCode::ProfileExportTagComma);
        assert!(said(&tag).contains("a,b"));
        let config = export_refusal(
            ExportError::Config(ConfigError::SchemaMissing {
                file: "/cfg/c.toml".to_owned(),
            }),
            path,
        );
        assert_eq!(config.code, ErrorCode::ConfigSchemaMissing);
    }

    #[test]
    fn a_clash_names_the_alias_or_the_plan() {
        let path = std::path::Path::new("/cfg/c.toml");
        let alias = import_refusal(
            ImportError::AliasTaken(RepositoryAlias::parse("nas").expect("a slug")),
            path,
        );
        assert_eq!(alias.code, ErrorCode::RepositoryAliasTaken);
        assert!(said(&alias).contains("nas"));
        let configured = import_refusal(
            ImportError::AlreadyConfigured(RepositoryAlias::parse("nas").expect("a slug")),
            path,
        );
        assert_eq!(configured.code, ErrorCode::RepositoryAlreadyConfigured);
        assert!(said(&configured).contains("nas"));
        let plan = import_refusal(
            ImportError::PlanTaken(PlanId::parse("home").expect("a slug")),
            path,
        );
        assert_eq!(plan.code, ErrorCode::PlanIdTaken);
        assert!(said(&plan).contains("home"));
    }

    #[test]
    fn a_name_that_is_not_an_alias_is_refused_by_its_code_alone() {
        let refusal = repository_alias_invalid();
        assert_eq!(refusal.code, ErrorCode::RepositoryAliasInvalid);
        assert!(refusal.args.is_empty());
        assert_eq!(refusal.detail, None);
        assert!(said(&refusal).contains("lower-case letters"));
    }

    #[test]
    fn no_folder_for_the_configuration_is_refused_by_its_code_alone() {
        let refusal = config_folder_unknown();
        assert_eq!(refusal.code, ErrorCode::ConfigFolderUnknown);
        assert!(refusal.args.is_empty());
        assert_eq!(refusal.detail, None);
        let shown = said(&refusal);
        assert!(shown.contains("LADDERBACK_CONFIG_DIR"), "{shown}");
    }

    #[test]
    fn a_runtime_that_did_not_start_is_refused_with_the_operating_systems_words() {
        let refusal = runtime_unavailable(&std::io::Error::other("Too many open files"));
        assert_eq!(refusal.code, ErrorCode::RuntimeUnavailable);
        assert!(refusal.args.is_empty());
        assert_eq!(refusal.detail.as_deref(), Some("Too many open files"));
        let shown = said(&refusal);
        assert!(shown.contains("Too many open files"), "{shown}");
        assert!(shown.contains("open files and threads"), "{shown}");
    }

    #[test]
    fn a_merged_configuration_that_is_not_valid_names_the_configuration_file() {
        let refusal = import_refusal(
            ImportError::Config(ConfigError::Invalid {
                file: String::new(),
                key: None,
                at: None,
                problem: app::ports::Problem::Violation(domain::planning::Violation::NoSources {
                    index: 0,
                    plan: PlanId::parse("home").expect("a slug"),
                }),
                detail: String::new(),
            }),
            std::path::Path::new("/cfg/config.toml"),
        );
        assert_eq!(refusal.code, ErrorCode::ConfigInvalid);
        let said = said(&refusal);
        assert!(said.contains("/cfg/config.toml"), "{said}");
    }

    #[test]
    fn a_store_that_cannot_be_read_is_the_configuration_codes() {
        let path = std::path::Path::new("/cfg/config.toml");
        let unavailable = import_refusal(
            ImportError::Config(ConfigError::Unavailable {
                file: "/cfg/config.toml".to_owned(),
                detail: "permission denied".to_owned(),
            }),
            path,
        );
        assert_eq!(unavailable.code, ErrorCode::ConfigUnavailable);
        assert!(said(&unavailable).contains("permission denied"));
        let newer = import_refusal(
            ImportError::Config(ConfigError::SchemaNewer {
                file: "/cfg/config.toml".to_owned(),
                found: 9,
                supported: 1,
            }),
            path,
        );
        assert_eq!(newer.code, ErrorCode::ConfigSchemaNewer);
        assert!(said(&newer).contains('9'));
    }

    #[test]
    fn every_notice_is_a_warning_that_renders_what_it_names() {
        let notices = [
            (ProfileNotice::KeyIgnored { key: "copy".into() }, "copy"),
            (ProfileNotice::Prune, "Pruning will run from one place"),
            (ProfileNotice::EnvIgnored, "global.env"),
            (ProfileNotice::HooksPerPlan, "once per plan"),
            (
                ProfileNotice::UsedProfileMissing {
                    name: "ghost".into(),
                },
                "ghost",
            ),
            (
                ProfileNotice::InlineSecret {
                    field: "repository.password".into(),
                },
                "repository.password",
            ),
            (
                ProfileNotice::LocationSecret {
                    field: "repository.repository".into(),
                    rclone: false,
                },
                "The address in the repository.repository setting holds a password as text",
            ),
            (
                ProfileNotice::LocationSecret {
                    field: "repository.repo-hot".into(),
                    rclone: true,
                },
                "Set up the remote in rclone's own configuration file",
            ),
            (
                ProfileNotice::GlobNeverMatches {
                    plan: "home".into(),
                    glob: "!.local/share/containers".into(),
                    absolute: Some("!/home/user/.local/share/containers".into()),
                },
                "Write the whole path instead, as in !/home/user/.local/share/containers.",
            ),
            (
                ProfileNotice::GlobNeverMatches {
                    plan: "home".into(),
                    glob: "!docker/app/cache".into(),
                    absolute: None,
                },
                "Write the whole path instead, starting with the folder of the source it belongs to.",
            ),
            (
                ProfileNotice::TagDropped {
                    tag: "ladderback-plan:x".into(),
                },
                "ladderback-plan:x",
            ),
            (
                ProfileNotice::OptionLeftOut {
                    option: "secret-key".into(),
                    variable: "OPENDAL_SECRET_KEY".into(),
                },
                "OPENDAL_SECRET_KEY",
            ),
            (ProfileNotice::RetentionEmpty, "keep-*"),
            (
                ProfileNotice::RepositorySettingKept {
                    repository: "demo-nas".into(),
                    setting: "group-by".into(),
                },
                "demo-nas was already configured, so the profile's plans were added to it, and it kept its own group-by",
            ),
            (
                ProfileNotice::LocationsNotExported { further: 1 },
                "1 further location was left out",
            ),
            (
                ProfileNotice::LocationsNotExported { further: 2 },
                "2 further locations were left out",
            ),
        ];
        for (notice, expected) in notices {
            let notice = profile_notice(notice);
            let shown = english().warning(notice.code, &notice.args, None);
            assert!(shown.problems().is_empty(), "{:?}", shown.problems());
            assert!(shown.text().contains(expected), "{expected}: {shown}");
        }
    }

    /// A keyring command the export's PATH lacks is a warning that names it,
    /// and for `secret-tool` the packages it comes in, by distribution.
    #[test]
    fn a_missing_keyring_command_is_a_warning_naming_its_package() {
        for (program, expected) in [
            (
                "secret-tool",
                &[
                    "with secret-tool, which is not on this computer's PATH",
                    "the libsecret package on Fedora",
                    "libsecret-tools on Debian and Ubuntu",
                ][..],
            ),
            (
                "security",
                &["with security, which is not on this computer's PATH"][..],
            ),
        ] {
            let notice = profile_notice(ProfileNotice::KeyringCommandMissing {
                program: program.to_owned(),
            });
            assert_eq!(notice.code, WarningCode::ProfileKeyringCommandMissing);
            assert_eq!(notice.args, vec![("program", text(program))]);
            let shown = english().warning(notice.code, &notice.args, None);
            assert!(shown.problems().is_empty(), "{:?}", shown.problems());
            for expected in expected {
                assert!(shown.text().contains(expected), "{expected}: {shown}");
            }
        }
    }

    /// What a check found reaches the page and the operation's detail under
    /// the code of its level, not as a warning this version can't describe.
    #[test]
    fn a_checks_findings_reach_the_page_under_the_codes_of_their_levels() {
        for (kind, code) in [
            (WarningKind::CheckWarning, WarningCode::CheckWarning),
            (WarningKind::CheckError, WarningCode::CheckError),
        ] {
            assert_eq!(warning_code(kind), code);
            let mut publish = Publish {
                operation: "018f0000-0000-7000-8000-000000000001".to_owned(),
                progress: LiveProgress::new(),
            };
            let found = EngineEvent::Warning(domain::execution::Warning {
                kind,
                item: None,
                during: "check".to_owned(),
                detail: "pack 5ca1ab1e not referenced in index".to_owned(),
            });
            assert_eq!(
                publish.ui(&found),
                Some(UiEvent::OperationWarning {
                    operation: "018f0000-0000-7000-8000-000000000001".to_owned(),
                    code,
                    args: Vec::new(),
                })
            );
        }
    }

    /// A secret file others can read reaches the page with the two
    /// arguments its message takes: the file's path and its mode (#386).
    #[test]
    fn a_readable_secret_file_reaches_the_page_by_its_path_and_mode() {
        let mut publish = Publish {
            operation: "018f0000-0000-7000-8000-000000000001".to_owned(),
            progress: LiveProgress::new(),
        };
        let found = EngineEvent::Warning(domain::execution::Warning {
            kind: WarningKind::SecretFileReadable,
            item: Some("/home/demo/nas-password".to_owned()),
            during: "secret".to_owned(),
            detail: "0644".to_owned(),
        });
        let Some(UiEvent::OperationWarning { code, args, .. }) = publish.ui(&found) else {
            panic!("the warning reaches the page");
        };
        assert_eq!(code, WarningCode::SecretFileReadable);
        assert_eq!(
            args,
            [
                MsgArg::Text("/home/demo/nas-password".to_owned()),
                MsgArg::Text("0644".to_owned()),
            ]
        );
    }
}

#[cfg(test)]
mod repository_service_tests {
    use std::assert_matches;

    use super::captured_log::{self, capture};
    use std::collections::BTreeMap;
    use std::sync::{Mutex, PoisonError};

    use api::{ErrorCode, MsgArg, PasswordDto, RepositorySettingsDto};
    use app::fakes::{FakeSecrets, InMemoryConfigStore};
    use app::ports::JobControl;

    use domain::RepositoryId;

    use super::*;

    /// An engine that answers every job with the scripted outcome, and keeps
    /// the jobs it was given. A location in `ids` is a repository of its
    /// own: a success there names that ID instead of the scripted one.
    struct Scripted {
        outcome: Mutex<Result<RepositoryId, FailureReason>>,
        /// What a `Job::Identify` finds instead of the outcome, once set.
        found: Mutex<Option<Result<RepositoryId, FailureReason>>>,
        ids: Mutex<BTreeMap<String, RepositoryId>>,
        refuses: Mutex<Option<EngineError>>,
        /// How many of the next jobs never answer.
        hangs: Mutex<usize>,
        seen: Mutex<Vec<Job>>,
    }

    struct Stop;

    #[async_trait]
    impl JobControl for Stop {
        async fn stop(&mut self) {}
    }

    #[async_trait]
    impl Engine for Scripted {
        async fn run(&self, _op: OperationId, job: Job) -> Result<RunningJob, EngineError> {
            let own = match &job {
                Job::Identify(access) | Job::Init(access) => self
                    .ids
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&access.repository)
                    .cloned(),
                _ => None,
            };
            let found = match &job {
                Job::Identify(_) => self
                    .found
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
                _ => None,
            };
            self.seen
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(job);
            if let Some(error) = self
                .refuses
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
            {
                return Err(error);
            }
            {
                let mut hangs = self.hangs.lock().unwrap_or_else(PoisonError::into_inner);
                if *hangs > 0 {
                    *hangs -= 1;
                    return Ok(RunningJob {
                        events: futures::stream::pending().boxed(),
                        control: Box::new(Stop),
                    });
                }
            }
            let outcome = found.unwrap_or_else(|| {
                self.outcome
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone()
            });
            let event = match outcome {
                Ok(id) => EngineEvent::Succeeded(EngineResult::Repository {
                    id: own.unwrap_or(id),
                }),
                Err(reason) => EngineEvent::Failed(reason),
            };
            Ok(RunningJob {
                events: futures::stream::iter([event]).boxed(),
                control: Box::new(Stop),
            })
        }
    }

    /// A configuration store that lets the other task run between a read
    /// and the write that follows it, as a slow disk does.
    struct Yielding(InMemoryConfigStore);

    #[async_trait]
    impl ConfigStore for Yielding {
        async fn load(&self) -> Result<domain::planning::AgentConfig, ConfigError> {
            let loaded = self.0.load().await;
            tokio::task::yield_now().await;
            loaded
        }

        async fn save(&self, config: &domain::planning::AgentConfig) -> Result<(), ConfigError> {
            self.0.save(config).await
        }

        async fn record_id(
            &self,
            alias: &domain::RepositoryAlias,
            read: &domain::planning::RepositoryConfig,
            id: &RepositoryId,
        ) -> Result<(), app::ports::RecordError> {
            self.0.record_id(alias, read, id).await
        }
    }

    /// A configuration store that can't be read.
    struct Broken(ConfigError);

    #[async_trait]
    impl ConfigStore for Broken {
        async fn load(&self) -> Result<domain::planning::AgentConfig, ConfigError> {
            Err(self.0.clone())
        }

        async fn save(&self, _config: &domain::planning::AgentConfig) -> Result<(), ConfigError> {
            Err(self.0.clone())
        }

        async fn record_id(
            &self,
            _alias: &domain::RepositoryAlias,
            _read: &domain::planning::RepositoryConfig,
            _id: &RepositoryId,
        ) -> Result<(), app::ports::RecordError> {
            Err(app::ports::RecordError::Store(self.0.clone()))
        }
    }

    fn repository_id() -> RepositoryId {
        RepositoryId::parse(&"a".repeat(64)).expect("a repository ID")
    }

    fn engine() -> Arc<Scripted> {
        Arc::new(Scripted {
            outcome: Mutex::new(Ok(repository_id())),
            found: Mutex::new(None),
            ids: Mutex::new(BTreeMap::new()),
            refuses: Mutex::new(None),
            hangs: Mutex::new(0),
            seen: Mutex::new(Vec::new()),
        })
    }

    /// The service over `config` and an operation log of its own, empty.
    fn service(engine: &Arc<Scripted>, config: Arc<dyn ConfigStore>) -> RepositoryService {
        RepositoryService::new(
            clock(),
            engine.clone(),
            Arc::new(app::fakes::FakeNetworkLink::new()),
            ResolveRepository::new(Arc::new(FakeSecrets::new())),
            config,
            Arc::new(app::fakes::InMemoryOperationStore::new()),
            Arc::default(),
        )
    }

    /// A clock that jumps to the deadline of whoever sleeps on it, so a job
    /// that never answers is out of time the moment it is waited for.
    fn clock() -> Arc<dyn Clock> {
        Arc::new(app::fakes::FakeClock::auto_advancing(
            domain::Timestamp::from_second(1_800_000_000).expect("a timestamp"),
        ))
    }

    fn jobs(engine: &Scripted) -> usize {
        engine
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    fn settings_dto() -> RepositorySettingsDto {
        RepositorySettingsDto {
            repository: "/srv/demo/repo".to_owned(),
            repo_hot: None,
            password: PasswordDto::Inline {
                value: "demo-password".to_owned(),
            },
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        }
    }

    fn alias(name: &str) -> RepositoryAlias {
        RepositoryAlias::parse(name).expect("an alias")
    }

    fn connect(alias: &str, settings: RepositorySettingsDto) -> api::ConnectRepositoryDto {
        api::ConnectRepositoryDto {
            alias: alias.to_owned(),
            settings,
            maintain_here: false,
        }
    }

    fn init(alias: Option<&str>, settings: RepositorySettingsDto) -> api::InitRepositoryDto {
        api::InitRepositoryDto {
            alias: alias.map(str::to_owned),
            settings,
        }
    }

    async fn saved(config: &dyn ConfigStore) -> domain::planning::AgentConfig {
        config.load().await.expect("the configuration loads")
    }

    /// Leaves the configuration as an imported profile's are before anything
    /// reached their repositories: no entry records its repository's ID, and
    /// the plan `demo-home` backs up to `demo`.
    async fn imported(config: &dyn ConfigStore) {
        let recorded = saved(config).await;
        let repositories = recorded
            .repositories()
            .iter()
            .map(|(alias, entry)| {
                let mut entry = entry.clone();
                entry.id = None;
                (alias.clone(), entry)
            })
            .collect();
        let mut plans = recorded.plans().to_vec();
        plans.push(domain::planning::Plan::new(
            domain::PlanId::parse("demo-home").expect("a plan ID"),
            alias("demo"),
            vec![domain::planning::Source::parse("/home/demo").expect("a source")],
        ));
        let forgotten = domain::planning::AgentConfig::new(
            recorded.agent().clone(),
            recorded.hooks().clone(),
            repositories,
            plans,
        )
        .expect("a valid configuration");
        config
            .save(&forgotten)
            .await
            .expect("the configuration saves");
    }

    /// Records in `operations` a backup of the plan `demo-home` that ended
    /// as `end` has it, succeeded (`Ok`) or failed.
    async fn backed_up(operations: &dyn OperationStore, end: Result<(), FailureReason>) {
        let at = domain::Timestamp::UNIX_EPOCH;
        let mut backup = domain::execution::Operation::queue(
            OperationId::from_uuid(Uuid::now_v7()),
            domain::execution::OperationKind::Backup,
            domain::execution::Trigger::Manual,
            domain::execution::Target {
                plan: Some(domain::PlanId::parse("demo-home").expect("a plan ID")),
                repository: None,
            },
            at,
        );
        backup.start(at).expect("it starts");
        match end {
            Ok(()) => backup.succeed(at),
            Err(reason) => backup.fail(reason, at),
        }
        .expect("it ends");
        operations
            .save(&backup, None)
            .await
            .expect("the backup is recorded");
    }

    /// [`imported`], then a backup of `demo-home` that found no repository
    /// at `demo`'s location: what makes its row offer Create (decision 53).
    /// The engine finds the location empty now too, as Create looks first.
    async fn imported_and_found_none(
        engine: &Scripted,
        service: &RepositoryService,
        config: &dyn ConfigStore,
    ) {
        imported(config).await;
        backed_up(&*service.operations, Err(FailureReason::RepositoryNotFound)).await;
        *engine.found.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Err(FailureReason::RepositoryNotFound));
    }

    #[tokio::test]
    async fn a_test_reports_the_id_and_saves_nothing() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let answer = service(&engine, config.clone())
            .test(api::TestRepositoryDto {
                settings: settings_dto(),
            })
            .await;
        assert_eq!(answer, Ok(api::RepositoryIdDto { id: "a".repeat(64) }));
        assert_matches!(
            engine.seen.lock().unwrap_or_else(PoisonError::into_inner)[0],
            Job::Identify(_)
        );
        assert!(saved(&*config).await.repositories().is_empty());
    }

    #[tokio::test]
    async fn a_connection_saves_the_settings_as_written() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let mut settings = settings_dto();
        settings.repo_hot = Some("/srv/hot".to_owned());
        settings.password = PasswordDto::File {
            path: "/run/secrets/pw".to_owned(),
        };
        settings.options = [("a".to_owned(), "1".to_owned())].into();
        settings.options_hot = [("b".to_owned(), "2".to_owned())].into();
        settings.options_cold = [("c".to_owned(), "3".to_owned())].into();
        let secrets = FakeSecrets::new();
        secrets.write_file("/run/secrets/pw", b"demo-password\n", 0o600);
        let answer = RepositoryService::new(
            clock(),
            engine.clone(),
            Arc::new(app::fakes::FakeNetworkLink::new()),
            ResolveRepository::new(Arc::new(secrets)),
            config.clone(),
            Arc::new(app::fakes::InMemoryOperationStore::new()),
            Arc::default(),
        )
        .connect(connect("demo", settings))
        .await;
        assert_eq!(answer.map(|id| id.id), Ok("a".repeat(64)));
        let config = saved(&*config).await;
        let repository = config.repository(&alias("demo")).expect("saved");
        let settings = &repository.settings;
        assert_eq!(settings.repository, "/srv/demo/repo");
        assert_eq!(settings.repo_hot.as_deref(), Some("/srv/hot"));
        // A reference stays a reference: the file is read when a job needs it.
        assert_eq!(
            settings.password,
            domain::secrets::SecretRef::File {
                path: "/run/secrets/pw".to_owned(),
                read: domain::secrets::FileRead::FirstLine,
            }
        );
        assert_eq!(settings.options.get("a").map(String::as_str), Some("1"));
        assert_eq!(settings.options_hot.get("b").map(String::as_str), Some("2"));
        assert_eq!(
            settings.options_cold.get("c").map(String::as_str),
            Some("3")
        );
    }

    #[tokio::test]
    async fn a_connection_asks_this_agent_to_maintain_the_repository_only_when_the_request_does() {
        for (maintain_here, saved_as) in [(true, Maintenance::Local), (false, Maintenance::None)] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            service(&engine, config.clone())
                .connect(api::ConnectRepositoryDto {
                    maintain_here,
                    ..connect("demo", settings_dto())
                })
                .await
                .unwrap_or_else(|e| panic!("{maintain_here}: {e:?}"));
            let config = saved(&*config).await;
            assert_eq!(
                config.repository(&alias("demo")).map(|r| r.maintenance),
                Some(saved_as),
                "maintain-here {maintain_here}"
            );
        }
    }

    #[tokio::test]
    async fn the_other_password_sources_become_their_references() {
        use domain::secrets::{KeyringEntry, SecretRef, SecretValue};
        for (dto, expected) in [
            (
                PasswordDto::Inline {
                    value: "typed".into(),
                },
                SecretRef::Inline(SecretValue::new("typed")),
            ),
            (
                PasswordDto::Keyring {
                    entry: "ladderback/nas".into(),
                },
                SecretRef::Keyring(KeyringEntry::parse("ladderback/nas").expect("an entry")),
            ),
            (
                PasswordDto::Env {
                    name: "DEMO_PASSWORD".into(),
                },
                SecretRef::Env("DEMO_PASSWORD".into()),
            ),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let mut settings = settings_dto();
            settings.password = dto.clone();
            // Only a connection saves the reference, and testing it first
            // resolves it: the fake knows this variable and this entry.
            let secrets = FakeSecrets::new();
            secrets.set_env("DEMO_PASSWORD", "demo-password");
            secrets.set_keyring_entry(
                &KeyringEntry::parse("ladderback/nas").expect("an entry"),
                "demo-password",
            );
            let service = RepositoryService::new(
                clock(),
                engine.clone(),
                Arc::new(app::fakes::FakeNetworkLink::new()),
                ResolveRepository::new(Arc::new(secrets)),
                config.clone(),
                Arc::new(app::fakes::InMemoryOperationStore::new()),
                Arc::default(),
            );
            service
                .connect(connect("demo", settings))
                .await
                .unwrap_or_else(|e| panic!("{dto:?}: {e:?}"));
            let config = saved(&*config).await;
            let kept = &config.repository(&alias("demo")).expect("saved").settings;
            assert_eq!(kept.password, expected, "{dto:?}");
        }
    }

    #[tokio::test]
    async fn a_value_the_settings_cannot_take_is_an_invalid_body_and_nothing_runs() {
        for password in [
            PasswordDto::File {
                path: String::new(),
            },
            PasswordDto::Keyring {
                entry: "no-slash".into(),
            },
            PasswordDto::Keyring {
                entry: "/name".into(),
            },
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            let mut settings = settings_dto();
            settings.password = password.clone();
            assert_eq!(
                service
                    .test(api::TestRepositoryDto {
                        settings: settings.clone()
                    })
                    .await,
                Err(RepositoriesError::InvalidBody),
                "{password:?}"
            );
            assert_eq!(
                service.connect(connect("demo", settings.clone())).await,
                Err(RepositoriesError::InvalidBody),
                "{password:?}"
            );
            assert_eq!(
                service.init(init(None, settings)).await,
                Err(RepositoriesError::InvalidBody),
                "{password:?}"
            );
            assert_eq!(jobs(&engine), 0, "{password:?}");
        }
    }

    #[tokio::test]
    async fn a_name_that_is_not_an_alias_is_refused_before_anything_runs() {
        let long = "a".repeat(65);
        for name in [
            "Not A Name",
            "",
            "-lead",
            "under_score",
            "demo/x",
            long.as_str(),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            assert_eq!(
                service.connect(connect(name, settings_dto())).await,
                Err(RepositoriesError::AliasInvalid),
                "{name:?}"
            );
            // An initialisation that would be saved is refused before it
            // creates a repository it couldn't then keep.
            assert_eq!(
                service.init(init(Some(name), settings_dto())).await,
                Err(RepositoriesError::AliasInvalid),
                "{name:?}"
            );
            assert_eq!(jobs(&engine), 0, "{name:?} ran a job");
            assert!(saved(&*config).await.repositories().is_empty());
        }
    }

    #[tokio::test]
    async fn an_alias_already_used_is_refused_and_the_first_repository_stays() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the first connection");
        let mut other = settings_dto();
        other.repository = "/srv/other".to_owned();
        assert_eq!(
            service.connect(connect("demo", other)).await,
            Err(RepositoriesError::AliasTaken {
                alias: "demo".to_owned()
            })
        );
        let config = saved(&*config).await;
        assert_eq!(config.repositories().len(), 1);
        assert_eq!(
            config
                .repository(&alias("demo"))
                .expect("kept")
                .settings
                .repository,
            "/srv/demo/repo"
        );
    }

    #[tokio::test]
    async fn an_initialisation_is_saved_only_when_it_names_an_alias() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        let answer = service.init(init(None, settings_dto())).await;
        assert_eq!(
            answer,
            Ok(api::RepositoryInitDto {
                id: "a".repeat(64),
                saved: false
            })
        );
        assert_matches!(
            engine.seen.lock().unwrap_or_else(PoisonError::into_inner)[0],
            Job::Init(_)
        );
        assert!(saved(&*config).await.repositories().is_empty());

        let answer = service.init(init(Some("fresh"), settings_dto())).await;
        assert_eq!(
            answer,
            Ok(api::RepositoryInitDto {
                id: "a".repeat(64),
                saved: true
            })
        );
        assert!(saved(&*config).await.repository(&alias("fresh")).is_some());
    }

    #[tokio::test]
    async fn a_saved_repository_is_created_from_its_entry_which_records_only_its_new_id() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the entry is saved");
        // The entry as the connection left it, recording the ID it reached.
        let recorded = saved(&*config).await;
        imported_and_found_none(&engine, &service, &*config).await;
        let answer = service.create_saved("demo".to_owned()).await;
        assert_eq!(answer, Ok(api::RepositoryIdDto { id: "a".repeat(64) }));
        {
            let seen = engine.seen.lock().unwrap_or_else(PoisonError::into_inner);
            let [_, Job::Identify(looked), Job::Init(access)] = seen.as_slice() else {
                panic!("creating looks at the location, then runs Job::Init, got {seen:?}");
            };
            assert_eq!(looked.repository, "/srv/demo/repo");
            assert_eq!(access.repository, "/srv/demo/repo");
        }
        assert_eq!(
            saved(&*config).await.repositories(),
            recorded.repositories()
        );
    }

    /// An entry that records an ID was reached once: it is refused as a
    /// repository already there, its location redacted, and nothing runs.
    #[tokio::test]
    async fn a_saved_repository_whose_entry_records_an_id_is_refused_as_existing_and_nothing_runs()
    {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        let mut settings = settings_dto();
        settings.repository = "rest:https://backup:hunter2@nas.example/repo".to_owned();
        service
            .connect(connect("demo", settings))
            .await
            .expect("the entry is saved");
        let before = saved(&*config).await;
        let Err(RepositoriesError::Exists { location }) =
            service.create_saved("demo".to_owned()).await
        else {
            panic!("an entry that records an ID is refused as existing");
        };
        assert!(!location.contains("hunter2"), "{location}");
        assert!(location.contains("nas.example/repo"), "{location}");
        assert_eq!(jobs(&engine), 1, "only the connection's test ran");
        assert_eq!(saved(&*config).await, before);
    }

    /// A row that doesn't offer Create is refused it as not offered, not as a
    /// repository already there, whatever page sent the request, and nothing
    /// runs: nothing on record, or a backup that once succeeded before the
    /// latest found none there (decision 53).
    #[tokio::test]
    async fn a_saved_repository_no_job_found_missing_is_refused_as_not_offered_and_nothing_runs() {
        let histories: [&[Result<(), FailureReason>]; 2] =
            [&[], &[Ok(()), Err(FailureReason::RepositoryNotFound)]];
        for history in histories {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            service
                .connect(connect("demo", settings_dto()))
                .await
                .expect("the entry is saved");
            imported(&*config).await;
            for end in history {
                backed_up(&*service.operations, end.clone()).await;
            }
            let before = saved(&*config).await;
            assert_eq!(
                service.create_saved("demo".to_owned()).await,
                Err(RepositoriesError::CreateNotOffered),
                "{history:?}"
            );
            assert_eq!(jobs(&engine), 1, "only the connection's test ran");
            assert_eq!(saved(&*config).await, before, "{history:?}");
        }
    }

    /// The row offers Create on what a backup found, wherever it ran; the
    /// entry may since name a location whose folder isn't there, as on a
    /// drive that isn't plugged in. Create looks first, and is refused as
    /// not offered before anything is created (decision 53).
    #[tokio::test]
    async fn a_saved_repository_whose_folder_is_not_there_is_refused_as_not_offered() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the entry is saved");
        imported_and_found_none(&engine, &service, &*config).await;
        *engine.found.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Err(FailureReason::EngineFailed {
                detail: "the repository folder is not there".into(),
            }));
        let before = saved(&*config).await;
        assert_eq!(
            service.create_saved("demo".to_owned()).await,
            Err(RepositoriesError::CreateNotOffered)
        );
        assert_eq!(saved(&*config).await, before);
        let seen = engine.seen.lock().unwrap_or_else(PoisonError::into_inner);
        assert_matches!(
            seen.as_slice(),
            [_, Job::Identify(_)],
            "only the connection's test and Create's look ran: {seen:?}"
        );
    }

    /// An operation log that can't be read can't say whether the row offers
    /// Create: it is refused with the log's own words, and nothing runs.
    #[tokio::test]
    async fn creating_while_the_operation_log_cannot_be_read_is_refused_and_nothing_runs() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = RepositoryService::new(
            clock(),
            engine.clone(),
            Arc::new(app::fakes::FakeNetworkLink::new()),
            ResolveRepository::new(Arc::new(FakeSecrets::new())),
            config.clone(),
            Arc::new(NoLog),
            Arc::default(),
        );
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the entry is saved");
        imported(&*config).await;
        assert_eq!(
            service.create_saved("demo".to_owned()).await,
            Err(RepositoriesError::Unavailable {
                code: ErrorCode::OperationFailed,
                args: vec![],
                detail: Some("the database is locked".to_owned()),
            })
        );
        assert_eq!(jobs(&engine), 1, "only the connection's test ran");
    }

    #[tokio::test]
    async fn creating_an_unknown_or_unnamed_repository_is_refused_and_nothing_runs() {
        let engine = engine();
        let service = service(&engine, Arc::new(InMemoryConfigStore::new()));
        for alias in ["demo", "Not A Name"] {
            assert_eq!(
                service.create_saved(alias.to_owned()).await,
                Err(RepositoriesError::Unknown {
                    alias: alias.to_owned()
                }),
                "{alias}"
            );
        }
        assert_eq!(jobs(&engine), 0);
    }

    #[tokio::test]
    async fn a_saved_repository_already_there_is_refused_with_its_location_redacted() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        let mut settings = settings_dto();
        settings.repository = "rest:https://backup:hunter2@nas.example/repo".to_owned();
        service
            .connect(connect("demo", settings))
            .await
            .expect("the entry is saved");
        imported_and_found_none(&engine, &service, &*config).await;
        *engine
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(FailureReason::RepositoryExists);
        let Err(RepositoriesError::Exists { location }) =
            service.create_saved("demo".to_owned()).await
        else {
            panic!("an existing repository is refused as one");
        };
        assert!(!location.contains("hunter2"), "{location}");
        assert!(location.contains("nas.example/repo"), "{location}");
    }

    /// A configuration that can't be read refuses the creation with the
    /// configuration's own code: the alias may well be there.
    #[tokio::test]
    async fn creating_from_a_configuration_that_cannot_be_read_is_refused_with_its_code() {
        let engine = engine();
        let service = service(
            &engine,
            Arc::new(Broken(ConfigError::Unavailable {
                file: "/c/config.toml".into(),
                detail: "permission denied".into(),
            })),
        );
        let Err(RepositoriesError::Unavailable { code, .. }) =
            service.create_saved("demo".to_owned()).await
        else {
            panic!("a configuration that can't be read is refused as one");
        };
        assert_eq!(code, ErrorCode::ConfigUnavailable);
        assert_eq!(jobs(&engine), 0);
    }

    /// The entry's password that can't be read is refused with the secret's
    /// code, as a test of the same settings would be.
    #[tokio::test]
    async fn a_saved_repository_whose_password_cannot_be_read_is_refused_with_the_secrets_code() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let mut settings = settings_dto();
        settings.password = PasswordDto::Env {
            name: "DEMO_PASSWORD".into(),
        };
        // Saved while the variable was set; created once it no longer is.
        let secrets = FakeSecrets::new();
        secrets.set_env("DEMO_PASSWORD", "demo-password");
        RepositoryService::new(
            clock(),
            engine.clone(),
            Arc::new(app::fakes::FakeNetworkLink::new()),
            ResolveRepository::new(Arc::new(secrets)),
            config.clone(),
            Arc::new(app::fakes::InMemoryOperationStore::new()),
            Arc::default(),
        )
        .connect(connect("demo", settings))
        .await
        .expect("the entry is saved");
        let creating = service(&engine, config.clone());
        imported_and_found_none(&engine, &creating, &*config).await;
        assert_eq!(
            creating.create_saved("demo".to_owned()).await,
            Err(RepositoriesError::SecretRefused(SecretRefusal {
                code: ErrorCode::SecretMissing,
                args: vec![
                    ("field", MsgArg::Text("password".to_owned())),
                    ("source", MsgArg::Text("DEMO_PASSWORD".to_owned())),
                ],
                detail: None,
            }))
        );
        assert_eq!(jobs(&engine), 1, "only the connection's test ran");
    }

    /// A job that failed is refused with the code of its failure: one that
    /// finds no repository as `repository-not-found`, and any other in the
    /// engine's words, as an initialisation of new settings is.
    #[tokio::test]
    async fn a_saved_repository_whose_job_failed_is_refused_with_the_failures_code() {
        for (reason, refusal) in [
            (
                FailureReason::RepositoryNotFound,
                RepositoriesError::NotFound,
            ),
            (
                FailureReason::EngineFailed {
                    detail: "disk full".into(),
                },
                RepositoriesError::OperationFailed {
                    detail: "disk full".into(),
                },
            ),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            service
                .connect(connect("demo", settings_dto()))
                .await
                .expect("the entry is saved");
            imported_and_found_none(&engine, &service, &*config).await;
            *engine
                .outcome
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Err(reason);
            assert_eq!(service.create_saved("demo".to_owned()).await, Err(refusal));
        }
    }

    #[tokio::test]
    async fn an_empty_password_or_location_is_an_invalid_body_and_nothing_runs() {
        let mut empty_password = settings_dto();
        empty_password.password = PasswordDto::Inline {
            value: String::new(),
        };
        let mut empty_location = settings_dto();
        empty_location.repository = String::new();
        let mut empty_hot = settings_dto();
        empty_hot.repo_hot = Some(String::new());
        for (what, settings) in [
            ("password", empty_password),
            ("location", empty_location),
            ("hot location", empty_hot),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            assert_eq!(
                service
                    .test(api::TestRepositoryDto {
                        settings: settings.clone()
                    })
                    .await,
                Err(RepositoriesError::InvalidBody),
                "an empty {what}"
            );
            assert_eq!(
                service.connect(connect("demo", settings.clone())).await,
                Err(RepositoriesError::InvalidBody),
                "an empty {what}"
            );
            assert_eq!(
                service.init(init(Some("demo"), settings)).await,
                Err(RepositoriesError::InvalidBody),
                "an empty {what}"
            );
            assert_eq!(jobs(&engine), 0, "an empty {what} ran a job");
            assert!(saved(&*config).await.repositories().is_empty());
        }
    }

    // The configuration refuses settings longer than a job takes when it
    // loads (#236), so a request that would save them is refused first: the
    // daemon would otherwise not start again.
    #[tokio::test]
    async fn settings_over_a_cap_are_an_invalid_body_and_nothing_runs() {
        use domain::secrets::{MAX_LOCATION_BYTES, MAX_OPTION_VALUE_BYTES, MAX_OPTIONS};
        let long = |bytes: usize| format!("/{}", "v".repeat(bytes - 1));
        let long_option =
            || BTreeMap::from([("endpoint".to_owned(), long(MAX_OPTION_VALUE_BYTES + 1))]);
        let many = || {
            (0..=MAX_OPTIONS)
                .map(|n| (format!("o{n}"), "demo".to_owned()))
                .collect::<BTreeMap<_, _>>()
        };
        let with = |change: &dyn Fn(&mut RepositorySettingsDto)| {
            let mut settings = settings_dto();
            change(&mut settings);
            settings
        };
        for (what, settings) in [
            (
                "location",
                with(&|s| s.repository = long(MAX_LOCATION_BYTES + 1)),
            ),
            (
                "hot location",
                with(&|s| s.repo_hot = Some(long(MAX_LOCATION_BYTES + 1))),
            ),
            ("option", with(&|s| s.options = long_option())),
            ("hot option", with(&|s| s.options_hot = long_option())),
            ("cold option", with(&|s| s.options_cold = long_option())),
            ("table of options", with(&|s| s.options = many())),
            ("table of hot options", with(&|s| s.options_hot = many())),
            ("table of cold options", with(&|s| s.options_cold = many())),
            // Refused for its length before it is found relative.
            (
                "relative location",
                with(&|s| s.repository = format!("backups/{}", "r".repeat(MAX_LOCATION_BYTES))),
            ),
            (
                "relative option",
                with(&|s| {
                    s.repository = "opendal:fs".to_owned();
                    s.options = BTreeMap::from([(
                        "root".to_owned(),
                        format!("backups/{}", "r".repeat(MAX_OPTION_VALUE_BYTES)),
                    )]);
                }),
            ),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            assert_eq!(
                service
                    .test(api::TestRepositoryDto {
                        settings: settings.clone()
                    })
                    .await,
                Err(RepositoriesError::InvalidBody),
                "a long {what}"
            );
            assert_eq!(
                service.connect(connect("demo", settings.clone())).await,
                Err(RepositoriesError::InvalidBody),
                "a long {what}"
            );
            assert_eq!(
                service.init(init(Some("demo"), settings)).await,
                Err(RepositoriesError::InvalidBody),
                "a long {what}"
            );
            assert_eq!(jobs(&engine), 0, "a long {what} ran a job");
            assert!(saved(&*config).await.repositories().is_empty());
        }
    }

    // The configuration refuses these when it loads, so a saved one would
    // stop the daemon from starting again. The refusal names the setting, so
    // the page can show it there.
    #[tokio::test]
    async fn a_relative_path_on_this_machine_is_refused_at_its_setting_and_nothing_runs() {
        let mut relative = settings_dto();
        relative.repository = "backups/repo".to_owned();
        let mut relative_hot = settings_dto();
        relative_hot.repo_hot = Some("~/hot".to_owned());
        let mut relative_file = settings_dto();
        relative_file.password = PasswordDto::File {
            path: "password.txt".into(),
        };
        // A relative password file is read before the caps are checked, so
        // it is refused first, even beside a location over its cap.
        let mut relative_file_long_location = settings_dto();
        relative_file_long_location.password = PasswordDto::File {
            path: "password.txt".into(),
        };
        relative_file_long_location.repository =
            format!("/{}", "v".repeat(domain::secrets::MAX_LOCATION_BYTES));
        let mut relative_root = settings_dto();
        relative_root.repository = "opendal:fs".to_owned();
        relative_root.options = [("root".to_owned(), "backups".to_owned())].into();
        let mut relative_cert = settings_dto();
        relative_cert.repository = "rest:https://nas.example/repo".to_owned();
        relative_cert.options_cold = [("http-cacert".to_owned(), "ca.pem".to_owned())].into();
        for (settings, field) in [
            (relative, "repository"),
            (relative_hot, "repo-hot"),
            (relative_file, "password"),
            (relative_file_long_location, "password"),
            (relative_root, "options.root"),
            (relative_cert, "options-cold.http-cacert"),
        ] {
            let refused = Err(RepositoriesError::PathRelative {
                field: field.to_owned(),
            });
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            assert_eq!(
                service
                    .test(api::TestRepositoryDto {
                        settings: settings.clone()
                    })
                    .await
                    .map(|_| ()),
                refused,
                "{field}"
            );
            assert_eq!(
                service
                    .connect(connect("demo", settings.clone()))
                    .await
                    .map(|_| ()),
                refused,
                "{field}"
            );
            assert_eq!(
                service.init(init(Some("demo"), settings)).await.map(|_| ()),
                refused,
                "{field}"
            );
            assert_eq!(jobs(&engine), 0, "a relative {field} ran a job");
            assert!(saved(&*config).await.repositories().is_empty());
        }
    }

    #[tokio::test]
    async fn an_initialisation_under_an_alias_already_used_runs_no_job() {
        let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the first connection");
        let jobs_before = jobs(&engine);
        let mut other = settings_dto();
        other.repository = "/srv/other".to_owned();
        assert_eq!(
            service.init(init(Some("demo"), other)).await,
            Err(RepositoriesError::AliasTaken {
                alias: "demo".to_owned()
            })
        );
        assert_eq!(jobs(&engine), jobs_before, "a repository was created");
        assert_eq!(saved(&*config).await.repositories().len(), 1);
    }

    #[tokio::test]
    async fn two_connections_made_at_the_same_moment_are_both_kept() {
        let engine = engine();
        let mut other = settings_dto();
        other.repository = "/srv/other/repo".to_owned();
        engine
            .ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                other.repository.clone(),
                RepositoryId::parse(&"b".repeat(64)).expect("a repository ID"),
            );
        let config = Arc::new(Yielding(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        let (first, second) = tokio::join!(
            service.connect(connect("first", settings_dto())),
            service.connect(connect("second", other)),
        );
        first.expect("the first connection");
        second.expect("the second connection");
        let config = saved(&*config).await;
        assert!(config.repository(&alias("first")).is_some());
        assert!(config.repository(&alias("second")).is_some());
    }

    /// Both connections find the configuration without the repository; the
    /// one that writes second finds the other's entry, and is refused.
    #[tokio::test]
    async fn one_repository_connected_under_two_aliases_at_the_same_moment_is_kept_once() {
        let engine = engine();
        let config = Arc::new(Yielding(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        let (first, second) = tokio::join!(
            service.connect(connect("first", settings_dto())),
            service.connect(connect("second", settings_dto())),
        );
        let (kept, refused) = match (first.map(|_| ()), second.map(|_| ())) {
            (Ok(()), Err(refused)) => ("first", refused),
            (Err(refused), Ok(())) => ("second", refused),
            other => panic!("one connection is kept and the other refused: {other:?}"),
        };
        assert_eq!(
            refused,
            RepositoriesError::AlreadyConfigured {
                alias: kept.to_owned()
            }
        );
        let config = saved(&*config).await;
        assert_eq!(
            config.repositories().keys().collect::<Vec<_>>(),
            [&alias(kept)]
        );
    }

    /// Both Creates find the entry recording no ID; the one that takes the
    /// lock second finds the ID the first recorded, and is refused as a
    /// repository already there.
    #[tokio::test]
    async fn one_saved_repository_created_twice_at_the_same_moment_is_created_once() {
        let engine = engine();
        let config = Arc::new(Yielding(InMemoryConfigStore::new()));
        let service = service(&engine, config.clone());
        service
            .connect(connect("demo", settings_dto()))
            .await
            .expect("the entry is saved");
        imported_and_found_none(&engine, &service, &*config).await;
        let (first, second) = tokio::join!(
            service.create_saved("demo".to_owned()),
            service.create_saved("demo".to_owned()),
        );
        match (first, second) {
            (Ok(_), Err(RepositoriesError::Exists { .. }))
            | (Err(RepositoriesError::Exists { .. }), Ok(_)) => {}
            other => panic!("one Create runs and the other is refused: {other:?}"),
        }
        assert_eq!(
            jobs(&engine),
            3,
            "the connection's test, and one Create's look and initialisation"
        );
    }

    #[tokio::test]
    async fn a_repository_that_never_answers_is_refused_and_the_lock_is_released() {
        // A job that is waited on for ever is a failure of the test, not a
        // stuck run.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let timed_out = || {
                Err(RepositoriesError::Unavailable {
                    code: ErrorCode::RepositoryTimedOut,
                    args: vec![],
                    detail: None,
                })
            };
            let engine = engine();
            *engine.hangs.lock().unwrap_or_else(PoisonError::into_inner) = 3;
            let config = Arc::new(InMemoryConfigStore::new());
            let service = service(&engine, config.clone());
            assert_eq!(
                service
                    .test(api::TestRepositoryDto {
                        settings: settings_dto()
                    })
                    .await
                    .map(|_| ()),
                timed_out()
            );
            assert_eq!(
                service
                    .connect(connect("first", settings_dto()))
                    .await
                    .map(|_| ()),
                timed_out()
            );
            assert_eq!(
                service
                    .init(init(Some("second"), settings_dto()))
                    .await
                    .map(|_| ()),
                timed_out()
            );
            assert!(saved(&*config).await.repositories().is_empty());
            // The lock was let go each time: the next connection takes it.
            service
                .connect(connect("third", settings_dto()))
                .await
                .expect("a connection after the timeouts");
            assert!(saved(&*config).await.repository(&alias("third")).is_some());
        })
        .await
        .expect("the service waited for ever");
    }

    #[tokio::test]
    async fn a_connection_waiting_for_the_lock_goes_on_when_the_one_holding_it_times_out() {
        // A job that is waited on for ever is a failure of the test, not a
        // stuck run.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let engine = engine();
            // Only the first job to reach the engine hangs.
            *engine.hangs.lock().unwrap_or_else(PoisonError::into_inner) = 1;
            let config = Arc::new(Yielding(InMemoryConfigStore::new()));
            let service = service(&engine, config.clone());
            let (first, second) = tokio::join!(
                service.connect(connect("first", settings_dto())),
                service.connect(connect("second", settings_dto())),
            );
            assert_eq!(
                first.map(|_| ()),
                Err(RepositoriesError::Unavailable {
                    code: ErrorCode::RepositoryTimedOut,
                    args: vec![],
                    detail: None,
                })
            );
            second.expect("the second connection, once the first let go");
            let config = saved(&*config).await;
            assert!(config.repository(&alias("first")).is_none());
            assert!(config.repository(&alias("second")).is_some());
        })
        .await
        .expect("the service waited for ever");
    }

    #[tokio::test]
    async fn a_failed_job_is_refused_by_the_kind_of_its_failure() {
        use FailureReason as F;
        let text = |value: &str| MsgArg::Text(value.to_owned());
        let unavailable = |code, args: Vec<(&'static str, MsgArg)>, detail: Option<&str>| {
            Err(RepositoriesError::Unavailable {
                code,
                args,
                detail: detail.map(str::to_owned),
            })
        };
        for (reason, expected) in [
            (
                F::EngineFailed {
                    detail: "wrong password".into(),
                },
                Err(RepositoriesError::OperationFailed {
                    detail: "wrong password".into(),
                }),
            ),
            (
                F::EngineUnavailable {
                    detail: "no worker".into(),
                },
                unavailable(ErrorCode::EngineUnavailable, vec![], Some("no worker")),
            ),
            (
                F::JobRejected {
                    detail: "refused".into(),
                },
                unavailable(ErrorCode::EngineUnavailable, vec![], Some("refused")),
            ),
            (
                F::EngineExited { exit_code: Some(3) },
                unavailable(
                    ErrorCode::EngineExited,
                    vec![("exit_code", text("3"))],
                    None,
                ),
            ),
            (
                F::EngineExited { exit_code: None },
                unavailable(ErrorCode::EngineExited, vec![], None),
            ),
            (
                F::DeadlinePassed,
                unavailable(ErrorCode::DeadlinePassed, vec![], None),
            ),
            (
                F::EngineStartTimedOut,
                unavailable(ErrorCode::EngineStartTimedOut, vec![], None),
            ),
            (
                F::EngineExitTimedOut,
                unavailable(ErrorCode::EngineExitTimedOut, vec![], None),
            ),
            (
                F::DaemonStopped,
                unavailable(ErrorCode::DaemonStopped, vec![], None),
            ),
            (
                F::SecretsTooLargeToMask {
                    field: Some("HTTPS_PROXY".to_owned()),
                    limit: 262_144,
                },
                Err(RepositoriesError::SecretRefused(SecretRefusal {
                    code: ErrorCode::SecretsTooLargeToMask,
                    args: vec![
                        ("limit", MsgArg::Bytes(262_144)),
                        ("field", text("HTTPS_PROXY")),
                    ],
                    detail: None,
                })),
            ),
            (
                F::RepositoryReplaced,
                unavailable(ErrorCode::RepositoryReplaced, vec![], None),
            ),
            (
                F::CredentialsRefused,
                unavailable(ErrorCode::CredentialsRefused, vec![], None),
            ),
            (
                F::RepositoryExists,
                Err(RepositoriesError::Exists {
                    location: "/srv/demo/repo".into(),
                }),
            ),
        ] {
            let (engine, config) = (engine(), Arc::new(InMemoryConfigStore::new()));
            *engine
                .outcome
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Err(reason.clone());
            let service = service(&engine, config.clone());
            let tested = service
                .test(api::TestRepositoryDto {
                    settings: settings_dto(),
                })
                .await;
            assert_eq!(
                tested.map(|_| ()),
                expected.clone().map(|_: ()| ()),
                "{reason:?}"
            );
            assert_eq!(
                service
                    .connect(connect("demo", settings_dto()))
                    .await
                    .map(|_| ()),
                expected.map(|_: ()| ()),
                "{reason:?}"
            );
            assert!(
                saved(&*config).await.repositories().is_empty(),
                "{reason:?} saved something"
            );
        }
    }

    #[tokio::test]
    async fn the_engine_not_starting_is_unavailable() {
        for (error, expected) in [
            (
                EngineError::Unavailable {
                    detail: "no such program".into(),
                },
                RepositoriesError::Unavailable {
                    code: ErrorCode::EngineUnavailable,
                    args: vec![],
                    detail: Some("no such program".into()),
                },
            ),
            (
                EngineError::StartTimedOut,
                RepositoriesError::Unavailable {
                    code: ErrorCode::EngineStartTimedOut,
                    args: vec![],
                    detail: None,
                },
            ),
        ] {
            let engine = engine();
            *engine
                .refuses
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(error);
            let answer = service(&engine, Arc::new(InMemoryConfigStore::new()))
                .test(api::TestRepositoryDto {
                    settings: settings_dto(),
                })
                .await;
            assert_eq!(answer, Err(expected));
        }
    }

    /// An engine that refuses a job for more secrets than it masks is told
    /// by the code of a secret refusal, with the limit and the setting where
    /// the secrets passed it, or none when the job's own did (§10.2).
    #[tokio::test]
    async fn the_engine_refusing_a_job_with_too_much_to_mask_is_told_by_its_code() {
        for field in [Some("HTTPS_PROXY"), None] {
            let engine = engine();
            *engine
                .refuses
                .lock()
                .unwrap_or_else(PoisonError::into_inner) =
                Some(EngineError::SecretsTooLargeToMask {
                    field: field.map(str::to_owned),
                    limit: 262_144,
                });
            let answer = service(&engine, Arc::new(InMemoryConfigStore::new()))
                .test(api::TestRepositoryDto {
                    settings: settings_dto(),
                })
                .await;
            let mut args = vec![("limit", MsgArg::Bytes(262_144))];
            args.extend(field.map(|field| ("field", MsgArg::Text(field.to_owned()))));
            assert_eq!(
                answer,
                Err(RepositoriesError::SecretRefused(SecretRefusal {
                    code: ErrorCode::SecretsTooLargeToMask,
                    args,
                    detail: None,
                })),
                "{field:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_existing_repository_is_refused_with_its_location_redacted() {
        let engine = engine();
        *engine
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(FailureReason::RepositoryExists);
        let mut settings = settings_dto();
        settings.repository = "rest:https://backup:hunter2@nas.example/repo".to_owned();
        let Err(RepositoriesError::Exists { location }) =
            service(&engine, Arc::new(InMemoryConfigStore::new()))
                .init(init(None, settings))
                .await
        else {
            panic!("an existing repository is refused as one");
        };
        assert!(!location.contains("hunter2"), "{location}");
        assert!(location.contains("nas.example/repo"), "{location}");
    }

    /// A job that finds no repository at the location is refused as
    /// `repository-not-found`, not as a failure in the engine's words, by a
    /// test and a connection alike (decision 53).
    #[tokio::test]
    async fn a_location_with_no_repository_is_refused_as_one() {
        let engine = engine();
        *engine
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(FailureReason::RepositoryNotFound);
        let service = service(&engine, Arc::new(InMemoryConfigStore::new()));
        assert_eq!(
            service
                .test(api::TestRepositoryDto {
                    settings: settings_dto(),
                })
                .await,
            Err(RepositoriesError::NotFound)
        );
        assert_eq!(
            service.connect(connect("demo", settings_dto())).await,
            Err(RepositoriesError::NotFound)
        );
    }

    #[tokio::test]
    async fn an_existing_rclone_repository_is_refused_without_the_credential_in_its_location() {
        let engine = engine();
        *engine
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Err(FailureReason::RepositoryExists);
        let mut settings = settings_dto();
        settings.repository =
            "rclone::sftp,host=nas.example.com,pass=CANARY-rclone-exists:/backups".to_owned();
        let Err(RepositoriesError::Exists { location }) =
            service(&engine, Arc::new(InMemoryConfigStore::new()))
                .init(init(None, settings))
                .await
        else {
            panic!("an existing repository is refused as one");
        };
        assert_eq!(
            location,
            "rclone::sftp,host=nas.example.com,pass=***:/backups"
        );
    }

    #[tokio::test]
    async fn a_secret_that_cannot_be_read_names_where_it_is() {
        for (mut settings, field) in [
            (settings_dto(), "password"),
            (settings_dto(), "repository"),
            (settings_dto(), "repo-hot"),
            (settings_dto(), "options.token"),
        ] {
            match field {
                "password" => {
                    settings.password = PasswordDto::Env {
                        name: "NOT_SET_ANYWHERE".into(),
                    }
                }
                "repository" => settings.repository = "${NOT_SET_ANYWHERE}/repo".into(),
                "repo-hot" => settings.repo_hot = Some("${NOT_SET_ANYWHERE}/hot".into()),
                _ => {
                    settings.options =
                        [("token".to_owned(), "${NOT_SET_ANYWHERE}".to_owned())].into()
                }
            }
            let engine = engine();
            let answer = service(&engine, Arc::new(InMemoryConfigStore::new()))
                .test(api::TestRepositoryDto { settings })
                .await;
            assert_eq!(
                answer,
                Err(RepositoriesError::SecretRefused(SecretRefusal {
                    code: ErrorCode::SecretMissing,
                    args: vec![
                        ("field", MsgArg::Text(field.to_owned())),
                        ("source", MsgArg::Text("NOT_SET_ANYWHERE".to_owned())),
                    ],
                    detail: None,
                }))
            );
            assert_eq!(jobs(&engine), 0, "{field}");
        }
    }

    fn env_source(name: &str) -> domain::secrets::SourceName {
        domain::secrets::SourceName::Env {
            name: name.to_owned(),
        }
    }

    fn unresolved(
        field: Field,
        source: domain::secrets::SourceName,
        problem: domain::secrets::Problem,
    ) -> ResolveError {
        ResolveError {
            field,
            cause: Cause::Unresolved(domain::secrets::SecretError { source, problem }),
        }
    }

    /// Every cause a secret or a template can fail by has a code of its own,
    /// with the names the message reads: the setting, and the source.
    #[tokio::test]
    async fn a_secret_that_failed_to_resolve_is_refused_with_the_code_of_its_cause() {
        use domain::secrets::{Problem, SourceName, TemplateError};
        let text = |value: &str| MsgArg::Text(value.to_owned());
        let count = |value: u64| MsgArg::Count(value);
        let settings = settings_of(settings_dto()).expect("settings");
        let file = || SourceName::File {
            path: "/run/secrets/pw".to_owned(),
        };
        let command = || SourceName::Command {
            program: "pass".to_owned(),
        };
        let at = |field: &str, source: &str| vec![("field", text(field)), ("source", text(source))];
        type Case = (
            ResolveError,
            ErrorCode,
            Vec<(&'static str, MsgArg)>,
            Option<&'static str>,
        );
        let cases: Vec<Case> = vec![
            (
                unresolved(Field::Password, env_source("PW"), Problem::Missing),
                ErrorCode::SecretMissing,
                at("password", "PW"),
                None,
            ),
            (
                unresolved(
                    Field::Password,
                    file(),
                    Problem::Unreachable {
                        detail: "permission denied".to_owned(),
                    },
                ),
                ErrorCode::SecretUnreadable,
                at("password", "/run/secrets/pw"),
                Some("permission denied"),
            ),
            (
                unresolved(
                    Field::Option("token".to_owned()),
                    file(),
                    Problem::NotUnicode,
                ),
                ErrorCode::SecretNotText,
                at("options.token", "/run/secrets/pw"),
                None,
            ),
            (
                unresolved(
                    Field::OptionHot("token".to_owned()),
                    file(),
                    Problem::TooLarge { limit: 4096 },
                ),
                ErrorCode::SecretTooLarge,
                vec![
                    ("field", text("options-hot.token")),
                    ("source", text("/run/secrets/pw")),
                    ("limit", MsgArg::Bytes(4096)),
                ],
                None,
            ),
            (
                unresolved(
                    Field::Password,
                    command(),
                    Problem::CommandFailed {
                        status: Some(3),
                        detail: "no such entry".to_owned(),
                    },
                ),
                ErrorCode::SecretCommandFailed,
                at("password", "pass"),
                Some("no such entry"),
            ),
            (
                unresolved(
                    Field::Password,
                    command(),
                    Problem::TimedOut { seconds: 60 },
                ),
                ErrorCode::SecretCommandTimedOut,
                vec![
                    ("field", text("password")),
                    ("source", text("pass")),
                    ("seconds", count(60)),
                ],
                None,
            ),
            (
                unresolved(Field::Password, command(), Problem::OutputCut),
                ErrorCode::SecretCommandOutputOpen,
                at("password", "pass"),
                None,
            ),
            (
                ResolveError {
                    field: Field::Repository,
                    cause: Cause::ReferenceInProcessArguments,
                },
                ErrorCode::SecretInProcessArguments,
                vec![("field", text("repository"))],
                None,
            ),
            (
                ResolveError {
                    field: Field::RepoHot,
                    cause: Cause::ExpansionTooLong { limit: 4096 },
                },
                ErrorCode::SecretExpansionTooLong,
                vec![("field", text("repo-hot")), ("limit", MsgArg::Bytes(4096))],
                None,
            ),
            (
                ResolveError {
                    field: Field::Option("note".to_owned()),
                    cause: Cause::TooMuchToMask { limit: 262_144 },
                },
                ErrorCode::SecretsTooLargeToMask,
                vec![
                    ("limit", MsgArg::Bytes(262_144)),
                    ("field", text("options.note")),
                ],
                None,
            ),
            (
                ResolveError {
                    field: Field::RepoHot,
                    cause: Cause::TooManyCredentials { limit: 16 },
                },
                ErrorCode::LocationTooManyCredentials,
                vec![("field", text("repo-hot"))],
                None,
            ),
            (
                ResolveError {
                    field: Field::OptionCold("token".to_owned()),
                    cause: Cause::Invalid(TemplateError::Unterminated { at: 3 }),
                },
                ErrorCode::TemplateInvalid,
                vec![("field", text("options-cold.token")), ("at", text("3"))],
                None,
            ),
            (
                ResolveError {
                    field: Field::Repository,
                    cause: Cause::Invalid(TemplateError::TooLong { limit: 4096 }),
                },
                ErrorCode::TemplateInvalid,
                vec![("field", text("repository"))],
                None,
            ),
        ];
        for (error, code, args, detail) in cases {
            assert_eq!(
                secret_refusal(&error, &settings, &SystemSecrets::default()).await,
                SecretRefusal {
                    code,
                    args,
                    detail: detail.map(str::to_owned),
                },
                "{error:?}"
            );
        }
    }

    /// A refusal that shows nothing a secret could be in reads no secret: a
    /// location with more credentials than a job takes, or a reference that
    /// would reach the arguments of a process, is refused before anything is
    /// read (§10.2), and so is its answer. One that shows what a source
    /// said reads the repository's secrets again, to mask them.
    #[tokio::test]
    async fn a_refusal_that_shows_no_words_reads_no_secret() {
        use domain::secrets::{Problem, TemplateError};
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.password = SecretRef::Env("DEMO_PASSWORD".to_owned());
        settings.options = [("token".to_owned(), "${DEMO_TOKEN}".to_owned())].into();
        let secrets = FakeSecrets::new();
        secrets.set_env("DEMO_PASSWORD", "demo-password");
        secrets.set_env("DEMO_TOKEN", "demo-token");
        for cause in [
            Cause::TooManyCredentials { limit: 16 },
            Cause::ReferenceInProcessArguments,
            Cause::ExpansionTooLong { limit: 4096 },
            Cause::TooMuchToMask { limit: 262_144 },
            Cause::Invalid(TemplateError::TooLong { limit: 4096 }),
        ] {
            let error = ResolveError {
                field: Field::Repository,
                cause,
            };
            let _ = secret_refusal(&error, &settings, &secrets).await;
            assert_eq!(secrets.reads(), [], "{error:?}");
        }
        let error = unresolved(Field::Password, env_source("PW"), Problem::Missing);
        let _ = secret_refusal(&error, &settings, &secrets).await;
        assert_eq!(secrets.reads().len(), 2);
    }

    /// A location a reference made relative is refused as one written so is
    /// at load and at the repository routes: `repository-path-relative`,
    /// naming the setting. Nothing a reference held is in it, and it reads
    /// no secret (I4, #247).
    #[tokio::test]
    async fn a_location_resolved_to_a_relative_path_is_refused_by_its_setting_alone() {
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.repository = "${DEMO_REPO}".to_owned();
        settings.repo_hot = Some("local:${DEMO_HOT}".to_owned());
        let secrets = FakeSecrets::new();
        secrets.set_env("DEMO_REPO", "CANARY-relative/repo");
        secrets.set_env("DEMO_HOT", "CANARY-relative/hot");
        for (field, name) in [
            (Field::Repository, "repository"),
            (Field::RepoHot, "repo-hot"),
        ] {
            let error = ResolveError {
                field,
                cause: Cause::RelativePath,
            };
            assert_eq!(
                secret_refusal(&error, &settings, &secrets).await,
                SecretRefusal {
                    code: ErrorCode::RepositoryPathRelative,
                    args: vec![("field", MsgArg::Text(name.to_owned()))],
                    detail: None,
                },
            );
        }
        assert_eq!(secrets.reads(), []);
    }

    /// What a command said, and what a platform said, is masked of every
    /// secret of the repository before it is an answer: the typed password,
    /// a credential option written inline, and a credential a reference in
    /// the settings resolves to (I4).
    #[tokio::test]
    async fn the_words_of_a_failed_secret_are_masked_of_every_secret_of_the_repository() {
        use domain::secrets::Problem;
        let scratch = tempfile::tempdir().expect("a scratch folder");
        let referenced = scratch.path().join("token");
        std::fs::write(&referenced, "canary-from-a-file\n").expect("the file is written");
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.options = [
            ("secret_access_key".to_owned(), "canary-inline".to_owned()),
            (
                "application_key".to_owned(),
                format!("${{file:{}}}", referenced.display()),
            ),
        ]
        .into();
        let said = "wrote canary-inline, canary-from-a-file and demo-password to stderr";
        let error = unresolved(
            Field::Password,
            domain::secrets::SourceName::Command {
                program: "pass".to_owned(),
            },
            Problem::CommandFailed {
                status: Some(1),
                detail: said.to_owned(),
            },
        );
        let refusal = secret_refusal(&error, &settings, &SystemSecrets::default()).await;
        let detail = refusal.detail.expect("the command's words are kept");
        assert_eq!(
            detail,
            format!(
                "wrote {0}, {0} and {0} to stderr",
                domain::secrets::REDACTED
            )
        );
        // Not the name of an option or of a program, which are not secrets.
        assert_eq!(refusal.code, ErrorCode::SecretCommandFailed);
    }

    /// The secrets read again for a redactor stop where they pass what one
    /// masks: the rest isn't read, and what the failure says is masked
    /// whole, since an unread secret could be in it (I4).
    #[tokio::test]
    async fn the_words_of_a_failed_secret_are_masked_whole_past_what_a_redactor_masks() {
        use domain::secrets::{MAX_MASKED_BYTES, MAX_SECRET_BYTES, Problem, REDACTED};
        let mut settings = settings_of(settings_dto()).expect("settings");
        let secrets = FakeSecrets::new();
        let count = MAX_MASKED_BYTES / MAX_SECRET_BYTES + 1;
        for at in 1..=count {
            let letter =
                char::from_digit(u32::try_from(at + 9).expect("small"), 36).expect("a letter");
            secrets.set_env(
                &format!("DEMO_{at}"),
                &letter.to_string().repeat(MAX_SECRET_BYTES),
            );
            settings
                .options
                .insert(format!("note-{at}"), format!("${{DEMO_{at}}}"));
        }
        settings
            .options
            .insert("note-9".to_owned(), "${DEMO_LATER}".to_owned());
        secrets.set_env("DEMO_LATER", "demo-later");
        let error = unresolved(
            Field::Password,
            domain::secrets::SourceName::Command {
                program: "pass".to_owned(),
            },
            Problem::CommandFailed {
                status: Some(1),
                detail: "nothing of a secret here".to_owned(),
            },
        );
        let refusal = secret_refusal(&error, &settings, &secrets).await;
        assert_eq!(refusal.detail.as_deref(), Some(REDACTED));
        assert_eq!(
            secrets.read_count(&SecretRef::Env("DEMO_LATER".to_owned())),
            0
        );
    }

    /// The lines logged so far about the snapshots of `alias`.
    fn logged_about(alias: &str) -> Vec<String> {
        captured_log::lines()
            .into_iter()
            .map(|(_, line)| line)
            .filter(|line| line.contains(&format!("snapshots of {alias} ")))
            .collect()
    }

    /// A catalog whose repository `alias` has the password from `command`, a
    /// credential written inline, and one read from a file.
    async fn catalog_with_a_password_command(
        alias: &str,
        command: &str,
        engine: Arc<dyn Engine>,
    ) -> (ConfigCatalog, tempfile::TempDir) {
        catalog_at(alias, "/srv/demo/repo", command, engine).await
    }

    /// A catalog as [`catalog_with_a_password_command`] makes it, with its
    /// repository at `location`.
    async fn catalog_at(
        alias: &str,
        location: &str,
        command: &str,
        engine: Arc<dyn Engine>,
    ) -> (ConfigCatalog, tempfile::TempDir) {
        let scratch = tempfile::tempdir().expect("a scratch folder");
        let referenced = scratch.path().join("token");
        std::fs::write(&referenced, "canary-from-a-file\n").expect("the file is written");
        let config = Arc::new(InMemoryConfigStore::new());
        let settings = RepositorySettings {
            repository: location.to_owned(),
            repo_hot: None,
            password: SecretRef::Command(
                domain::secrets::CommandLine::parse(command).expect("a command"),
            ),
            options: [
                ("secret_access_key".to_owned(), "canary-inline".to_owned()),
                (
                    "application_key".to_owned(),
                    format!("${{file:{}}}", referenced.display()),
                ),
            ]
            .into(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        config
            .save(
                &domain::planning::AgentConfig::new(
                    Default::default(),
                    Default::default(),
                    std::collections::BTreeMap::from([(
                        RepositoryAlias::parse(alias).expect("an alias"),
                        domain::planning::RepositoryConfig {
                            locations: Default::default(),
                            id: None,
                            settings,
                            maintenance: domain::planning::Maintenance::None,
                            group_by: Default::default(),
                            hooks: Default::default(),
                        },
                    )]),
                    Vec::new(),
                )
                .expect("a valid configuration"),
            )
            .await
            .expect("the configuration saves");
        let catalog = ConfigCatalog {
            config,
            engine,
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        (catalog, scratch)
    }

    /// The plans list carries the scheduler's planned run of each scheduled
    /// plan, and none for a plan without a schedule, even where a run is
    /// still kept for it (§5, #195).
    #[tokio::test]
    async fn the_plans_list_carries_the_planned_run_of_each_scheduled_plan() {
        let scratch = tempfile::tempdir().expect("a scratch folder");
        let file = scratch.path().join("config.toml");
        std::fs::write(
            &file,
            "schema = 1\n\n[repositories.demo]\nrepository = \"/srv/demo\"\npassword = \"canary\"\n\n\
             [[plans]]\nid = \"home\"\nrepository = \"demo\"\nsources = [\"/home/demo\"]\n\
             schedule = { cron = \"0 3 * * *\" }\n\n\
             [[plans]]\nid = \"docs\"\nrepository = \"demo\"\nsources = [\"/home/demo\"]\n",
        )
        .expect("the config file");
        let schedules = Arc::new(app::fakes::InMemoryScheduleStore::new());
        for (plan, second) in [("home", 1_794_200_400), ("docs", 1_794_200_000)] {
            schedules
                .plan_run(&app::ports::PlannedRun {
                    plan: PlanId::try_from(plan.to_owned()).expect("a plan ID"),
                    at: domain::Timestamp::from_second(second).expect("a time"),
                })
                .await
                .expect("the run is kept");
        }
        let catalog = ConfigCatalog {
            config: Arc::new(FileConfigStore::new(file, Arc::new(SystemClock))),
            engine: engine(),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules,
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        let listed = catalog.plans().await.expect("the plans");
        let shown: Vec<(&str, bool, Option<&str>)> = listed
            .plans
            .iter()
            .map(|plan| (plan.id.as_str(), plan.scheduled, plan.next_run.as_deref()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("home", true, Some("2026-11-09T05:00:00Z")),
                ("docs", false, None),
            ]
        );
    }

    /// A listing whose password command fails with the other secrets of the
    /// repository in its stderr is refused with the code of the failure,
    /// and neither the answer nor the log holds one of them (I4).
    #[tokio::test]
    async fn a_listing_whose_password_command_fails_is_refused_with_its_code_and_masks_the_secrets()
    {
        capture();
        let (catalog, _scratch) = catalog_with_a_password_command(
            "failing-pass",
            "sh -c 'echo canary-inline canary-from-a-file >&2; exit 3'",
            engine(),
        )
        .await;
        let refused = catalog
            .snapshots("failing-pass".to_owned())
            .await
            .expect_err("the listing is refused");
        let SnapshotsError::SecretRefused(refusal) = refused else {
            panic!("a secret refusal, not {refused:?}");
        };
        assert_eq!(refusal.code, ErrorCode::SecretCommandFailed);
        let said = format!("{refusal:?}");
        let logged = logged_about("failing-pass").join("\n");
        for line in [&said, &logged] {
            assert!(!line.contains("canary-inline"), "{line}");
            assert!(!line.contains("canary-from-a-file"), "{line}");
        }
        assert!(logged.contains("SecretCommandFailed"), "{logged}");
        assert!(logged.contains(domain::secrets::REDACTED), "{logged}");
    }

    /// An engine that can't be started says what it likes: what reaches the
    /// log of the listing has the secrets of the repository masked, and the
    /// debug text of the error is not logged as it is.
    #[tokio::test]
    async fn the_log_of_a_listing_the_engine_refused_masks_the_secrets_of_the_repository() {
        capture();
        let engine = engine();
        *engine
            .refuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(EngineError::Unavailable {
            detail: "could not start with canary-inline and canary-from-a-file".to_owned(),
        });
        let (catalog, _scratch) =
            catalog_with_a_password_command("engine-down", "echo not-a-real-password", engine)
                .await;
        let refused = catalog.snapshots("engine-down".to_owned()).await;
        assert_eq!(refused, Err(engine_unavailable()));
        let logged = logged_about("engine-down").join("\n");
        assert!(logged.contains(domain::secrets::REDACTED), "{logged}");
        assert!(!logged.contains("canary-inline"), "{logged}");
        assert!(!logged.contains("canary-from-a-file"), "{logged}");
    }

    /// A listing the engine refused for more secrets than it masks is
    /// refused with that code, in the answer and in the log (§10.2).
    #[tokio::test]
    async fn a_listing_the_engine_refused_for_too_much_to_mask_is_refused_with_its_code() {
        capture();
        let engine = engine();
        *engine
            .refuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(EngineError::SecretsTooLargeToMask {
            field: Some("HTTPS_PROXY".to_owned()),
            limit: 262_144,
        });
        let (catalog, _scratch) =
            catalog_with_a_password_command("too-much", "echo not-a-real-password", engine).await;
        let refused = catalog.snapshots("too-much".to_owned()).await;
        assert_eq!(
            refused,
            Err(SnapshotsError::SecretRefused(SecretRefusal {
                code: ErrorCode::SecretsTooLargeToMask,
                args: vec![
                    ("limit", MsgArg::Bytes(262_144)),
                    ("field", MsgArg::Text("HTTPS_PROXY".to_owned())),
                ],
                detail: None,
            }))
        );
        let logged = logged_about("too-much").join("\n");
        assert!(logged.contains("SecretsTooLargeToMask"), "{logged}");
    }

    /// What the engine says of an rclone location is logged without the
    /// credential written in it (I4).
    #[tokio::test]
    async fn the_log_of_a_listing_masks_the_credential_written_in_an_rclone_location() {
        capture();
        let engine = engine();
        let location = "rclone::sftp,host=nas.example.com,pass=CANARY-rclone-log:/backups";
        *engine
            .refuses
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(EngineError::Unavailable {
            detail: format!("rclone could not open {location}"),
        });
        let (catalog, _scratch) =
            catalog_at("rclone-down", location, "echo not-a-real-password", engine).await;
        let refused = catalog.snapshots("rclone-down".to_owned()).await;
        assert_eq!(refused, Err(engine_unavailable()));
        let logged = logged_about("rclone-down").join("\n");
        assert!(
            logged.contains("rclone::sftp,host=nas.example.com,pass=***:/backups"),
            "{logged}"
        );
        assert!(!logged.contains("CANARY-rclone-log"), "{logged}");
    }

    /// A command's line cut at the cap can end in the beginning of a secret
    /// the mask would not find: the first characters of it are not shown
    /// either (I4).
    #[tokio::test]
    async fn the_words_of_a_failed_command_cut_at_the_cap_show_no_start_of_a_secret() {
        use domain::secrets::Problem;
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.options = [("secret_access_key".to_owned(), "canary-inline".to_owned())].into();
        let stderr = format!("{}canary-inline", "x".repeat(505));
        let error = unresolved(
            Field::Password,
            domain::secrets::SourceName::Command {
                program: "pass".to_owned(),
            },
            Problem::command_failed(Some(1), stderr.as_bytes()),
        );
        let detail = secret_refusal(&error, &settings, &SystemSecrets::default())
            .await
            .detail
            .expect("the words are kept");
        assert!(detail.ends_with('…'), "{detail}");
        assert!(!detail.contains("canary"), "{detail}");
    }

    /// A whole secret that ends just before the cap of a command's line is
    /// masked whole: nothing is cut away to leave its start behind (I4).
    #[tokio::test]
    async fn the_words_of_a_failed_command_show_no_start_of_a_secret_just_before_the_cap() {
        use domain::secrets::{MAX_DETAIL_BYTES, Problem};
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.options = [("secret_access_key".to_owned(), "canary-inline".to_owned())].into();
        let before = "x".repeat(MAX_DETAIL_BYTES - "canary-inline".len() - 2);
        let stderr = format!("{before}canary-inlineyyyyyyyy");
        let error = unresolved(
            Field::Password,
            domain::secrets::SourceName::Command {
                program: "pass".to_owned(),
            },
            Problem::command_failed(Some(1), stderr.as_bytes()),
        );
        let detail = secret_refusal(&error, &settings, &SystemSecrets::default())
            .await
            .detail
            .expect("the words are kept");
        assert!(detail.ends_with('…'), "{detail}");
        assert!(!detail.contains("ca"), "{detail}");
    }

    /// A keyring with no service to ask is refused with a code of its own,
    /// whose words say what is missing and what to use instead; what the
    /// platform said is the detail, masked of the repository's secrets (I4).
    #[tokio::test]
    async fn a_keyring_with_no_service_is_refused_with_what_to_use_instead() {
        use domain::secrets::{Problem, SourceName};
        let mut settings = settings_of(settings_dto()).expect("settings");
        settings.options = [("secret_access_key".to_owned(), "canary-inline".to_owned())].into();
        let error = unresolved(
            Field::Password,
            SourceName::Keyring {
                entry: "ladderback/nas".to_owned(),
            },
            Problem::KeyringUnavailable {
                detail: "DBus error: no bus canary-inline".to_owned(),
            },
        );
        let refusal = secret_refusal(&error, &settings, &SystemSecrets::default()).await;
        assert_eq!(
            refusal,
            SecretRefusal {
                code: ErrorCode::SecretKeyringUnavailable,
                args: vec![
                    ("field", MsgArg::Text("password".to_owned())),
                    ("source", MsgArg::Text("ladderback/nas".to_owned())),
                ],
                detail: Some("DBus error: no bus ***".to_owned()),
            }
        );
        let shown =
            l10n::Localizer::new(&[]).error(refusal.code, &refusal.args, refusal.detail.as_deref());
        assert!(shown.problems().is_empty(), "{:?}", shown.problems());
        let text = shown.text();
        for expected in [
            "could not read the keyring entry ladderback/nas",
            "no keyring service it can use (DBus error: no bus ***)",
            "keep the secret in a file, a command or an environment variable instead",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }

    /// A file or a keyring that doesn't answer in time is a source that
    /// can't be read, not a command that ran for too long.
    #[tokio::test]
    async fn a_file_or_keyring_that_does_not_answer_in_time_is_unreadable_not_a_slow_command() {
        use domain::secrets::{Problem, SourceName};
        let settings = settings_of(settings_dto()).expect("settings");
        for (source, field) in [
            (
                SourceName::File {
                    path: "/mnt/nfs/pw".to_owned(),
                },
                "/mnt/nfs/pw",
            ),
            (
                SourceName::Keyring {
                    entry: "ladderback/demo".to_owned(),
                },
                "ladderback/demo",
            ),
        ] {
            let error = unresolved(Field::Password, source, Problem::TimedOut { seconds: 60 });
            let refusal = secret_refusal(&error, &settings, &SystemSecrets::default()).await;
            assert_eq!(refusal.code, ErrorCode::SecretUnreadable, "{field}");
            assert_eq!(
                refusal.args,
                vec![
                    ("field", MsgArg::Text("password".to_owned())),
                    ("source", MsgArg::Text(field.to_owned())),
                ]
            );
            assert_eq!(refusal.detail, None);
        }
    }

    /// A configuration that can't be used, and the code, the arguments and
    /// the detail a refusal of it carries.
    pub(super) type Unusable = (
        ConfigError,
        ErrorCode,
        Vec<(&'static str, MsgArg)>,
        Option<&'static str>,
    );

    /// Each way a configuration can't be used, as every route refuses it.
    pub(super) fn unusable_configurations() -> Vec<Unusable> {
        let text = |value: &str| MsgArg::Text(value.to_owned());
        vec![
            (
                ConfigError::Unavailable {
                    file: "/c/config.toml".into(),
                    detail: "permission denied".into(),
                },
                ErrorCode::ConfigUnavailable,
                vec![("file", text("/c/config.toml"))],
                Some("permission denied"),
            ),
            (
                ConfigError::TooLarge {
                    file: "/c/config.toml".into(),
                    limit: 1024,
                },
                ErrorCode::ConfigTooLarge,
                vec![
                    ("file", text("/c/config.toml")),
                    ("limit", MsgArg::Bytes(1024)),
                ],
                None,
            ),
            (
                ConfigError::SchemaMissing {
                    file: "/c/config.toml".into(),
                },
                ErrorCode::ConfigSchemaMissing,
                vec![("file", text("/c/config.toml"))],
                None,
            ),
            (
                ConfigError::SchemaNewer {
                    file: "/c/config.toml".into(),
                    found: 9,
                    supported: 1,
                },
                ErrorCode::ConfigSchemaNewer,
                vec![
                    ("file", text("/c/config.toml")),
                    ("found", text("9")),
                    ("supported", text("1")),
                ],
                None,
            ),
            (
                ConfigError::Invalid {
                    file: "/c/config.toml".into(),
                    key: Some("plans[0].id".into()),
                    at: Some(app::ports::Position { line: 7, column: 3 }),
                    problem: app::ports::Problem::Syntax,
                    detail: "expected a table".into(),
                },
                ErrorCode::ConfigInvalid,
                vec![("file", text("/c/config.toml")), ("line", text("7"))],
                Some("plans[0].id: expected a table"),
            ),
            (
                ConfigError::Invalid {
                    file: "/c/config.toml".into(),
                    key: None,
                    at: None,
                    problem: app::ports::Problem::Syntax,
                    detail: String::new(),
                },
                ErrorCode::ConfigInvalid,
                vec![("file", text("/c/config.toml"))],
                None,
            ),
        ]
    }

    #[tokio::test]
    async fn a_configuration_that_cannot_be_used_is_unavailable_with_its_code() {
        for (error, code, args, detail) in unusable_configurations() {
            let engine = engine();
            let service = service(&engine, Arc::new(Broken(error.clone())));
            let expected = Err(RepositoriesError::Unavailable {
                code,
                args,
                detail: detail.map(str::to_owned),
            });
            assert_eq!(
                service
                    .connect(connect("demo", settings_dto()))
                    .await
                    .map(|_| ()),
                expected.clone().map(|_: ()| ()),
                "{error:?}"
            );
            assert_eq!(
                service
                    .init(init(Some("demo"), settings_dto()))
                    .await
                    .map(|_| ()),
                expected.map(|_: ()| ()),
                "{error:?}"
            );
        }
    }

    /// The lists of plans and repositories refuse a configuration they can't
    /// read as a save does (#347): a mistake names its file, its line and its
    /// setting.
    #[tokio::test]
    async fn the_lists_refuse_a_configuration_that_cannot_be_used_as_a_save_does() {
        for (error, code, args, detail) in unusable_configurations() {
            let catalog = ConfigCatalog {
                config: Arc::new(Broken(error.clone())),
                engine: engine(),
                operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
                schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
                sources: Arc::new(app::fakes::FakeSources),
                link: Arc::new(NameWatch::new(
                    Arc::new(app::fakes::FakeNetworkLink::new()),
                    || {},
                )),
            };
            let expected = CatalogError::Unavailable {
                code,
                args,
                detail: detail.map(str::to_owned),
            };
            assert_eq!(
                catalog.plans().await.map(|_| ()),
                Err(expected.clone()),
                "{error:?}"
            );
            assert_eq!(
                catalog.repositories().await.map(|_| ()),
                Err(expected),
                "{error:?}"
            );
        }
    }

    /// A listing of a repository's snapshots and the start of a backup read
    /// the configuration first, and refuse one they can't use as a save does
    /// (#347): the engine is never asked.
    #[tokio::test]
    async fn a_listing_and_a_backup_refuse_a_configuration_that_cannot_be_used_as_a_save_does() {
        for (error, code, args, detail) in unusable_configurations() {
            let (scripted, config) = (engine(), Arc::new(Broken(error.clone())));
            let catalog = ConfigCatalog {
                config: config.clone(),
                engine: scripted.clone(),
                operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
                schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
                sources: Arc::new(app::fakes::FakeSources),
                link: Arc::new(NameWatch::new(
                    Arc::new(app::fakes::FakeNetworkLink::new()),
                    || {},
                )),
            };
            let runner = PlanRunner {
                config,
                resolve: Arc::new(ResolveRepository::new(Arc::new(SystemSecrets::default()))),
                agent_id: AgentId::from_uuid(Uuid::from_u128(1)),
                engine: scripted.clone(),
                locations: None,
                retention: Arc::new(engine::RusticRetention),
                store: Arc::new(app::fakes::InMemoryOperationStore::new()),
                registry: Arc::new(CancelRegistry::new()),
                hooks: Arc::new(ProcessHookRunner::new()),
                events: Arc::new(BroadcastEventBus::new()),
                queue: Arc::new(RunQueue::new()),
                ends: app::retry::run_ends().0,
            };
            let detail = detail.map(str::to_owned);
            assert_eq!(
                catalog.snapshots("demo".to_owned()).await.map(|_| ()),
                Err(SnapshotsError::Unavailable {
                    code,
                    args: args.clone(),
                    detail: detail.clone(),
                }),
                "{error:?}"
            );
            assert_eq!(
                runner.start_backup("home".to_owned()).await,
                Err(StartError::Unavailable { code, args, detail }),
                "{error:?}"
            );
            assert_eq!(jobs(&scripted), 0, "{error:?}");
        }
    }

    /// A scheduled run over a configuration the daemon can't use doesn't
    /// start: nothing is queued and the engine is never asked, and the log
    /// names the plan and the configuration's code (#347), never the store's
    /// own words, which may quote a line of the file (I4).
    #[tokio::test]
    async fn a_scheduled_run_over_a_configuration_that_cannot_be_used_queues_nothing_and_logs_its_code()
     {
        capture();
        let plan = domain::PlanId::try_from("home".to_owned()).expect("a plan ID");
        for (error, code, _, detail) in unusable_configurations() {
            let scripted = engine();
            let store = Arc::new(app::fakes::InMemoryOperationStore::new());
            let runner = PlanRunner {
                config: Arc::new(Broken(error.clone())),
                resolve: Arc::new(ResolveRepository::new(Arc::new(SystemSecrets::default()))),
                agent_id: AgentId::from_uuid(Uuid::from_u128(1)),
                engine: scripted.clone(),
                locations: None,
                retention: Arc::new(engine::RusticRetention),
                store: store.clone(),
                registry: Arc::new(CancelRegistry::new()),
                hooks: Arc::new(ProcessHookRunner::new()),
                events: Arc::new(BroadcastEventBus::new()),
                queue: Arc::new(RunQueue::new()),
                ends: app::retry::run_ends().0,
            };

            let place = runner
                .queue
                .push(&plan, domain::planning::Concurrency::default());
            PlanRuns::start(&runner, &plan, Trigger::Schedule, place).await;

            assert_eq!(jobs(&scripted), 0, "{error:?}");
            let history = store
                .list(&app::ports::HistoryQuery::default())
                .await
                .expect("the log reads");
            assert!(history.operations.is_empty(), "{error:?}: {history:?}");
            let said = captured_log::lines();
            // The code as the page and the API name it, not its Rust name.
            let wire = serde_json::to_value(code).expect("a code is JSON");
            let wire = wire.as_str().expect("a code is a string");
            let line = said
                .iter()
                .find(|(level, line)| {
                    *level == log::Level::Warn
                        && line.contains("the scheduled run of plan home didn't start")
                        && line.contains(&format!("({wire})"))
                })
                .map(|(_, line)| line);
            let line =
                line.unwrap_or_else(|| panic!("{error:?}: no warning names {code:?}: {said:?}"));
            if let Some(detail) = detail {
                assert!(
                    !line.contains(detail),
                    "{error:?}: the log quotes the store: {line}"
                );
            }
        }
    }

    /// A backup started by hand whose repository's secret can't be read is
    /// refused, and the daemon's log says so, as it does for a scheduled run
    /// (#483): the plan, the refusal's code, the setting and where the
    /// secret was looked for, a name and never a secret (I4). The engine is
    /// never asked.
    #[tokio::test]
    async fn a_backup_started_by_hand_and_refused_for_its_secret_says_why_in_the_log() {
        capture();
        let variable = "LADDERBACK_TEST_PASSWORD_NEVER_SET";
        let alias = domain::RepositoryAlias::parse("demo").expect("a repository alias");
        let config = domain::planning::AgentConfig::new(
            domain::planning::AgentSettings::default(),
            Default::default(),
            std::collections::BTreeMap::from([(
                alias.clone(),
                domain::planning::RepositoryConfig {
                    locations: Default::default(),
                    id: None,
                    settings: RepositorySettings {
                        repository: "s3:s3.example.net/demo".to_owned(),
                        repo_hot: None,
                        password: SecretRef::Env(variable.to_owned()),
                        options: Default::default(),
                        options_hot: Default::default(),
                        options_cold: Default::default(),
                    },
                    maintenance: domain::planning::Maintenance::None,
                    group_by: Default::default(),
                    hooks: Default::default(),
                },
            )]),
            vec![domain::planning::Plan::new(
                domain::PlanId::parse("home").expect("a plan ID"),
                alias,
                vec![domain::planning::Source::parse("/home/demo").expect("a source")],
            )],
        )
        .expect("a valid configuration");
        let config_store = Arc::new(InMemoryConfigStore::new());
        app::ports::ConfigStore::save(&*config_store, &config)
            .await
            .expect("the config saves");
        let scripted = engine();
        let runner = PlanRunner {
            config: config_store,
            resolve: Arc::new(ResolveRepository::new(Arc::new(SystemSecrets::default()))),
            agent_id: AgentId::from_uuid(Uuid::from_u128(1)),
            engine: scripted.clone(),
            locations: None,
            retention: Arc::new(engine::RusticRetention),
            store: Arc::new(app::fakes::InMemoryOperationStore::new()),
            registry: Arc::new(CancelRegistry::new()),
            hooks: Arc::new(ProcessHookRunner::new()),
            events: Arc::new(BroadcastEventBus::new()),
            queue: Arc::new(RunQueue::new()),
            ends: app::retry::run_ends().0,
        };

        let refused = runner.start_backup("home".to_owned()).await;

        assert_matches!(
            &refused,
            Err(StartError::SecretRefused(SecretRefusal {
                code: ErrorCode::SecretMissing,
                ..
            }))
        );
        assert_eq!(jobs(&scripted), 0);
        let said = captured_log::lines();
        let line = said
            .iter()
            .find(|(level, line)| {
                *level == log::Level::Warn
                    && line.contains("the manual run of plan home didn't start")
                    && line.contains(variable)
            })
            .map(|(_, line)| line)
            .unwrap_or_else(|| panic!("no warning names the plan and the variable: {said:?}"));
        assert!(line.contains("(secret-missing)"), "{line}");
        assert!(line.contains("password"), "{line}");
    }

    /// A scheduled run whose repository's password can't be read doesn't
    /// start, and nobody is there to hear a refusal: it ends as one failed
    /// operation that says why, never started, the engine never asked (§5).
    /// The schedule hears it failed, so it is retried as any failed backup
    /// the schedule started (decision 76), and its place in the queue is
    /// given up. The log names the plan, the operation, the code, the setting
    /// and where the secret was looked for, as a refusal by hand does (#483).
    #[tokio::test]
    async fn a_scheduled_run_refused_for_a_secret_ends_as_one_failed_operation_that_says_why() {
        capture();
        const UNSET: &str = "LADDERBACK_TEST_REFUSED_RUN_PASSWORD_UNSET";
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let (runner, heard, scripted, plan) =
            refused_password_runner(UNSET, store.clone(), Arc::new(BroadcastEventBus::new())).await;

        let place = runner
            .queue
            .push(&plan, domain::planning::Concurrency::default());
        PlanRuns::start(&runner, &plan, Trigger::Schedule, place).await;

        assert_eq!(jobs(&scripted), 0, "the engine was asked");
        let history = store
            .list(&app::ports::HistoryQuery::default())
            .await
            .expect("the log reads");
        let [record] = history.operations.as_slice() else {
            panic!("one operation is recorded: {history:?}");
        };
        assert_eq!(
            record.status,
            OperationStatus::Failed {
                reason: FailureReason::SecretRefused {
                    field: "password".to_owned(),
                    refusal: domain::execution::refusal::SettingRefusal::SecretMissing {
                        source: UNSET.to_owned(),
                    },
                },
            }
        );
        assert_eq!(record.kind, OperationKind::Backup);
        assert_eq!(record.trigger, Trigger::Schedule);
        assert_eq!(record.target.plan.as_ref(), Some(&plan));
        assert_eq!(record.started_at, None, "a refused run never started");

        let end = tokio::time::timeout(std::time::Duration::from_secs(5), heard.next())
            .await
            .expect("the schedule hears how the run ended");
        assert_eq!(
            end,
            Some(app::retry::RunEnd {
                operation: record.id,
                plan: plan.clone(),
                trigger: Trigger::Schedule,
                outcome: app::retry::RunOutcome::Failed,
            })
        );

        let mut next = runner
            .queue
            .push(&plan, domain::planning::Concurrency::default());
        assert!(
            futures::FutureExt::now_or_never(next.turn()).is_some(),
            "the refused run gave up its place"
        );

        let operation = record.id.as_uuid().hyphenated().to_string();
        let said = captured_log::lines();
        let line = said
            .iter()
            .find(|(level, line)| *level == log::Level::Warn && line.contains(&operation))
            .map(|(_, line)| line)
            .unwrap_or_else(|| panic!("no warning names the operation: {said:?}"));
        assert!(
            line.contains("the scheduled run of plan home didn't start"),
            "{line}"
        );
        assert!(line.contains("(secret-missing)"), "{line}");
        // Where the secret was looked for is a name, never a secret (#483).
        assert!(
            line.contains(&format!("its password, from {UNSET}")),
            "{line}"
        );
    }

    /// A runner for the plan `home` on the repository `demo`, whose password
    /// is the environment variable `unset`, which nothing sets.
    async fn refused_password_runner(
        unset: &str,
        store: Arc<dyn OperationStore>,
        events: Arc<BroadcastEventBus>,
    ) -> (
        PlanRunner,
        app::retry::RunEnds,
        Arc<Scripted>,
        domain::PlanId,
    ) {
        let settings = domain::planning::RepositorySettings {
            repository: "/srv/demo-repo".to_owned(),
            repo_hot: None,
            password: SecretRef::Env(unset.to_owned()),
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        refused_runner(settings, store, events).await
    }

    /// A scheduled run whose password command fails with the other secrets
    /// of the repository in its stderr keeps the command's words in its
    /// history row's failure masked as the refusal of a run by hand masks
    /// them, and no secret reaches the row (I4).
    #[tokio::test]
    async fn a_refused_runs_history_row_says_what_the_command_said_masked_as_its_refusal() {
        let scratch = tempfile::tempdir().expect("a scratch folder");
        let referenced = scratch.path().join("token");
        std::fs::write(&referenced, "canary-from-a-file\n").expect("the file is written");
        let settings = domain::planning::RepositorySettings {
            repository: "/srv/demo-repo".to_owned(),
            repo_hot: None,
            password: SecretRef::Command(
                domain::secrets::CommandLine::parse(
                    "sh -c 'echo canary-inline canary-from-a-file >&2; exit 3'",
                )
                .expect("a command"),
            ),
            options: [
                ("secret_access_key".to_owned(), "canary-inline".to_owned()),
                (
                    "application_key".to_owned(),
                    format!("${{file:{}}}", referenced.display()),
                ),
            ]
            .into(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let (runner, _heard, _, plan) =
            refused_runner(settings, store, Arc::new(BroadcastEventBus::new())).await;

        let by_hand = Operations::start_backup(&runner, plan.as_str().to_owned())
            .await
            .expect_err("a run by hand is refused");
        let StartError::SecretRefused(alert) = by_hand else {
            panic!("a secret refusal, not {by_hand:?}");
        };
        let place = runner
            .queue
            .push(&plan, domain::planning::Concurrency::default());
        PlanRuns::start(&runner, &plan, Trigger::Schedule, place).await;

        let page = Operations::history(&runner, None, None, None)
            .await
            .expect("the history reads");
        let [row] = page.operations.as_slice() else {
            panic!("one operation is recorded: {page:?}");
        };
        let failure = row.failure.as_ref().expect("the row says why it failed");
        assert_eq!(failure.code, ErrorCode::SecretCommandFailed);
        let detail = failure
            .detail
            .as_deref()
            .expect("the command's words are kept");
        assert_eq!(Some(detail), alert.detail.as_deref());
        assert!(detail.contains(domain::secrets::REDACTED), "{detail}");
        let wire = serde_json::to_string(row).expect("the row serializes");
        assert!(!wire.contains("canary-inline"), "{wire}");
        assert!(!wire.contains("canary-from-a-file"), "{wire}");
    }

    /// A runner for the plan `home` on the repository `demo` of `settings`.
    async fn refused_runner(
        settings: domain::planning::RepositorySettings,
        store: Arc<dyn OperationStore>,
        events: Arc<BroadcastEventBus>,
    ) -> (
        PlanRunner,
        app::retry::RunEnds,
        Arc<Scripted>,
        domain::PlanId,
    ) {
        let plan = domain::PlanId::parse("home").expect("a plan ID");
        let alias = domain::RepositoryAlias::parse("demo").expect("a repository alias");
        let config = domain::planning::AgentConfig::new(
            domain::planning::AgentSettings::default(),
            Default::default(),
            std::collections::BTreeMap::from([(
                alias.clone(),
                domain::planning::RepositoryConfig {
                    locations: Default::default(),
                    id: None,
                    settings,
                    maintenance: domain::planning::Maintenance::Local,
                    group_by: Default::default(),
                    hooks: Default::default(),
                },
            )]),
            vec![domain::planning::Plan::new(
                plan.clone(),
                alias,
                vec![domain::planning::Source::parse("/home/demo").expect("a source")],
            )],
        )
        .expect("a valid configuration");
        let config_store = Arc::new(InMemoryConfigStore::new());
        app::ports::ConfigStore::save(&*config_store, &config)
            .await
            .expect("the config saves");
        let scripted = engine();
        let (ends, heard) = app::retry::run_ends();
        let runner = PlanRunner {
            config: config_store,
            resolve: Arc::new(ResolveRepository::new(Arc::new(SystemSecrets::default()))),
            agent_id: AgentId::from_uuid(Uuid::from_u128(1)),
            engine: scripted.clone(),
            locations: None,
            retention: Arc::new(engine::RusticRetention),
            store,
            registry: Arc::new(CancelRegistry::new()),
            hooks: Arc::new(ProcessHookRunner::new()),
            events,
            queue: Arc::new(RunQueue::new()),
            ends,
        };
        (runner, heard, scripted, plan)
    }

    /// A retry the schedule started that is refused for its secret ends as a
    /// failed retry: the schedule hears it with its own trigger, so the
    /// retry's count goes on rather than starting over (decision 76).
    #[tokio::test]
    async fn a_refused_retry_is_heard_as_a_failed_retry() {
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let (runner, heard, _, plan) = refused_password_runner(
            "LADDERBACK_TEST_REFUSED_RETRY_PASSWORD_UNSET",
            store.clone(),
            Arc::new(BroadcastEventBus::new()),
        )
        .await;

        let place = runner
            .queue
            .push(&plan, domain::planning::Concurrency::default());
        PlanRuns::start(&runner, &plan, Trigger::Retry, place).await;

        let history = store
            .list(&app::ports::HistoryQuery::default())
            .await
            .expect("the log reads");
        let [record] = history.operations.as_slice() else {
            panic!("one operation is recorded: {history:?}");
        };
        assert_eq!(record.trigger, Trigger::Retry);
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), heard.next())
            .await
            .expect("the schedule hears how the retry ended");
        assert_eq!(
            end,
            Some(app::retry::RunEnd {
                operation: record.id,
                plan,
                trigger: Trigger::Retry,
                outcome: app::retry::RunOutcome::Failed,
            })
        );
    }

    /// A refused run whose write the operation log refuses still ends: the
    /// schedule hears it failed and a page is told, and the daemon's log says
    /// the write failed by its code, never in the store's own words (I4).
    #[tokio::test]
    async fn a_refused_run_the_log_cannot_keep_still_ends_and_is_announced() {
        capture();
        let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let store = Arc::new(super::tests::RefusingSaves {
            inner: Arc::new(app::fakes::InMemoryOperationStore::new()),
            refused: refused.clone(),
        });
        let events = Arc::new(BroadcastEventBus::new());
        let mut page = events.subscribe().await;
        let (runner, heard, scripted, plan) = refused_password_runner(
            "LADDERBACK_TEST_REFUSED_UNKEPT_PASSWORD_UNSET",
            store,
            events,
        )
        .await;

        let place = runner
            .queue
            .push(&plan, domain::planning::Concurrency::default());
        PlanRuns::start(&runner, &plan, Trigger::Schedule, place).await;

        assert_eq!(jobs(&scripted), 0, "the engine was asked");
        assert_eq!(refused.load(std::sync::atomic::Ordering::SeqCst), 1);
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), heard.next())
            .await
            .expect("the schedule hears how the run ended")
            .expect("an end");
        assert_eq!(end.outcome, app::retry::RunOutcome::Failed);
        let told = futures::FutureExt::now_or_never(page.next()).flatten();
        assert!(
            matches!(
                told,
                Some(UiEvent::OperationFailed {
                    code: ErrorCode::SecretMissing,
                    ..
                })
            ),
            "{told:?}"
        );
        let operation = end.operation.as_uuid().hyphenated().to_string();
        let said = captured_log::lines();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains(&operation)
                && line.contains("a write to the operation log failed")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .all(|(_, line)| !line.contains(super::tests::REFUSAL)),
            "{said:?}"
        );
    }

    /// A backup of the plan `home` on `repository`, as the log holds it
    /// queued under `id` when the daemon starts again.
    fn queued_home_backup(
        id: u128,
        repository: Option<RepositoryId>,
    ) -> domain::execution::Operation {
        domain::execution::Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(id)),
            OperationKind::Backup,
            Trigger::Schedule,
            Target {
                plan: Some(domain::PlanId::parse("home").expect("a plan ID")),
                repository,
            },
            domain::Timestamp::from_second(1_800_000_000).expect("a time"),
        )
    }

    /// A backup left queued when the daemon stopped, whose repository's
    /// password can't be read when the next start takes it up again, ends
    /// failed with the refusal, not `daemon-stopped` (#490): under its own
    /// ID, never started, the engine never asked. The schedule hears it
    /// failed, so its retries follow it as they follow a refused scheduled
    /// run (decision 76), a page is told, and the daemon's log names the
    /// operation and the refusal's code.
    #[tokio::test]
    async fn a_resumed_backup_refused_for_its_secret_ends_with_the_refusal() {
        capture();
        const UNSET: &str = "LADDERBACK_TEST_RESUMED_PASSWORD_UNSET";
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let events = Arc::new(BroadcastEventBus::new());
        let mut page = events.subscribe().await;
        let (runner, heard, scripted, plan) =
            refused_password_runner(UNSET, store.clone(), events).await;
        let queued = queued_home_backup(0x0490, None);
        store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");

        PlanRuns::resume(&runner, queued.clone()).await;

        assert_eq!(jobs(&scripted), 0, "the engine was asked");
        let history = store
            .list(&app::ports::HistoryQuery::default())
            .await
            .expect("the log reads");
        let [record] = history.operations.as_slice() else {
            panic!("one operation is recorded: {history:?}");
        };
        assert_eq!(record.id, queued.id());
        assert_eq!(
            record.status,
            OperationStatus::Failed {
                reason: FailureReason::SecretRefused {
                    field: "password".to_owned(),
                    refusal: domain::execution::refusal::SettingRefusal::SecretMissing {
                        source: UNSET.to_owned(),
                    },
                },
            }
        );
        assert_eq!(record.started_at, None, "a refused run never started");
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), heard.next())
            .await
            .expect("the schedule hears how the run ended");
        assert_eq!(
            end,
            Some(app::retry::RunEnd {
                operation: queued.id(),
                plan,
                trigger: Trigger::Schedule,
                outcome: app::retry::RunOutcome::Failed,
            })
        );
        let told = futures::FutureExt::now_or_never(page.next()).flatten();
        assert!(
            matches!(
                told,
                Some(UiEvent::OperationFailed {
                    code: ErrorCode::SecretMissing,
                    ..
                })
            ),
            "{told:?}"
        );
        let operation = queued.id().as_uuid().hyphenated().to_string();
        let said = captured_log::lines();
        assert!(
            said.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains(&operation)
                && line.contains("(secret-missing)")),
            "no warning names the operation and the code: {said:?}"
        );
    }

    /// The history row of such a backup keeps what a failing password
    /// command said masked of the repository's secrets, as a refused
    /// scheduled run's row does, and no secret reaches it (I4).
    #[tokio::test]
    async fn a_resumed_backups_refusal_is_kept_masked_of_the_repositorys_secrets() {
        let scratch = tempfile::tempdir().expect("a scratch folder");
        let referenced = scratch.path().join("token");
        std::fs::write(&referenced, "canary-from-a-file\n").expect("the file is written");
        let settings = domain::planning::RepositorySettings {
            repository: "/srv/demo-repo".to_owned(),
            repo_hot: None,
            password: SecretRef::Command(
                domain::secrets::CommandLine::parse(
                    "sh -c 'echo canary-inline canary-from-a-file >&2; exit 3'",
                )
                .expect("a command"),
            ),
            options: [
                ("secret_access_key".to_owned(), "canary-inline".to_owned()),
                (
                    "application_key".to_owned(),
                    format!("${{file:{}}}", referenced.display()),
                ),
            ]
            .into(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let (runner, _heard, _, _) =
            refused_runner(settings, store.clone(), Arc::new(BroadcastEventBus::new())).await;
        let queued = queued_home_backup(0x0491, None);
        store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");

        PlanRuns::resume(&runner, queued).await;

        let page = Operations::history(&runner, None, None, None)
            .await
            .expect("the history reads");
        let [row] = page.operations.as_slice() else {
            panic!("one operation is recorded: {page:?}");
        };
        let failure = row.failure.as_ref().expect("the row says why it failed");
        assert_eq!(failure.code, ErrorCode::SecretCommandFailed);
        let detail = failure
            .detail
            .as_deref()
            .expect("the command's words are kept");
        assert!(detail.contains(domain::secrets::REDACTED), "{detail}");
        let wire = serde_json::to_string(row).expect("the row serializes");
        assert!(!wire.contains("canary-inline"), "{wire}");
        assert!(!wire.contains("canary-from-a-file"), "{wire}");
    }

    /// A refused backup whose plan now backs up to another repository than
    /// the one it was queued for ends `daemon-stopped`, as any backup whose
    /// plan moved does (decision 73): the refusal is of a repository it was
    /// never queued for.
    #[tokio::test]
    async fn a_resumed_backup_whose_plan_moved_ends_as_daemon_stopped_though_its_secret_is_refused()
    {
        let store = Arc::new(app::fakes::InMemoryOperationStore::new());
        let (runner, _heard, _, _) = refused_password_runner(
            "LADDERBACK_TEST_MOVED_PASSWORD_UNSET",
            store.clone(),
            Arc::new(BroadcastEventBus::new()),
        )
        .await;
        let queued = queued_home_backup(
            0x0492,
            Some(RepositoryId::parse(&"b".repeat(64)).expect("a repository ID")),
        );
        store
            .save(&queued, None)
            .await
            .expect("the queued backup is saved");

        PlanRuns::resume(&runner, queued.clone()).await;

        let record = store
            .get(queued.id())
            .await
            .expect("the log reads")
            .expect("the backup is in the log")
            .record;
        assert_eq!(
            record.status,
            OperationStatus::Failed {
                reason: FailureReason::DaemonStopped
            }
        );
    }

    /// A listing the engine refused, told by its code alone.
    fn engine_unavailable() -> SnapshotsError {
        SnapshotsError::Unavailable {
            code: ErrorCode::EngineUnavailable,
            args: Vec::new(),
            detail: None,
        }
    }

    #[tokio::test]
    async fn the_list_shows_where_each_password_comes_from_and_never_the_password() {
        use domain::secrets::{CommandLine, KeyringEntry};
        let config = Arc::new(InMemoryConfigStore::new());
        let sources = [
            (
                "inline",
                SecretRef::Inline(SecretValue::new("typed-secret")),
                api::PasswordSourceDto::Inline,
            ),
            (
                "file",
                SecretRef::File {
                    path: "/run/pw".to_owned(),
                    read: FileRead::FirstLine,
                },
                api::PasswordSourceDto::File {
                    path: "/run/pw".to_owned(),
                },
            ),
            (
                "keyring",
                SecretRef::Keyring(KeyringEntry::parse("ladderback/nas").expect("an entry")),
                api::PasswordSourceDto::Keyring {
                    entry: "ladderback/nas".to_owned(),
                },
            ),
            (
                "env",
                SecretRef::Env("PW".to_owned()),
                api::PasswordSourceDto::Env {
                    name: "PW".to_owned(),
                },
            ),
            (
                "command",
                SecretRef::Command(
                    CommandLine::parse("pass show --secret=hunter2 nas").expect("a command"),
                ),
                api::PasswordSourceDto::Command {
                    program: "pass".to_owned(),
                },
            ),
        ];
        let mut repositories = std::collections::BTreeMap::new();
        for (name, password, _) in &sources {
            repositories.insert(
                alias(name),
                domain::planning::RepositoryConfig {
                    locations: Default::default(),
                    id: None,
                    settings: RepositorySettings {
                        // One location each: two entries at one location
                        // are one repository under two names (§7.3).
                        repository: format!("rest:https://u:hunter2@nas.example/repo/{name}"),
                        repo_hot: Some("rest:https://u:hunter3@hot.example/repo".to_owned()),
                        password: password.clone(),
                        options: Default::default(),
                        options_hot: Default::default(),
                        options_cold: Default::default(),
                    },
                    maintenance: Default::default(),
                    group_by: Default::default(),
                    hooks: Default::default(),
                },
            );
        }
        config
            .save(
                &domain::planning::AgentConfig::new(
                    Default::default(),
                    Default::default(),
                    repositories,
                    Vec::new(),
                )
                .expect("a valid configuration"),
            )
            .await
            .expect("the configuration saves");
        let catalog = ConfigCatalog {
            config,
            engine: engine(),
            operations: Arc::new(app::fakes::InMemoryOperationStore::new()),
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        let listed = catalog.repositories().await.expect("the list");
        // In the order of the aliases, one row each.
        let mut expected: Vec<_> = sources
            .iter()
            .map(|(name, _, shown)| (*name, shown))
            .collect();
        expected.sort_by_key(|(name, _)| *name);
        assert_eq!(listed.repositories.len(), expected.len());
        for (row, (name, shown)) in listed.repositories.iter().zip(expected) {
            assert_eq!(row.alias, name);
            assert_eq!(&row.password, shown, "{name}");
        }
        let wire = serde_json::to_string(&listed).expect("the list serialises");
        for secret in ["typed-secret", "hunter2", "hunter3"] {
            assert!(!wire.contains(secret), "{secret} in {wire}");
        }
        let first = &listed.repositories[0];
        assert!(first.repository.contains("nas.example/repo"), "{first:?}");
        assert!(
            first
                .repo_hot
                .as_deref()
                .is_some_and(|hot| hot.contains("hot.example/repo")),
            "{first:?}"
        );
    }

    /// An operation log that can't be read: every call is refused.
    struct NoLog;

    #[async_trait]
    impl OperationStore for NoLog {
        async fn save(
            &self,
            _operation: &domain::execution::Operation,
            _result: Option<&app::ports::EngineResult>,
        ) -> Result<(), app::ports::StoreError> {
            Err(no_log())
        }

        async fn try_start(
            &self,
            _operation: &domain::execution::Operation,
        ) -> Result<app::ports::Start, app::ports::StoreError> {
            Err(no_log())
        }

        async fn get(
            &self,
            _id: OperationId,
        ) -> Result<Option<app::ports::OperationDetail>, app::ports::StoreError> {
            Err(no_log())
        }

        async fn list(
            &self,
            _query: &app::ports::HistoryQuery,
        ) -> Result<app::ports::HistoryPage, app::ports::StoreError> {
            Err(no_log())
        }

        async fn ever_succeeded(
            &self,
            _plan: &domain::PlanId,
        ) -> Result<bool, app::ports::StoreError> {
            Err(no_log())
        }

        async fn recover(
            &self,
            _reason: &FailureReason,
        ) -> Result<app::ports::Recovered, app::ports::StoreError> {
            Err(no_log())
        }
    }

    fn no_log() -> app::ports::StoreError {
        app::ports::StoreError::Unavailable {
            detail: "the database is locked".to_owned(),
        }
    }

    /// The rows of a configuration of `demo`, which the plan `demo-home`
    /// backs up to and whose entry records `demo_id`, and `spare`, which no
    /// plan does, each at its own location, listed over `operations`: each
    /// alias and whether its row offers to create it.
    async fn offers_listed(
        operations: Arc<dyn OperationStore>,
        demo_id: Option<RepositoryId>,
    ) -> Vec<(String, bool)> {
        let config = Arc::new(InMemoryConfigStore::new());
        let entry = |name: &str, id| domain::planning::RepositoryConfig {
            locations: Default::default(),
            settings: domain::planning::RepositorySettings {
                repository: format!("/srv/{name}/repo"),
                repo_hot: None,
                password: SecretRef::Env("DEMO_PASSWORD".to_owned()),
                options: Default::default(),
                options_hot: Default::default(),
                options_cold: Default::default(),
            },
            id,
            maintenance: Default::default(),
            group_by: Default::default(),
            hooks: Default::default(),
        };
        let plan = domain::planning::Plan::new(
            domain::PlanId::parse("demo-home").expect("a plan ID"),
            alias("demo"),
            vec![domain::planning::Source::parse("/home/demo").expect("a source")],
        );
        config
            .save(
                &domain::planning::AgentConfig::new(
                    Default::default(),
                    Default::default(),
                    [
                        (alias("demo"), entry("demo", demo_id)),
                        (alias("spare"), entry("spare", None)),
                    ]
                    .into(),
                    vec![plan],
                )
                .expect("a valid configuration"),
            )
            .await
            .expect("the configuration saves");
        let catalog = ConfigCatalog {
            config,
            engine: engine(),
            operations,
            schedules: Arc::new(app::fakes::InMemoryScheduleStore::new()),
            sources: Arc::new(app::fakes::FakeSources),
            link: Arc::new(NameWatch::new(
                Arc::new(app::fakes::FakeNetworkLink::new()),
                || {},
            )),
        };
        let listed = catalog.repositories().await.expect("the list");
        listed
            .repositories
            .into_iter()
            .map(|row| (row.alias, row.create_offered))
            .collect()
    }

    fn rows(offers: [(&str, bool); 2]) -> Vec<(String, bool)> {
        offers
            .into_iter()
            .map(|(alias, offered)| (alias.to_owned(), offered))
            .collect()
    }

    #[tokio::test]
    async fn a_row_offers_to_create_its_repository_only_once_a_job_found_none_and_none_reached_it()
    {
        let log = Arc::new(app::fakes::InMemoryOperationStore::new());
        assert_eq!(
            offers_listed(log.clone(), None).await,
            rows([("demo", false), ("spare", false)]),
            "nothing on record"
        );
        backed_up(&*log, Err(FailureReason::RepositoryNotFound)).await;
        assert_eq!(
            offers_listed(log.clone(), None).await,
            rows([("demo", true), ("spare", false)]),
            "a backup found none there"
        );
        backed_up(&*log, Ok(())).await;
        assert_eq!(
            offers_listed(log, None).await,
            rows([("demo", false), ("spare", false)]),
            "a backup reached it"
        );
    }

    #[tokio::test]
    async fn a_row_whose_entry_records_a_repository_id_offers_no_create() {
        let log = Arc::new(app::fakes::InMemoryOperationStore::new());
        backed_up(&*log, Err(FailureReason::RepositoryNotFound)).await;
        assert_eq!(
            offers_listed(log, Some(repository_id())).await,
            rows([("demo", false), ("spare", false)])
        );
    }

    /// The daemon logs that it offers nothing, with the log's failure as a
    /// code: the log's own words may hold a stored value nobody checked (I4).
    #[tokio::test]
    async fn an_operation_log_that_cannot_be_read_offers_nothing_to_create() {
        capture();
        assert_eq!(
            offers_listed(Arc::new(NoLog), None).await,
            rows([("demo", false), ("spare", false)])
        );
        let said: Vec<String> = captured_log::lines()
            .into_iter()
            .map(|(_, line)| line)
            .filter(|line| line.contains("no repository is offered to be created"))
            .collect();
        assert!(
            said.iter().any(|line| line.contains("store-unavailable")),
            "{said:?}"
        );
        assert!(
            said.iter()
                .all(|line| !line.contains("the database is locked")),
            "{said:?}"
        );
    }

    #[test]
    fn a_location_is_shown_without_the_password_in_it() {
        let mut settings = domain::planning::RepositorySettings {
            repository: "rest:https://backup:hunter2@nas.example/repo".to_owned(),
            repo_hot: Some("rest:https://backup:hunter3@hot.example/repo".to_owned()),
            password: domain::secrets::SecretRef::Inline(domain::secrets::SecretValue::new(
                "typed",
            )),
            options: Default::default(),
            options_hot: Default::default(),
            options_cold: Default::default(),
        };
        let shown = redacted_location(&settings, &settings.repository);
        assert!(
            !shown.contains("hunter2") && shown.contains("nas.example"),
            "{shown}"
        );
        let hot = settings.repo_hot.clone().expect("a hot half");
        let shown = redacted_location(&settings, &hot);
        assert!(
            !shown.contains("hunter3") && shown.contains("hot.example"),
            "{shown}"
        );
        // The typed password is not a needle: a location that happens to
        // hold the same text is still a location.
        settings.repository = "/srv/typed/repo".to_owned();
        assert_eq!(
            redacted_location(&settings, &settings.repository),
            "/srv/typed/repo"
        );
    }
}
