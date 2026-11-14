# Decision 71: Signing macOS builds without a Developer ID

**A build from source can be signed with a self-signed certificate the person names; a macOS archive is published only once it can be signed with a Developer ID and notarized** (recorded 2026-11-12, §12; #281, #344). Ladderback has no paid Apple Developer account, so no Developer ID certificate and no notarization.

- **M3:** on macOS, `just install` signs the installed binary with a code-signing identity from the person's own keychain when they name one, always with the same fixed identifier (`codesign -i`). Without one it installs as before, with the ad-hoc signature the linker gives every arm64 binary. The Full Disk Access checks of §12 run on the signed build.
- **M5:** release packaging publishes the Linux archives. A macOS archive waits for #344 (Developer ID signing and notarization, not scheduled); until then macOS users build from source.

Why:

- **The grant follows the signature.** TCC ties a Full Disk Access grant to the binary's designated requirement (§12). An ad-hoc signature's requirement is the binary's code hash (`cdhash H"…"`), so every rebuild is a different program to TCC and the grant is lost each time Ladderback is upgraded. A certificate's requirement is `identifier <id> and certificate leaf = H"…"`, the same for every build signed with that certificate under that identifier; checked with `codesign -d -r-` on two builds. That the grant then holds across an upgrade depends on TCC, not on `codesign`, so M3 confirms it on the current macOS release before the install guide tells anyone to rely on it. The identifier is fixed because `codesign` otherwise takes it from the file name, and a renamed copy would lose the grant.
- **Notarization matters only for downloads.** Gatekeeper assesses files that carry the quarantine attribute a browser or other download adds. A binary built on the machine has none, so a build from source needs no notarization; a downloaded archive does, and an unnotarized one is blocked unless each user overrides Gatekeeper. Hence no macOS archive without #344.
- **Nothing secret enters the build.** The identity stays in the person's keychain, and `codesign` finds it there; no certificate or password is passed to the recipe, stored in the checkout or used in CI.

Setting it up, for the install guide in M3: a certificate of the Code Signing type, made with Keychain Access's Certificate Assistant, in a keychain on the search list (normally the login keychain), unlocked. The first signing asks whether `codesign` may use the key; "Always Allow" answers it for later installs. Until then, a run that can't show the prompt, over SSH or with the keychain locked, fails to sign.

Cost: until M3, and afterwards for anyone who names no identity, each Ladderback upgrade on macOS means granting Full Disk Access again. A published macOS archive needs the paid account first.
