#!/usr/bin/env node
// Smoke-tests a release binary of Ladderback (technical design §12.1): what
// `just release-build` and the Release builds workflow
// (.github/workflows/release-builds.yml) make. Nothing here publishes anything.
//
// Checks on the file:
//   - its dynamic libraries are the base system's only: glibc's (libc, libm,
//     libpthread, libdl, librt, libutil, the loader) and libgcc_s on Linux,
//     read from the ELF file itself; `/usr/lib` and `/System/Library` on macOS,
//     from `otool -L`. The highest `GLIBC_` symbol version the binary needs,
//     its glibc floor, is reported, and fails above 2.35 (Ubuntu 22.04's);
//   - it has no pseudolocales: fluent-pseudo's table of accented letters
//     isn't in it.
// Checks of the program, here or, with `--container`, in a clean Linux
// container (Docker, no network, the binary copied in):
//   - `--version` names the build of this checkout (scripts/build-identity.ts):
//     the workspace version alone at its release tag, as cargo-dist builds a
//     release, and with the commit between releases (decision 90), as the
//     nightly and pull request builds are; `--version <x.y.z>` expects that
//     text instead;
//   - with `LANGUAGE=en-XA` it speaks plain English: a release build has no
//     pseudolocales (§8.4);
//   - `serve` starts in scratch folders (LADDERBACK_CONFIG_DIR,
//     LADDERBACK_DATA_DIR, LADDERBACK_CACHE_DIR) with no D-Bus session, and
//     `GET /` answers 200 with the embedded web UI, whose wasm has neither the
//     pseudolocales nor the component gallery (§4.5);
//   - a scratch local repository initialised through `/api/v1`, and one backup
//     of a folder of demo files into it, which succeeds and leaves one snapshot.
// The results go to standard output as Markdown, and are appended to the file
// `--summary` names, or to $GITHUB_STEP_SUMMARY in a GitHub Actions job.
//
// Usage: node scripts/smoke-release.ts <binary> [--container <name>|all]...
//          [--version <x.y.z>] [--summary <file>]
// Exit status: 0 when every check passed, 1 when one failed, 2 on bad usage.
// Needs Docker for `--container`, and `otool` for a macOS binary.

import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { appendFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { connect } from "node:net";
import { platform, tmpdir } from "node:os";
import path from "node:path";

import { checkoutBuild, shown, workspaceVersion } from "./build-identity.ts";

const ROOT = path.resolve(import.meta.dirname, "..");

// The clean containers a Linux build runs in, pinned by digest so that a run
// tests what the last one did; a multi-arch index, so the same pin serves
// x86_64 and aarch64. To move one, take the digest from `docker buildx
// imagetools inspect <image>` and record the tag beside it.
const IMAGES: Record<string, string> = {
  "debian-12": "debian:12@sha256:268fb24c39bca92d326dd83ea3abebe4a3f7751c804ffef23bb6d857dd343ff8",
  "ubuntu-22.04": "ubuntu:22.04@sha256:076e7bf82319619f28dd0aa9c02ab34b055ad37100395b4f49a2ffba04fb5e11",
  "ubuntu-24.04": "ubuntu:24.04@sha256:fe81560990fae86dc4743202b2cb4d3df98d0bcf949173cecef15ce1127ef852",
  fedora: "fedora:latest@sha256:a7d72ba2745b100a3351087472e5ee7b2450af7cf331e7673007e3817612839c",
};

// fluent-pseudo's table of the accented letters `en-XA` writes for a to z, as
// the bytes of a Rust `[char]`: each letter a 32-bit little-endian number, as
// x86_64, aarch64 and wasm32 all store it. Only that crate puts it in a
// binary, and so only a build with the pseudolocales has it: the debug build
// and `just web-test`'s wasm do. (Its regular expressions are no marker: the
// transform l10n calls uses `[a-zA-Z]` only, which anything may contain.)
const PSEUDO_MARKER = Buffer.concat(
  [..."aƀƈḓeƒɠħiĵķŀḿƞoƥɋřşŧuṽẇẋẏẑ"].map((letter) => {
    const bytes = Buffer.alloc(4);
    bytes.writeUInt32LE(letter.codePointAt(0) ?? 0);
    return bytes;
  }),
);
// An element ID only the component gallery's page has (crates/web/src/gallery.rs).
const GALLERY_MARKER = "gallery-review-body";

// The libraries every glibc system has, by soname: glibc's own and the
// loader, and libgcc_s, which glibc itself needs to unwind.
const LINUX_BASE = /^(?:libc|libm|libpthread|libdl|librt|libutil|libgcc_s)\.so\.\d+$|^ld-linux(?:-[\w-]+)?\.so\.\d+$/;
// macOS's own libraries and frameworks: the sealed system volume.
const MACOS_BASE = /^\/(?:usr\/lib|System\/Library)\//;
// The newest glibc a Linux build may need: Ubuntu 22.04's, which the release
// builds run on (§12.1). Debian 12 has 2.36, so both run what passes.
const GLIBC_CEILING = "2.35";

// A canary, not a secret: the scratch repository's password.
const PASSWORD = "smoke-scratch-password";
const PLAN = "smoke";

const COMMAND_LIMIT_MS = 60_000;
const LISTEN_LIMIT_MS = 60_000;
const BACKUP_LIMIT_MS = 180_000;
const DOCKER_LIMIT_MS = 300_000;

// ---------------------------------------------------------------------------
// Results

type Result = { check: string; ok: boolean; detail: string };

class Report {
  results: Result[] = [];
  notes: string[] = [];

  add(check: string, ok: boolean, detail: string): boolean {
    this.results.push({ check, ok, detail });
    return ok;
  }

  note(markdown: string): void {
    this.notes.push(markdown);
  }

  get failed(): boolean {
    return this.results.some((result) => !result.ok);
  }

  markdown(title: string): string {
    const cell = (text: string) => text.replace(/\|/g, "\\|").replace(/\r?\n/g, " ");
    const rows = this.results.map(
      (result) => `| ${result.ok ? "pass" : "**FAIL**"} | ${cell(result.check)} | ${cell(result.detail)} |`,
    );
    return [`### ${title}`, "", "| Result | Check | Detail |", "|---|---|---|", ...rows, "", ...this.notes, ""].join(
      "\n",
    );
  }
}

const details = (summary: string, body: string) =>
  ["<details>", `<summary>${summary}</summary>`, "", "```", body.trimEnd(), "```", "", "</details>", ""].join("\n");

// ---------------------------------------------------------------------------
// Processes

type Output = { status: number | null; stdout: Buffer; stderr: string };

/** Runs `command` to the end, or kills it after `limitMs`; never throws. */
function run(command: string, args: string[], options: { input?: Buffer | string; env?: NodeJS.ProcessEnv; limitMs?: number } = {}): Promise<Output> {
  const { promise, resolve } = Promise.withResolvers<Output>();
  const child = spawn(command, args, { env: options.env ?? process.env, stdio: ["pipe", "pipe", "pipe"] });
  const stdout: Buffer[] = [];
  let stderr = "";
  const timer = setTimeout(() => child.kill("SIGKILL"), options.limitMs ?? COMMAND_LIMIT_MS);
  child.stdout.on("data", (chunk: Buffer) => stdout.push(chunk));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  child.on("error", (error) => {
    clearTimeout(timer);
    resolve({ status: null, stdout: Buffer.alloc(0), stderr: `${stderr}${error.message}` });
  });
  child.on("close", (status) => {
    clearTimeout(timer);
    resolve({ status, stdout: Buffer.concat(stdout), stderr });
  });
  child.stdin.on("error", () => {});
  child.stdin.end(options.input ?? "");
  return promise;
}

/** A long-running program: what it has said on stderr so far, and a way to stop it. */
type Daemon = { stderr: () => string; exited: () => boolean; stop: () => Promise<void> };

function startDaemon(command: string, args: string[], env: NodeJS.ProcessEnv, stop: (pid: string) => Promise<void>): Daemon {
  const child = spawn(command, args, { env, stdio: ["ignore", "pipe", "pipe"] });
  let stderr = "";
  let stdout = "";
  let done = false;
  child.stdout.on("data", (chunk: Buffer) => (stdout += chunk.toString()));
  child.stderr.on("data", (chunk: Buffer) => (stderr += chunk.toString()));
  const closed = Promise.withResolvers<void>();
  child.on("close", () => {
    done = true;
    closed.resolve();
  });
  child.on("error", (error) => (stderr += error.message));
  return {
    stderr: () => stderr,
    exited: () => done,
    stop: async () => {
      if (!done) {
        // The first line of stdout is the daemon's process ID in the place it
        // runs: each `spawn` starts it through `sh -c 'echo "$$"; exec …'`.
        await stop(stdout.split("\n")[0]?.trim() ?? "");
        const killer = setTimeout(() => child.kill("SIGKILL"), 15_000);
        await closed.promise;
        clearTimeout(killer);
      }
    },
  };
}

// ---------------------------------------------------------------------------
// Where the program runs: on this machine, or in a container

type Env = Record<string, string>;
// The variables a daemon must not inherit: a locale that would decide its
// language before `LANGUAGE` does, and a D-Bus session, which a server has none of.
const UNSET = ["LC_ALL", "LC_MESSAGES", "LANG", "LANGUAGE", "DBUS_SESSION_BUS_ADDRESS"];

interface Place {
  name: string;
  binary: string;
  scratch: string;
  run(args: string[], env: Env): Promise<Output>;
  spawn(args: string[], env: Env): Daemon;
  mkdir(dir: string): Promise<void>;
  /** `secret`: readable by its owner only, as a password file should be. */
  writeFile(file: string, content: string, secret?: boolean): Promise<void>;
  /** Sends one raw HTTP request to 127.0.0.1:`port` and returns the raw response. */
  exchange(port: number, request: Buffer): Promise<Buffer>;
  describe(): Promise<string>;
  close(): Promise<void>;
}

function nativePlace(binary: string): Place {
  const scratch = mkdtempSync(path.join(tmpdir(), "ladderback-smoke-"));
  const environment = (env: Env) => {
    const merged: NodeJS.ProcessEnv = { ...process.env };
    for (const name of UNSET) delete merged[name];
    return { ...merged, ...env };
  };
  return {
    name: `this machine (${platform()})`,
    binary,
    scratch,
    run: (args, env) => run(binary, args, { env: environment(env) }),
    spawn: (args, env) =>
      startDaemon("sh", ["-c", 'echo "$$"; exec "$0" "$@"', binary, ...args], environment(env), async (pid) => {
        if (/^\d+$/.test(pid)) process.kill(Number(pid), "SIGTERM");
      }),
    mkdir: async (dir) => void mkdirSync(dir, { recursive: true }),
    writeFile: async (file, content, secret) => writeFileSync(file, content, secret ? { mode: 0o600 } : {}),
    exchange: (port, request) => {
      const { promise, resolve, reject } = Promise.withResolvers<Buffer>();
      const chunks: Buffer[] = [];
      // `write`, not `end`: hyper closes a connection whose read side closes
      // before it answers. `Connection: close` ends it after the answer.
      const socket = connect(port, "127.0.0.1", () => socket.write(request));
      socket.setTimeout(COMMAND_LIMIT_MS, () => socket.destroy(new Error("no answer in time")));
      socket.on("data", (chunk: Buffer) => chunks.push(chunk));
      socket.on("end", () => resolve(Buffer.concat(chunks)));
      socket.on("error", reject);
      return promise;
    },
    describe: async () => {
      if (platform() !== "linux") return `${platform()} ${process.arch}`;
      const ldd = await run("ldd", [binary]);
      return `ldd ${binary}:\n${ldd.stdout.toString()}${ldd.stderr}`;
    },
    close: async () => rmSync(scratch, { recursive: true, force: true }),
  };
}

async function docker(args: string[], input?: Buffer | string): Promise<Output> {
  return run("docker", args, { input, limitMs: DOCKER_LIMIT_MS });
}

async function containerPlace(name: string, image: string, binary: string): Promise<Place> {
  const container = `ladderback-smoke-${name.replace(/[^a-z0-9]+/g, "-")}-${process.pid}`;
  // No network: the daemon needs only loopback, and a clean system's own
  // libraries. The container's first process waits; everything runs beside it.
  const started = await docker(["run", "--detach", "--rm", "--network", "none", "--name", container, image, "sleep", "infinity"]);
  if (started.status !== 0) throw new Error(`docker run ${image}: ${started.stderr.trim()}`);
  const inside = "/usr/local/bin/ladderback";
  const copied = await docker(["cp", binary, `${container}:${inside}`]);
  if (copied.status !== 0) {
    await docker(["rm", "--force", container]);
    throw new Error(`docker cp: ${copied.stderr.trim()}`);
  }
  // `env` clears the variables a daemon must not inherit, then sets the ones
  // asked for, in that order: a `--env` of `docker exec` would come before it
  // and be cleared too.
  const command = (env: Env) => [
    "exec",
    container,
    "env",
    ...UNSET.flatMap((variable) => ["-u", variable]),
    ...Object.entries(env).map(([key, value]) => `${key}=${value}`),
  ];
  return {
    name: `${name} (${image.split("@")[0]})`,
    binary: inside,
    scratch: "/smoke",
    run: (args, env) => docker([...command(env), inside, ...args]),
    spawn: (args, env) =>
      startDaemon("docker", [...command(env), "sh", "-c", 'echo "$$"; exec "$0" "$@"', inside, ...args], process.env, async (pid) => {
        // The shell's own `kill`: Debian's and Ubuntu's images have no
        // /bin/kill (procps).
        if (!/^\d+$/.test(pid)) return;
        const stopped = await docker(["exec", container, "sh", "-c", 'kill -TERM "$0"', pid]);
        if (stopped.status !== 0) throw new Error(`kill ${pid}: ${stopped.stderr.trim()}`);
      }),
    mkdir: async (dir) => {
      const made = await docker(["exec", container, "mkdir", "-p", dir]);
      if (made.status !== 0) throw new Error(`mkdir ${dir}: ${made.stderr.trim()}`);
    },
    writeFile: async (file, content, secret) => {
      const write = secret ? 'umask 077 && cat > "$0"' : 'cat > "$0"';
      const written = await docker(["exec", "--interactive", container, "sh", "-c", write, file], content);
      if (written.status !== 0) throw new Error(`writing ${file}: ${written.stderr.trim()}`);
    },
    // bash's /dev/tcp: a clean image has no HTTP client, and a container
    // without a network can't be reached from outside it. The writer's
    // `<&0` is needed: bash -c gives a background command /dev/null for
    // stdin unless told otherwise, and the request would never be sent.
    exchange: async (port, request) => {
      const sent = await docker(
        ["exec", "--interactive", container, "bash", "-c", 'exec 3<>"/dev/tcp/127.0.0.1/$0" || exit 1; cat <&0 >&3 & cat <&3', String(port)],
        request,
      );
      if (sent.status !== 0 && sent.stdout.length === 0) throw new Error(`no connection: ${sent.stderr.trim()}`);
      return sent.stdout;
    },
    describe: async () => {
      const shown = await docker([
        "exec",
        container,
        "sh",
        "-c",
        '. /etc/os-release; echo "$PRETTY_NAME"; ldd --version 2>&1 | head -n 1; echo; echo "ldd $0:"; ldd "$0" 2>&1',
        inside,
      ]);
      return `${shown.stdout.toString()}${shown.stderr}`;
    },
    close: async () => void (await docker(["rm", "--force", container])),
  };
}

// ---------------------------------------------------------------------------
// HTTP, as the daemon's request checks want it: its own Host and Origin

type Answer = { status: number; headers: Map<string, string>; body: Buffer };

function request(method: string, address: string, target: string, body = ""): Buffer {
  const head = [
    `${method} ${target} HTTP/1.1`,
    `Host: ${address}`,
    `Origin: http://${address}`,
    "Accept: */*",
    "Connection: close",
    ...(method === "GET" ? [] : ["Content-Type: application/json", `Content-Length: ${Buffer.byteLength(body)}`]),
  ];
  return Buffer.from(`${head.join("\r\n")}\r\n\r\n${body}`);
}

function parseAnswer(raw: Buffer): Answer {
  const end = raw.indexOf("\r\n\r\n");
  if (end < 0) throw new Error(`not an HTTP answer: ${JSON.stringify(raw.subarray(0, 200).toString())}`);
  const [statusLine = "", ...lines] = raw.subarray(0, end).toString("latin1").split("\r\n");
  const status = Number(statusLine.split(" ")[1]);
  const headers = new Map<string, string>();
  for (const line of lines) {
    const colon = line.indexOf(":");
    if (colon > 0) headers.set(line.slice(0, colon).trim().toLowerCase(), line.slice(colon + 1).trim());
  }
  let body = raw.subarray(end + 4);
  if (headers.get("transfer-encoding")?.toLowerCase() === "chunked") {
    const parts: Buffer[] = [];
    let at = 0;
    for (;;) {
      const lineEnd = body.indexOf("\r\n", at);
      if (lineEnd < 0) break;
      const size = parseInt(body.subarray(at, lineEnd).toString("latin1"), 16);
      if (!Number.isFinite(size) || size === 0) break;
      parts.push(body.subarray(lineEnd + 2, lineEnd + 2 + size));
      at = lineEnd + 2 + size + 2;
    }
    body = Buffer.concat(parts);
  } else {
    const length = Number(headers.get("content-length"));
    if (Number.isInteger(length)) body = body.subarray(0, length);
  }
  return { status, headers, body };
}

async function call(place: Place, address: string, method: string, target: string, body = ""): Promise<Answer> {
  const port = Number(address.split(":").pop());
  return parseAnswer(await place.exchange(port, request(method, address, target, body)));
}

function json(answer: Answer): unknown {
  return JSON.parse(answer.body.toString());
}

const field = (value: unknown, ...keys: string[]): unknown =>
  keys.reduce<unknown>((at, key) => (at !== null && typeof at === "object" ? (at as Record<string, unknown>)[key] : undefined), value);

// ---------------------------------------------------------------------------
// The file: format, dynamic libraries, glibc floor

type Linkage = { format: string; libraries: string[]; glibcFloor: string | null };

/** The dynamic libraries an ELF file needs (DT_NEEDED) and the symbol versions it asks of them. */
function readElf(file: Buffer): Linkage {
  if (file.readUInt32BE(0) !== 0x7f454c46) throw new Error("not an ELF file");
  if (file[4] !== 2 || file[5] !== 1) throw new Error("not a 64-bit little-endian ELF file");
  const machine = file.readUInt16LE(0x12);
  const shoff = Number(file.readBigUInt64LE(0x28));
  const shentsize = file.readUInt16LE(0x3a);
  const shnum = file.readUInt16LE(0x3c);
  type Section = { type: number; offset: number; size: number; link: number; info: number };
  const sections: Section[] = [];
  for (let i = 0; i < shnum; i++) {
    const at = shoff + i * shentsize;
    sections.push({
      type: file.readUInt32LE(at + 4),
      offset: Number(file.readBigUInt64LE(at + 24)),
      size: Number(file.readBigUInt64LE(at + 32)),
      link: file.readUInt32LE(at + 40),
      info: file.readUInt32LE(at + 44),
    });
  }
  const string = (table: number, offset: number): string => {
    const strtab = sections[table];
    if (!strtab) throw new Error("a string table index out of range");
    const start = strtab.offset + offset;
    const end = file.indexOf(0, start);
    return file.toString("latin1", start, end < 0 ? strtab.offset + strtab.size : end);
  };
  const libraries: string[] = [];
  const versions: string[] = [];
  for (const section of sections) {
    if (section.type === 6) {
      // SHT_DYNAMIC: (tag, value) pairs; DT_NEEDED is 1, DT_NULL 0 ends them.
      for (let at = section.offset; at + 16 <= section.offset + section.size; at += 16) {
        const tag = file.readBigInt64LE(at);
        if (tag === 0n) break;
        if (tag === 1n) libraries.push(string(section.link, Number(file.readBigUInt64LE(at + 8))));
      }
    } else if (section.type === 0x6ffffffe) {
      // SHT_GNU_verneed: one entry per library, each with its versions.
      let need = section.offset;
      for (let i = 0; i < section.info; i++) {
        const count = file.readUInt16LE(need + 2);
        let aux = need + file.readUInt32LE(need + 8);
        for (let j = 0; j < count; j++) {
          versions.push(string(section.link, file.readUInt32LE(aux + 8)));
          aux += file.readUInt32LE(aux + 12);
        }
        need += file.readUInt32LE(need + 12);
      }
    }
  }
  const glibc = versions
    .map((version) => /^GLIBC_(\d+(?:\.\d+)*)$/.exec(version)?.[1])
    .filter((version): version is string => version !== undefined)
    .sort(compareVersions);
  const arch = machine === 0x3e ? "x86-64" : machine === 0xb7 ? "aarch64" : `machine ${machine}`;
  return { format: `ELF ${arch}`, libraries, glibcFloor: glibc.at(-1) ?? null };
}

function compareVersions(a: string, b: string): number {
  const x = a.split(".").map(Number);
  const y = b.split(".").map(Number);
  for (let i = 0; i < Math.max(x.length, y.length); i++) {
    const difference = (x[i] ?? 0) - (y[i] ?? 0);
    if (difference !== 0) return difference;
  }
  return 0;
}

async function checkFile(binary: string, report: Report): Promise<void> {
  const file = readFileSync(binary);
  const size = statSync(binary).size;
  report.add("size", true, `${(size / 1024 / 1024).toFixed(1)} MiB (${size} bytes)`);
  if (file.length >= 4 && file.readUInt32BE(0) === 0x7f454c46) {
    let linkage: Linkage;
    try {
      linkage = readElf(file);
    } catch (error) {
      report.add("dynamic libraries", false, `the ELF file can't be read: ${(error as Error).message}`);
      return;
    }
    const outside = linkage.libraries.filter((library) => !LINUX_BASE.test(library));
    report.add(
      "dynamic libraries",
      outside.length === 0 && linkage.libraries.length > 0,
      `${linkage.format}; needs ${linkage.libraries.join(", ") || "nothing"}${outside.length > 0 ? `; outside the base system: ${outside.join(", ")}` : ""}`,
    );
    const floor = linkage.glibcFloor;
    report.add(
      "glibc floor",
      floor !== null && compareVersions(floor, GLIBC_CEILING) <= 0,
      floor === null
        ? "no GLIBC_ version found"
        : `GLIBC_${floor}${compareVersions(floor, GLIBC_CEILING) <= 0 ? "" : `, above GLIBC_${GLIBC_CEILING}: Debian 12 and Ubuntu 22.04 can't run it`}`,
    );
  } else if (platform() === "darwin") {
    const otool = await run("otool", ["-L", binary]);
    const text = otool.stdout.toString();
    const libraries = text
      .split("\n")
      .slice(1)
      .map((line) => line.trim().split(" (")[0] ?? "")
      .filter((line) => line !== "");
    const outside = libraries.filter((library) => !MACOS_BASE.test(library));
    report.add(
      "dynamic libraries",
      otool.status === 0 && libraries.length > 0 && outside.length === 0,
      otool.status === 0
        ? `Mach-O; needs ${libraries.join(", ")}${outside.length > 0 ? `; outside the system: ${outside.join(", ")}` : ""}`
        : `otool failed: ${otool.stderr.trim()}`,
    );
    report.note(details(`otool -L ${path.basename(binary)}`, `${text}${otool.stderr}`));
  } else {
    report.add("dynamic libraries", false, "neither an ELF file nor a Mach-O file this machine can read (a macOS binary needs otool)");
  }
  report.add(
    "no pseudolocales in the binary",
    !file.includes(PSEUDO_MARKER),
    file.includes(PSEUDO_MARKER) ? "fluent-pseudo's letter table is in the binary" : "fluent-pseudo's letter table is absent",
  );
}

// ---------------------------------------------------------------------------
// The program

async function checkProgram(place: Place, version: string, report: Report): Promise<void> {
  report.note(details(`${place.name}: system and libraries`, await place.describe()));

  const shown = await place.run(["--version"], {});
  const said = shown.stdout.toString().trim();
  report.add("--version", shown.status === 0 && said === `ladderback ${version}`, `said ${JSON.stringify(said)}${shown.status === 0 ? "" : `, exit ${shown.status}: ${shown.stderr.trim()}`}`);

  const scratch = place.scratch;
  const folders = (name: string): Env => ({
    LADDERBACK_CONFIG_DIR: `${scratch}/${name}/config`,
    LADDERBACK_DATA_DIR: `${scratch}/${name}/data`,
    LADDERBACK_CACHE_DIR: `${scratch}/${name}/cache`,
  });
  const source = `${scratch}/source`;
  const repository = `${scratch}/repository`;
  const passwordFile = `${scratch}/password`;
  await place.mkdir(`${scratch}/daemon/config`);
  await place.mkdir(`${scratch}/daemon/data`);
  await place.mkdir(`${scratch}/pseudo/config`);
  await place.mkdir(`${scratch}/pseudo/data`);
  await place.mkdir(source);
  for (const n of [1, 2, 3]) await place.writeFile(`${source}/file-${n}.txt`, `demo file ${n}\n`);
  // Readable by its owner only: the daemon warns of a password file others
  // can read (secret-file-readable), and the backup must succeed cleanly.
  await place.writeFile(passwordFile, `${PASSWORD}\n`, true);
  await place.writeFile(
    `${scratch}/daemon/config/config.toml`,
    [
      "schema = 1",
      "",
      `[repositories.${PLAN}]`,
      `repository = ${JSON.stringify(repository)}`,
      `password-file = ${JSON.stringify(passwordFile)}`,
      "",
      "[[plans]]",
      `id = ${JSON.stringify(PLAN)}`,
      `repository = ${JSON.stringify(PLAN)}`,
      `sources = [${JSON.stringify(source)}]`,
      "",
    ].join("\n"),
  );

  const daemon = place.spawn(["serve", "--listen", "127.0.0.1:0"], folders("daemon"));
  try {
    const started = Date.now();
    let address: string | undefined;
    while (Date.now() - started < LISTEN_LIMIT_MS && !daemon.exited()) {
      address = /listening on http:\/\/([^\s/]+)/.exec(daemon.stderr())?.[1];
      if (address) break;
      await sleep(100);
    }
    if (!report.add("serve starts", address !== undefined, address ? `listening on ${address}` : `no address: ${daemon.stderr().trim().slice(-1000)}`)) return;
    const at = address as string;

    // A second daemon on the address the first holds: it can't listen, and
    // says so in the language asked for, which a release can't speak.
    const pseudo = await place.run(["serve", "--listen", at], { ...folders("pseudo"), LANGUAGE: "en-XA" });
    const line = pseudo.stderr.split("\n").find((text) => text.startsWith("ladderback: ")) ?? "";
    report.add(
      "no pseudolocales: LANGUAGE=en-XA gets English",
      pseudo.status !== 0 && line.startsWith(`ladderback: cannot listen on ${at}: `),
      `exit ${pseudo.status}, said ${JSON.stringify(line)}`,
    );

    const page = await call(place, at, "GET", "/");
    const html = page.body.toString();
    report.add(
      "GET / serves the web UI",
      page.status === 200 && (page.headers.get("content-type") ?? "").startsWith("text/html") && html.includes("<title>Ladderback</title>"),
      `${page.status} ${page.headers.get("content-type") ?? "no content type"}, ${page.body.length} bytes`,
    );
    const wasmPath = /href="(\/[^"]+\.wasm)"/.exec(html)?.[1];
    if (report.add("the page names its wasm", wasmPath !== undefined, wasmPath ?? "no .wasm in the page")) {
      const wasm = await call(place, at, "GET", wasmPath as string);
      const isWasm = wasm.status === 200 && wasm.body.subarray(0, 4).equals(Buffer.from([0, 0x61, 0x73, 0x6d]));
      report.add("the wasm is served", isWasm, `${wasm.status}, ${(wasm.body.length / 1024 / 1024).toFixed(1)} MiB`);
      if (isWasm) {
        report.add("no pseudolocales in the web UI", !wasm.body.includes(PSEUDO_MARKER), wasm.body.includes(PSEUDO_MARKER) ? "fluent-pseudo's letter table is in the wasm" : "fluent-pseudo's letter table is absent");
        report.add("no gallery in the web UI", !wasm.body.includes(GALLERY_MARKER), wasm.body.includes(GALLERY_MARKER) ? `${GALLERY_MARKER} is in the wasm` : `${GALLERY_MARKER} is absent`);
      }
    }

    const init = await call(
      place,
      at,
      "POST",
      "/api/v1/repositories/_init",
      JSON.stringify({ settings: { repository, password: { source: "file", path: passwordFile } } }),
    );
    if (!report.add("a scratch repository is initialised", init.status === 201, `${init.status} ${init.body.toString().slice(0, 300)}`)) return;

    const run = await call(place, at, "POST", "/api/v1/operations", JSON.stringify({ plan: PLAN }));
    const operation = run.status === 202 ? field(json(run), "operation") : undefined;
    if (!report.add("a backup is accepted", typeof operation === "string", `${run.status} ${run.body.toString().slice(0, 300)}`)) return;

    let status: unknown;
    let detail = "";
    const begun = Date.now();
    while (Date.now() - begun < BACKUP_LIMIT_MS) {
      const answer = await call(place, at, "GET", `/api/v1/operations/${operation as string}`);
      detail = answer.body.toString();
      status = answer.status === 200 ? field(json(answer), "operation", "status") : undefined;
      if (typeof status === "string" && ["succeeded", "succeeded-with-warnings", "failed", "cancelled"].includes(status)) break;
      await sleep(250);
    }
    report.add("the backup succeeds", status === "succeeded", `${String(status)} after ${((Date.now() - begun) / 1000).toFixed(1)} s${status === "succeeded" ? "" : `: ${detail.slice(0, 500)}`}`);

    const listed = await call(place, at, "GET", `/api/v1/repositories/${PLAN}/snapshots`);
    const snapshots = listed.status === 200 ? field(json(listed), "snapshots") : undefined;
    report.add(
      "the repository has its snapshot",
      Array.isArray(snapshots) && snapshots.length === 1,
      Array.isArray(snapshots) ? `${snapshots.length} snapshot(s)` : `${listed.status} ${listed.body.toString().slice(0, 300)}`,
    );
  } finally {
    await daemon.stop();
    report.note(details(`${place.name}: the daemon's log`, daemon.stderr() || "(nothing)"));
  }
}

// ---------------------------------------------------------------------------

function usage(message: string): never {
  console.error(`error: ${message}`);
  console.error(`usage: node scripts/smoke-release.ts <binary> [--container <${Object.keys(IMAGES).join("|")}|all>]... [--version <x.y.z>] [--summary <file>]`);
  process.exit(2);
}

async function main(argv: string[]): Promise<number> {
  let binary: string | undefined;
  let version: string | undefined;
  let summary = process.env["GITHUB_STEP_SUMMARY"];
  const containers: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i] ?? "";
    const value = () => argv[++i] ?? usage(`${arg} needs a value`);
    if (arg === "--help" || arg === "-h") usage("help");
    else if (arg === "--container") {
      const name = value();
      if (name === "all") containers.push(...Object.keys(IMAGES));
      else if (name in IMAGES) containers.push(name);
      else usage(`no container ${JSON.stringify(name)}`);
    } else if (arg === "--version") version = value();
    else if (arg === "--summary") summary = value();
    else if (arg.startsWith("-")) usage(`unknown option ${arg}`);
    else if (binary === undefined) binary = path.resolve(arg);
    else usage(`a second binary: ${arg}`);
  }
  if (binary === undefined) usage("no binary given");
  const build = checkoutBuild(ROOT);
  const expected = version ?? (build === undefined ? workspaceVersion(ROOT) : shown(build));
  const sections: string[] = [];
  let failed = false;

  const fileReport = new Report();
  await checkFile(binary, fileReport);
  sections.push(fileReport.markdown(`Release smoke test: ${path.basename(binary)} (the file)`));
  failed ||= fileReport.failed;

  const places = containers.length > 0 ? containers : ["native"];
  for (const name of places) {
    const report = new Report();
    let place: Place | undefined;
    try {
      place = name === "native" ? nativePlace(binary) : await containerPlace(name, IMAGES[name] as string, binary);
      await checkProgram(place, expected, report);
    } catch (error) {
      report.add("the run finishes", false, (error as Error).message);
    } finally {
      await place?.close();
    }
    sections.push(report.markdown(`Release smoke test: ${place?.name ?? name}`));
    failed ||= report.failed;
  }

  const text = sections.join("\n");
  console.log(text);
  if (summary) appendFileSync(summary, `${text}\n`);
  console.log(failed ? "smoke test: FAILED" : "smoke test: passed");
  return failed ? 1 : 0;
}

process.exitCode = await main(process.argv.slice(2));
