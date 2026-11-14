Feature: Crash recovery
  A daemon that stopped without finishing its work (a crash, a kill, a power
  cut, a restart) leaves operations the log still shows as queued or running.
  Nothing is running them any more. The next start ends each running one as
  failed because the daemon stopped: it never shows one as succeeded, and a
  maintenance operation that was running no longer holds its repository (I7).
  A queued one never started, so it is the persistent queue's: a backup of a
  plan the configuration still has, on the repository it was queued for,
  runs again under its own ID, oldest first. Anything else queued ends as
  failed because the daemon stopped, and the agent says why. Each operation
  a start ends as failed is announced as any failed operation is, to the
  notification rules for its failure, once: only after the log has its end,
  so a start that couldn't write the log announces nothing, and the next
  start finds nothing left to announce.

  Scenario: A backup the log shows as running is shown as failed
    Given the log shows a backup of the plan "home" as running
    When the daemon starts again
    Then the log shows the backup of the plan "home" as failed because the daemon stopped
    And the log never shows the backup of the plan "home" as succeeded

  Scenario: A queued backup runs again under its own ID
    Given the log shows a backup of the plan "home" as queued
    When the daemon starts again
    Then the backup of the plan "home" runs again under its own ID
    And the daemon's start ended 0 operations

  Scenario: Queued backups run again in the order they were queued
    Given the log shows a backup of the plan "home" as queued
    And the log shows a backup of the plan "docs" as queued
    When the daemon starts again
    Then the queued backups run again in the order "home", "docs"

  Scenario: A queued backup of a plan no longer configured ends as failed
    Given the log shows a backup of the plan "home" as queued
    And the configuration no longer has the plan "home"
    When the daemon starts again
    Then the log shows the backup of the plan "home" as failed because the daemon stopped
    And the agent said the queued backup of the plan "home" ended because its plan is gone
    And nothing runs again

  Scenario: A queued backup whose plan now backs up to another repository ends as failed
    Given the log shows a backup of the plan "home" on the repository "nas" as queued
    And the plan "home" now backs up to the repository "offsite"
    When the daemon starts again
    Then the log shows the backup of the plan "home" as failed because the daemon stopped
    And the agent said the queued backup of the plan "home" ended because its plan's repository changed
    And nothing runs again

  Scenario: A queued backup on the repository its plan still backs up to runs again
    Given the log shows a backup of the plan "home" on the repository "nas" as queued
    When the daemon starts again
    Then the backup of the plan "home" runs again under its own ID

  Scenario: A configuration that can't be read ends every queued backup
    Given the log shows a backup of the plan "home" as queued
    And the configuration can't be read
    When the daemon starts again
    Then the log shows the backup of the plan "home" as failed because the daemon stopped
    And the agent said the queued backup of the plan "home" ended because the configuration can't be read
    And nothing runs again

  Scenario: A queued maintenance operation ends as failed
    Given the log shows a check of the repository "nas" as queued
    When the daemon starts again
    Then the log shows the check of the repository "nas" as failed because the daemon stopped
    And the agent said the queued check of the repository "nas" ended because the log doesn't keep its job
    And nothing runs again

  Scenario: The warnings a running backup had recorded stay on it
    Given the log shows a backup of the plan "home" as running with 2 warnings recorded
    When the daemon starts again
    Then the log shows the backup of the plan "home" as failed because the daemon stopped
    And the log still shows 2 warnings on the backup of the plan "home"

  Scenario Outline: A running maintenance operation lets go of its repository
    Given the log shows a <kind> of the repository "nas" as running
    When the daemon starts again
    Then the log shows the <kind> of the repository "nas" as failed because the daemon stopped
    And the repository "nas" is free for a prune and for a check

    Examples:
      | kind   |
      | prune  |
      | check  |
      | forget |
      | adopt  |

  Scenario Outline: An operation that ended before the daemon stopped keeps its outcome
    Given the log shows a backup of the plan "home" that ended as <ending>
    And the log shows a backup of the plan "docs" as running
    When the daemon starts again
    Then the log still shows the backup of the plan "home" as <ending>
    And the log shows the backup of the plan "docs" as failed because the daemon stopped

    Examples:
      | ending                                       |
      | succeeded                                    |
      | succeeded with warnings                      |
      | failed because the repository was unreachable |
      | cancelled                                    |

  Scenario: A log whose operations all ended has nothing to recover
    Given the log shows a backup of the plan "home" that ended as succeeded
    And the log shows a backup of the plan "docs" that ended as cancelled
    When the daemon starts again
    Then the daemon's start ended 0 operations

  Scenario: An empty log has nothing to recover
    Given the log shows no operation at all
    When the daemon starts again
    Then the daemon's start ended 0 operations

  Scenario: Starting again after recovery ends nothing more
    Given the log shows a backup of the plan "home" as running
    When the daemon starts again
    And the daemon starts again
    Then the daemon's first start ended 1 operation
    And the daemon's start ended 0 operations
    And the log shows the backup of the plan "home" as failed because the daemon stopped

  Scenario: Every running operation is ended, and counted
    Given the log shows a backup of the plan "home" as queued
    And the log shows a backup of the plan "docs" as running
    And the log shows a check of the repository "nas" as running
    And the log shows a backup of the plan "photos" that ended as succeeded
    When the daemon starts again
    Then the daemon's start ended 2 operations
    And the backup of the plan "home" runs again under its own ID
    And the log shows the backup of the plan "docs" as failed because the daemon stopped
    And the log shows the check of the repository "nas" as failed because the daemon stopped
    And the log still shows the backup of the plan "photos" as succeeded

  Scenario: A queued backup outlasts a start on a log that cannot be written
    Given the log shows a backup of the plan "home" as queued
    When the daemon starts again on a log that cannot be written
    Then the daemon's start reports that the log could not be written
    And nothing runs again
    When the daemon starts again
    Then the backup of the plan "home" runs again under its own ID

  Scenario: A log that cannot be written leaves the operation running
    Given the log shows a backup of the plan "home" as running
    When the daemon starts again on a log that cannot be written
    Then the daemon's start reports that the log could not be written
    And the log still shows the backup of the plan "home" as running

  Scenario: A backup the daemon stopped during is announced as failed
    Given a webhook channel "ops"
    And a rule "failures" that sends "backup.failed" to "ops"
    And the log shows a backup of the plan "home" as running
    When the daemon starts again
    Then "ops" is sent "backup.failed" for the plan "home" with the code "daemon-stopped"

  Scenario: A maintenance operation the daemon stopped during is announced as failed
    Given a webhook channel "ops"
    And a rule "failures" that sends "check.failed" to "ops"
    And the log shows a check of the repository "nas" as running
    When the daemon starts again
    Then "ops" is sent "check.failed" for the repository "nas" with the code "daemon-stopped"

  Scenario: A queued backup that can't run again is announced as failed
    Given a webhook channel "ops"
    And a rule "failures" that sends "backup.failed" to "ops"
    And the log shows a backup of the plan "home" as queued
    And the configuration no longer has the plan "home"
    When the daemon starts again
    Then "ops" is sent "backup.failed" for the plan "home" with the code "daemon-stopped"

  Scenario: An operation a start ended is announced once, however often the daemon starts again
    Given a webhook channel "ops"
    And a rule "failures" that sends "backup.failed" to "ops"
    And the log shows a backup of the plan "home" as running
    When the daemon starts again
    And the daemon starts again
    Then "ops" is sent "backup.failed" for the plan "home" with the code "daemon-stopped"

  Scenario: Only what a start ends is announced
    Given a webhook channel "ops"
    And a rule "failures" that sends "backup.failed, check.failed" to "ops"
    And the log shows a backup of the plan "home" that ended as failed because the repository was unreachable
    And the log shows a backup of the plan "docs" as queued
    And the log shows a backup of the plan "photos" as running
    When the daemon starts again
    Then "ops" is sent "backup.failed" for the plan "photos" with the code "daemon-stopped"

  Scenario: A start that couldn't write the log announces nothing, and the next start announces the end
    Given a webhook channel "ops"
    And a rule "failures" that sends "backup.failed" to "ops"
    And the log shows a backup of the plan "home" as running
    When the daemon starts again on a log that cannot be written
    Then no channel is sent anything
    When the daemon starts again
    Then "ops" is sent "backup.failed" for the plan "home" with the code "daemon-stopped"
