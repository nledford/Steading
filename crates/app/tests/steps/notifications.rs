//! Steps for `features/notifications/rules.feature`, for the fake-backed
//! acceptance runner: channels are URLs a `RecordingNotifier` records, their
//! secrets live in `FakeSecrets`, and each event is the news a run announces
//! of an operation saved to the in-memory log.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use api::UiEvent;
use cucumber::{given, then, when};

use app::announce::{end_of, start_of};
use app::fakes::{FakeSecrets, InMemoryConfigStore, InMemoryOperationStore, RecordingNotifier};
use app::notifications::{Notify, Report};
use app::ports::{
    ConfigError, ConfigStore, Message, Notification, NotifyError, OperationStore, RecordError,
};
use async_trait::async_trait;
use domain::execution::{
    FailureReason, Operation, OperationKind, Target, Trigger, Warning, WarningKind,
};
use domain::notifications::{
    Attempt, Channel, ChannelKind, ChannelName, EventKind, Notifications, Ping, Rule, RuleName,
    Template,
};
use domain::planning::{
    AgentConfig, AgentSettings, GroupBy, Hooks, Locale, Maintenance, Plan, RepositoryConfig,
    RepositorySettings, Source, Violation,
};
use domain::secrets::SecretRef;
use domain::{OperationId, PlanId, RepositoryAlias, RepositoryId, Timestamp, Uuid};

use super::Agent;

/// The notification rules of a scenario, the channels they send to, and
/// what was sent and what wasn't.
#[derive(Default)]
pub struct NotificationsWorld {
    repositories: BTreeMap<RepositoryAlias, RepositoryConfig>,
    plans: Vec<Plan>,
    channels: BTreeMap<ChannelName, Channel>,
    rules: BTreeMap<RuleName, Rule>,
    /// Each channel's URL, as its secret holds it.
    urls: BTreeMap<String, String>,
    secrets: Arc<FakeSecrets>,
    notifier: Arc<RecordingNotifier>,
    operations: Arc<InMemoryOperationStore>,
    /// Each plan's backup that has started and not yet ended.
    running: BTreeMap<String, Operation>,
    /// Each plan's backup that failed last.
    failed: BTreeMap<String, OperationId>,
    reports: Vec<Report>,
    /// Whether the configuration can't be read: set by the step that says
    /// so, which `steps/recovery.rs` holds for both.
    pub(crate) unreadable: bool,
    /// The agent's locale, when the scenario names one.
    locale: Option<Locale>,
    next: u128,
}

impl NotificationsWorld {
    async fn notify(&self) -> Notify {
        self.notify_over(self.operations.clone()).await
    }

    /// The scenario's notification rules, reading the operations they tell
    /// of from `operations`: another feature's log, for news its own steps
    /// announce (`steps/recovery.rs`).
    pub(crate) async fn notify_over(&self, operations: Arc<dyn OperationStore>) -> Notify {
        let notifications = Notifications::new(self.channels.clone(), self.rules.clone())
            .expect("the scenario's rules send to its channels");
        let config = AgentConfig::new(
            AgentSettings {
                locale: self.locale.clone(),
                ..AgentSettings::default()
            },
            Hooks::default(),
            self.repositories.clone(),
            self.plans.clone(),
        )
        .and_then(|config| config.with_notifications(notifications))
        .expect("the scenario's configuration is valid");
        let store: Arc<dyn ConfigStore> = if self.unreadable {
            Arc::new(Unreadable)
        } else {
            let store = InMemoryConfigStore::new();
            store.save(&config).await.expect("saved");
            Arc::new(store)
        };
        Notify::new(
            store,
            operations,
            self.secrets.clone(),
            self.notifier.clone(),
        )
    }

    /// A moment after the last one, so each step happens later.
    fn now(&mut self) -> Timestamp {
        self.next += 1;
        Timestamp::from_second(1_800_000_000 + i64::try_from(self.next).expect("a few steps"))
            .expect("a time")
    }

    fn operation(&mut self, kind: OperationKind, plan: Option<&str>) -> Operation {
        self.next += 1;
        let at = self.now();
        let mut operation = Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(self.next)),
            kind,
            Trigger::Manual,
            Target {
                plan: plan.map(plan_id),
                repository: None,
            },
            at,
        );
        operation.start(at).expect("a queued operation starts");
        operation
    }

    async fn tell_start(&mut self, operation: &Operation) {
        self.operations
            .save(operation, None)
            .await
            .expect("the log takes it");
        let news = start_of(operation, None).expect("a running operation has news");
        let reports = self.notify().await.tell(news).await;
        self.reports.extend(reports);
    }

    async fn tell_end(&mut self, operation: &Operation) {
        self.operations
            .save(operation, None)
            .await
            .expect("the log takes it");
        let news = end_of(operation).expect("an ended operation has news");
        let reports = self.notify().await.tell(news).await;
        self.reports.extend(reports);
    }

    /// The backup of `plan` that has started, or one that starts now.
    fn backup(&mut self, plan: &str) -> Operation {
        match self.running.remove(plan) {
            Some(operation) => operation,
            None => self.operation(OperationKind::Backup, Some(plan)),
        }
    }

    fn channel(&mut self, name: &str, kind: ChannelKind, url: Option<String>) {
        let variable = format!("DEMO_{}_URL", name.to_uppercase().replace('-', "_"));
        let url = url.unwrap_or_else(|| format!("http://127.0.0.1/{name}"));
        self.urls.insert(name.to_owned(), url.clone());
        self.secrets.set_env(&variable, &url);
        self.add(name, kind, variable);
    }

    fn add(&mut self, name: &str, kind: ChannelKind, variable: String) {
        let channel = Channel::new(kind, SecretRef::Env(variable), None).expect("a channel");
        self.channels.insert(channel_name(name), channel);
    }

    fn rule(
        &mut self,
        name: &str,
        events: &str,
        channel: &str,
        plan: Option<&str>,
        template: Option<&str>,
    ) {
        let events: BTreeSet<EventKind> = events
            .split(", ")
            .map(|event| EventKind::parse(event).expect("the scenario's event is one"))
            .collect();
        let rule = Rule::new(
            events,
            channel_name(channel),
            plan.into_iter().map(plan_id).collect(),
            template.map(|text| Template::parse(text).expect("the scenario's template reads")),
        )
        .expect("a rule");
        self.rules.insert(rule_name(name), rule);
    }

    /// What `channel` was sent, in order.
    fn sent_to(&self, channel: &str) -> Vec<Notification> {
        let url = self.urls.get(channel).expect("a channel of the scenario");
        self.notifier
            .delivered()
            .into_iter()
            .filter(|notification| notification.destination.url.expose() == url)
            .collect()
    }
}

fn alias(text: &str) -> RepositoryAlias {
    RepositoryAlias::parse(text).expect("the scenario's repository alias is valid")
}

fn plan_id(text: &str) -> PlanId {
    PlanId::parse(text).expect("the scenario's plan ID is valid")
}

fn channel_name(text: &str) -> ChannelName {
    ChannelName::parse(text).expect("the scenario's channel name is valid")
}

fn rule_name(text: &str) -> RuleName {
    RuleName::parse(text).expect("the scenario's rule name is valid")
}

fn engine_failure(detail: &str) -> FailureReason {
    FailureReason::EngineFailed {
        detail: detail.to_owned(),
    }
}

/// Why the scenario's configuration can't be read.
fn unreadable() -> ConfigError {
    ConfigError::Unavailable {
        file: "/home/demo/.config/ladderback/config.toml".to_owned(),
        detail: "permission denied".to_owned(),
    }
}

/// A configuration store whose file can't be read.
struct Unreadable;

#[async_trait]
impl ConfigStore for Unreadable {
    async fn load(&self) -> Result<AgentConfig, ConfigError> {
        Err(unreadable())
    }

    async fn save(&self, _config: &AgentConfig) -> Result<(), ConfigError> {
        Err(unreadable())
    }

    async fn record_id(
        &self,
        _alias: &RepositoryAlias,
        _read: &RepositoryConfig,
        _id: &RepositoryId,
    ) -> Result<(), RecordError> {
        Err(RecordError::Store(unreadable()))
    }
}

#[given(expr = "the plans {string} and {string} back up to the repository {string}")]
fn plans(agent: &mut Agent, first: String, second: String, repository: String) {
    back_up(&mut agent.notifications, &[first, second], &repository);
}

#[given(expr = "the plan {string} backs up to the repository {string}")]
fn a_plan(agent: &mut Agent, plan: String, repository: String) {
    back_up(&mut agent.notifications, &[plan], &repository);
}

/// Adds the repository `repository` and a plan of each of `plans` on it.
fn back_up(world: &mut NotificationsWorld, plans: &[String], repository: &str) {
    world.repositories.insert(
        alias(repository),
        RepositoryConfig {
            locations: Default::default(),
            id: None,
            settings: RepositorySettings {
                repository: format!("/srv/{repository}"),
                repo_hot: None,
                password: SecretRef::Env("DEMO_PASSWORD".to_owned()),
                options: BTreeMap::new(),
                options_hot: BTreeMap::new(),
                options_cold: BTreeMap::new(),
            },
            maintenance: Maintenance::default(),
            group_by: GroupBy::default(),
            hooks: Hooks::default(),
        },
    );
    for id in plans {
        let source = Source::parse(&format!("/home/demo/{id}")).expect("a source");
        world
            .plans
            .push(Plan::new(plan_id(id), alias(repository), vec![source]));
    }
}

#[given(expr = "a webhook channel {string}")]
fn a_webhook(agent: &mut Agent, name: String) {
    agent
        .notifications
        .channel(&name, ChannelKind::Webhook, None);
}

#[given(expr = "an ntfy channel {string}")]
fn an_ntfy_topic(agent: &mut Agent, name: String) {
    agent.notifications.channel(&name, ChannelKind::Ntfy, None);
}

#[given(expr = "a healthchecks channel {string}")]
fn a_check(agent: &mut Agent, name: String) {
    agent
        .notifications
        .channel(&name, ChannelKind::Healthchecks, None);
}

#[given(expr = "a webhook channel {string} whose URL holds the canary {string}")]
fn a_webhook_with_a_canary(agent: &mut Agent, name: String, canary: String) {
    let url = format!("http://127.0.0.1/{canary}");
    agent
        .notifications
        .channel(&name, ChannelKind::Webhook, Some(url));
}

#[given(expr = "a webhook channel {string} that nothing answers at")]
fn a_webhook_nobody_answers(agent: &mut Agent, name: String) {
    let world = &mut agent.notifications;
    world.channel(&name, ChannelKind::Webhook, None);
    let url = world.urls[&name].clone();
    world.notifier.fail_at(&url, NotifyError::Unreachable);
}

#[given(expr = "a webhook channel {string} whose URL is in a variable that isn't set")]
fn a_webhook_without_its_url(agent: &mut Agent, name: String) {
    let world = &mut agent.notifications;
    world.urls.insert(name.clone(), String::new());
    world.add(&name, ChannelKind::Webhook, "DEMO_UNSET_URL".to_owned());
}

#[given(expr = "a rule {string} that sends {string} to {string}")]
fn a_rule(agent: &mut Agent, name: String, events: String, channel: String) {
    agent
        .notifications
        .rule(&name, &events, &channel, None, None);
}

#[given(expr = "a rule {string} that sends {string} to {string} for the plan {string}")]
fn a_rule_for_a_plan(
    agent: &mut Agent,
    name: String,
    events: String,
    channel: String,
    plan: String,
) {
    agent
        .notifications
        .rule(&name, &events, &channel, Some(&plan), None);
}

#[given(expr = "a rule {string} that sends {string} to {string} with the template {string}")]
fn a_rule_with_a_template(
    agent: &mut Agent,
    name: String,
    events: String,
    channel: String,
    template: String,
) {
    agent
        .notifications
        .rule(&name, &events, &channel, None, Some(&template));
}

#[when(expr = "the backup of {string} starts")]
async fn a_backup_starts(agent: &mut Agent, plan: String) {
    let world = &mut agent.notifications;
    let operation = world.operation(OperationKind::Backup, Some(&plan));
    world.tell_start(&operation).await;
    world.running.insert(plan, operation);
}

#[when(expr = "the backup of {string} succeeds")]
async fn a_backup_succeeds(agent: &mut Agent, plan: String) {
    let world = &mut agent.notifications;
    let mut operation = world.backup(&plan);
    let at = world.now();
    operation.succeed(at).expect("a running backup succeeds");
    world.tell_end(&operation).await;
}

#[when(expr = "the backup of {string} succeeds with a warning")]
async fn a_backup_warns(agent: &mut Agent, plan: String) {
    let world = &mut agent.notifications;
    let mut operation = world.backup(&plan);
    operation
        .record_warning(Warning {
            kind: WarningKind::SourceUnreadable,
            item: Some("/home/demo/locked".to_owned()),
            during: "backup".to_owned(),
            detail: "permission denied".to_owned(),
        })
        .expect("a running backup warns");
    let at = world.now();
    operation.succeed(at).expect("a running backup succeeds");
    world.tell_end(&operation).await;
}

#[when(expr = "the backup of {string} fails")]
async fn a_backup_fails(agent: &mut Agent, plan: String) {
    fails_saying(agent, plan, "repository locked".to_owned()).await;
}

#[when(expr = "the backup of {string} fails and the engine says {string}")]
async fn fails_saying(agent: &mut Agent, plan: String, detail: String) {
    let world = &mut agent.notifications;
    let mut operation = world.backup(&plan);
    let at = world.now();
    operation
        .fail(engine_failure(&detail), at)
        .expect("a running backup fails");
    world.tell_end(&operation).await;
    world.failed.insert(plan, operation.id());
}

#[when(
    expr = "the scheduler plans retry {int} of {int} of the failed backup of {string} for {string}"
)]
async fn a_retry_is_planned(
    agent: &mut Agent,
    attempt: u32,
    attempts: u32,
    plan: String,
    at: String,
) {
    let world = &mut agent.notifications;
    let failed = *world
        .failed
        .get(&plan)
        .expect("the scenario's backup of the plan failed");
    let news = UiEvent::BackupRetrying {
        operation: failed.as_uuid().hyphenated().to_string(),
        plan,
        attempt,
        attempts,
        at,
    };
    let reports = world.notify().await.tell(news).await;
    world.reports.extend(reports);
}

#[when(expr = "a prune of {string} fails")]
async fn a_prune_fails(agent: &mut Agent, repository: String) {
    let world = &mut agent.notifications;
    let mut operation = world
        .operation(OperationKind::Prune, None)
        .with_repository_alias(alias(&repository));
    let at = world.now();
    operation
        .fail(engine_failure("repository locked"), at)
        .expect("a running prune fails");
    world.tell_end(&operation).await;
}

#[then(expr = "{string} is sent {string} for the plan {string} with the code {string}")]
fn sent_for_a_plan(agent: &mut Agent, channel: String, event: String, plan: String, code: String) {
    let sent = agent.notifications.sent_to(&channel);
    let told: Vec<_> = sent
        .iter()
        .map(|notification| {
            let event = &notification.event;
            (
                event.kind.name(),
                event.plan.as_ref().map(PlanId::as_str),
                event.code,
            )
        })
        .collect();
    assert_eq!(
        told,
        [(event.as_str(), Some(plan.as_str()), Some(code.as_str()))]
    );
}

#[then(expr = "{string} is sent {string} for the repository {string} with the code {string}")]
fn sent_for_a_repository(
    agent: &mut Agent,
    channel: String,
    event: String,
    repository: String,
    code: String,
) {
    let sent = agent.notifications.sent_to(&channel);
    let told: Vec<_> = sent
        .iter()
        .map(|notification| {
            let event = &notification.event;
            (
                event.kind.name(),
                event.plan.clone(),
                event.repository.as_ref().map(RepositoryAlias::as_str),
                event.code,
            )
        })
        .collect();
    assert_eq!(
        told,
        [(
            event.as_str(),
            None,
            Some(repository.as_str()),
            Some(code.as_str())
        )]
    );
}

#[then(expr = "{string} is sent only {string} for the plan {string}")]
fn sent_only(agent: &mut Agent, channel: String, event: String, plan: String) {
    let told: Vec<_> = agent
        .notifications
        .sent_to(&channel)
        .iter()
        .map(|notification| {
            (
                notification.event.kind.name(),
                notification
                    .event
                    .plan
                    .as_ref()
                    .map(|id| id.as_str().to_owned()),
            )
        })
        .collect();
    assert_eq!(told, [(event.as_str(), Some(plan))]);
}

#[then("no channel is sent anything")]
fn nothing_sent(agent: &mut Agent) {
    assert_eq!(agent.notifications.notifier.delivered(), []);
}

#[then(expr = "{string} is pinged {string} and then {string}")]
fn pinged(agent: &mut Agent, channel: String, first: String, second: String) {
    let ping = |name: &str| match name {
        "start" => Ping::Start,
        "success" => Ping::Success,
        "fail" => Ping::Fail,
        other => panic!("no ping {other:?}"),
    };
    let pings: Vec<Ping> = agent
        .notifications
        .sent_to(&channel)
        .iter()
        .map(|notification| notification.event.kind.ping())
        .collect();
    assert_eq!(pings, [ping(&first), ping(&second)]);
}

#[then(expr = "{string} is sent the message {string}")]
fn sent_the_message(agent: &mut Agent, channel: String, message: String) {
    let messages: Vec<Message> = agent
        .notifications
        .sent_to(&channel)
        .into_iter()
        .map(|notification| notification.message)
        .collect();
    assert_eq!(messages, [Message::Custom(message)]);
}

#[given(expr = "the agent's locale is {string}")]
fn the_agents_locale(agent: &mut Agent, locale: String) {
    agent.notifications.locale = Some(Locale::parse(&locale).expect("the scenario's locale"));
}

#[then(expr = "{string} is sent the default message in the locale {string}")]
fn sent_the_default_message(agent: &mut Agent, channel: String, locale: String) {
    let sent: Vec<(Message, Option<Locale>)> = agent
        .notifications
        .sent_to(&channel)
        .into_iter()
        .map(|notification| (notification.message, notification.locale))
        .collect();
    let locale = Locale::parse(&locale).expect("the scenario's locale");
    assert_eq!(sent, [(Message::Default, Some(locale))]);
}

#[then(expr = "nothing {string} is sent shows {string} or {string}")]
fn shows_neither(agent: &mut Agent, channel: String, first: String, second: String) {
    let sent = agent.notifications.sent_to(&channel);
    assert!(!sent.is_empty(), "{channel} was sent something");
    for notification in sent {
        // What the channel is told: the event's facts and the message.
        let told = format!("{:?} {:?}", notification.event, notification.message);
        assert!(!told.contains(&first), "{told}");
        assert!(!told.contains(&second), "{told}");
    }
}

#[then(expr = "the notification of {string} to {string} is reported as {string}")]
fn reported(agent: &mut Agent, event: String, channel: String, reason: String) {
    let reported: Vec<_> = agent
        .notifications
        .reports
        .iter()
        .map(|report| match report {
            Report::Undelivered(undelivered) => format!(
                "{} {} {}",
                undelivered.event.name(),
                undelivered.channel.as_str(),
                undelivered.reason.reason()
            ),
            Report::ConfigUnavailable(error) => format!("{error:?}"),
        })
        .collect();
    assert_eq!(reported, [format!("{event} {channel} {reason}")]);
}

#[then(expr = "that the configuration can't be read is reported once for each of the {int} events")]
fn config_reported(agent: &mut Agent, events: usize) {
    assert_eq!(
        agent.notifications.reports,
        vec![Report::ConfigUnavailable(unreadable()); events]
    );
}

#[then(
    expr = "the configuration is refused because the rule {string} sends to no channel {string}"
)]
fn refused(agent: &mut Agent, rule: String, channel: String) {
    let world = &agent.notifications;
    let refused = Notifications::new(world.channels.clone(), world.rules.clone());
    assert_eq!(
        refused,
        Err(Violation::UnknownChannel {
            rule: rule_name(&rule),
            channel: channel_name(&channel),
        })
    );
}

#[then(expr = "{string} is sent {string} for the plan {string}: retry {int} of {int} at {string}")]
fn sent_a_retry(
    agent: &mut Agent,
    channel: String,
    event: String,
    plan: String,
    attempt: u32,
    attempts: u32,
    at: String,
) {
    let at: Timestamp = at.parse().expect("the scenario's time");
    let sent = agent.notifications.sent_to(&channel);
    let told: Vec<_> = sent
        .iter()
        .map(|notification| {
            let event = &notification.event;
            (
                event.kind.name(),
                event.plan.as_ref().map(PlanId::as_str),
                event.attempt,
                event.at,
            )
        })
        .collect();
    assert_eq!(
        told,
        [(
            event.as_str(),
            Some(plan.as_str()),
            Some(Attempt {
                number: attempt,
                of: attempts
            }),
            at
        )]
    );
}
