# Sign-in and remote access

## Sign-in

Sign-in is off until you set an admin password with `ladderback set-password`, which turns it on the next time Ladderback starts (for the service, run `ladderback install-service` again). With sign-in off, anyone with an account on the machine can use Ladderback, so set a password on a machine others use.

## Listening beyond loopback

`ladderback serve --listen` takes an address beyond loopback only with at least one `--allowed-host`, the name the browser reaches Ladderback by. With sign-in off it then listens too, as Backrest does, and warns at start and on every page that **anyone who can reach that address can run and change backups**: do that only on a network you trust.

It serves plain HTTP, so with sign-in on put a reverse proxy with TLS in front of it. [Technical design §10.1](../design/security.md#101-web-ui) has the details.
