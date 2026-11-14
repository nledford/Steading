Feature: Restarting after a crash
  A crash, a kill or a power cut leaves operations in the log that no
  process is running. The next start ends each running one as failed, with
  the reason that the daemon stopped, before it accepts a request, and
  announces it to the notification rules for its failure; a maintenance
  operation it ended stops holding its repository. A queued backup never
  started, so the persistent queue runs it once the daemon serves, under
  its own ID. One daemon at a time serves a data folder: a second one is
  refused and touches nothing.

  # A plan's after hook that waits until it is let go holds its backup
  # running: an operation has not ended until its ending hooks have, so the
  # steps below find it running however soon the engine finishes its part.
  @e2e
  Scenario: An operation a killed daemon left running is failed by the next start
    Given Ladderback is serving on loopback with the plan "home"
    And the plan's after hook waits until it is let go
    And the daemon is running a backup of the plan "home"
    When the daemon is killed
    And Ladderback is started again on the same folders
    Then the history, read once, shows the operation failed with the code "daemon-stopped"

  @e2e
  Scenario: A backup a killed daemon left running is announced as failed by the next start
    Given a webhook listening on loopback that the rule "failures" sends "backup.failed" to
    And Ladderback is serving on loopback with the plan "home"
    And the plan's after hook waits until it is let go
    And the daemon is running a backup of the plan "home"
    When the daemon is killed
    And Ladderback is started again on the same folders
    Then the webhook is told "backup.failed" for the plan "home" with the code "daemon-stopped"

  @e2e
  Scenario: A maintenance operation a killed daemon left running no longer holds its repository
    Given Ladderback is serving on loopback with the plan "home"
    And the daemon is killed
    And the log, while the daemon is down, shows a running prune of the repository
    When Ladderback is started again on the same folders
    Then the history, read once, shows the operation failed with the code "daemon-stopped"
    And a prune of the repository is accepted

  @e2e
  Scenario: A backup a stopped daemon left queued runs after the next start
    Given Ladderback is serving on loopback with the plan "home"
    And the daemon is killed
    And the log, while the daemon is down, shows a queued backup of the plan "home"
    When Ladderback is started again on the same folders
    Then the history page shows the operation succeeded

  @e2e
  Scenario: A second daemon on a folder that is in use is refused and disturbs nothing
    Given Ladderback is serving on loopback with the plan "home"
    And the plan's after hook waits until it is let go
    And the daemon is running a backup of the plan "home"
    When a second Ladderback is started on the same folders
    Then the second Ladderback is refused, naming the data folder and the process that holds it
    And the history page shows the operation running
    When the plan's after hook is let go
    Then the history page shows the operation succeeded
