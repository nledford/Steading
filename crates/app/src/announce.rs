//! What a page is told when an operation starts and when it ends (technical
//! design §4.4).
//!
//! The run that records an operation tells of each once the log has it: its
//! start after the write that says it is running, its end after its last
//! write, so a page that reads the history on the news finds it there. The
//! engine's own start and end are no news: only the run knows when the log
//! has them.

use api::{ErrorCode, ErrorDto, MsgArg, UiEvent};
use domain::execution::refusal::SettingRefusal;
use domain::execution::{FailureReason, Operation, OperationStatus};
use domain::{OperationId, RepositoryId, Timestamp};

use crate::ports::EventBus;

/// The news that `operation` started: what is running, on which repository,
/// and when it started, as the log has it. `opened` is the repository the
/// engine says it opened, for a job that knew no ID when it was made (a
/// first job, decision 57). `None` for an operation that is not running.
pub fn start_of(operation: &Operation, opened: Option<&RepositoryId>) -> Option<UiEvent> {
    if *operation.status() != OperationStatus::Running {
        return None;
    }
    Some(UiEvent::OperationStarted {
        operation: operation.id().as_uuid().hyphenated().to_string(),
        repository: operation
            .target()
            .repository
            .as_ref()
            .or(opened)
            .map(|id| id.as_str().to_owned()),
        started_at: operation.started_at()?.to_string(),
    })
}

/// Tells `events`, when a run has a bus, that `operation` started, as
/// [`start_of`] has it. A run calls it once, after the write that says the
/// operation is running: whether the log took that write, refused it or was
/// given up on after [`SAVE_BOUND`](crate::log_write::SAVE_BOUND), the news
/// goes out.
pub(crate) async fn announce_start(
    events: Option<&dyn EventBus>,
    operation: &Operation,
    opened: Option<&RepositoryId>,
) {
    if let Some(events) = events
        && let Some(news) = start_of(operation, opened)
    {
        events.publish(news).await;
    }
}

/// The news that `operation` ended: a success or a failure, with the time it
/// finished. `None` for an operation that has not ended, and for a cancelled
/// one, which no page is told of: the page that cancelled it reads the
/// history until it settles.
pub fn end_of(operation: &Operation) -> Option<UiEvent> {
    let finished_at = operation.finished_at()?.to_string();
    let id = operation.id().as_uuid().hyphenated().to_string();
    match operation.status() {
        OperationStatus::Succeeded | OperationStatus::SucceededWithWarnings => {
            Some(UiEvent::OperationSucceeded {
                operation: id,
                finished_at,
            })
        }
        OperationStatus::Failed { reason } => {
            let failure = failure(reason);
            Some(UiEvent::OperationFailed {
                operation: id,
                code: failure.code,
                args: failure.positional(),
                finished_at,
            })
        }
        OperationStatus::Queued | OperationStatus::Running | OperationStatus::Cancelled => None,
    }
}

/// Tells `events`, when a run has a bus, how `operation` ended, as
/// [`end_of`] has it. A run calls it once, after its last write to the log:
/// whether the log took that write, refused it or was given up on after
/// [`SAVE_BOUND`](crate::log_write::SAVE_BOUND), the news goes out.
pub(crate) async fn announce_end(events: Option<&dyn EventBus>, operation: &Operation) {
    if let Some(events) = events
        && let Some(news) = end_of(operation)
    {
        events.publish(news).await;
    }
}

/// The news that startup recovery ended `operation`, which the log showed
/// as running, as failed with [`FailureReason::DaemonStopped`]: its time is
/// `at`, when the start ended it, since when it stopped is unknown and the
/// log keeps no finish time for it (decision 40).
pub fn daemon_stopped(operation: OperationId, at: Timestamp) -> UiEvent {
    let failure = failure(&FailureReason::DaemonStopped);
    UiEvent::OperationFailed {
        operation: operation.as_uuid().hyphenated().to_string(),
        code: failure.code,
        args: failure.positional(),
        finished_at: at.to_string(),
    }
}

/// Why an operation failed, as a page shows it: the code, the arguments its
/// message takes, by name, and the engine's own words as the detail, which
/// the engine masked of every secret of the job (I4). A run refused for a
/// setting is shown as its refusal is ([`refused`]), with what a platform or
/// a command said as the detail, masked of the repository's secrets.
pub fn failure(reason: &FailureReason) -> ErrorDto {
    use FailureReason as F;
    let said = |detail: &str| Some(detail.to_owned());
    match reason {
        F::EngineUnavailable { detail } | F::JobRejected { detail } => {
            ErrorDto::new(ErrorCode::EngineUnavailable, [], said(detail))
        }
        F::EngineFailed { detail } => ErrorDto::new(ErrorCode::OperationFailed, [], said(detail)),
        F::HookFailed { .. } => ErrorDto::new(ErrorCode::OperationFailed, [], None),
        F::EngineExited { exit_code } => ErrorDto::new(
            ErrorCode::EngineExited,
            exit_code.map(|code| ("exit_code", MsgArg::Text(code.to_string()))),
            None,
        ),
        F::ProtocolViolated { transition } => ErrorDto::new(
            ErrorCode::ProtocolViolated,
            [("transition", MsgArg::Text(format!("{transition:?}")))],
            None,
        ),
        F::DeadlinePassed => ErrorDto::new(ErrorCode::DeadlinePassed, [], None),
        F::EngineStartTimedOut => ErrorDto::new(ErrorCode::EngineStartTimedOut, [], None),
        F::EngineExitTimedOut => ErrorDto::new(ErrorCode::EngineExitTimedOut, [], None),
        F::RepositoryExists => ErrorDto::new(ErrorCode::RepositoryExists, [], None),
        F::RepositoryReplaced => ErrorDto::new(ErrorCode::RepositoryReplaced, [], None),
        F::RepositoryNotFound => ErrorDto::new(ErrorCode::RepositoryNotFound, [], None),
        F::CredentialsRefused => ErrorDto::new(ErrorCode::CredentialsRefused, [], None),
        F::SecretsTooLargeToMask { field, limit } => ErrorDto::new(
            ErrorCode::SecretsTooLargeToMask,
            std::iter::once((
                "limit",
                MsgArg::Bytes(u64::try_from(*limit).unwrap_or(u64::MAX)),
            ))
            .chain(
                field
                    .as_deref()
                    .map(|field| ("field", MsgArg::Text(field.to_owned()))),
            ),
            None,
        ),
        F::DaemonStopped => ErrorDto::new(ErrorCode::DaemonStopped, [], None),
        F::NoLocationUsable => ErrorDto::new(ErrorCode::NoLocationUsable, [], None),
        F::NoLocationRuleHolds => ErrorDto::new(ErrorCode::NoLocationRuleHolds, [], None),
        F::PasswordNotAccepted => ErrorDto::new(ErrorCode::PasswordNotAccepted, [], None),
        F::SecretRefused { field, refusal } => {
            let (code, args, detail) = refused(field, refusal);
            ErrorDto::new(code, args, detail)
        }
    }
}

/// A run's refusal as a request to start it is answered and as a page shows
/// it: the code, the arguments its message takes, by name, in the order the
/// code declares them, and what a platform or a command said, as `detail`.
/// Every text was masked of the repository's secrets before it was kept
/// (I4).
pub fn refused(
    field: &str,
    refusal: &SettingRefusal,
) -> (ErrorCode, Vec<(&'static str, MsgArg)>, Option<String>) {
    use SettingRefusal as R;
    let text = |text: &str| MsgArg::Text(text.to_owned());
    let at = ("field", text(field));
    let from = |source: &str| vec![at.clone(), ("source", text(source))];
    let with = |mut args: Vec<(&'static str, MsgArg)>, arg| {
        args.push(arg);
        args
    };
    match refusal {
        R::SecretMissing { source } => (ErrorCode::SecretMissing, from(source), None),
        R::SecretUnreadable { source, detail } => {
            (ErrorCode::SecretUnreadable, from(source), detail.clone())
        }
        R::SecretKeyringUnavailable { source, detail } => (
            ErrorCode::SecretKeyringUnavailable,
            from(source),
            detail.clone(),
        ),
        R::SecretNotText { source } => (ErrorCode::SecretNotText, from(source), None),
        R::SecretTooLarge { source, limit } => (
            ErrorCode::SecretTooLarge,
            with(from(source), ("limit", MsgArg::Bytes(*limit))),
            None,
        ),
        R::SecretCommandFailed { source, detail } => {
            (ErrorCode::SecretCommandFailed, from(source), detail.clone())
        }
        R::SecretCommandTimedOut { source, seconds } => (
            ErrorCode::SecretCommandTimedOut,
            with(from(source), ("seconds", MsgArg::Count(*seconds))),
            None,
        ),
        R::SecretCommandOutputOpen { source } => {
            (ErrorCode::SecretCommandOutputOpen, from(source), None)
        }
        R::SecretInProcessArguments => (ErrorCode::SecretInProcessArguments, vec![at], None),
        R::SecretExpansionTooLong { limit } => (
            ErrorCode::SecretExpansionTooLong,
            vec![at, ("limit", MsgArg::Bytes(*limit))],
            None,
        ),
        R::LocationTooManyCredentials => (ErrorCode::LocationTooManyCredentials, vec![at], None),
        R::RepositoryPathRelative => (ErrorCode::RepositoryPathRelative, vec![at], None),
        R::TemplateInvalid { at: place } => (
            ErrorCode::TemplateInvalid,
            std::iter::once(at)
                .chain(place.map(|place| ("at", MsgArg::Text(place.to_string()))))
                .collect(),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use domain::execution::{
        FailureReason, Operation, OperationKind, OperationStatus, Target, Transition, Trigger,
        Warning, WarningKind,
    };
    use domain::{OperationId, Timestamp, Uuid};

    use super::*;

    const ID: &str = "00000000-0000-0000-0000-000000000001";

    fn t(seconds: i64) -> Timestamp {
        Timestamp::from_second(1_800_000_000 + seconds).expect("the demo time is valid")
    }

    fn queued() -> Operation {
        Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(1)),
            OperationKind::Backup,
            Trigger::Manual,
            Target::default(),
            t(0),
        )
    }

    fn running() -> Operation {
        let mut operation = queued();
        operation.start(t(1)).expect("a queued operation starts");
        operation
    }

    fn warning() -> Warning {
        Warning {
            kind: WarningKind::SourceUnreadable,
            item: Some("/home/demo/secret".to_owned()),
            during: "backup".to_owned(),
            detail: "permission denied".to_owned(),
        }
    }

    #[test]
    fn a_success_is_announced_with_the_time_it_finished() {
        let mut operation = running();
        operation.succeed(t(60)).expect("a running operation ends");
        assert_eq!(
            end_of(&operation),
            Some(UiEvent::OperationSucceeded {
                operation: ID.to_owned(),
                finished_at: t(60).to_string(),
            })
        );
    }

    #[test]
    fn a_success_with_warnings_is_announced_as_a_success() {
        let mut operation = running();
        operation
            .record_warning(warning())
            .expect("a running operation takes a warning");
        operation.succeed(t(60)).expect("a running operation ends");
        assert_eq!(operation.status(), &OperationStatus::SucceededWithWarnings);
        assert_eq!(
            end_of(&operation),
            Some(UiEvent::OperationSucceeded {
                operation: ID.to_owned(),
                finished_at: t(60).to_string(),
            })
        );
    }

    #[test]
    fn a_failure_is_announced_with_its_code_and_the_time_it_finished() {
        let mut operation = running();
        operation
            .fail(
                FailureReason::EngineFailed {
                    detail: "repository not found".to_owned(),
                },
                t(60),
            )
            .expect("a running operation ends");
        assert_eq!(
            end_of(&operation),
            Some(UiEvent::OperationFailed {
                operation: ID.to_owned(),
                code: ErrorCode::OperationFailed,
                args: vec![MsgArg::Text("repository not found".to_owned())],
                finished_at: t(60).to_string(),
            })
        );
    }

    #[test]
    fn an_operation_that_has_not_ended_is_not_announced() {
        assert_eq!(end_of(&queued()), None);
        assert_eq!(end_of(&running()), None);
    }

    fn repository(digit: &str) -> RepositoryId {
        RepositoryId::parse(&digit.repeat(64)).expect("a repository ID")
    }

    #[test]
    fn a_start_is_announced_with_the_time_the_log_has() {
        assert_eq!(
            start_of(&running(), None),
            Some(UiEvent::OperationStarted {
                operation: ID.to_owned(),
                repository: None,
                started_at: t(1).to_string(),
            })
        );
    }

    #[test]
    fn a_start_names_the_repository_the_job_knew_before_the_one_the_engine_opened() {
        let mut operation = Operation::queue(
            OperationId::from_uuid(Uuid::from_u128(1)),
            OperationKind::Backup,
            Trigger::Manual,
            Target {
                plan: None,
                repository: Some(repository("a")),
            },
            t(0),
        );
        operation.start(t(1)).expect("a queued operation starts");
        let Some(UiEvent::OperationStarted {
            repository: named, ..
        }) = start_of(&operation, Some(&repository("b")))
        else {
            panic!("a running operation is announced");
        };
        assert_eq!(named, Some("a".repeat(64)));
    }

    #[test]
    fn a_first_jobs_start_names_the_repository_the_engine_opened() {
        let Some(UiEvent::OperationStarted {
            repository: named, ..
        }) = start_of(&running(), Some(&repository("b")))
        else {
            panic!("a running operation is announced");
        };
        assert_eq!(named, Some("b".repeat(64)));
    }

    #[test]
    fn only_a_running_operation_is_announced_as_started() {
        assert_eq!(start_of(&queued(), None), None);
        let mut ended = running();
        ended.succeed(t(60)).expect("a running operation ends");
        assert_eq!(start_of(&ended, None), None);
    }

    #[test]
    fn a_cancelled_operation_is_not_announced() {
        let mut operation = running();
        operation.cancel(t(5)).expect("a running operation cancels");
        assert_eq!(end_of(&operation), None);
    }

    #[test]
    fn every_failure_has_the_code_and_arguments_a_page_renders() {
        use ErrorCode as C;
        use FailureReason as F;
        let text = |text: &str| MsgArg::Text(text.to_owned());
        let cases = [
            (
                F::EngineUnavailable {
                    detail: "no such program".to_owned(),
                },
                C::EngineUnavailable,
                vec![text("no such program")],
            ),
            (
                F::JobRejected {
                    detail: "bad job".to_owned(),
                },
                C::EngineUnavailable,
                vec![text("bad job")],
            ),
            (
                F::EngineFailed {
                    detail: "repository not found".to_owned(),
                },
                C::OperationFailed,
                vec![text("repository not found")],
            ),
            (
                F::HookFailed { exit_code: Some(3) },
                C::OperationFailed,
                vec![],
            ),
            (
                F::EngineExited { exit_code: Some(3) },
                C::EngineExited,
                vec![text("3")],
            ),
            (F::EngineExited { exit_code: None }, C::EngineExited, vec![]),
            (
                F::ProtocolViolated {
                    transition: Transition::Succeed,
                },
                C::ProtocolViolated,
                vec![text("Succeed")],
            ),
            (F::DeadlinePassed, C::DeadlinePassed, vec![]),
            (F::EngineStartTimedOut, C::EngineStartTimedOut, vec![]),
            (F::EngineExitTimedOut, C::EngineExitTimedOut, vec![]),
            (F::RepositoryExists, C::RepositoryExists, vec![]),
            (F::RepositoryReplaced, C::RepositoryReplaced, vec![]),
            (F::RepositoryNotFound, C::RepositoryNotFound, vec![]),
            (F::CredentialsRefused, C::CredentialsRefused, vec![]),
            (F::DaemonStopped, C::DaemonStopped, vec![]),
            (
                F::SecretsTooLargeToMask {
                    field: Some("HTTPS_PROXY".to_owned()),
                    limit: 262_144,
                },
                C::SecretsTooLargeToMask,
                vec![MsgArg::Bytes(262_144), text("HTTPS_PROXY")],
            ),
            (
                F::SecretsTooLargeToMask {
                    field: None,
                    limit: 262_144,
                },
                C::SecretsTooLargeToMask,
                vec![MsgArg::Bytes(262_144)],
            ),
        ];
        // A refused run is told as its refusal is, what was said last.
        let refused = refusals()
            .into_iter()
            .map(|(refusal, code, _, args, detail)| {
                (
                    F::SecretRefused {
                        field: "options.token".to_owned(),
                        refusal,
                    },
                    code,
                    args.into_iter()
                        .map(|(_, arg)| arg)
                        .chain(detail.map(MsgArg::Text))
                        .collect(),
                )
            });
        for (reason, code, args) in cases.into_iter().chain(refused) {
            let mut operation = running();
            operation
                .fail(reason.clone(), t(60))
                .expect("a running operation ends");
            let Some(UiEvent::OperationFailed {
                code: told,
                args: told_args,
                ..
            }) = end_of(&operation)
            else {
                panic!("a failure is announced: {reason:?}");
            };
            assert_eq!((told, told_args), (code, args), "{reason:?}");
        }
    }

    /// What a page shows of a failure: its code, its arguments by name, and
    /// the engine's own words (masked by the engine, I4) as the detail.
    #[test]
    fn every_failure_is_shown_by_its_code_its_arguments_by_name_and_the_engines_words() {
        use ErrorCode as C;
        use FailureReason as F;
        let text = |text: &str| MsgArg::Text(text.to_owned());
        let said = |text: &str| Some(text.to_owned());
        let cases = [
            (
                F::EngineUnavailable {
                    detail: "no such program".to_owned(),
                },
                ErrorDto::new(C::EngineUnavailable, [], said("no such program")),
            ),
            (
                F::JobRejected {
                    detail: "bad job".to_owned(),
                },
                ErrorDto::new(C::EngineUnavailable, [], said("bad job")),
            ),
            (
                F::EngineFailed {
                    detail: "no key opens the repository".to_owned(),
                },
                ErrorDto::new(C::OperationFailed, [], said("no key opens the repository")),
            ),
            (
                F::HookFailed { exit_code: Some(3) },
                ErrorDto::new(C::OperationFailed, [], None),
            ),
            (
                F::EngineExited { exit_code: Some(3) },
                ErrorDto::new(C::EngineExited, [("exit_code", text("3"))], None),
            ),
            (
                F::EngineExited { exit_code: None },
                ErrorDto::new(C::EngineExited, [], None),
            ),
            (
                F::ProtocolViolated {
                    transition: Transition::Succeed,
                },
                ErrorDto::new(C::ProtocolViolated, [("transition", text("Succeed"))], None),
            ),
            (
                F::DeadlinePassed,
                ErrorDto::new(C::DeadlinePassed, [], None),
            ),
            (
                F::EngineStartTimedOut,
                ErrorDto::new(C::EngineStartTimedOut, [], None),
            ),
            (
                F::EngineExitTimedOut,
                ErrorDto::new(C::EngineExitTimedOut, [], None),
            ),
            (
                F::RepositoryExists,
                ErrorDto::new(C::RepositoryExists, [], None),
            ),
            (
                F::RepositoryReplaced,
                ErrorDto::new(C::RepositoryReplaced, [], None),
            ),
            (
                F::RepositoryNotFound,
                ErrorDto::new(C::RepositoryNotFound, [], None),
            ),
            (
                F::CredentialsRefused,
                ErrorDto::new(C::CredentialsRefused, [], None),
            ),
            (F::DaemonStopped, ErrorDto::new(C::DaemonStopped, [], None)),
            (
                F::NoLocationUsable,
                ErrorDto::new(C::NoLocationUsable, [], None),
            ),
            (
                F::NoLocationRuleHolds,
                ErrorDto::new(C::NoLocationRuleHolds, [], None),
            ),
            (
                F::PasswordNotAccepted,
                ErrorDto::new(C::PasswordNotAccepted, [], None),
            ),
            (
                F::SecretsTooLargeToMask {
                    field: Some("HTTPS_PROXY".to_owned()),
                    limit: 262_144,
                },
                ErrorDto::new(
                    C::SecretsTooLargeToMask,
                    [
                        ("limit", MsgArg::Bytes(262_144)),
                        ("field", text("HTTPS_PROXY")),
                    ],
                    None,
                ),
            ),
            (
                F::SecretsTooLargeToMask {
                    field: None,
                    limit: 262_144,
                },
                ErrorDto::new(
                    C::SecretsTooLargeToMask,
                    [("limit", MsgArg::Bytes(262_144))],
                    None,
                ),
            ),
        ];
        // A refused run is shown as its refusal is, the arguments by the
        // names its alert gives them and what was said as the detail.
        let refused = refusals()
            .into_iter()
            .map(|(refusal, code, _, args, detail)| {
                (
                    F::SecretRefused {
                        field: "options.token".to_owned(),
                        refusal,
                    },
                    ErrorDto::new(code, args, detail),
                )
            });
        for (reason, shown) in cases.into_iter().chain(refused) {
            assert_eq!(failure(&reason), shown, "{reason:?}");
            // An event tells it by position; read back, it is the same.
            assert_eq!(
                ErrorDto::from_positional(shown.code, &shown.positional()),
                shown,
                "{reason:?}"
            );
        }
    }

    /// A refusal of a setting, its codes, its arguments and what was said.
    type Refusal = (
        SettingRefusal,
        ErrorCode,
        &'static str,
        Vec<(&'static str, MsgArg)>,
        Option<String>,
    );

    /// Every refusal of a setting, with the code a request to start its run
    /// is refused with, the code the log keeps it under, and its arguments
    /// by name, in the order the code declares them.
    fn refusals() -> Vec<Refusal> {
        use SettingRefusal as R;
        let text = |text: &str| MsgArg::Text(text.to_owned());
        let field = || ("field", text("options.token"));
        let source = || ("source", text("ladderback/demo"));
        let said = || Some("the platform said no".to_owned());
        vec![
            (
                R::SecretMissing {
                    source: "ladderback/demo".to_owned(),
                },
                ErrorCode::SecretMissing,
                "secret-missing",
                vec![field(), source()],
                None,
            ),
            (
                R::SecretUnreadable {
                    source: "ladderback/demo".to_owned(),
                    detail: said(),
                },
                ErrorCode::SecretUnreadable,
                "secret-unreadable",
                vec![field(), source()],
                said(),
            ),
            (
                R::SecretKeyringUnavailable {
                    source: "ladderback/demo".to_owned(),
                    detail: said(),
                },
                ErrorCode::SecretKeyringUnavailable,
                "secret-keyring-unavailable",
                vec![field(), source()],
                said(),
            ),
            (
                R::SecretNotText {
                    source: "ladderback/demo".to_owned(),
                },
                ErrorCode::SecretNotText,
                "secret-not-text",
                vec![field(), source()],
                None,
            ),
            (
                R::SecretTooLarge {
                    source: "ladderback/demo".to_owned(),
                    limit: 65_536,
                },
                ErrorCode::SecretTooLarge,
                "secret-too-large",
                vec![field(), source(), ("limit", MsgArg::Bytes(65_536))],
                None,
            ),
            (
                R::SecretCommandFailed {
                    source: "ladderback/demo".to_owned(),
                    detail: None,
                },
                ErrorCode::SecretCommandFailed,
                "secret-command-failed",
                vec![field(), source()],
                None,
            ),
            (
                R::SecretCommandTimedOut {
                    source: "ladderback/demo".to_owned(),
                    seconds: 30,
                },
                ErrorCode::SecretCommandTimedOut,
                "secret-command-timed-out",
                vec![field(), source(), ("seconds", MsgArg::Count(30))],
                None,
            ),
            (
                R::SecretCommandOutputOpen {
                    source: "ladderback/demo".to_owned(),
                },
                ErrorCode::SecretCommandOutputOpen,
                "secret-command-output-open",
                vec![field(), source()],
                None,
            ),
            (
                R::SecretInProcessArguments,
                ErrorCode::SecretInProcessArguments,
                "secret-in-process-arguments",
                vec![field()],
                None,
            ),
            (
                R::SecretExpansionTooLong { limit: 65_536 },
                ErrorCode::SecretExpansionTooLong,
                "secret-expansion-too-long",
                vec![field(), ("limit", MsgArg::Bytes(65_536))],
                None,
            ),
            (
                R::LocationTooManyCredentials,
                ErrorCode::LocationTooManyCredentials,
                "location-too-many-credentials",
                vec![field()],
                None,
            ),
            (
                R::RepositoryPathRelative,
                ErrorCode::RepositoryPathRelative,
                "repository-path-relative",
                vec![field()],
                None,
            ),
            (
                R::TemplateInvalid { at: Some(7) },
                ErrorCode::TemplateInvalid,
                "template-invalid",
                vec![field(), ("at", text("7"))],
                None,
            ),
            (
                R::TemplateInvalid { at: None },
                ErrorCode::TemplateInvalid,
                "template-invalid",
                vec![field()],
                None,
            ),
        ]
    }

    /// A refused run's failure is the refusal a request to start it is
    /// answered with: the same code, the same arguments, the same detail,
    /// and the log keeps it under that code (no code of its own).
    #[test]
    fn each_refusal_is_answered_and_kept_under_the_code_a_request_is_refused_with() {
        for (refusal, code, kept, args, detail) in refusals() {
            assert_eq!(
                refused("options.token", &refusal),
                (code, args, detail),
                "{refusal:?}"
            );
            assert_eq!(refusal.code(), kept, "{refusal:?}");
            let reason = FailureReason::SecretRefused {
                field: "options.token".to_owned(),
                refusal: refusal.clone(),
            };
            assert_eq!(reason.code(), kept, "{refusal:?}");
        }
    }

    /// A refused run's end is announced with the refusal's code, its
    /// arguments in the order the code declares them, and what was said
    /// last; a page shows the arguments by name and what was said as the
    /// detail.
    #[test]
    fn a_refused_runs_failure_is_announced_with_the_refusals_code_and_arguments() {
        let text = |text: &str| MsgArg::Text(text.to_owned());
        let mut operation = running();
        operation
            .fail(
                FailureReason::SecretRefused {
                    field: "password".to_owned(),
                    refusal: SettingRefusal::SecretCommandFailed {
                        source: "pass".to_owned(),
                        detail: Some("pass: no such entry".to_owned()),
                    },
                },
                t(60),
            )
            .expect("a running operation ends");
        let OperationStatus::Failed { reason } = operation.status() else {
            panic!("the operation failed: {operation:?}");
        };
        assert_eq!(
            failure(reason),
            ErrorDto::new(
                ErrorCode::SecretCommandFailed,
                [("field", text("password")), ("source", text("pass"))],
                Some("pass: no such entry".to_owned()),
            )
        );
        assert_eq!(
            end_of(&operation),
            Some(UiEvent::OperationFailed {
                operation: ID.to_owned(),
                code: ErrorCode::SecretCommandFailed,
                args: vec![text("password"), text("pass"), text("pass: no such entry")],
                finished_at: t(60).to_string(),
            })
        );
    }
}
