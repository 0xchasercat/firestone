---
icon: code
---

# API reference

Every operation the REST server exposes, generated from the same OpenAPI contract the test suite enforces.

The API listens on a private Unix socket by default, or on a loopback TCP listener with a session token when you run `firestone serve --listen tcp:127.0.0.1:8642 --token FILE`. A hosted page cannot reach either listener, so there is no in-page request runner here; copy the curl samples and point them at your own machine. Transport, authentication and streaming conventions are covered in [CLI and REST](../cli-and-rest.md).

```sh
curl --unix-socket "$XDG_RUNTIME_DIR/firestone/serve.sock" http://firestone/v1/version
```
