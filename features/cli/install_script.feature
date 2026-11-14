@e2e
Feature: Install Ladderback with one command
  Until releases come with binaries (decision 54), a person installs
  Ladderback by building it from source. The install script does that in one
  command, piped from the web into bash (README, "Install"; decision
  79): it checks the tools the build needs, installs the ones that need no
  administrator rights after asking once, keeps Ladderback's source in a
  folder of its own, checks out the newest release and installs it. It
  never runs sudo, and running it again updates to the newest release.

  In the examples the home folder is a scratch folder, Ladderback's source
  comes from a local copy with releases of its own, the script is read from
  standard input as `curl … | bash` gives it, and every tool the build needs
  is a stand-in that keeps a list of how it was called: nothing is
  downloaded or built, and no real terminal is asked anything.

  Background:
    Given Ladderback has released "v0.1.0", "v0.2.0" and "v0.3.0-rc.1"
    And every tool the build needs is installed

  Rule: It installs the newest release that isn't a release candidate

    Scenario: A first install fetches the source and installs the newest release
      When the owner runs the install script with "-- --root {home}/apps"
      Then the command exits with status 0
      And the source folder holds the release "v0.2.0"
      And the release was installed with "--root {home}/apps"
      And standard output says "ladderback 0.2.0"
      And standard output says "ladderback serve"
      And standard output says "ladderback install-service"
      And nothing ran sudo or a package manager

    Scenario: Running it again updates to the newest release
      Given the owner has run the install script, and its build rewrote the web UI's stylesheet
      And Ladderback has since released "v0.2.1"
      When the owner runs the install script
      Then the command exits with status 0
      And the source was updated, not fetched again in full
      And the source folder holds the release "v0.2.1"
      And standard output says "ladderback 0.2.1"

    Scenario: The source folder is in the data folder XDG_DATA_HOME names
      Given XDG_DATA_HOME is "{home}/data"
      When the owner runs the install script
      Then the command exits with status 0
      And the folder "{home}/data/ladderback/source" holds the release "v0.2.0"

    Scenario: With only release candidates there is nothing to install
      Given Ladderback's only releases are release candidates
      When the owner runs the install script
      Then the command exits with status 1
      And standard error says "no release"
      And no release was installed

  Rule: It never installs what needs administrator rights, and says how

    Scenario Outline: A missing C compiler is named with the command that installs it
      Given the system is <system>
      And there is no C compiler
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "<command>"
      And nothing was installed, fetched or built
      And nothing ran sudo or a package manager

      Examples:
        | system           | command                                             |
        | macOS            | xcode-select --install                              |
        | Debian or Ubuntu | sudo apt install build-essential                    |
        | Fedora           | sudo dnf install gcc                                |
        | Arch Linux       | sudo pacman -S --needed base-devel                  |
        | another Linux    | install gcc or clang with your system's package manager |

    Scenario Outline: Node.js older than 20 is named with the command that installs a newer one
      Given the system is <system>
      And Node.js is version "18.19.0"
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "found 18.19.0"
      And standard error says "<command>"
      And nothing was installed, fetched or built
      And nothing ran sudo or a package manager

      Examples:
        | system              | command                            |
        | macOS with Homebrew | brew install node                  |
        | macOS               | https://nodejs.org                 |
        | Debian or Ubuntu    | sudo apt install nodejs npm        |
        | Fedora              | sudo dnf install nodejs npm        |
        | Arch Linux          | sudo pacman -S --needed nodejs npm |
        | another Linux       | https://nodejs.org                 |

    Scenario: Missing git and Node.js are named together
      Given the system is Debian or Ubuntu
      And there is no git
      And there is no Node.js
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "sudo apt install git"
      And standard error says "sudo apt install nodejs npm"
      And nothing was installed, fetched or built

    Scenario: On macOS without the Command Line Tools, the compiler and git macOS ships don't count
      Given the system is macOS
      And Apple's Command Line Tools aren't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "xcode-select --install"
      And nothing was installed, fetched or built
      And nothing ran sudo or a package manager

  Rule: It installs what needs no administrator rights after asking once

    Scenario: Missing just and trunk are built with the stable Rust
      Given just and trunk aren't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And the tools were installed with "cargo +stable install --locked just trunk"
      And the source folder holds the release "v0.2.0"
      And the release was installed

    Scenario: Only the missing tool is installed
      Given trunk isn't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And the tools were installed with "cargo +stable install --locked trunk"

    Scenario: A just older than 1.40 is replaced
      Given just is version "1.39.0"
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And the tools were installed with "cargo +stable install --locked just"
      And the release was installed

    Scenario: Rust is installed with rustup's own installer, which asks nothing under --yes
      Given Rust isn't installed
      And just and trunk aren't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And rustup's installer ran with "-y"
      And the tools were installed with "cargo +stable install --locked just trunk"
      And the release was installed

    Scenario: Asked at the terminal, the owner agrees once and rustup asks its own questions
      Given Rust isn't installed
      And the owner will answer "y" at the terminal
      When the owner runs the install script
      Then the command exits with status 0
      And standard error says "Rust, with rustup's installer from https://sh.rustup.rs"
      And rustup's installer ran, asking its own questions
      And the release was installed

    Scenario: Answering no installs nothing
      Given just isn't installed
      And the owner will answer "n" at the terminal
      When the owner runs the install script
      Then the command exits with status 1
      And standard error says "Nothing was installed"
      And nothing was installed, fetched or built

    Scenario: With no terminal to ask and no --yes, it refuses
      Given just isn't installed
      And there is no terminal
      When the owner runs the install script
      Then the command exits with status 1
      And standard error says "--yes"
      And nothing was installed, fetched or built

    Scenario: rustup's Rust is used even behind the system's own Rust on the PATH
      Given rustup's Rust is installed in the home folder, behind the system's own Rust on the PATH
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And the release was installed
      And the system's own Rust wasn't used

    Scenario: Without rustup, a Rust older than the release needs is refused
      Given Rust "1.99.0" is installed without rustup
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "Rust 1.99.0 is older than 1.100.0"
      And no release was installed

    Scenario: Without rustup, a Rust without the WebAssembly target is refused
      Given Rust "1.100.0" is installed without rustup or the WebAssembly target
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "wasm32-unknown-unknown"
      And no release was installed

    Scenario: Without rustup, the Rust the owner's environment names is left as it is
      Given Rust "1.100.0" is installed without rustup
      And the owner's environment sets RUSTC to "/opt/rust-1.100.0/bin/rustc"
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And the release was built with "/opt/rust-1.100.0/bin/rustc", the Rust the owner's environment names
      And standard output doesn't say "Ignoring"

    Scenario: Without rustup, a missing just is named with where to get it
      Given Rust "1.100.0" is installed without rustup
      And just isn't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 1
      And standard error says "https://github.com/casey/just/releases"
      And nothing was installed, fetched or built

  Rule: With rustup, it builds with the Rust the release pins
    A release names the Rust it builds with in `rust-toolchain.toml`
    (decision 78). A variable in the owner's environment that names another
    Rust would outrank it, so the script leaves those out of the build and
    says so; the owner's shell keeps them. The pinned Rust is installed
    before the build when rustup doesn't have it, rather than left to
    rustup's automatic install, which rustup has deprecated.

    Scenario Outline: A Rust the owner's environment names is ignored, and the script says so
      Given the owner's environment sets <variable> to "<value>"
      When the owner runs the install script
      Then the command exits with status 0
      And standard output says "Ignoring <variable>=<value>"
      And the release was built with the Rust it pins, "1.100.0"

      Examples:
        | variable          | value                      |
        | RUSTUP_TOOLCHAIN  | 1.99.1                     |
        | RUSTC             | /opt/rust-1.99.1/bin/rustc |
        | CARGO_BUILD_RUSTC | /opt/rust-1.99.1/bin/rustc |

    Scenario: The Rust the release pins is installed before the build when rustup doesn't have it
      Given rustup doesn't have the Rust the release pins
      When the owner runs the install script
      Then the command exits with status 0
      And the Rust the release pins was installed, with the components and targets it lists, before the release

    Scenario Outline: The Rust the release pins gets <part> it lacks
      Given rustup has the Rust the release pins without <part>
      When the owner runs the install script
      Then the command exits with status 0
      And the Rust the release pins was installed, with the components and targets it lists, before the release

      Examples:
        | part                   |
        | the WebAssembly target |
        | Clippy                 |

    Scenario: Running it again doesn't install the Rust the release pins again
      Given rustup doesn't have the Rust the release pins
      And the owner has run the install script, and its build rewrote the web UI's stylesheet
      When the owner runs the install script
      Then the command exits with status 0
      And the Rust the release pins was installed by the first run only

    Scenario: wasm-bindgen is built with the Rust the release pins too
      Given the system is Linux on "aarch64" with glibc "2.36"
      And the owner's environment sets RUSTUP_TOOLCHAIN to "1.99.1"
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And wasm-bindgen was built with the Rust the release pins, "1.100.0"

    Scenario: A release that pins no Rust is refused
      Given Ladderback has since released "v0.2.1", which pins no Rust
      When the owner runs the install script
      Then the command exits with status 1
      And standard error says "names no Rust version"
      And no release was installed

  Rule: It keeps the source in a folder of its own and never deletes one

    Scenario: A folder that holds something else is refused and left as it was
      Given the folder "{home}/projects" holds a file "notes.txt"
      When the owner runs the install script with "--yes --dir {home}/projects"
      Then the command exits with status 1
      And standard error says "isn't a clone of Ladderback"
      And the folder "{home}/projects" holds only "notes.txt", unchanged
      And nothing was installed, fetched or built

    Scenario: A clone of another project is refused
      Given the folder "{home}/other" is a clone of another project
      When the owner runs the install script with "--yes --dir {home}/other"
      Then the command exits with status 1
      And standard error says "isn't a clone of Ladderback"
      And nothing was installed, fetched or built

    Scenario: A folder given as --dir=~/… is in the home folder
      When the owner runs the install script with "--yes --dir=~/ladderback"
      Then the command exits with status 0
      And the folder "{home}/ladderback" holds the release "v0.2.0"

    Scenario: A source folder with changes is refused and the changes are kept
      Given the owner has run the install script, and its build rewrote the web UI's stylesheet
      And a file in the source folder has been changed
      When the owner runs the install script
      Then the command exits with status 1
      And standard error says "changes"
      And the change in the source folder is still there
      And no release was installed

    Scenario: A build that failed after rewriting the web UI's stylesheet can be run again
      Given the owner's install failed after its build rewrote the web UI's stylesheet
      When the owner runs the install script
      Then the command exits with status 0
      And the release was installed

    Scenario: A release that tracks the web UI's stylesheet gets it back after the build
      When the owner runs the install script
      Then the command exits with status 0
      And the source folder has no changes
      And the web UI's stylesheet in the source folder is the release's

    Scenario: A release that leaves the web UI's stylesheet to its build keeps the one it built
      Given the owner has run the install script, and its build rewrote the web UI's stylesheet
      And Ladderback has since released "v0.2.1", which leaves the web UI's stylesheet to its build
      When the owner runs the install script
      Then the command exits with status 0
      And the source folder holds the release "v0.2.1"
      And the source folder has no changes
      And the web UI's stylesheet in the source folder is the one its build wrote

  Rule: On 64-bit ARM Linux with a glibc older than 2.38 it builds wasm-bindgen first

    Scenario: An older glibc on 64-bit ARM builds wasm-bindgen before the release
      Given the system is Linux on "aarch64" with glibc "2.36"
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And wasm-bindgen was built before the release was installed

    Scenario Outline: Elsewhere wasm-bindgen is not built
      Given the system is Linux on "<machine>" with glibc "<glibc>"
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And wasm-bindgen was not built

      Examples:
        | machine | glibc |
        | aarch64 | 2.38  |
        | x86_64  | 2.36  |

  Rule: It says what comes next, and how it is used

    Scenario: On macOS it says to grant Full Disk Access
      Given the system is macOS
      When the owner runs the install script
      Then the command exits with status 0
      And standard output says "Full Disk Access"

    Scenario: After installing Rust, it says to open a new terminal
      Given Rust isn't installed
      When the owner runs the install script with "--yes"
      Then the command exits with status 0
      And standard output says "Open a new terminal"

    Scenario: Help says how it is used
      When the owner runs the install script with "--help"
      Then the command exits with status 0
      And standard output says "Usage:"
      And nothing was installed, fetched or built

    Scenario: An unknown option is refused with the usage
      When the owner runs the install script with "--frobnicate"
      Then the command exits with status 2
      And standard error says "Usage:"
      And nothing was installed, fetched or built
