# Moving a repository's password out of config.toml

The Repositories page saves the password you type into `config.toml`, and the page and the log warn that it is there. To move it, keep it in one of these places, then replace the repository's `password = "…"` line with the line that names it; a repository has exactly one:

- **The system keyring**, on a desktop or laptop. Store the password under the service `ladderback` and the repository's name; each command asks for it, so it stays out of your shell's history:

  ```sh
  # macOS: the login keychain
  security add-generic-password -s ladderback -a nas -w
  # Linux: the Secret Service, such as GNOME Keyring
  secret-tool store --label="Ladderback: nas" service ladderback username nas
  ```

  Then write `password-keyring = "ladderback/nas"`, the service and the name.
- **A password file** that only you can read. Ladderback reads its first line, and warns when others can read it:

  ```sh
  touch ~/.config/ladderback/nas.password && chmod 600 ~/.config/ladderback/nas.password
  ```

  Write the password on its first line with an editor, then write `password-file = "/home/demo/.config/ladderback/nas.password"`: the path is absolute, since nothing expands `~`. On macOS, keep the file in the config folder there, and quote the path, since it holds a space:

  ```sh
  touch "$HOME/Library/Application Support/ladderback/nas.password" && chmod 600 "$HOME/Library/Application Support/ladderback/nas.password"
  ```
- **An environment variable**: `password-env = "NAS_PASSWORD"`. The service doesn't get your shell's variables, so this suits `ladderback serve` started by hand, or a container.

The copies of earlier versions of `config.toml` in the `history` folder beside it may still hold the password: delete those that do. [Technical design §10.2](../design/security.md#102-repository-secrets) has every source, a password command among them.
