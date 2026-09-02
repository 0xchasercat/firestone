# Web interface

What `firestone ui` serves, what each screen holds, and how the terminal page attaches to a guest.

`firestone ui` serves the same API and an embedded web interface on an ephemeral loopback port, then opens your browser:

```sh
firestone ui
```

It prints a URL carrying a session token generated for this run only:

```text
Firestone UI   http://127.0.0.1:47318/?token=<64 hex>
Press Ctrl-C to stop.
```

The first page load trades that token for an `HttpOnly`, `SameSite=Strict` cookie and rewrites the address bar, so the token does not sit in browser history. The token lives in process memory; stopping `firestone ui` invalidates it.

On a headless host, print the URL and reach it through an SSH tunnel. Firestone refuses to bind a routable address, so a tunnel is the supported path:

```sh
firestone ui --no-open
```

`--print-url` prints the URL and nothing else, which is what you want in a script. Add `--json` for a machine-readable record of address, port and URL.

## What the interface holds

The overview screen carries a host summary, the doctor report, and panels for machines and the image cache. `/machines` lists every machine with its status, image, CPU and memory, uptime, and forward chips; a forward chip is a link when the machine is running, the protocol is TCP, and the host side is a single port. Anything else renders as a plain chip, because a link that navigates nowhere teaches you to distrust the rest.

The machine detail page has four tabs. `spec` renders the effective specification, `logs` renders the guest console with its own ANSI colors, `snapshots` lists and manages snapshots, and `vmconfig` shows the exact JSON Firestone handed the VMM. Above the tabs, a running machine carries a live utilization strip: CPU per cent, memory against allocation, and disk throughput, drawn as sparklines from samples the browser takes every three seconds. History is a 60-sample ring buffer per browser tab. It is not stored on the host, so a reload starts over. That is the honest consequence of running no metrics daemon.

Machine creation, editing, cloning, snapshot create and restore, image delete, and both prunes are dialogs. Every one of them writes to the documented `/v1` endpoints and renders the resulting NDJSON as it arrives, so what the browser does is what `curl` would do. The system-prune dialog previews before it removes: it runs the same request with `dry_run` set, renders the list it gets back, and only then enables the confirm button.

`⌘K` or `/` opens a command palette over machines, catalog entries, and the actions the screens themselves offer. It deliberately has no start, stop, restart or delete entry. Those four render their progress on the button that dispatched them, and a palette row has no button.

## The terminal page

`/machines/<name>/terminal` is a full-window terminal with Console and Shell tabs. Console attaches to the guest's `hvc0` through a WebSocket, which is the same single-client console `firestone console` takes; if the CLI already holds it, the page says so and names `firestone console <name>`. Shell opens an SSH session on a host pseudo-terminal over the same transport `firestone shell` uses.

Both tabs need a running machine, and the Terminal link appears on the detail page only while a machine is running. The page itself renders for a machine in any state, because a terminal that cannot attach should say why rather than return a 404. Nothing reconnects on its own; every failure overlay offers a Reconnect button and waits for you to decide.

If the browser cannot instantiate the terminal emulator, the page falls back to a plain transcript that strips the sequences it cannot draw and still sends keystrokes. It says in the footer that it is degraded.

Everything the interface needs is compiled into the binary. No CDN, no outbound request, no second process.

The endpoints behind these screens are in [CLI and REST](cli-and-rest.md), and the authority the interface holds is in [security](security.md). The page list is in the [documentation index](README.md).
