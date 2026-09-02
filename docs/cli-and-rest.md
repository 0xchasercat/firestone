# CLI and REST

Copying files, the REST server over both transports, the prune ladder, and what `--json` guarantees.

## Copying files

`cp` copies between the host and a machine over the same vsock transport `shell` uses. There is no guest network involved and nothing to add to `~/.ssh/config`:

```sh
firestone cp ./notes.txt dev:/root/notes.txt
firestone cp dev:/var/log/syslog ./syslog
firestone cp -r ./project dev:/root/project
```

Exactly one operand is remote. An operand is remote when it holds a colon and everything before the first colon is a machine name, which is lowercase letters, digits and dashes. Everything else is local. `./dev:/etc/hostname` is a local file, and so is `/srv/dev:/etc`, because the colon comes after a `/`. An IPv6 literal is not operand syntax: `fe80::1:/etc` reads as machine `fe80`, so write `./fe80::1:/etc` for the local file.

Zero remote operands and two remote operands are both usage errors, and each hint names the `./` escape. A machine that is not running is refused with the same message `shell` gives; `cp` never starts a machine.

Under the hood this is `scp` with Firestone's option block, so `-r`, the progress meter and the exit status are `scp`'s own. OpenSSH 9 `scp` transfers over SFTP, so a remote wildcard is expanded by the guest's SFTP server rather than a shell. Quote a remote glob so your own shell does not expand it first.

## REST API

`firestone serve` is optional and stateless. It projects the same actions, the same locks and the same event stream as the CLI. The default listener is `$XDG_RUNTIME_DIR/firestone/serve.sock`, or `/tmp/firestone-<uid>/serve.sock` when the runtime fallback is active.

The full contract is [`openapi.json`](openapi.json), an OpenAPI 3.1 document covering request and response shapes, the NDJSON streams, `Accept: application/json` aggregation, error statuses, limits and both transports. It is a checked-in artifact, not a runtime endpoint; Firestone does not serve it.

Start the server and find its socket:

```sh
firestone serve &
serve_pid=$!
if test -n "${XDG_RUNTIME_DIR:-}"; then
  firestone_socket="$XDG_RUNTIME_DIR/firestone/serve.sock"
else
  firestone_socket="/tmp/firestone-$(id -u)/serve.sock"
fi
```

List machines and stream a start:

```sh
curl --fail --silent --show-error --unix-socket "$firestone_socket" http://firestone/v1/machines
curl --fail --no-buffer --unix-socket "$firestone_socket" \
  -H 'Content-Type: application/json' \
  -X POST http://firestone/v1/machines/dev/start \
  -d '{"wait":true,"timeout_s":600}'
```

Stop the front end when you are done. Running shims and VMs are independent of it:

```sh
kill "$serve_pid"
wait "$serve_pid"
```

The socket is mode 0600. Holding the same user account is the whole authentication story.

A browser cannot open a Unix socket, so `serve` also takes a loopback TCP listener. It must be loopback and it must carry a token; Firestone refuses anything else before it binds:

```sh
firestone serve --listen tcp:127.0.0.1:8642 --token ~/.local/share/firestone/api-token
```

`--host ADDR` and `--port PORT` spell the same listener more briefly. `--port 8642` alone means `tcp:127.0.0.1:8642`, and `--host` picks which loopback address to bind:

```sh
firestone serve --port 8642 --token ~/.local/share/firestone/api-token
firestone serve --host ::1 --port 8642 --token ~/.local/share/firestone/api-token
```

They are sugar for `--listen`, so passing both spellings is a usage error, and every rule above still holds: a non-loopback address is refused, and a TCP listener still needs `--token`.

The token file is created mode 0600 when it does not exist, and validated when it does. Send it as a bearer token:

```sh
curl --fail --silent --show-error \
  -H "Authorization: Bearer $(cat ~/.local/share/firestone/api-token)" \
  http://127.0.0.1:8642/v1/machines
```

Every TCP request passes a `Host` allowlist before the token is compared, which is what stops a rebound DNS name from spending a cookie your browser would attach for it. A WebSocket upgrade must additionally prove same origin. The transport is plaintext, so anything with local root can read it. Loopback TCP is a convenience for the browser, not a replacement for the 0600 socket.

Two routes leave HTTP: `GET /v1/machines/{name}/console/ws` and `GET /v1/machines/{name}/shell/ws` carry a terminal as a byte stream. An attached terminal has no idle point, so shutting `serve` down closes an open terminal rather than waiting for the person at it.

## Host and machine samples

`GET /v1/machines/{name}/metrics` samples one running machine. `GET /v1/host/metrics` samples the host itself: processor count and load averages, memory, and free space on the filesystem holding the data directory. It needs no machine and takes no lock. The CLI prints the same sample:

```sh
firestone system metrics
firestone system metrics --json
```

Firestone keeps no history, so a figure it cannot read is `null` rather than a zero, and a rate is two samples and a subtraction on your side.

## Reclaiming disk space

`firestone images prune` removes unreferenced base images and nothing else. `firestone system prune` reclaims everything Firestone is holding, arranged as a ladder whose bottom tier cannot destroy work:

```sh
firestone system prune --dry-run
firestone system prune
firestone system prune --images
firestone system prune --all --dry-run
```

With no flags it removes only inert artifacts: a stale runtime directory for a machine that is not active, a rotated `console.log.previous`, an unfinished `.partial` from an interrupted pull or copy, an orphaned removal directory, and an unpublished snapshot working directory. Every one of those is debris from an operation that already finished or died.

`--images` adds base images that nothing references, using the same reference set `images rm` refuses to break: a machine's pinned image and every published snapshot's image both count.

`--machines` is the destructive tier. It removes every machine that is `stopped`, `created` or `failed`, with its disk, spec, snapshots and logs, exactly as `firestone rm` does. On a terminal it prints the machine names and asks; without a terminal it needs `--force` or `--yes`. A machine that is starting, running or stopping is never a candidate. `--all` is `--machines --images`.

`--dry-run` is what makes the ladder usable. It produces the same list, the same per-row byte counts and the same total that a real run against the same state would produce, and deletes nothing. Byte counts are allocated blocks measured immediately before deletion, so a sparse overlay is not reported as its virtual size. The tiers run in ladder order, so a machine removed by the last tier does not release its base image within the same call; that image becomes prunable on the next prune. Doing it the other way would make a real run reclaim more than its own preview promised.

## JSON, pipes, and exit status

Put `--json` on any command for newline-delimited JSON events on stdout:

```sh
firestone ls --json
firestone start dev --json
firestone doctor --json
```

Human progress goes to stderr and data goes to stdout, so pipes work. When stderr is not a terminal, Firestone writes plain lines with no color, cursor control or spinner frame. `NO_COLOR` and `--no-color` disable color explicitly.

Exit status is stable:

| Code | Meaning |
|---|---|
| 0 | Success |
| 1 | Generic failure |
| 2 | Usage or invalid specification |
| 3 | Machine or image not found |
| 4 | Conflict: already exists, already running, name in use, or a busy lock |
| 5 | Missing or broken host dependency |
| 6 | Timeout |
| 7 | Checksum or verification failure |
| 130 | Interrupted |

`run` and `shell` propagate the guest command's exit status instead.

What each error kind means for repair is in [troubleshooting](troubleshooting.md), and the authority a listener holds is in [security](security.md). The page list is in the [documentation index](README.md).
