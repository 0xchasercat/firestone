/* Firestone web UI runtime.
 *
 * htmx does the navigation, the polling and every plain swap. This file exists
 * only for the four things htmx genuinely cannot do:
 *
 *   1. Consume a chunked application/x-ndjson mutation stream and render each
 *      record as it arrives.
 *   2. Follow an open-ended text/plain log with correct pin-to-bottom.
 *   3. Toasts and the command palette's keyboard model.
 *   4. Theme persistence.
 *
 * Two rules run through all of it:
 *
 *   - The server is the only source of truth for state. Nothing here ever
 *     writes a terminal status. A button may render a *transitional* state
 *     because the action provably dispatched, and on the terminal record it
 *     asks the server what actually happened.
 *   - The Content-Security-Policy served with every page has no 'unsafe-inline'
 *     and no 'unsafe-eval'. That rules out htmx trigger filters ("[expr]"),
 *     hx-on:* attributes and "js:" values, all of which htmx compiles with
 *     new Function(). Everything they would have done is done here instead,
 *     with delegated listeners that survive every swap for free.
 */
(function () {
  "use strict";

  var STREAM_LINGER_MS = 1600;
  var TOAST_MS = 4500;
  var MAX_TOASTS = 3;
  var MAX_STREAM_LINES = 8;
  var PALETTE_DEBOUNCE_MS = 120;

  /* Machines with an open mutation stream. While this is non-empty the 5 s
   * poll is suppressed, so a row cannot be re-rendered out from under a
   * transition the user is watching. */
  var streaming = Object.create(null);
  var logReader = null;
  var paletteTimer = null;

  /* ------------------------------------------------------------- helpers -- */

  function el(tag, className, text) {
    var node = document.createElement(tag);
    if (className) {
      node.className = className;
    }
    if (text !== undefined && text !== null) {
      node.textContent = String(text);
    }
    return node;
  }

  function qs(selector, root) {
    return (root || document).querySelector(selector);
  }

  function isStreaming() {
    for (var key in streaming) {
      if (Object.prototype.hasOwnProperty.call(streaming, key)) {
        return true;
      }
    }
    return false;
  }

  function markStreaming(name, active) {
    if (active) {
      streaming[name] = true;
    } else {
      delete streaming[name];
    }
    document.body.toggleAttribute("data-fs-streaming", isStreaming());
  }

  /* Ask the server for current state now, rather than waiting up to 5 s for
   * the next poll tick. Every live region listens for this on <body>. */
  function refreshFromServer() {
    document.body.dispatchEvent(
      new CustomEvent("fs:refresh", { bubbles: false })
    );
  }

  function compactJson(value) {
    try {
      return JSON.stringify(value);
    } catch (error) {
      return String(value);
    }
  }

  /* ------------------------------------------------------------- records -- */

  /* A record is terminal when it is the action's typed Result, or an
   * ErrorEnvelope. Note the envelope can arrive *under HTTP 200* after
   * progress has already streamed, so branching on status alone is wrong. */
  function isErrorEnvelope(record) {
    return !!(record && record.error && typeof record.error === "object");
  }

  function isTerminal(record) {
    if (!record) {
      return false;
    }
    return record.type === "Result" || isErrorEnvelope(record);
  }

  /* --------------------------------------------------------- ndjson read -- */

  /* Reads an LF-delimited compact-JSON body, handing every complete record to
   * onRecord as it arrives. Resolves with the terminal record, or null when
   * the stream ended without one. */
  async function streamAction(url, options, onRecord) {
    var response = await fetch(url, options);

    if (response.status === 204) {
      var empty = { type: "Result", action: "", payload: null };
      onRecord(empty);
      return empty;
    }

    if (!response.ok) {
      var failure;
      try {
        failure = await response.json();
      } catch (error) {
        failure = {
          error: {
            kind: "generic",
            message: "the server returned " + response.status,
            hint: "check that firestone serve is still running"
          }
        };
      }
      onRecord(failure);
      return failure;
    }

    /* A 200 aggregate (Accept: application/json) is one object, not a stream. */
    var contentType = response.headers.get("content-type") || "";
    if (contentType.indexOf("application/x-ndjson") === -1) {
      var aggregate = await response.json();
      onRecord(aggregate);
      return aggregate;
    }

    var reader = response.body.getReader();
    var decoder = new TextDecoder();
    var buffer = "";
    var terminal = null;

    for (;;) {
      var chunk = await reader.read();
      if (chunk.done) {
        break;
      }
      buffer += decoder.decode(chunk.value, { stream: true });
      var index;
      while ((index = buffer.indexOf("\n")) >= 0) {
        var line = buffer.slice(0, index);
        buffer = buffer.slice(index + 1);
        if (!line.trim()) {
          continue;
        }
        var record;
        try {
          record = JSON.parse(line);
        } catch (error) {
          continue;
        }
        onRecord(record);
        if (isTerminal(record)) {
          terminal = record;
        }
      }
    }
    return terminal;
  }

  /* -------------------------------------------------------- stream drawer -- */

  /* The drawer is per-machine: records for "web" never render into the detail
   * page for "builder". */
  function drawerFor(name) {
    var host = qs('[data-fs-stream-host="' + CSS.escape(name) + '"]');
    if (!host) {
      return null;
    }
    var drawer = qs(".fs-stream", host);
    if (drawer) {
      return drawer;
    }

    drawer = el("div", "fs-stream");
    drawer.setAttribute("role", "log");
    drawer.setAttribute("aria-label", "action progress");

    var head = el("div", "fs-stream__head");
    head.appendChild(el("span", "fs-stream__dot"));
    head.appendChild(
      el("span", "fs-stream__head-text", "200 · application/x-ndjson · streaming")
    );
    drawer.appendChild(head);
    drawer.appendChild(el("div", "fs-stream__lines"));

    host.appendChild(drawer);
    return drawer;
  }

  function pushStreamLine(name, record) {
    var drawer = drawerFor(name);
    if (!drawer) {
      return;
    }
    var lines = qs(".fs-stream__lines", drawer);
    var className = "fs-stream__line";
    if (isErrorEnvelope(record)) {
      className += " fs-stream__line--fail";
    } else if (record.type === "Result") {
      className += " fs-stream__line--ok";
    }
    lines.appendChild(el("div", className, compactJson(record)));
    while (lines.children.length > MAX_STREAM_LINES) {
      lines.removeChild(lines.firstChild);
    }
  }

  function closeDrawer(name) {
    window.setTimeout(function () {
      if (streaming[name]) {
        return;
      }
      var host = qs('[data-fs-stream-host="' + CSS.escape(name) + '"]');
      var drawer = host && qs(".fs-stream", host);
      if (drawer) {
        drawer.remove();
      }
    }, STREAM_LINGER_MS);
  }

  /* -------------------------------------------------------------- toasts -- */

  /* Toasts announce the completion of asynchronous work, never validation.
   * The sub-line carries the raw terminal record: seeing the actual bytes is
   * what makes the result believable. */
  function toast(title, sub, kind) {
    var stack = qs("#fs-toasts");
    if (!stack) {
      return;
    }

    var node = el("div", "fs-toast");
    var head = el("div", "fs-toast__head");
    var dot = el("span", "fs-dot fs-dot--xs");
    dot.setAttribute("data-status", kind || "ok");
    head.appendChild(dot);
    head.appendChild(el("span", "fs-toast__title", title));
    node.appendChild(head);
    if (sub) {
      var subNode = el("div", "fs-toast__sub", sub);
      subNode.title = sub;
      node.appendChild(subNode);
    }

    stack.appendChild(node);
    while (stack.children.length > MAX_TOASTS) {
      stack.removeChild(stack.firstChild);
    }

    window.setTimeout(function () {
      node.classList.add("is-leaving");
      window.setTimeout(function () {
        node.remove();
      }, 200);
    }, TOAST_MS);
  }

  /* ------------------------------------------------------- inline notices -- */

  /* A 400 belongs beside the field that caused it and a 409 beside the action
   * that hit it. Neither is a toast: the user is already looking at the thing
   * that failed. */
  function inlineNotice(anchor, envelope, severe) {
    if (!anchor) {
      return;
    }
    clearInlineNotice(anchor);
    var notice = el(
      "div",
      "fs-inline-notice" + (severe ? " fs-inline-notice--fail" : "")
    );
    notice.setAttribute("data-fs-notice", "");
    notice.setAttribute("role", "status");
    notice.appendChild(el("span", null, envelope.error.message));
    if (envelope.error.hint) {
      notice.appendChild(
        el("span", "fs-inline-notice__hint", envelope.error.hint)
      );
    }
    var container = anchor.closest("[data-fs-notice-slot]") || anchor.parentNode;
    container.appendChild(notice);
  }

  function clearInlineNotice(anchor) {
    var container = anchor.closest("[data-fs-notice-slot]") || anchor.parentNode;
    var existing = container && qs("[data-fs-notice]", container);
    if (existing) {
      existing.remove();
    }
  }

  /* ------------------------------------------------- lifecycle mutations -- */

  var ACTIONS = {
    start: { path: "/start", method: "POST", transitional: "starting", busy: "Starting…" },
    stop: { path: "/stop", method: "POST", transitional: "stopping", busy: "Stopping…" },
    restart: { path: "/restart", method: "POST", transitional: "starting", busy: "Restarting…" },
    delete: { path: "", method: "DELETE", transitional: "stopping", busy: "Removing…" }
  };

  /* Render the transitional state the moment the request leaves, so the UI
   * answers within one frame instead of one round trip. This is *not* a
   * predicted outcome: "starting" is a real state the server also reports,
   * and it is replaced by server truth on the terminal record. */
  function beginTransition(button, name, action) {
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
    if (!button.hasAttribute("data-fs-label")) {
      button.setAttribute("data-fs-label", button.textContent.trim());
    }
    button.textContent = action.busy;

    var row = qs('[data-fs-row="' + CSS.escape(name) + '"]');
    var status = row && qs("[data-status]", row);
    if (status) {
      status.setAttribute("data-status", action.transitional);
      var label = qs(".fs-status__text", status);
      if (label) {
        label.textContent = action.transitional;
      }
    }
    markStreaming(name, true);
  }

  function endTransition(button, name, options) {
    markStreaming(name, false);
    if (button && button.isConnected) {
      button.disabled = false;
      button.removeAttribute("aria-busy");
      var label = button.getAttribute("data-fs-label");
      if (label) {
        button.textContent = label;
      }
    }
    closeDrawer(name);
    /* A deleted machine has no server state left to read; refreshing its
     * detail region would only produce a 404 toast. */
    if (!(options && options.skipRefresh)) {
      refreshFromServer();
    }
  }

  function onDetailPageFor(name) {
    return !!qs('[data-fs-stream-host="' + CSS.escape(name) + '"]');
  }

  /* After removing the machine whose page you are on, go back to the list
   * rather than sitting on a page that no longer describes anything. */
  function leaveDeletedMachine(name) {
    if (!onDetailPageFor(name) || !window.htmx) {
      return;
    }
    window.setTimeout(function () {
      window.htmx.ajax("GET", "/machines", {
        target: "#fs-main-content",
        swap: "innerHTML"
      });
      window.history.pushState({}, "", "/machines");
    }, STREAM_LINGER_MS);
  }

  async function runLifecycle(button, name, kind, force) {
    var action = ACTIONS[kind];
    if (!action || button.disabled) {
      return;
    }
    clearInlineNotice(button);
    beginTransition(button, name, action);

    var url = "/v1/machines/" + encodeURIComponent(name) + action.path;
    if (kind === "delete" && force) {
      url += "?force=true";
    }
    var options = { method: action.method, headers: { Accept: "application/x-ndjson" } };
    /* restart rejects even "{}" — its body must contain zero bytes. */
    if (kind === "start" || kind === "stop") {
      options.headers["Content-Type"] = "application/json";
      options.body = "{}";
    }

    var started = performance.now();
    var terminal = null;
    try {
      terminal = await streamAction(url, options, function (record) {
        pushStreamLine(name, record);
      });
    } catch (error) {
      toast(name + " · " + kind + " failed", String(error), "fail");
      endTransition(button, name);
      return;
    }

    var elapsed = Math.round(performance.now() - started);
    if (isErrorEnvelope(terminal)) {
      /* A conflict is expected and self-explanatory; it goes inline next to
       * the button, amber, with no toast. Anything else is a real failure. */
      var conflict = terminal.error.kind === "conflict";
      inlineNotice(button, terminal, !conflict);
      if (!conflict) {
        toast(name + " · " + kind + " failed", terminal.error.message, "fail");
      }
    } else if (kind === "delete") {
      fadeOutRow(name);
      toast("removed " + name, compactJson(terminal), "ok");
      endTransition(button, name, { skipRefresh: onDetailPageFor(name) });
      leaveDeletedMachine(name);
      return;
    } else if (terminal) {
      toast(
        name + " · " + kind + " finished in " + formatMs(elapsed),
        compactJson(terminal),
        "ok"
      );
    }
    endTransition(button, name);
  }

  function formatMs(ms) {
    return ms >= 1000 ? (ms / 1000).toFixed(1) + " s" : ms + " ms";
  }

  function fadeOutRow(name) {
    var row = qs('[data-fs-row="' + CSS.escape(name) + '"]');
    if (row) {
      row.classList.add("is-leaving");
    }
  }

  /* ---------------------------------------------------------- image pull -- */

  async function runPull(button, reference) {
    if (button.disabled) {
      return;
    }
    var card = button.closest("[data-fs-card]");
    clearInlineNotice(button);

    button.disabled = true;
    var slot = button.parentNode;
    var state = el("span", "fs-pullstate", "resolve");
    slot.replaceChild(state, button);

    var bar = null;
    if (card) {
      var track = el("div", "fs-progress");
      bar = el("div", "fs-progress__bar");
      track.appendChild(bar);
      card.insertBefore(track, qs(".fs-card__source", card));
    }

    markStreaming("image:" + reference, true);

    var terminal = null;
    try {
      terminal = await streamAction(
        "/v1/images/pull",
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            Accept: "application/x-ndjson"
          },
          body: JSON.stringify({ ref: reference })
        },
        function (record) {
          applyPullProgress(state, bar, record);
        }
      );
    } catch (error) {
      terminal = {
        error: { kind: "generic", message: String(error), hint: null }
      };
    }

    markStreaming("image:" + reference, false);
    if (isErrorEnvelope(terminal)) {
      toast("pull failed · " + reference, terminal.error.message, "fail");
    } else {
      toast("pulled " + reference, compactJson(terminal), "ok");
    }
    refreshFromServer();
  }

  /* Driven by what the pull action actually emits, which is one "image" step:
   * StepStart, then Progress records carrying done/total bytes, then StepDone
   * or a StepSkip whose reason is "cached".
   *
   * A Progress record without a total means the source did not advertise a
   * length. The bar then goes indeterminate rather than inventing a
   * percentage — a progress bar that lies is worse than one that admits it
   * does not know. */
  function applyPullProgress(state, bar, record) {
    if (record.type === "StepStart") {
      state.textContent = "resolving";
      if (bar) {
        bar.classList.add("is-indeterminate");
      }
      return;
    }

    if (record.type === "Progress") {
      if (!record.total) {
        state.textContent = "downloading " + formatBytes(record.done);
        if (bar) {
          bar.classList.add("is-indeterminate");
        }
        return;
      }
      var percent = Math.min(100, Math.round((record.done / record.total) * 100));
      state.textContent = percent + "%";
      if (bar) {
        bar.classList.remove("is-indeterminate");
        bar.style.width = percent + "%";
      }
      return;
    }

    if (record.type === "StepSkip") {
      state.textContent = record.reason;
      return;
    }

    if (record.type === "StepDone") {
      /* Bytes are on disk; verification and qcow2 conversion still have to
       * finish, and neither reports a ratio. */
      state.textContent = "verifying";
      if (bar) {
        bar.classList.add("is-indeterminate");
        bar.style.width = "100%";
      }
    }
  }

  function formatBytes(bytes) {
    if (bytes >= 1e9) {
      return (bytes / 1e9).toFixed(1) + " GB";
    }
    if (bytes >= 1e6) {
      return Math.round(bytes / 1e6) + " MB";
    }
    return Math.round(bytes / 1e3) + " kB";
  }

  /* ---------------------------------------------------------- log follow -- */

  function stopFollowing() {
    if (logReader) {
      logReader.abort();
      logReader = null;
    }
  }

  /* Pin-to-bottom, done properly: only stick to the tail if the reader was
   * already at the tail. Never scrollIntoView — it moves the page, not just
   * the pane, and steals the viewport from whatever the user was reading. */
  async function follow(view) {
    stopFollowing();

    var name = view.getAttribute("data-fs-machine");
    var source = view.getAttribute("data-fs-source") || "serial";
    var controller = new AbortController();
    logReader = controller;

    var pinned = true;
    view.addEventListener("scroll", function () {
      pinned = view.scrollHeight - view.scrollTop - view.clientHeight < 24;
    });

    var url =
      "/v1/machines/" +
      encodeURIComponent(name) +
      "/logs?follow=true&source=" +
      encodeURIComponent(source);

    try {
      var response = await fetch(url, { signal: controller.signal });
      if (!response.ok) {
        return;
      }
      var reader = response.body.getReader();
      var decoder = new TextDecoder();
      for (;;) {
        var chunk = await reader.read();
        if (chunk.done) {
          break;
        }
        /* Bytes arrive already sanitized: newlines and SGR colour preserved,
         * every other escape sequence replaced with U+FFFD. The renderer
         * turns the surviving SGR into classes and text nodes, never markup. */
        appendLogChunk(view, decoder.decode(chunk.value, { stream: true }));
        if (pinned) {
          view.scrollTop = view.scrollHeight;
        }
      }
    } catch (error) {
      /* Abort is the normal way this ends: switching source, leaving the tab,
       * or navigating away. Closing the stream cancels the server-side read. */
    }
  }

  function syncFollowState() {
    var view = qs("[data-fs-logview]");
    if (!view) {
      stopFollowing();
      return;
    }
    view.scrollTop = view.scrollHeight;
    if (view.getAttribute("data-fs-follow") === "true") {
      follow(view);
    } else {
      stopFollowing();
    }
  }

  /* ------------------------------------------------------------- palette -- */

  function openPalette() {
    var dialog = qs("#fs-palette");
    if (!dialog || dialog.open) {
      return;
    }
    dialog.showModal();
    var input = qs("#fs-palette-input", dialog);
    if (input) {
      input.value = "";
      input.focus();
    }
  }

  function paletteMove(delta) {
    var items = Array.prototype.slice.call(
      document.querySelectorAll("#fs-palette-results [data-fs-palette-item]")
    );
    if (!items.length) {
      return;
    }
    var current = items.findIndex(function (item) {
      return item.classList.contains("is-active");
    });
    var next = current < 0 ? 0 : (current + delta + items.length) % items.length;
    items.forEach(function (item) {
      item.classList.remove("is-active");
    });
    items[next].classList.add("is-active");
    items[next].scrollIntoView({ block: "nearest" });
  }

  function paletteChoose() {
    var active = qs("#fs-palette-results [data-fs-palette-item].is-active");
    var target = active || qs("#fs-palette-results [data-fs-palette-item]");
    if (target) {
      target.click();
    }
  }

  /* ------------------------------------------------------------ listeners -- */

  /* Capture phase, because an action button sits inside a clickable row: the
   * button must win and the row navigation must not also fire. */
  document.addEventListener(
    "click",
    function (event) {
      var actionButton = event.target.closest("[data-fs-action]");
      if (actionButton) {
        event.preventDefault();
        event.stopPropagation();
        var kind = actionButton.getAttribute("data-fs-action");
        var name = actionButton.getAttribute("data-fs-machine");
        if (kind === "delete") {
          confirmDelete(actionButton, name);
        } else {
          runLifecycle(actionButton, name, kind, false);
        }
        return;
      }

      var pullButton = event.target.closest("[data-fs-pull]");
      if (pullButton) {
        event.preventDefault();
        event.stopPropagation();
        runPull(pullButton, pullButton.getAttribute("data-fs-pull"));
        return;
      }

      var pullInputButton = event.target.closest("[data-fs-pull-input]");
      if (pullInputButton) {
        event.preventDefault();
        event.stopPropagation();
        var field = qs("#fs-pull-ref");
        var reference = field && field.value.trim();
        if (reference) {
          runPull(pullInputButton, reference);
        } else if (field) {
          field.focus();
        }
        return;
      }

      var closeButton = event.target.closest("[data-fs-dialog-close]");
      if (closeButton) {
        var dialog = closeButton.closest("dialog");
        if (dialog) {
          dialog.close();
        }
        /* A palette entry closes the palette but must still navigate, so it
         * is not stopped here. */
        if (closeButton.hasAttribute("data-fs-palette-item")) {
          return;
        }
        event.preventDefault();
        event.stopPropagation();
        return;
      }

      var searchButton = event.target.closest(".fs-search");
      if (searchButton) {
        event.preventDefault();
        openPalette();
        return;
      }

      var themeButton = event.target.closest("[data-fs-theme]");
      if (themeButton) {
        event.preventDefault();
        toggleTheme(themeButton);
        return;
      }

      var followButton = event.target.closest("[data-fs-follow-toggle]");
      if (followButton) {
        event.preventDefault();
        event.stopPropagation();
        toggleFollow(followButton);
        return;
      }
    },
    true
  );

  /* Whole-row click, without giving up text selection or link semantics: the
   * name cell is a real anchor, and a click anywhere else in the row replays
   * it — unless the user was selecting text, or aimed at something
   * interactive. */
  document.addEventListener("click", function (event) {
    var row = event.target.closest("[data-fs-row]");
    if (!row || event.defaultPrevented) {
      return;
    }
    if (event.target.closest("a, button, input, select, textarea, label")) {
      return;
    }
    var selection = window.getSelection();
    if (selection && String(selection).length > 0) {
      return;
    }
    var link = qs("[data-fs-row-link]", row);
    if (link) {
      link.click();
    }
  });

  function toggleTheme(button) {
    var root = document.documentElement;
    var current = root.getAttribute("data-theme");
    if (!current) {
      current = window.matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light";
    }
    var next = current === "dark" ? "light" : "dark";
    root.setAttribute("data-theme", next);
    try {
      window.localStorage.setItem("firestone-theme", next);
    } catch (error) {
      /* A viewer with site data blocked still gets the toggle for this
       * session; only the persistence is lost. */
    }
    var value = qs(".fs-theme-toggle__value", button);
    if (value) {
      value.textContent = next;
    }
  }

  function toggleFollow(button) {
    var view = qs("[data-fs-logview]");
    if (!view) {
      return;
    }
    var on = view.getAttribute("data-fs-follow") !== "true";
    view.setAttribute("data-fs-follow", on ? "true" : "false");
    button.classList.toggle("is-on", on);
    button.setAttribute("aria-pressed", on ? "true" : "false");
    var label = qs("[data-fs-follow-label]", button);
    if (label) {
      label.textContent = on ? "following" : "follow off";
    }
    syncFollowState();
  }

  function confirmDelete(button, name) {
    var dialog = qs("#fs-confirm");
    if (!dialog) {
      runLifecycle(button, name, "delete", false);
      return;
    }
    var target = qs("[data-fs-confirm-name]", dialog);
    if (target) {
      target.textContent = name;
    }
    var force = qs("#fs-confirm-force", dialog);
    if (force) {
      force.checked = false;
    }
    dialog.returnValue = "";
    dialog.showModal();
    dialog.addEventListener(
      "close",
      function () {
        if (dialog.returnValue === "confirm") {
          runLifecycle(button, name, "delete", !!(force && force.checked));
        }
      },
      { once: true }
    );
  }

  document.addEventListener("keydown", function (event) {
    var palette = qs("#fs-palette");
    var inPalette = palette && palette.open;

    if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "k") {
      event.preventDefault();
      openPalette();
      return;
    }

    if (inPalette) {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        paletteMove(1);
      } else if (event.key === "ArrowUp") {
        event.preventDefault();
        paletteMove(-1);
      } else if (event.key === "Enter") {
        event.preventDefault();
        paletteChoose();
      }
      return;
    }

    /* "/" focuses search the way it does in every developer tool, but only
     * when the user is not already typing into something. */
    if (
      event.key === "/" &&
      !event.target.closest("input, textarea, select, [contenteditable]")
    ) {
      event.preventDefault();
      openPalette();
    }
  });

  document.addEventListener("input", function (event) {
    if (event.target.id !== "fs-palette-input") {
      return;
    }
    window.clearTimeout(paletteTimer);
    var value = event.target.value;
    paletteTimer = window.setTimeout(function () {
      loadPalette(value);
    }, PALETTE_DEBOUNCE_MS);
  });

  function loadPalette(query) {
    var results = qs("#fs-palette-results");
    if (!results || !window.htmx) {
      return;
    }
    window.htmx.ajax("GET", "/ui/palette?q=" + encodeURIComponent(query), {
      target: results,
      swap: "innerHTML"
    });
  }

  /* -------------------------------------------------------- htmx wiring -- */

  /* The polling gate. With no 'unsafe-eval' an htmx trigger filter ("[expr]")
   * is not available, so the tick is cancelled here instead: a poll must never
   * repaint a row whose transition the user is currently watching. htmx fires
   * htmx:poll:trigger before each tick and skips the request when it is
   * cancelled, which is exactly the hook a trigger filter would have used. */
  document.body.addEventListener("htmx:poll:trigger", function (event) {
    if (isStreaming()) {
      event.preventDefault();
    }
  });

  document.body.addEventListener("htmx:afterSwap", function () {
    syncFollowState();
    /* A dialog arrives as ordinary swapped-in markup, then opens itself. That
     * keeps the server rendering plain HTML and leaves the modal semantics —
     * focus trapping, inert background, Escape — to the platform. */
    var pending = qs("dialog[data-fs-autoopen]");
    if (pending && !pending.open) {
      pending.removeAttribute("data-fs-autoopen");
      pending.showModal();
      var first = qs("input, select, textarea", pending);
      if (first) {
        first.focus();
      }
    }
  });

  /* A form that posts to the UI clears its own slot on success. */
  document.body.addEventListener("fs:created", function (event) {
    var slot = qs("#fs-dialog-slot");
    if (slot) {
      slot.innerHTML = "";
    }
    var detail = event.detail || {};
    if (detail.name) {
      toast("created " + detail.name, detail.sub, "ok");
    }
    refreshFromServer();
  });

  /* A swap can replace the element a live log was reading into. Stop first so
   * the abandoned reader cannot append into a detached node. */
  document.body.addEventListener("htmx:beforeSwap", function (event) {
    var view = qs("[data-fs-logview]");
    if (view && event.detail.target.contains(view)) {
      stopFollowing();
    }
  });

  document.body.addEventListener("htmx:responseError", function (event) {
    var xhr = event.detail.xhr;
    var envelope;
    try {
      envelope = JSON.parse(xhr.responseText);
    } catch (error) {
      envelope = null;
    }
    var message =
      envelope && envelope.error
        ? envelope.error.message
        : "the request failed with " + xhr.status;
    toast("request failed", message, "fail");
  });

  document.body.addEventListener("htmx:sendError", function () {
    toast(
      "lost the connection",
      "firestone serve is no longer answering on this socket",
      "fail"
    );
  });

  /* Server-raised toasts, e.g. after a successful create. */
  document.body.addEventListener("fs:toast", function (event) {
    var detail = event.detail || {};
    toast(detail.title, detail.sub, detail.kind);
  });

  window.addEventListener("beforeunload", stopFollowing);

  /* The server cannot know the viewer's stored theme, so it renders "auto" and
   * the real value is filled in here once. */
  function syncThemeLabel() {
    var value = qs(".fs-theme-toggle__value");
    if (!value) {
      return;
    }
    var chosen = document.documentElement.getAttribute("data-theme");
    value.textContent = chosen || "auto";
  }

  document.addEventListener("DOMContentLoaded", function () {
    syncThemeLabel();
    syncFollowState();
  });

  /* =========================================================== ansi logs ==
   *
   * The server keeps exactly one escape family alive in log text: SGR, the
   * ESC [ … m sequences that only paint. Everything else — cursor moves, OSC
   * title and clipboard writes, DCS — is already one U+FFFD by the time these
   * bytes exist, so this code never has to defend against an escape it does
   * not understand. It turns the surviving colour into classes, because the
   * served Content-Security-Policy forbids inline style attributes.
   *
   * Rendering is line-oriented. A completed line becomes an immutable node
   * and is never touched again, so following a busy log stays cheap; only the
   * partial trailing line is re-rendered as bytes arrive.
   * ==================================================================== */

  var ANSI_ESC = "\u001b";
  /* A folded line wider than this is a runaway, not output someone reads. */
  var ANSI_MAX_COLUMNS = 8192;

  function freshSgrState() {
    return {
      bold: false,
      dim: false,
      italic: false,
      underline: false,
      invert: false,
      strike: false,
      fg: null,
      bg: null
    };
  }

  function copySgrState(state) {
    return {
      bold: state.bold,
      dim: state.dim,
      italic: state.italic,
      underline: state.underline,
      invert: state.invert,
      strike: state.strike,
      fg: state.fg,
      bg: state.bg
    };
  }

  /* Index of the final "m" when text[start] opens an SGR sequence, else -1. */
  function sgrEnd(text, start) {
    if (text.charAt(start) !== ANSI_ESC || text.charAt(start + 1) !== "[") {
      return -1;
    }
    for (var i = start + 2; i < text.length; i++) {
      var code = text.charCodeAt(i);
      if (code === 109 /* m */) {
        return i;
      }
      var digit = code >= 48 && code <= 57;
      if (!digit && code !== 59 /* ; */) {
        return -1;
      }
    }
    return -1;
  }

  function sgrParams(body) {
    if (!body) {
      /* ESC[m is ESC[0m. */
      return [0];
    }
    return body.split(";").map(function (part) {
      return part === "" ? 0 : parseInt(part, 10);
    });
  }

  /* Extended colour (38/48) is consumed and discarded: the palette is the
   * sixteen themed tokens, and a truecolour triple has no token. Consuming it
   * is what matters — otherwise its arguments would be read as attributes. */
  function applySgr(state, params) {
    for (var i = 0; i < params.length; i++) {
      var code = params[i];
      if (code === 38 || code === 48) {
        var mode = params[i + 1];
        i += mode === 5 ? 2 : mode === 2 ? 4 : 1;
        continue;
      }
      if (code === 0) {
        var cleared = freshSgrState();
        state.bold = cleared.bold;
        state.dim = cleared.dim;
        state.italic = cleared.italic;
        state.underline = cleared.underline;
        state.invert = cleared.invert;
        state.strike = cleared.strike;
        state.fg = cleared.fg;
        state.bg = cleared.bg;
      } else if (code === 1) {
        state.bold = true;
      } else if (code === 2) {
        state.dim = true;
      } else if (code === 3) {
        state.italic = true;
      } else if (code === 4) {
        state.underline = true;
      } else if (code === 7) {
        state.invert = true;
      } else if (code === 9) {
        state.strike = true;
      } else if (code === 22) {
        state.bold = false;
        state.dim = false;
      } else if (code === 23) {
        state.italic = false;
      } else if (code === 24) {
        state.underline = false;
      } else if (code === 27) {
        state.invert = false;
      } else if (code === 29) {
        state.strike = false;
      } else if (code === 39) {
        state.fg = null;
      } else if (code === 49) {
        state.bg = null;
      } else if (code >= 30 && code <= 37) {
        state.fg = code - 30;
      } else if (code >= 40 && code <= 47) {
        state.bg = code - 40;
      } else if (code >= 90 && code <= 97) {
        state.fg = code - 90 + 8;
      } else if (code >= 100 && code <= 107) {
        state.bg = code - 100 + 8;
      }
    }
  }

  function sgrClasses(state) {
    var classes = [];
    if (state.bold) {
      classes.push("fs-ansi-bold");
    }
    if (state.dim) {
      classes.push("fs-ansi-dim");
    }
    if (state.italic) {
      classes.push("fs-ansi-italic");
    }
    if (state.underline) {
      classes.push("fs-ansi-underline");
    }
    if (state.strike) {
      classes.push("fs-ansi-strike");
    }
    var fg = state.fg;
    var bg = state.bg;
    if (state.invert) {
      /* Inverse is a swap, and CSS cannot swap two custom properties. It is
       * done here; the class carries the default-on-default case. */
      classes.push("fs-ansi-invert");
      var swapped = fg;
      fg = bg;
      bg = swapped;
    }
    if (fg !== null) {
      classes.push("fs-ansi-fg-" + fg);
    }
    if (bg !== null) {
      classes.push("fs-ansi-bg-" + bg);
    }
    return classes;
  }

  /* Splits text into lines of runs, carrying attributes across lines the way
   * a terminal does. `state` is mutated, so a caller streaming chunk by chunk
   * can keep it between calls. */
  function sgrScan(text, state) {
    var lines = [];
    var runs = [];
    var buffer = "";
    var classes = sgrClasses(state);

    function flushRun() {
      if (buffer !== "") {
        runs.push({ text: buffer, classes: classes });
        buffer = "";
      }
    }

    function endLine() {
      flushRun();
      lines.push(runs);
      runs = [];
    }

    var i = 0;
    while (i < text.length) {
      var character = text.charAt(i);
      if (character === "\n") {
        endLine();
        i += 1;
        continue;
      }
      if (character === ANSI_ESC) {
        var end = sgrEnd(text, i);
        if (end < 0) {
          /* The server does not emit this; if it ever did, it would be shown
           * the same way the server shows a sequence it refuses. */
          buffer += "\ufffd";
          i += 1;
          continue;
        }
        flushRun();
        applySgr(state, sgrParams(text.slice(i + 2, end)));
        classes = sgrClasses(state);
        i = end + 1;
        continue;
      }
      buffer += character;
      i += 1;
    }
    endLine();
    return lines;
  }

  /* Pure: text in, per-line runs out. Each run is { text, classes }. */
  function parseSgr(text) {
    return sgrScan(text, freshSgrState());
  }

  function isSgrReset(params) {
    for (var i = 0; i < params.length; i++) {
      if (params[i] === 0) {
        return true;
      }
    }
    return false;
  }

  /* Last writer wins, per column. A progress line rewrites itself with \r
   * rather than a newline, so the raw text holds every intermediate state; a
   * terminal shows only the final one, and so must this.
   *
   * SGR sequences occupy no column, so each written cell remembers the
   * attributes in force when it was written and the folded line re-emits them
   * only where they change. */
  function foldCarriageReturns(line) {
    if (line.indexOf("\r") < 0) {
      return line;
    }
    var cells = [];
    var column = 0;
    var active = "";
    var i = 0;
    while (i < line.length) {
      var character = line.charAt(i);
      if (character === "\r") {
        column = 0;
        i += 1;
        continue;
      }
      if (character === ANSI_ESC) {
        var end = sgrEnd(line, i);
        if (end >= 0) {
          var sequence = line.slice(i, end + 1);
          active = isSgrReset(sgrParams(line.slice(i + 2, end)))
            ? sequence
            : active + sequence;
          i = end + 1;
          continue;
        }
      }
      if (column < ANSI_MAX_COLUMNS) {
        cells[column] = { sgr: active, ch: character };
      }
      column += 1;
      i += 1;
    }

    var out = "";
    var previous = "";
    for (var c = 0; c < cells.length; c++) {
      var cell = cells[c] || { sgr: previous, ch: " " };
      if (cell.sgr !== previous) {
        out += ANSI_ESC + "[0m" + cell.sgr;
        previous = cell.sgr;
      }
      out += cell.ch;
    }
    if (active !== previous) {
      out += ANSI_ESC + "[0m" + active;
    }
    return out;
  }

  /* ------------------------------------------------------- log renderer -- */

  var logRenderers = new WeakMap();

  function runNode(run) {
    if (!run.classes.length) {
      return document.createTextNode(run.text);
    }
    var span = document.createElement("span");
    span.className = run.classes.join(" ");
    span.textContent = run.text;
    return span;
  }

  function fillLine(node, runs) {
    for (var i = 0; i < runs.length; i++) {
      node.appendChild(runNode(runs[i]));
    }
  }

  function createLogRenderer(view) {
    /* Attributes in force at the start of the partial line. */
    var state = freshSgrState();
    var pendingText = "";
    var pendingNode = null;

    function ensurePending() {
      if (!pendingNode || pendingNode.parentNode !== view) {
        pendingNode = el("span", "fs-logline");
        view.appendChild(pendingNode);
      }
    }

    function renderPending() {
      var folded = foldCarriageReturns(pendingText);
      /* A progress line that never ends is one line for as long as it runs.
       * The folded form is what it means, so keep that and drop the history
       * of everything it overwrote. */
      if (folded.length < pendingText.length) {
        pendingText = folded;
      }
      /* A copy: the partial line is not finished, so it must not advance the
       * state the next completed line starts from. */
      var draft = copySgrState(state);
      var runs = sgrScan(folded, draft)[0] || [];
      pendingNode.textContent = "";
      fillLine(pendingNode, runs);
    }

    function push(chunk) {
      if (!chunk) {
        return;
      }
      ensurePending();
      var parts = (pendingText + chunk).split("\n");
      pendingText = parts.pop();
      for (var i = 0; i < parts.length; i++) {
        var runs = sgrScan(foldCarriageReturns(parts[i]), state)[0] || [];
        var line = el("span", "fs-logline");
        fillLine(line, runs);
        view.insertBefore(line, pendingNode);
        view.insertBefore(document.createTextNode("\n"), pendingNode);
      }
      renderPending();
    }

    function reset(text) {
      state = freshSgrState();
      pendingText = "";
      pendingNode = null;
      view.textContent = "";
      push(text);
    }

    return { push: push, reset: reset };
  }

  function logRendererFor(view) {
    var renderer = logRenderers.get(view);
    if (!renderer) {
      renderer = createLogRenderer(view);
      logRenderers.set(view, renderer);
    }
    return renderer;
  }

  /* The server renders the first screenful as plain text inside the <pre>.
   * Read it once, clear it, and put it back through the same renderer the
   * follow stream uses, so both paths produce identical DOM. */
  function ensureLogViewRendered(view) {
    if (logRenderers.has(view)) {
      return;
    }
    var text = view.textContent;
    logRendererFor(view).reset(text);
  }

  function appendLogChunk(view, chunk) {
    ensureLogViewRendered(view);
    logRendererFor(view).push(chunk);
  }

  function renderLogViews() {
    var view = qs("[data-fs-logview]");
    if (!view) {
      return;
    }
    var wasPinned =
      view.scrollHeight - view.scrollTop - view.clientHeight < 24 ||
      view.scrollTop === 0;
    ensureLogViewRendered(view);
    if (wasPinned) {
      view.scrollTop = view.scrollHeight;
    }
  }

  document.addEventListener("DOMContentLoaded", renderLogViews);
  document.body.addEventListener("htmx:afterSwap", renderLogViews);

  /* Exposed for the sake of being testable in isolation; nothing else reads
   * them. */
  window.firestoneAnsi = {
    parseSgr: parseSgr,
    foldCarriageReturns: foldCarriageReturns
  };
})();
