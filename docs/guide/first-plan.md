# A first plan

A plan says what to back up, into which repository, when, and how long to keep its snapshots. A plan backs up to a repository by its name, so make one in this order:

1. **Connect a repository** on the Repositories page (the link at the top of every page): fill in its location and password, test it, give it a name such as `nas`, and connect it, or create a new one there. To have this machine forget old snapshots by your plans' keep policies, turn on "Maintain this repository from this agent", unless another machine already maintains it.
2. **Add a plan to `config.toml`**, in Ladderback's config folder: `~/.config/ladderback` on Linux, `~/Library/Application Support/ladderback` on macOS (`$XDG_CONFIG_HOME/ladderback` on Linux when that is set, and `$LADDERBACK_CONFIG_DIR` on either when that is set). The Repositories page wrote the repository there; add a `[[plans]]` table after it. Ladderback reads the file again by itself, with no restart, and its start page lists the plan with its next run.

This is a whole `config.toml` with one plan, which backs up a home folder every hour and leaves out caches and game libraries; on macOS, change `/home/demo` to `/Users/demo` in `iglobs` too:

```toml
schema = 1

# The repository, as the Repositories page wrote it, with its password moved
# out of this file (see the secrets guide). Yours may have more lines, such as its
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

A run the machine slept through runs when it wakes. Ladderback doesn't forget on a schedule yet: the keep policy takes effect when you press "Forget snapshots" on the Snapshots page, on a repository this machine maintains. Every key a plan takes, among them tags, a time zone, retries and hooks, is in [technical design §6.1](../design/scheduling-and-config.md#61-source-of-truth); the filter and `keep-*` keys are rustic's own.
