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

  /* The shape every failure in this file is reduced to, so one branch can
   * report a rejected fetch, a torn stream and a real ErrorEnvelope the same
   * way. It is deliberately the server's own envelope shape: nothing
   * downstream has to know which of the two it is looking at. */
  function errorEnvelope(message, hint) {
    return {
      error: {
        kind: "generic",
        message: String(message),
        hint: hint === undefined ? null : hint
      }
    };
  }

  /* The slot a screen renders for the dialogs it opens. The palette can reach
   * a dialog from a screen that has no slot of its own, so one is created on
   * demand rather than assumed; without this a palette command would open
   * nothing and say nothing. */
  function dialogSlot() {
    var slot = qs("#fs-dialog-slot");
    if (!slot) {
      slot = el("div");
      slot.id = "fs-dialog-slot";
      document.body.appendChild(slot);
    }
    return slot;
  }

  function clearDialogSlot() {
    var slot = qs("#fs-dialog-slot");
    if (slot) {
      slot.innerHTML = "";
    }
  }

  /* Loads a `/ui` read fragment into the dialog slot. Every dialog the UI
   * opens from a button or from the palette comes through here, so they all
   * arrive the same way and open through the same htmx:afterSwap hook. */
  function openDialogFragment(url) {
    if (!window.htmx) {
      window.location.assign(url);
      return;
    }
    window.htmx.ajax("GET", url, { target: dialogSlot(), swap: "innerHTML" });
  }

  /* One confirm flow for every `<dialog>` the shell renders. `prepare` fills
   * it in; `run` fires only for the confirm button's value, so Escape, the
   * backdrop and Cancel are indistinguishable from never having opened it —
   * which is what makes the six dialogs behave identically. A dialog that is
   * somehow absent must not silently swallow the command, so the fallback
   * runs it: the server is still the one that decides. */
  function withDialog(id, prepare, run) {
    var dialog = qs(id);
    if (!dialog) {
      run(null);
      return;
    }
    prepare(dialog);
    dialog.returnValue = "";
    dialog.showModal();
    focusFirstControl(dialog);
    dialog.addEventListener(
      "close",
      function () {
        if (dialog.returnValue === "confirm") {
          run(dialog);
        }
      },
      { once: true }
    );
  }

  /* A modal opens on its first editable control when it has one, and on its
   * confirm button when it does not, so the keyboard never lands nowhere. */
  function focusFirstControl(root) {
    var first = qs(
      "input:not([type='hidden']):not([disabled]), select, textarea",
      root
    );
    if (first) {
      first.focus();
      return;
    }
    var confirm = qs("[value='confirm']", root);
    if (confirm) {
      confirm.focus();
    }
  }

  function setText(root, selector, value) {
    var node = qs(selector, root);
    if (node) {
      node.textContent = value;
    }
    return node;
  }

  function show(node, visible) {
    if (node) {
      node.hidden = !visible;
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
        failure = errorEnvelope(
          "the server returned " + response.status,
          "check that firestone serve is still running"
        );
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

  /* Toast phrasing, in one place, because nine feature surfaces written apart
   * from each other otherwise each invent their own (SPEC §16.5):
   *
   *   - a completed mutation is "<past-tense verb> <subject>", lower case,
   *     with the server's own terminal record on the sub-line;
   *   - a rejected mutation is "<subject> · <verb> failed", with the
   *     ErrorEnvelope's message on the sub-line.
   *
   * Nothing else raises a toast. A field problem belongs beside its field and
   * a conflict beside the button that hit it. */
  function toastDone(title, sub) {
    toast(title, sub, "ok");
  }

  function toastFailed(subject, verb, message) {
    toast(subject + " · " + verb + " failed", message, "fail");
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

  /* `done` is the past-tense verb the success toast is built from, so the four
   * lifecycle commands read the same way as every other mutation in the UI. */
  var ACTIONS = {
    start: {
      path: "/start",
      method: "POST",
      transitional: "starting",
      busy: "Starting…",
      done: "started"
    },
    stop: {
      path: "/stop",
      method: "POST",
      transitional: "stopping",
      busy: "Stopping…",
      done: "stopped"
    },
    restart: {
      path: "/restart",
      method: "POST",
      transitional: "starting",
      busy: "Restarting…",
      done: "restarted"
    },
    delete: {
      path: "",
      method: "DELETE",
      transitional: "stopping",
      busy: "Removing…",
      done: "removed"
    }
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
      toastFailed(name, kind, String(error));
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
        toastFailed(name, kind, terminal.error.message);
      }
    } else if (kind === "delete") {
      fadeOutRow(name);
      toastDone("removed " + name, compactJson(terminal));
      endTransition(button, name, { skipRefresh: onDetailPageFor(name) });
      leaveDeletedMachine(name);
      return;
    } else if (terminal) {
      toastDone(
        action.done + " " + name,
        "finished in " + formatMs(elapsed) + " · " + compactJson(terminal)
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
      terminal = errorEnvelope(error);
    }

    markStreaming("image:" + reference, false);
    if (isErrorEnvelope(terminal)) {
      toastFailed(reference, "pull", terminal.error.message);
    } else {
      toastDone("pulled " + reference, compactJson(terminal));
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
        state.textContent = "downloading " + formatByteSize(record.done);
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
    /* `summary` and `details` are here for the row's overflow menu: opening a
     * menu is not a request to open the machine. */
    if (
      event.target.closest(
        "a, button, input, select, textarea, label, summary, details"
      )
    ) {
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

  /* The delete confirm goes through the same shared dialog helper the other
   * five confirms use, so Escape, the backdrop and Cancel mean the same thing
   * here as they do everywhere else. */
  function confirmDelete(button, name) {
    withDialog(
      "#fs-confirm",
      function (dialog) {
        setText(dialog, "[data-fs-confirm-name]", name);
        var force = qs("#fs-confirm-force", dialog);
        if (force) {
          force.checked = false;
        }
      },
      function (dialog) {
        var force = dialog && qs("#fs-confirm-force", dialog);
        runLifecycle(button, name, "delete", !!(force && force.checked));
      }
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
     * focus trapping, inert background, Escape — to the platform. Focus lands
     * through the same helper the shell's own dialogs use. */
    var pending = qs("dialog[data-fs-autoopen]");
    if (pending && !pending.open) {
      pending.removeAttribute("data-fs-autoopen");
      pending.showModal();
      focusFirstControl(pending);
    }
  });

  /* A form that posts to the UI clears its own slot on success. */
  document.body.addEventListener("fs:created", function (event) {
    clearDialogSlot();
    var detail = event.detail || {};
    if (detail.name) {
      toastDone("created " + detail.name, detail.sub);
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

  /* ==========================================================================
   * SECTION: create-dialog field composition                        (M6-23)
   * ==========================================================================
   *
   * The create dialog offers friendly controls — an image listbox, a number
   * plus a unit, repeatable forward and mount rows — over the exact fields the
   * server already parses. Nothing here invents a second grammar: every
   * control composes into `image`, `memory`, `disk`, `forward` or `mounts`,
   * and those strings are the ones POST /ui/machines reads, character for
   * character, the same way `firestone create` reads its flags.
   *
   * Two consequences follow, and both are deliberate:
   *
   *   - A raw value the composer cannot round-trip is never rewritten. The
   *     group marks itself raw-only, reveals the text field, and lets the
   *     server answer, rather than quietly dropping a forward it could not
   *     parse.
   *   - Visibility is the `hidden` attribute, never a style. The CSP has no
   *     'unsafe-inline'.
   */

  var CREATE_FORM = "[data-fs-create-form]";

  function fieldsOf(root, selector) {
    return Array.prototype.slice.call(root.querySelectorAll(selector));
  }

  function valueOf(root, selector) {
    var node = qs(selector, root);
    return node ? node.value.trim() : "";
  }

  function formOf(node) {
    return node && node.closest ? node.closest(CREATE_FORM) : null;
  }

  /* ------------------------------------------------------- image picker -- */

  function pickImage(form, reference, fromCustom) {
    var hidden = qs("[data-fs-picker-value]", form);
    if (hidden) {
      hidden.value = reference;
    }
    if (fromCustom) {
      fieldsOf(form, "[data-fs-picker-option]").forEach(function (radio) {
        radio.checked = false;
      });
      return;
    }
    var custom = qs("[data-fs-picker-custom]", form);
    if (custom) {
      custom.value = "";
    }
  }

  /* Re-reads the picker after a pull so the entry that was just fetched shows
   * its cached badge and size. The fragment is a read and knows nothing about
   * this form, so whatever the user had chosen or typed is stashed across the
   * swap and put back. */
  function refreshImagePicker(form) {
    var list = qs("[data-fs-picker-list]", form);
    if (!list || !window.htmx) {
      return Promise.resolve();
    }
    var hidden = qs("[data-fs-picker-value]", form);
    var chosen = hidden ? hidden.value : "";
    var typed = valueOf(form, "[data-fs-picker-custom]");

    return Promise.resolve(
      window.htmx.ajax("GET", "/ui/machines/new/images", {
        target: list,
        swap: "innerHTML"
      })
    ).then(function () {
      var custom = qs("[data-fs-picker-custom]", form);
      if (custom) {
        custom.value = typed;
      }
      fieldsOf(form, "[data-fs-picker-option]").forEach(function (radio) {
        radio.checked = !typed && radio.value === chosen;
      });
      if (hidden) {
        hidden.value = chosen;
      }
    });
  }

  /* ---------------------------------------------------------- unit sizes -- */

  /* `G` is GiB and `M` is MiB in the ByteSize grammar, which is why the select
   * says GiB and MiB. Composing 8 + G gives "8G": 8192 MiB, not 8000 MB. */
  function syncSize(field) {
    var amount = qs("[data-fs-size-amount]", field);
    var unit = qs("[data-fs-size-unit]", field);
    var value = qs("[data-fs-size-value]", field);
    if (!amount || !unit || !value) {
      return;
    }
    var text = amount.value.trim();
    value.value = text ? text + unit.value : "";
  }

  /* ------------------------------------------------------- repeated rows -- */

  function addRow(group, templateSelector, hostSelector) {
    var template = qs(templateSelector, group);
    var host = qs(hostSelector, group);
    if (!template || !host) {
      return null;
    }
    var row = template.content.firstElementChild.cloneNode(true);
    host.appendChild(row);
    return row;
  }

  /* The repeatable groups' shared empty state: shown only when the group has
   * no rows *and* is not raw-only, because a group that fell back to raw text
   * is not empty — it holds a value these rows could not express. */
  function syncRowsEmpty(group) {
    if (!group) {
      return;
    }
    var note = qs("[data-fs-rows-empty]", group);
    if (!note) {
      return;
    }
    var rows = fieldsOf(group, "[data-fs-forward-row], [data-fs-mount-row]");
    note.hidden = rows.length > 0 || group.hasAttribute("data-fs-raw-only");
  }

  function setRawOnly(group, rawOnly, rawSelector) {
    var raw = qs(rawSelector, group);
    group.toggleAttribute("data-fs-raw-only", rawOnly);
    if (rawOnly && raw) {
      raw.hidden = false;
      var toggle = qs("[data-fs-raw-toggle]", group);
      if (toggle) {
        toggle.setAttribute("aria-pressed", "true");
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
  /* Splits `[proto:][bind:]HOST:GUEST` the way the server does: the guest is
   * the last colon-separated field and the host the one before it, so an IPv6
   * literal in brackets survives untouched. */
  function splitForward(token) {
    var guestAt = token.lastIndexOf(":");
    if (guestAt < 0) {
      return null;
    }
    var guest = token.slice(guestAt + 1);
    var head = token.slice(0, guestAt);
    var hostAt = head.lastIndexOf(":");
    var host = hostAt < 0 ? head : head.slice(hostAt + 1);
    var bind = hostAt < 0 ? "" : head.slice(0, hostAt);
    var proto = "tcp";
    if (bind === "tcp" || bind === "udp") {
      proto = bind;
      bind = "";
    } else if (bind.indexOf("tcp:") === 0 || bind.indexOf("udp:") === 0) {
      proto = bind.slice(0, 3);
      bind = bind.slice(4);
    }
    if (!host || !guest) {
      return null;
    }
    return { proto: proto, bind: bind, host: host, guest: guest };
  }

  function hydrateForwards(group) {
    var raw = qs("[data-fs-forward-value]", group);
    var host = qs("[data-fs-forward-rows]", group);
    if (!raw || !host) {
      return;
    }
    host.textContent = "";
    var complete = true;
    raw.value
      .split(",")
      .map(function (token) {
        return token.trim();
      })
      .filter(function (token) {
        return token.length > 0;
      })
      .forEach(function (token) {
        var parsed = splitForward(token);
        if (!parsed) {
          complete = false;
          return;
        }
        var row = addRow(
          group,
          "[data-fs-forward-template]",
          "[data-fs-forward-rows]"
        );
        if (!row) {
          return;
        }
        qs("[data-fs-forward-proto]", row).value = parsed.proto;
        qs("[data-fs-forward-bind]", row).value = parsed.bind;
        qs("[data-fs-forward-host]", row).value = parsed.host;
        qs("[data-fs-forward-guest]", row).value = parsed.guest;
      });
    if (!complete) {
      host.textContent = "";
    }
    setRawOnly(group, !complete, "[data-fs-forward-value]");
    syncRowsEmpty(group);
  }

  function composeForwards(group) {
    if (group.hasAttribute("data-fs-raw-only")) {
      return;
    }
    var raw = qs("[data-fs-forward-value]", group);
    if (!raw) {
      return;
    }
    var parts = [];
    fieldsOf(group, "[data-fs-forward-row]").forEach(function (row) {
      var host = valueOf(row, "[data-fs-forward-host]");
      var guest = valueOf(row, "[data-fs-forward-guest]");
      if (!host && !guest) {
        return;
      }
      var prefix = "";
      if (valueOf(row, "[data-fs-forward-proto]") === "udp") {
        prefix += "udp:";
      }
      var bind = valueOf(row, "[data-fs-forward-bind]");
      if (bind) {
        prefix += bind + ":";
      }
      parts.push(prefix + host + ":" + guest);
    });
    raw.value = parts.join(", ");
  }

  function splitMount(line) {
    var parts = line.split(":");
    if (parts.length === 2 && parts[0] && parts[1]) {
      return { host: parts[0], guest: parts[1], readonly: false };
    }
    if (parts.length === 3 && parts[0] && parts[1] && parts[2] === "ro") {
      return { host: parts[0], guest: parts[1], readonly: true };
    }
    return null;
  }

  function hydrateMounts(group) {
    var raw = qs("[data-fs-mount-value]", group);
    var host = qs("[data-fs-mount-rows]", group);
    if (!raw || !host) {
      return;
    }
    host.textContent = "";
    var complete = true;
    raw.value
      .split("\n")
      .map(function (line) {
        return line.trim();
      })
      .filter(function (line) {
        return line.length > 0;
      })
      .forEach(function (line) {
        var parsed = splitMount(line);
        if (!parsed) {
          complete = false;
          return;
        }
        var row = addRow(
          group,
          "[data-fs-mount-template]",
          "[data-fs-mount-rows]"
        );
        if (!row) {
          return;
        }
        qs("[data-fs-mount-host]", row).value = parsed.host;
        qs("[data-fs-mount-guest]", row).value = parsed.guest;
        qs("[data-fs-mount-ro]", row).checked = parsed.readonly;
      });
    if (!complete) {
      host.textContent = "";
    }
    setRawOnly(group, !complete, "[data-fs-mount-value]");
    syncRowsEmpty(group);
  }

  function composeMounts(group) {
    if (group.hasAttribute("data-fs-raw-only")) {
      return;
    }
    var raw = qs("[data-fs-mount-value]", group);
    if (!raw) {
      return;
    }
    var lines = [];
    fieldsOf(group, "[data-fs-mount-row]").forEach(function (row) {
      var host = valueOf(row, "[data-fs-mount-host]");
      var guest = valueOf(row, "[data-fs-mount-guest]");
      if (!host && !guest) {
        return;
      }
      var readonly = qs("[data-fs-mount-ro]", row);
      lines.push(host + ":" + guest + (readonly && readonly.checked ? ":ro" : ""));
    });
    raw.value = lines.join("\n");
  }

  /* ------------------------------------------------------------- network -- */

  function syncNetMode(form) {
    var select = qs("[data-fs-net-mode]", form);
    var field = qs("[data-fs-tap-field]", form);
    if (select && field) {
      field.hidden = select.value !== "tap";
    }
  }

  /* --------------------------------------------------------- dialog init -- */

  function initCreateForm(form) {
    if (form.hasAttribute("data-fs-ready")) {
      return;
    }
    form.setAttribute("data-fs-ready", "");
    var forwards = qs("[data-fs-forwards]", form);
    if (forwards) {
      hydrateForwards(forwards);
    }
    var mounts = qs("[data-fs-mounts]", form);
    if (mounts) {
      hydrateMounts(mounts);
    }
    syncNetMode(form);
  }

  /* Composed once more on the way out, so a value edited without firing a
   * change event — an autofilled row, a programmatic edit — still reaches the
   * server in canonical form. */
  function composeCreateForm(form) {
    var forwards = qs("[data-fs-forwards]", form);
    if (forwards) {
      composeForwards(forwards);
    }
    var mounts = qs("[data-fs-mounts]", form);
    if (mounts) {
      composeMounts(mounts);
    }
    fieldsOf(form, "[data-fs-size][data-fs-touched]").forEach(syncSize);
  }

  document.body.addEventListener("htmx:afterSwap", function () {
    var form = qs(CREATE_FORM);
    if (form) {
      initCreateForm(form);
    }
  });

  /* Capture at the document, so the composition runs before htmx's own submit
   * listener on the form gathers the parameters. */
  document.addEventListener(
    "submit",
    function (event) {
      var form = formOf(event.target);
      if (form) {
        composeCreateForm(form);
      }
    },
    true
  );

  document.addEventListener("click", function (event) {
    var form = formOf(event.target);
    if (!form) {
      return;
    }

    var raw = event.target.closest("[data-fs-raw-toggle]");
    if (raw) {
      event.preventDefault();
      var group = raw.closest("[data-fs-forwards], [data-fs-mounts]");
      var field = group && qs("[data-fs-forward-value], [data-fs-mount-value]", group);
      if (field) {
        field.hidden = !field.hidden;
        raw.setAttribute("aria-pressed", field.hidden ? "false" : "true");
      }
      return;
    }

    if (event.target.closest("[data-fs-forward-add]")) {
      event.preventDefault();
      var forwards = qs("[data-fs-forwards]", form);
      if (forwards) {
        setRawOnly(forwards, false, "[data-fs-forward-value]");
        addRow(forwards, "[data-fs-forward-template]", "[data-fs-forward-rows]");
        composeForwards(forwards);
        syncRowsEmpty(forwards);
      }
      return;
    }

    if (event.target.closest("[data-fs-mount-add]")) {
      event.preventDefault();
      var mounts = qs("[data-fs-mounts]", form);
      if (mounts) {
        setRawOnly(mounts, false, "[data-fs-mount-value]");
        addRow(mounts, "[data-fs-mount-template]", "[data-fs-mount-rows]");
        composeMounts(mounts);
        syncRowsEmpty(mounts);
      }
      return;
    }

    var remove = event.target.closest("[data-fs-row-remove]");
    if (remove) {
      event.preventDefault();
      var owner = remove.closest("[data-fs-forwards], [data-fs-mounts]");
      var row = remove.closest("[data-fs-forward-row], [data-fs-mount-row]");
      if (row) {
        row.remove();
      }
      if (owner) {
        composeForwards(owner);
        composeMounts(owner);
        syncRowsEmpty(owner);
      }
    }
  });

  /* The in-dialog pull reuses the same NDJSON machinery the catalog uses, then
   * re-reads the picker so the entry it just fetched reports its real size
   * instead of still offering a pull. */
  document.addEventListener(
    "click",
    function (event) {
      var button = event.target.closest("[data-fs-pull-picker]");
      if (!button) {
        return;
      }
      event.preventDefault();
      event.stopPropagation();
      var form = formOf(button);
      runPull(button, button.getAttribute("data-fs-pull-picker")).then(
        function () {
          if (form) {
            refreshImagePicker(form);
          }
        }
      );
    },
    true
  );

  document.addEventListener("change", function (event) {
    var form = formOf(event.target);
    if (!form) {
      return;
    }
    if (event.target.closest("[data-fs-net-mode]")) {
      syncNetMode(form);
      return;
    }
    if (event.target.closest("[data-fs-picker-option]")) {
      pickImage(form, event.target.value, false);
      return;
    }
    var rawField = event.target.closest(
      "[data-fs-forward-value], [data-fs-mount-value]"
    );
    if (rawField) {
      var group = rawField.closest("[data-fs-forwards], [data-fs-mounts]");
      if (group) {
        setRawOnly(group, false, "[data-fs-forward-value], [data-fs-mount-value]");
        hydrateForwards(group);
        hydrateMounts(group);
        syncRowsEmpty(group);
      }
      return;
    }
    var mountGroup = event.target.closest("[data-fs-mounts]");
    if (mountGroup) {
      composeMounts(mountGroup);
    }
    var forwardGroup = event.target.closest("[data-fs-forwards]");
    if (forwardGroup) {
      composeForwards(forwardGroup);
    }
  });

  document.addEventListener("input", function (event) {
    var form = formOf(event.target);
    if (!form) {
      return;
    }
    var custom = event.target.closest("[data-fs-picker-custom]");
    if (custom) {
      pickImage(form, custom.value.trim(), true);
      return;
    }
    var size = event.target.closest("[data-fs-size]");
    if (size) {
      size.setAttribute("data-fs-touched", "");
      syncSize(size);
      return;
    }
    var forwardGroup = event.target.closest("[data-fs-forwards]");
    if (forwardGroup && !event.target.closest("[data-fs-forward-value]")) {
      composeForwards(forwardGroup);
    }
    var mountGroup = event.target.closest("[data-fs-mounts]");
    if (mountGroup && !event.target.closest("[data-fs-mount-value]")) {
      composeMounts(mountGroup);
    }
  });

  /* ==========================================================================
   * SECTION: live utilization                                        (M6-25)
   * ==========================================================================
   *
   * `GET /v1/machines/{name}/metrics` is one sample of cumulative counters.
   * Firestone runs no metrics daemon and stores no time series, so the rates a
   * person actually wants — per cent of a vCPU, bytes per second off a disk —
   * exist only as the difference between two samples. That difference is
   * derived here, in the client, and the history lives in a 60-sample ring
   * buffer in this tab. Nothing is persisted; reloading the page starts over,
   * which is the honest consequence of not storing history on the host.
   *
   * Three rules run through the whole section:
   *
   *   - A `null` counter means *absent*, never zero. A device that does not
   *     report `read_bytes` is left out of the sum instead of being counted as
   *     idle, and a figure with no sample behind it renders as an em dash.
   *   - A counter that goes backwards is a restarted VMM, not negative work.
   *     That pair is dropped rather than drawn as a spike.
   *   - No chart library. Every sparkline is a `<polyline>` whose `points`
   *     attribute is written from a pure function, and every colour is a
   *     class: the CSP has no 'unsafe-inline', so nothing here writes a style
   *     attribute.
   *
   * The derivation functions take samples and return numbers. They touch no
   * DOM, so they can be read — and tested — on their own.
   */

  /* Detail page cadence. Fast enough to feel live, slow enough that the poll
   * itself is not what the machine is busy doing. */
  var METRICS_DETAIL_MS = 3000;
  /* Overview cadence, and the cap on how many rows poll at all. Eight running
   * machines at five seconds is the bound on request fan-out from one glance
   * at the overview; the rest of the fleet reports status only. */
  var METRICS_OVERVIEW_MS = 5000;
  var METRICS_OVERVIEW_CAP = 8;
  /* Backoff after a 409 (the machine stopped) or a failed read. Polling keeps
   * running slowly so a machine started again recovers on its own. */
  var METRICS_IDLE_MS = 15000;
  /* Ring-buffer depth: 60 samples is three minutes at the detail cadence. */
  var METRICS_SAMPLES = 60;
  /* Sparkline user units. The SVG scales to its box; these are only the
   * coordinate space the points are written in. */
  var SPARK_W = 100;
  var SPARK_H = 28;
  var METER_W = 100;

  /* ----------------------------------------------------------- derivation -- */

  /* A counter Firestone could not read is null in the payload and null here.
   * Anything that is not a finite number is treated the same way. */
  function metricsCounter(value) {
    return typeof value === "number" && isFinite(value) ? value : null;
  }

  /* `sampled_at` is an RFC 3339 timestamp with nanosecond precision. The
   * ECMAScript date grammar specifies exactly three fractional digits, so the
   * tail is trimmed rather than left to a lenient parser; an unparseable
   * timestamp falls back to when the response was read. */
  function metricsTimestamp(text, fallbackMs) {
    if (typeof text !== "string") {
      return fallbackMs;
    }
    var parsed = Date.parse(text.replace(/(\.\d{3})\d+/, "$1"));
    return isFinite(parsed) ? parsed : fallbackMs;
  }

  /* Normalizes one payload into the flat shape the derivations read. */
  function metricsSample(payload, receivedMs) {
    if (!payload || typeof payload !== "object") {
      return null;
    }
    var cpu = payload.cpu || {};
    var memory = payload.memory || {};
    var block = Object.create(null);
    var devices = Array.isArray(payload.block) ? payload.block : [];
    for (var i = 0; i < devices.length; i++) {
      var device = devices[i];
      if (!device || typeof device.device !== "string") {
        continue;
      }
      block[device.device] = {
        read: metricsCounter(device.read_bytes),
        written: metricsCounter(device.written_bytes)
      };
    }
    return {
      atMs: metricsTimestamp(payload.sampled_at, receivedMs),
      vcpus: metricsCounter(cpu.vcpus),
      cpuTimeNs: metricsCounter(cpu.cpu_time_ns),
      rssBytes: metricsCounter(memory.rss_bytes),
      allocatedBytes: metricsCounter(memory.allocated_bytes),
      guestActualBytes: metricsCounter(memory.guest_actual_bytes),
      block: block
    };
  }

  /* Per cent of the machine's allocated vCPUs, from the VMM's cumulative
   * processor time. Clamped to 0-100: the host may schedule the VMM's own
   * threads beyond the guest's share, and a CPU meter reading 140% teaches the
   * reader nothing. Null when either sample lacks the counter — which is every
   * sample on a host without /proc. */
  function metricsCpuPercent(previous, next) {
    if (!previous || !next) {
      return null;
    }
    if (previous.cpuTimeNs === null || next.cpuTimeNs === null) {
      return null;
    }
    var elapsedNs = (next.atMs - previous.atMs) * 1e6;
    if (!(elapsedNs > 0)) {
      return null;
    }
    var delta = next.cpuTimeNs - previous.cpuTimeNs;
    if (delta < 0) {
      return null;
    }
    var vcpus = next.vcpus !== null && next.vcpus > 0 ? next.vcpus : 1;
    return Math.max(0, Math.min(100, (delta / elapsedNs / vcpus) * 100));
  }

  /* Bytes per second summed over every block device that reported the counter
   * in both samples. A device missing from either sample, or reporting null,
   * contributes nothing and is not counted as zero; when no device qualifies
   * the whole figure is absent. */
  function metricsBlockRate(previous, next, key) {
    if (!previous || !next) {
      return null;
    }
    var seconds = (next.atMs - previous.atMs) / 1000;
    if (!(seconds > 0)) {
      return null;
    }
    var total = 0;
    var seen = false;
    for (var device in next.block) {
      if (!Object.prototype.hasOwnProperty.call(next.block, device)) {
        continue;
      }
      var before = previous.block[device];
      var after = next.block[device];
      if (!before || !after || before[key] === null || after[key] === null) {
        continue;
      }
      var delta = after[key] - before[key];
      if (delta < 0) {
        continue;
      }
      total += delta;
      seen = true;
    }
    return seen ? total / seconds : null;
  }

  /* Every series the tiles draw, derived from one ring buffer in one pass.
   * Rates come from adjacent pairs, so a buffer of n samples yields at most
   * n - 1 rate points; levels come from the samples themselves. */
  function metricsSeries(samples) {
    var series = { cpu: [], read: [], write: [], rss: [], guest: [] };
    for (var i = 0; i < samples.length; i++) {
      if (samples[i].rssBytes !== null) {
        series.rss.push(samples[i].rssBytes);
      }
      if (samples[i].guestActualBytes !== null) {
        series.guest.push(samples[i].guestActualBytes);
      }
      if (i === 0) {
        continue;
      }
      var cpu = metricsCpuPercent(samples[i - 1], samples[i]);
      if (cpu !== null) {
        series.cpu.push(cpu);
      }
      var read = metricsBlockRate(samples[i - 1], samples[i], "read");
      if (read !== null) {
        series.read.push(read);
      }
      var written = metricsBlockRate(samples[i - 1], samples[i], "written");
      if (written !== null) {
        series.write.push(written);
      }
    }
    return series;
  }

  /* Maps a series onto a polyline inside a w-by-h box, oldest point at the
   * left. The scale is the series maximum unless `max` is given: CPU is always
   * drawn against 0-100 so an idle machine reads as idle, while a byte rate
   * has no natural ceiling and autoscales. An empty series draws nothing
   * rather than a line along the floor. */
  function sparklinePoints(series, w, h, max) {
    if (!series || series.length === 0) {
      return "";
    }
    var fixed = typeof max === "number" && isFinite(max);
    var ceiling = fixed ? max : 0;
    if (!fixed) {
      for (var i = 0; i < series.length; i++) {
        if (series[i] > ceiling) {
          ceiling = series[i];
        }
      }
    }
    if (!(ceiling > 0)) {
      ceiling = 1;
    }
    var span = series.length > 1 ? series.length - 1 : 1;
    var points = [];
    for (var j = 0; j < series.length; j++) {
      var x = series.length > 1 ? (j / span) * w : 0;
      var value = Math.max(0, Math.min(ceiling, series[j]));
      points.push(sparkRound(x) + "," + sparkRound(h - (value / ceiling) * h));
    }
    return points.join(" ");
  }

  function sparkRound(value) {
    return Math.round(value * 100) / 100;
  }

  function seriesMax(series) {
    var max = 0;
    for (var i = 0; i < series.length; i++) {
      if (series[i] > max) {
        max = series[i];
      }
    }
    return max;
  }

  function lastOf(series) {
    return series.length ? series[series.length - 1] : null;
  }

  /* --------------------------------------------------------- formatting -- */

  /* Decimal units with one decimal below a hundred, which is exactly what
   * `view.rs::format_bytes` prints for an image size. One convention for a
   * byte count across the CLI, the REST payloads and this page. */
  function formatByteSize(bytes) {
    if (bytes === null || !isFinite(bytes)) {
      return "—";
    }
    var units = [["GB", 1e9], ["MB", 1e6], ["kB", 1e3], ["B", 1]];
    for (var i = 0; i < units.length; i++) {
      var unit = units[i][0];
      var scale = units[i][1];
      if (bytes >= scale) {
        var whole = Math.floor(bytes / scale);
        if (unit === "B" || whole >= 100) {
          return whole + " " + unit;
        }
        return whole + "." + Math.floor(((bytes % scale) * 10) / scale) + " " + unit;
      }
    }
    return "0 B";
  }

  function formatRate(bytesPerSecond) {
    return bytesPerSecond === null ? "—" : formatByteSize(bytesPerSecond) + "/s";
  }

  function formatPercent(value) {
    return value === null ? "—" : value.toFixed(1) + "%";
  }

  /* ------------------------------------------------------------- polling -- */

  /* One reading: either a sample, or the reason there is none. A 409 is the
   * documented answer for a machine that is not running and is not an error. */
  async function readMetrics(name) {
    var response;
    try {
      response = await fetch(
        "/v1/machines/" + encodeURIComponent(name) + "/metrics",
        { headers: { Accept: "application/json" } }
      );
    } catch (error) {
      return { state: "unavailable" };
    }
    if (response.status === 409) {
      return { state: "idle" };
    }
    if (!response.ok) {
      return { state: "unavailable" };
    }
    var payload;
    try {
      payload = await response.json();
    } catch (error) {
      return { state: "unavailable" };
    }
    var sample = metricsSample(payload, Date.now());
    return sample ? { state: "ok", sample: sample } : { state: "unavailable" };
  }

  function pushSample(samples, sample) {
    samples.push(sample);
    while (samples.length > METRICS_SAMPLES) {
      samples.shift();
    }
  }

  /* ------------------------------------------------------- detail strip -- */

  var detailMetrics = null;

  function stopDetailMetrics() {
    if (!detailMetrics) {
      return;
    }
    detailMetrics.stopped = true;
    window.clearTimeout(detailMetrics.timer);
    detailMetrics = null;
  }

  function startDetailMetrics(strip) {
    stopDetailMetrics();
    var session = {
      name: strip.getAttribute("data-fs-metrics") || "",
      node: strip,
      samples: [],
      timer: null,
      stopped: false
    };
    detailMetrics = session;
    tickDetailMetrics(session);
  }

  async function tickDetailMetrics(session) {
    var reading = await readMetrics(session.name);
    /* The page may have been swapped out while the request was in flight. */
    if (session.stopped || !session.node.isConnected) {
      return;
    }

    var delay = METRICS_DETAIL_MS;
    if (reading.state === "ok") {
      pushSample(session.samples, reading.sample);
    } else {
      /* A stopped machine has no counters to carry forward, and the next run
       * is a new process whose counters start again from zero. */
      session.samples.length = 0;
      delay = METRICS_IDLE_MS;
    }
    renderDetailMetrics(session, reading.state);
    session.timer = window.setTimeout(function () {
      tickDetailMetrics(session);
    }, delay);
  }

  function tileOf(strip, name) {
    return qs('[data-fs-tile="' + name + '"]', strip);
  }

  function setTile(tile, value, sub) {
    if (!tile) {
      return;
    }
    var valueNode = qs("[data-fs-tile-value]", tile);
    var subNode = qs("[data-fs-tile-sub]", tile);
    if (valueNode) {
      valueNode.textContent = value;
    }
    if (subNode) {
      subNode.textContent = sub;
    }
  }

  function setSpark(tile, key, series, max) {
    if (!tile) {
      return;
    }
    var line = qs('[data-fs-spark="' + key + '"]', tile);
    if (line) {
      line.setAttribute("points", sparklinePoints(series, SPARK_W, SPARK_H, max));
    }
  }

  /* The meter is an SVG rect whose width is an attribute, not a style: the CSP
   * forbids the latter in markup, and an attribute needs no exception. */
  function setMeter(tile, used, total) {
    if (!tile) {
      return;
    }
    var fill = qs("[data-fs-meter]", tile);
    if (!fill) {
      return;
    }
    var fraction =
      used === null || total === null || !(total > 0) ? 0 : used / total;
    fill.setAttribute(
      "width",
      String(sparkRound(Math.max(0, Math.min(1, fraction)) * METER_W))
    );
  }

  function renderDetailMetrics(session, state) {
    var strip = session.node;
    var samples = session.samples;
    var latest = samples.length ? samples[samples.length - 1] : null;
    var series = metricsSeries(samples);
    var collecting = samples.length < 2;

    strip.toggleAttribute("data-fs-collecting", collecting);
    var note = qs("[data-fs-metrics-note]", strip);
    if (note) {
      if (state === "idle") {
        note.textContent = "not running";
      } else if (state === "unavailable") {
        note.textContent = "metrics unavailable";
      } else if (collecting) {
        note.textContent = "collecting…";
      } else {
        note.textContent =
          samples.length +
          " samples · " +
          METRICS_DETAIL_MS / 1000 +
          " s poll · rates derived in this tab";
      }
    }

    var cpuTile = tileOf(strip, "cpu");
    var cpu = lastOf(series.cpu);
    setTile(
      cpuTile,
      formatPercent(cpu),
      latest === null
        ? "—"
        : latest.cpuTimeNs === null
          ? "vmm cpu time unavailable on this host"
          : (latest.vcpus === null ? "?" : latest.vcpus) + " vCPU allocated"
    );
    setSpark(cpuTile, "cpu", series.cpu, 100);

    var memoryTile = tileOf(strip, "memory");
    var rss = latest ? latest.rssBytes : null;
    var allocated = latest ? latest.allocatedBytes : null;
    setTile(
      memoryTile,
      formatByteSize(rss),
      rss === null
        ? "vmm rss unavailable on this host"
        : "of " + formatByteSize(allocated) + " allocated"
    );
    setSpark(memoryTile, "rss", series.rss);
    setMeter(memoryTile, rss, allocated);

    var guestTile = tileOf(strip, "guest");
    var guest = latest ? latest.guestActualBytes : null;
    setTile(
      guestTile,
      formatByteSize(guest),
      guest === null
        ? "the guest reports no balloon figure"
        : "of " + formatByteSize(allocated) + " allocated"
    );
    setSpark(guestTile, "guest", series.guest);
    setMeter(guestTile, guest, allocated);

    var diskTile = tileOf(strip, "disk");
    var read = lastOf(series.read);
    var write = lastOf(series.write);
    setTile(
      diskTile,
      formatRate(read) + " / " + formatRate(write),
      "read / write · summed over every block device"
    );
    var diskMax = Math.max(seriesMax(series.read), seriesMax(series.write));
    setSpark(diskTile, "read", series.read, diskMax);
    setSpark(diskTile, "write", series.write, diskMax);
  }

  /* ---------------------------------------------------- overview figures -- */

  /* The overview panel is repolled by htmx every five seconds and morphed in
   * place, which would revert any text written here. The last figure per
   * machine is therefore kept and re-applied after every swap. */
  var overviewMetrics = {
    timer: null,
    previous: Object.create(null),
    text: Object.create(null)
  };

  function overviewCpuCells() {
    return Array.prototype.slice.call(
      document.querySelectorAll("[data-fs-cpu]")
    );
  }

  function stopOverviewMetrics() {
    window.clearTimeout(overviewMetrics.timer);
    overviewMetrics.timer = null;
  }

  function paintOverviewMetrics() {
    overviewCpuCells().forEach(function (cell) {
      var name = cell.getAttribute("data-fs-cpu");
      var text = overviewMetrics.text[name];
      if (text) {
        cell.textContent = text;
      }
    });
  }

  async function tickOverviewMetrics() {
    var names = overviewCpuCells()
      .map(function (cell) {
        return cell.getAttribute("data-fs-cpu");
      })
      .filter(function (name) {
        return !!name;
      })
      .slice(0, METRICS_OVERVIEW_CAP);

    await Promise.all(
      names.map(async function (name) {
        var reading = await readMetrics(name);
        if (reading.state !== "ok") {
          delete overviewMetrics.previous[name];
          delete overviewMetrics.text[name];
          return;
        }
        var previous = overviewMetrics.previous[name];
        overviewMetrics.previous[name] = reading.sample;
        var cpu = metricsCpuPercent(previous, reading.sample);
        if (cpu !== null) {
          overviewMetrics.text[name] = formatPercent(cpu) + " cpu";
        }
      })
    );

    paintOverviewMetrics();
    if (overviewMetrics.timer !== null) {
      overviewMetrics.timer = window.setTimeout(
        tickOverviewMetrics,
        METRICS_OVERVIEW_MS
      );
    }
  }

  function startOverviewMetrics() {
    if (overviewMetrics.timer !== null) {
      return;
    }
    /* A placeholder id so a tick that finishes before the first timeout is
     * scheduled still knows the poller is meant to be running. */
    overviewMetrics.timer = -1;
    tickOverviewMetrics();
  }

  /* ----------------------------------------------------------- lifecycle -- */

  /* The same hooks the log follower uses: htmx owns navigation, so a swap is
   * the only event that tells this page it is looking at something else. */
  function syncMetricsState() {
    var strip = qs("[data-fs-metrics]");
    if (!strip) {
      stopDetailMetrics();
    } else if (!detailMetrics || detailMetrics.node !== strip) {
      startDetailMetrics(strip);
    }

    if (overviewCpuCells().length === 0) {
      stopOverviewMetrics();
    } else {
      paintOverviewMetrics();
      startOverviewMetrics();
    }
  }

  document.body.addEventListener("htmx:beforeSwap", function (event) {
    var strip = qs("[data-fs-metrics]");
    if (strip && event.detail.target.contains(strip)) {
      stopDetailMetrics();
    }
  });

  document.body.addEventListener("htmx:afterSwap", syncMetricsState);
  document.addEventListener("DOMContentLoaded", syncMetricsState);
  window.addEventListener("beforeunload", function () {
    stopDetailMetrics();
    stopOverviewMetrics();
  });

  /* Exposed so the derivations can be exercised in isolation; nothing else
   * reads them. */
  window.firestoneMetrics = {
    metricsSample: metricsSample,
    metricsCpuPercent: metricsCpuPercent,
    metricsBlockRate: metricsBlockRate,
    metricsSeries: metricsSeries,
    sparklinePoints: sparklinePoints,
    formatByteSize: formatByteSize,
    formatRate: formatRate
  };

  /* Provisioning section (M6-27).
   *
   * A self-contained runtime for the create dialog's Provisioning fields, kept
   * apart from the block above so it survives that block being reorganised.
   *
   * It does two things, and deliberately nothing else:
   *
   *   1. Counts the bytes of the inline user-data as it is typed and warns past
   *      32 KiB. This is a courtesy, not a check: shared validation owns the
   *      real limit, the warning never blocks a submission, and the server's
   *      answer is what lands beside the field.
   *   2. Reveals the "provisioning off" warning when the toggle is cleared,
   *      through the hidden attribute, because the CSP forbids inline styles.
   *
   * The password field is untouched here. Nothing in this file reads it, stores
   * it, or copies it anywhere.
   */
  (function () {
    "use strict";

    var SOFT_CAP_BYTES = 32 * 1024;

    function bytesOf(text) {
      /* The cap is a byte cap, not a character cap, so the counter measures the
       * UTF-8 the server will actually receive. */
      if (typeof TextEncoder === "function") {
        return new TextEncoder().encode(text).length;
      }
      return unescape(encodeURIComponent(text)).length;
    }

    function formatBytes(count) {
      if (count < 1024) {
        return count + " B";
      }
      return (count / 1024).toFixed(count < 10240 ? 1 : 0) + " KiB";
    }

    function syncUserData(field) {
      var input = field.querySelector("[data-fs-userdata-value]");
      var note = field.querySelector("[data-fs-userdata-count]");
      var over = field.querySelector("[data-fs-userdata-over]");
      if (!input) {
        return;
      }
      var bytes = bytesOf(input.value);
      if (note) {
        note.textContent = bytes === 0 ? "" : formatBytes(bytes) + " of 32 KiB";
      }
      if (over) {
        over.hidden = bytes <= SOFT_CAP_BYTES;
      }
    }

    function syncProvisioning(section) {
      var toggle = section.querySelector("[data-fs-provisioning-toggle]");
      var warning = section.querySelector("[data-fs-provisioning-warning]");
      if (toggle && warning) {
        warning.hidden = toggle.checked;
      }
    }

    function sync(root) {
      var sections = (root || document).querySelectorAll("[data-fs-provisioning]");
      Array.prototype.forEach.call(sections, function (section) {
        syncProvisioning(section);
        var field = section.querySelector("[data-fs-userdata]");
        if (field) {
          syncUserData(field);
        }
      });
    }

    document.addEventListener("DOMContentLoaded", function () {
      sync(document);
    });

    document.body.addEventListener("htmx:afterSwap", function () {
      sync(document);
    });

    document.addEventListener("input", function (event) {
      var field =
        event.target.closest && event.target.closest("[data-fs-userdata]");
      if (field) {
        syncUserData(field);
      }
    });

    document.addEventListener("change", function (event) {
      if (!event.target.closest) {
        return;
      }
      if (event.target.closest("[data-fs-provisioning-toggle]")) {
        var section = event.target.closest("[data-fs-provisioning]");
        if (section) {
          syncProvisioning(section);
        }
      }
    });
  })();

  /* ========================================================= machine edit ==
   *
   * The edit dialog is the create dialog's fields pointed at a machine that
   * already exists, and it writes the way every other mutation in this UI
   * writes: straight to `/v1`. Two routes, chosen by what actually changed.
   *
   *   - PATCH /v1/machines/{name} takes a *sparse* MachineSpecPatch. Only the
   *     fields the operator touched are sent, and an optional field they
   *     emptied is sent as a `clear` entry rather than as an empty string,
   *     because that is what the patch grammar means by "remove this" (§7).
   *   - POST /v1/machines/{name}/resize takes cpus and memory on a running
   *     machine: those two are the only fields that change a live VM rather
   *     than the file it will boot from next time (§9.5).
   *
   * Repeatable lists are the one place the patch grammar and a form disagree.
   * An action patch *appends* to `network.forward` and `mount`, and a layer may
   * not clear and set the same leaf, so a row the operator deleted cannot be
   * expressed in a single request. Deleting or reordering rows therefore sends
   * two patches with the clear first: if the second is rejected the list is
   * empty, which is always a valid spec, and the dialog says exactly that and
   * keeps the rows for a retry. Appending rows, and emptying a list, each still
   * take one request.
   *
   * Nothing here parses a Firestone grammar. A value the server cannot accept
   * comes back as an ErrorEnvelope and is rendered against `error.field` when
   * it names one; the only client-side refusals are the two the browser needs
   * to build the request body at all: a field that must not be empty, and a
   * mount row that cannot be turned into a MountSpec object.
   * ==================================================================== */

  var EDIT_FORM = "[data-fs-edit-form]";

  /* Machines whose spec was saved while they were running, in a way the server
   * cannot observe (ui/view.rs `observable_drift` explains which fields those
   * are). Held for this page session only, and dropped as soon as the machine
   * is seen in any state but running. */
  var editedWhileRunning = Object.create(null);

  /* ErrorInfo.field speaks dotted spec paths; the form speaks the field names
   * the CLI and REST already accept. This is the whole translation. */
  var EDIT_FIELD_CONTROLS = {
    image: "image",
    cpus: "cpus",
    memory: "memory",
    disk: "disk",
    user: "user",
    "network.mode": "net_mode",
    "network.tap": "tap",
    "network.mac": "mac",
    "network.forward": "forward",
    mount: "mounts"
  };

  /* Fields with no meaningful empty value: emptying one is a mistake, not a
   * request to clear it, and the patch grammar has no clear path for any of
   * them. */
  var EDIT_REQUIRED = {
    image: "an image reference is required",
    cpus: "cpus must be 1 through 255",
    memory: "a memory size is required",
    disk: "a disk size is required",
    user: "a guest user is required"
  };

  function editFormOf(node) {
    return node && node.closest ? node.closest(EDIT_FORM) : null;
  }

  function trimmedText(value) {
    return value === undefined || value === null ? "" : String(value).trim();
  }

  function editControl(form, path) {
    var name =
      EDIT_FIELD_CONTROLS[path] ||
      EDIT_FIELD_CONTROLS[String(path).split("[")[0]];
    return name ? qs('[name="' + name + '"]', form) : null;
  }

  /* ------------------------------------------------------- field errors -- */

  function clearEditErrors(form) {
    fieldsOf(form, "[data-fs-edit-error]").forEach(function (node) {
      node.remove();
    });
    fieldsOf(form, '[aria-invalid="true"]').forEach(function (node) {
      node.removeAttribute("aria-invalid");
    });
    var banner = qs("[data-fs-edit-banner]", form);
    if (banner) {
      banner.textContent = "";
    }
  }

  function showEditFieldError(form, path, message, hint) {
    var control = editControl(form, path);
    if (!control) {
      return false;
    }
    var field = control.closest(".fs-field") || control.parentNode;
    control.setAttribute("aria-invalid", "true");
    var error = el("div", "fs-field__error", message);
    error.setAttribute("data-fs-edit-error", "");
    field.appendChild(error);
    if (hint) {
      var note = el("div", "fs-field__hint", hint);
      note.setAttribute("data-fs-edit-error", "");
      field.appendChild(note);
    }
    return true;
  }

  function showEditBanner(form, message, hint, severe) {
    var banner = qs("[data-fs-edit-banner]", form);
    if (!banner) {
      return;
    }
    var notice = el(
      "div",
      "fs-inline-notice" + (severe ? " fs-inline-notice--fail" : "")
    );
    notice.setAttribute("role", severe ? "alert" : "status");
    notice.appendChild(el("span", null, message));
    if (hint) {
      notice.appendChild(el("span", "fs-inline-notice__hint", hint));
    }
    banner.textContent = "";
    banner.appendChild(notice);
  }

  /* A rejection is answered beside the field it names. An error that names no
   * field — a `usage` rejection of the whole body, a conflict about the machine
   * rather than about one value — has nowhere better to go than the dialog. */
  function reportEditError(form, envelope) {
    var info = (envelope && envelope.error) || {};
    var message = info.message || "the edit was rejected";
    if (info.field && showEditFieldError(form, info.field, message, info.hint)) {
      return;
    }
    showEditBanner(form, message, info.hint, true);
  }

  /* --------------------------------------------------------- the patch --- */

  function readSpecForm(form) {
    return {
      image: valueOf(form, '[name="image"]'),
      cpus: valueOf(form, '[name="cpus"]'),
      memory: valueOf(form, '[name="memory"]'),
      disk: valueOf(form, '[name="disk"]'),
      user: valueOf(form, '[name="user"]'),
      net_mode: valueOf(form, '[name="net_mode"]'),
      forward: valueOf(form, '[name="forward"]'),
      tap: valueOf(form, '[name="tap"]'),
      mac: valueOf(form, '[name="mac"]'),
      mounts: valueOf(form, '[name="mounts"]')
    };
  }

  function splitList(raw, separator) {
    return trimmedText(raw)
      .split(separator)
      .map(function (item) {
        return item.trim();
      })
      .filter(function (item) {
        return item.length > 0;
      });
  }

  function sameList(before, after) {
    if (before.length !== after.length) {
      return false;
    }
    for (var index = 0; index < before.length; index += 1) {
      if (before[index] !== after[index]) {
        return false;
      }
    }
    return true;
  }

  /* What one repeatable list needs, given the patch grammar's append merge:
   * nothing, a clear, an append of the new tail, or a clear followed by the
   * whole list in a second patch. */
  function listChange(before, after) {
    if (sameList(before, after)) {
      return { mode: "same", values: [] };
    }
    if (!after.length) {
      return { mode: "clear", values: [] };
    }
    if (
      before.length < after.length &&
      sameList(before, after.slice(0, before.length))
    ) {
      return { mode: "append", values: after.slice(before.length) };
    }
    return { mode: "replace", values: after };
  }

  /* cpus is the one numeric leaf in the patch. A value that is not a whole
   * number is passed through unchanged rather than coerced: the route rejects
   * it, which is the same answer the CLI would give. */
  function numericLeaf(text) {
    return /^[0-9]+$/.test(text) ? Number(text) : text;
  }

  /* The ByteSize grammar, exactly: a bare integer is MiB, a `M`/`m` suffix is
   * MiB, a `G`/`g` suffix is GiB. Anything else is not a size and is left to
   * the server to reject. Sizes are compared as byte counts rather than as the
   * strings the unit select composed, so re-entering 4096M as 4G is correctly
   * seen as no change and sends no patch. */
  function sizeBytes(text) {
    var match = /^([0-9]+)([MmGg]?)$/.exec(trimmedText(text));
    if (!match) {
      return null;
    }
    var unit = match[2].toLowerCase() === "g" ? 1024 * 1024 * 1024 : 1024 * 1024;
    return Number(match[1]) * unit;
  }

  /* True when two size fields mean different amounts. Unparsable text falls
   * back to a text comparison so a typo still reaches the server. */
  function sizeChanged(before, after) {
    var left = sizeBytes(before);
    var right = sizeBytes(after);
    if (left === null || right === null) {
      return trimmedText(before) !== trimmedText(after);
    }
    return left !== right;
  }

  /* Builds the request plan for one edit. Pure: it reads the form it is given
   * and the original projection it is given, and touches nothing else. */
  function patchFromForm(form, original) {
    var before = original || {};
    var after = readSpecForm(form);
    var running = form.getAttribute("data-fs-running") === "true";

    var plan = { patches: [], resize: null, replaced: [], errors: {} };
    var patch = {};
    var network = {};
    var clear = [];
    var second = null;

    var SIZE_KEYS = { memory: true, disk: true };

    function changed(key) {
      if (SIZE_KEYS[key]) {
        return sizeChanged(before[key], after[key]);
      }
      return after[key] !== trimmedText(before[key]);
    }

    Object.keys(EDIT_REQUIRED).forEach(function (key) {
      if (changed(key) && !after[key]) {
        plan.errors[key] = EDIT_REQUIRED[key];
      }
    });

    if (changed("image")) {
      patch.image = after.image;
    }
    if (changed("user")) {
      patch.user = after.user;
    }
    if (changed("disk")) {
      patch.disk = after.disk;
    }

    /* A running machine resizes; a stopped one is only ever a spec change. */
    if (running) {
      if (changed("cpus") || changed("memory")) {
        plan.resize = {};
        if (changed("cpus")) {
          plan.resize.cpus = numericLeaf(after.cpus);
        }
        if (changed("memory")) {
          plan.resize.memory = after.memory;
        }
      }
    } else {
      if (changed("cpus")) {
        patch.cpus = numericLeaf(after.cpus);
      }
      if (changed("memory")) {
        patch.memory = after.memory;
      }
    }

    if (changed("net_mode")) {
      network.mode = after.net_mode;
    }
    if (changed("tap")) {
      if (after.tap) {
        network.tap = after.tap;
      } else {
        clear.push("network.tap");
      }
    }
    if (changed("mac")) {
      if (after.mac) {
        network.mac = after.mac;
      } else {
        clear.push("network.mac");
      }
    }

    var forwards = listChange(
      splitList(before.forward, ","),
      splitList(after.forward, ",")
    );
    if (forwards.mode === "clear") {
      clear.push("network.forward");
    } else if (forwards.mode === "append") {
      network.forward = forwards.values;
    } else if (forwards.mode === "replace") {
      clear.push("network.forward");
      second = second || {};
      second.network = { forward: forwards.values };
      plan.replaced.push("port forwards");
    }

    var mounts = listChange(
      splitList(before.mounts, "\n"),
      splitList(after.mounts, "\n")
    );
    var mountValues = [];
    for (var index = 0; index < mounts.values.length; index += 1) {
      var parsed = splitMount(mounts.values[index]);
      if (!parsed) {
        plan.errors.mount =
          mounts.values[index] + " is not HOST:GUEST[:ro]";
        mountValues = null;
        break;
      }
      mountValues.push({
        host: parsed.host,
        guest: parsed.guest,
        readonly: parsed.readonly
      });
    }
    if (mounts.mode === "clear") {
      clear.push("mount");
    } else if (mountValues && mounts.mode === "append") {
      patch.mount = mountValues;
    } else if (mountValues && mounts.mode === "replace") {
      clear.push("mount");
      second = second || {};
      second.mount = mountValues;
      plan.replaced.push("shared folders");
    }

    if (Object.keys(network).length) {
      patch.network = network;
    }
    if (clear.length) {
      patch.clear = clear;
    }
    if (Object.keys(patch).length) {
      plan.patches.push(patch);
    }
    if (second) {
      plan.patches.push(second);
    }
    return plan;
  }

  /* ------------------------------------------------------------ writing -- */

  async function sendSpecPatch(name, patch) {
    var response = await fetch("/v1/machines/" + encodeURIComponent(name), {
      method: "PATCH",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify(patch)
    });
    if (response.ok) {
      return null;
    }
    var envelope;
    try {
      envelope = await response.json();
    } catch (error) {
      envelope = null;
    }
    return isErrorEnvelope(envelope)
      ? envelope
      : errorEnvelope(
          "the server returned " + response.status,
          "check that firestone serve is still running"
        );
  }

  /* The live half. resize streams NDJSON like every other action, so it uses
   * the same drawer the lifecycle buttons use. */
  async function runResize(name, body) {
    var terminal = await streamAction(
      "/v1/machines/" + encodeURIComponent(name) + "/resize",
      {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
          Accept: "application/x-ndjson"
        },
        body: JSON.stringify(body)
      },
      function (record) {
        pushStreamLine(name, record);
      }
    );
    return isErrorEnvelope(terminal) ? terminal : null;
  }

  async function submitEdit(form) {
    var submit = qs("[data-fs-edit-submit]", form);
    if (submit && submit.disabled) {
      return;
    }
    var name = form.getAttribute("data-fs-machine");
    var running = form.getAttribute("data-fs-running") === "true";
    clearEditErrors(form);

    var original;
    try {
      original = JSON.parse(form.getAttribute("data-fs-original") || "{}");
    } catch (error) {
      original = {};
    }

    var plan = patchFromForm(form, original);
    var problems = Object.keys(plan.errors);
    if (problems.length) {
      problems.forEach(function (path) {
        if (!showEditFieldError(form, path, plan.errors[path])) {
          showEditBanner(form, plan.errors[path], null, true);
        }
      });
      return;
    }
    if (!plan.patches.length && !plan.resize) {
      showEditBanner(
        form,
        "nothing changed",
        "edit a field, or close the dialog",
        false
      );
      return;
    }

    if (submit) {
      submit.disabled = true;
      submit.setAttribute("aria-busy", "true");
    }
    markStreaming("edit:" + name, true);

    var applied = [];
    var failed = null;
    var afterClear = false;
    try {
      for (var index = 0; index < plan.patches.length; index += 1) {
        failed = await sendSpecPatch(name, plan.patches[index]);
        if (failed) {
          afterClear = index > 0 && plan.replaced.length > 0;
          break;
        }
        applied.push("PATCH /v1/machines/" + name);
      }
      if (!failed && plan.resize) {
        failed = await runResize(name, plan.resize);
        if (!failed) {
          applied.push("POST /v1/machines/" + name + "/resize");
        }
      }
    } catch (error) {
      failed = errorEnvelope(error);
    } finally {
      markStreaming("edit:" + name, false);
      closeDrawer(name);
      if (submit) {
        submit.disabled = false;
        submit.removeAttribute("aria-busy");
      }
    }

    if (failed) {
      reportEditError(form, failed);
      if (afterClear) {
        showEditBanner(
          form,
          "the " +
            plan.replaced.join(" and ") +
            " list was emptied before this was rejected, so it is now empty",
          "fix the rows above and save again",
          true
        );
      }
      /* Something may still have been written before the rejection, so the
       * page is re-read either way rather than left showing stale values. */
      refreshFromServer();
      return;
    }

    var dialog = form.closest("dialog");
    if (dialog) {
      dialog.close();
    }
    clearDialogSlot();
    /* A resize-only edit applies live *and* persists the spec, so nothing is
     * left pending and the pill would be a lie. Only a spec patch — the fields
     * the running VM cannot take now — raises it. */
    if (running && plan.patches.length) {
      editedWhileRunning[name] = true;
    }
    toastDone("saved " + name, applied.join(" · "));
    refreshFromServer();
    applyClientDrift();
  }

  /* ---------------------------------------------------------- drift pill -- */

  /* The server proves drift for the three fields state.json records. For every
   * other editable field there is nothing to compare against, so the pill is
   * driven by the one thing the browser does know: this session saved a spec
   * change while the machine was running. It is dropped the moment the machine
   * is seen in any other state, and a page reload drops it too — it is a
   * reminder, never a record. */
  function applyClientDrift() {
    Object.keys(editedWhileRunning).forEach(function (name) {
      var head = qs('[data-fs-row="' + CSS.escape(name) + '"]');
      if (!head) {
        return;
      }
      var status = qs("[data-status]", head);
      if (status && status.getAttribute("data-status") !== "running") {
        delete editedWhileRunning[name];
        return;
      }
      var ident = qs(".fs-detail-head__ident", head);
      if (!ident || qs("[data-fs-drift]", ident)) {
        return;
      }
      var pill = el(
        "span",
        "fs-badge fs-badge--pending fs-badge--sentence",
        "spec drift · edited while running"
      );
      pill.setAttribute("data-fs-drift", "client");
      pill.title =
        "the spec was saved while " +
        name +
        " was running; restart it to apply the change";
      ident.appendChild(pill);
    });
  }

  /* --------------------------------------------------------- listeners -- */

  /* Capture, and registered after the composition listener above, so the raw
   * forward and mount fields are already canonical by the time the patch is
   * built from them. */
  document.addEventListener(
    "submit",
    function (event) {
      var form = editFormOf(event.target);
      if (!form) {
        return;
      }
      event.preventDefault();
      submitEdit(form);
    },
    true
  );

  document.body.addEventListener("htmx:afterSwap", applyClientDrift);
  document.addEventListener("DOMContentLoaded", applyClientDrift);

  /* Exposed for isolated testing, like the ANSI helpers above; nothing else
   * reads them. */
  window.firestoneEdit = {
    patchFromForm: patchFromForm,
    listChange: listChange
  };

  /* ================== snapshots, clone, images, prune (M6-26) ==============
   *
   * Six commands, one shape. Every one of them opens a dialog the shell
   * already rendered, reads the operator's answer from the dialog's own
   * returnValue, and then writes to a documented /v1 route — never to /ui.
   * The UI stays the read surface it says it is (§16.5).
   *
   * Two honesty rules run through the whole section, and both cost something:
   *
   *   - A dialog states what the action actually does to the machine, in the
   *     machine's current state: a snapshot of a running machine pauses the
   *     guest (§23.4), and a restore of a running machine is refused unless it
   *     is stopped first (§23.5). Neither is discovered from a 409.
   *   - The system prune previews with dry_run before it removes, and the
   *     confirm stays disabled until that preview has actually answered. The
   *     operator approves a list, not a promise.
   * ==================================================================== */

  var SYSTEM_PRUNE_URL = "/v1/system/prune";

  /* Streams one machine-scoped action into that machine's own drawer, with
   * the poll suppressed for its duration exactly as a lifecycle button does. */
  async function streamForMachine(name, url, options) {
    markStreaming(name, true);
    var terminal = null;
    try {
      terminal = await streamAction(url, options, function (record) {
        pushStreamLine(name, record);
      });
    } catch (error) {
      terminal = errorEnvelope(error);
    } finally {
      markStreaming(name, false);
      closeDrawer(name);
    }
    return terminal;
  }

  function jsonBody(value) {
    return {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Accept: "application/x-ndjson"
      },
      body: JSON.stringify(value)
    };
  }

  /* The snapshots tab is not polled — a five-second swap under a table of
   * destructive buttons would be its own hazard — so it is re-read explicitly
   * after each command that changes it. */
  function refreshSnapshots(name) {
    var panel = qs("#fs-tabpanel");
    var host = qs('[data-fs-snapshots="' + CSS.escape(name) + '"]');
    if (!panel || !host || !window.htmx) {
      refreshFromServer();
      return;
    }
    window.htmx.ajax(
      "GET",
      "/ui/machines/" + encodeURIComponent(name) + "/tab/snapshots",
      { target: panel, swap: "innerHTML" }
    );
    refreshFromServer();
  }

  /* ----------------------------------------------------------- snapshots -- */

  function openSnapshotDialog(button, name, running) {
    withDialog(
      "#fs-snapshot-create",
      function (dialog) {
        var names = dialog.querySelectorAll("[data-fs-snapshot-name]");
        Array.prototype.forEach.call(names, function (node) {
          node.textContent = name;
        });
        show(qs('[data-fs-snapshot-tier="warm"]', dialog), running);
        show(qs('[data-fs-snapshot-tier="cold"]', dialog), !running);
        var field = qs("#fs-snapshot-name", dialog);
        if (field) {
          field.value = "";
        }
      },
      function (dialog) {
        var field = dialog && qs("#fs-snapshot-name", dialog);
        runSnapshotCreate(button, name, field ? field.value.trim() : "");
      }
    );
  }

  async function runSnapshotCreate(button, name, snapshot) {
    if (button) {
      clearInlineNotice(button);
    }
    /* The identifier is optional: absent asks the server for
     * snap-<yyyymmdd>-<hhmmss> from the UTC instant of the request (§23.1).
     * An empty body would mean the same thing; "{}" is sent because that is
     * what the route's optional-JSON parser reads either way. */
    var body = snapshot ? { snapshot: snapshot } : {};
    var terminal = await streamForMachine(
      name,
      "/v1/machines/" + encodeURIComponent(name) + "/snapshots",
      jsonBody(body)
    );

    if (isErrorEnvelope(terminal)) {
      if (button) {
        inlineNotice(button, terminal, terminal.error.kind !== "conflict");
      }
      toastFailed(name, "snapshot", terminal.error.message);
      return;
    }
    var payload = (terminal && terminal.payload) || {};
    toastDone(
      "took snapshot " + (payload.snapshot || snapshot || "of " + name),
      payload.kind ? payload.kind + " · " + compactJson(terminal) : compactJson(terminal)
    );
    refreshSnapshots(name);
  }

  function openRestoreDialog(button, name, snapshot, warm, running) {
    withDialog(
      "#fs-snapshot-restore",
      function (dialog) {
        setText(dialog, "[data-fs-restore-name]", name);
        setText(dialog, "[data-fs-restore-snapshot]", snapshot);
        show(qs("[data-fs-restore-warm]", dialog), warm);
        /* force exists for exactly one situation: the machine is running and
         * a restore would otherwise be refused. Offering the checkbox on a
         * stopped machine would suggest the restore needs permission it does
         * not need. */
        show(qs("[data-fs-restore-force-row]", dialog), running);
        var force = qs("#fs-restore-force", dialog);
        if (force) {
          force.checked = false;
        }
      },
      function (dialog) {
        var force = dialog && qs("#fs-restore-force", dialog);
        runSnapshotRestore(button, name, snapshot, !!(force && force.checked));
      }
    );
  }

  async function runSnapshotRestore(button, name, snapshot, force) {
    if (button) {
      clearInlineNotice(button);
    }
    var terminal = await streamForMachine(
      name,
      "/v1/machines/" +
        encodeURIComponent(name) +
        "/snapshots/" +
        encodeURIComponent(snapshot) +
        "/restore",
      jsonBody({ force: force })
    );

    if (isErrorEnvelope(terminal)) {
      if (button) {
        inlineNotice(button, terminal, terminal.error.kind !== "conflict");
      }
      toastFailed(name, "restore", terminal.error.message);
      return;
    }
    var payload = (terminal && terminal.payload) || {};
    toastDone(
      "restored " + name + " to " + snapshot,
      payload.started ? "running again" : "stopped and startable"
    );
    refreshSnapshots(name);
  }

  function openSnapshotDeleteDialog(button, name, snapshot) {
    withDialog(
      "#fs-snapshot-delete",
      function (dialog) {
        setText(dialog, "[data-fs-snapdelete-name]", name);
        setText(dialog, "[data-fs-snapdelete-snapshot]", snapshot);
      },
      function () {
        runSnapshotDelete(button, name, snapshot);
      }
    );
  }

  async function runSnapshotDelete(button, name, snapshot) {
    if (button) {
      clearInlineNotice(button);
    }
    var terminal = await streamForMachine(
      name,
      "/v1/machines/" +
        encodeURIComponent(name) +
        "/snapshots/" +
        encodeURIComponent(snapshot),
      { method: "DELETE", headers: { Accept: "application/json" } }
    );

    if (isErrorEnvelope(terminal)) {
      if (button) {
        inlineNotice(button, terminal, true);
      }
      toastFailed(name, "snapshot delete", terminal.error.message);
      return;
    }
    toastDone("removed snapshot " + snapshot, "DELETE 204 · " + name);
    refreshSnapshots(name);
  }

  /* --------------------------------------------------------------- clone -- */

  function openCloneDialog(button, source) {
    withDialog(
      "#fs-clone",
      function (dialog) {
        setText(dialog, "[data-fs-clone-source]", source);
        var field = qs("#fs-clone-name", dialog);
        if (field) {
          field.value = "";
        }
        var fresh = qs("#fs-clone-fresh", dialog);
        if (fresh) {
          fresh.checked = false;
        }
      },
      function (dialog) {
        var field = dialog && qs("#fs-clone-name", dialog);
        var fresh = dialog && qs("#fs-clone-fresh", dialog);
        var dest = field ? field.value.trim() : "";
        if (!dest) {
          /* The one refusal the browser makes on its own: without a name there
           * is no request body to build. Every other rule about the name is
           * the server's. */
          toastFailed(source, "clone", "give the new machine a name");
          return;
        }
        runClone(button, source, dest, !!(fresh && fresh.checked));
      }
    );
  }

  async function runClone(button, source, dest, freshDisk) {
    if (button) {
      clearInlineNotice(button);
    }
    var terminal = await streamForMachine(
      source,
      "/v1/machines/" + encodeURIComponent(source) + "/clone",
      jsonBody({ name: dest, fresh_disk: freshDisk })
    );

    if (isErrorEnvelope(terminal)) {
      if (button) {
        inlineNotice(button, terminal, terminal.error.kind !== "conflict");
      }
      toastFailed(source, "clone", terminal.error.message);
      return;
    }
    toastDone("cloned " + source + " to " + dest, compactJson(terminal));
    refreshFromServer();
    goToMachine(dest);
  }

  /* The clone's whole point is the new machine, so the terminal Result lands
   * on its page rather than leaving the operator to find it in the list. */
  function goToMachine(name) {
    var path = "/machines/" + encodeURIComponent(name);
    if (!window.htmx || !qs("#fs-main-content")) {
      window.location.assign(path);
      return;
    }
    window.htmx.ajax("GET", path, {
      target: "#fs-main-content",
      swap: "innerHTML"
    });
    window.history.pushState({}, "", path);
  }

  /* -------------------------------------------------------------- images -- */

  function openImageDeleteDialog(button, id, reference) {
    withDialog(
      "#fs-image-delete",
      function (dialog) {
        setText(dialog, "[data-fs-imagedelete-ref]", reference || id);
      },
      function () {
        runImageDelete(button, id, reference);
      }
    );
  }

  async function runImageDelete(button, id, reference) {
    if (button) {
      clearInlineNotice(button);
      button.disabled = true;
    }
    var terminal;
    try {
      terminal = await streamAction(
        "/v1/images/" + encodeURIComponent(id),
        { method: "DELETE", headers: { Accept: "application/json" } },
        function () {}
      );
    } catch (error) {
      terminal = errorEnvelope(error);
    }
    if (button && button.isConnected) {
      button.disabled = false;
    }

    if (isErrorEnvelope(terminal)) {
      /* The refusal that matters here names what is holding the image — a
       * machine, or a snapshot that pins it as its base (§8.4, §23.2). It is
       * shown beside the row, in full, because "conflict" alone would leave
       * the operator guessing at which machine to stop. */
      if (button) {
        inlineNotice(button, terminal, terminal.error.kind !== "conflict");
      }
      toastFailed(reference || id, "image delete", terminal.error.message);
      return;
    }
    toastDone("removed image " + (reference || id), "DELETE 204 · " + id);
    refreshFromServer();
  }

  function openImagePruneDialog(button) {
    withDialog(
      "#fs-image-prune",
      function () {},
      function () {
        runImagePrune(button);
      }
    );
  }

  async function runImagePrune(button) {
    if (button) {
      clearInlineNotice(button);
      button.disabled = true;
    }
    var terminal;
    try {
      /* The route refuses a body outright, so none is sent and no
       * Content-Type is declared either. */
      terminal = await streamAction(
        "/v1/images/prune",
        { method: "POST", headers: { Accept: "application/json" } },
        function () {}
      );
    } catch (error) {
      terminal = errorEnvelope(error);
    }
    if (button && button.isConnected) {
      button.disabled = false;
    }

    if (isErrorEnvelope(terminal)) {
      if (button) {
        inlineNotice(button, terminal, true);
      }
      toastFailed("unused images", "prune", terminal.error.message);
      return;
    }
    var payload = prunePayload(terminal) || {};
    var removed = payload.removed ? payload.removed.length : 0;
    toastDone(
      removed === 1 ? "pruned 1 image" : "pruned " + removed + " images",
      /* `ImagePruneResult` counts in `bytes_freed`; the system prune's
       * `PruneResult` counts in `reclaimed_bytes`. Two payloads, two names. */
      formatByteSize(payload.bytes_freed || 0) + " reclaimed"
    );
    refreshFromServer();
  }

  /* --------------------------------------------------------- system prune -- */

  /* The aggregate JSON body and the NDJSON terminal record carry the same
   * payload in two shapes, and this section reads both rather than pinning
   * the surface to one Accept header. */
  function prunePayload(record) {
    if (!record || typeof record !== "object") {
      return null;
    }
    if (record.payload && typeof record.payload === "object") {
      return record.payload;
    }
    if (record.removed || record.reclaimed_bytes !== undefined) {
      return record;
    }
    return null;
  }

  function pruneRequest(dialog, dryRun) {
    var machines = qs("#fs-prune-machines", dialog);
    var images = qs("#fs-prune-images", dialog);
    var wantsMachines = !!(machines && machines.checked);
    return {
      machines: wantsMachines,
      images: !!(images && images.checked),
      /* Removing a stopped machine is still removing a machine, so the tier is
       * only ever requested with the flag that says so out loud. */
      force: wantsMachines,
      dry_run: !!dryRun
    };
  }

  /* The one dialog that reads before it writes, on the same shared helper as
   * the other five: the only difference is that `prepare` also starts the dry
   * run whose answer the confirm button waits for. A dialog the shell did not
   * render must not fall back to running a prune nobody previewed, so this is
   * the one command whose fallback is to do nothing. */
  function openSystemPruneDialog() {
    if (!qs("#fs-system-prune")) {
      return;
    }
    withDialog(
      "#fs-system-prune",
      function (dialog) {
        var machines = qs("#fs-prune-machines", dialog);
        var images = qs("#fs-prune-images", dialog);
        if (machines) {
          machines.checked = false;
        }
        if (images) {
          images.checked = false;
        }
        previewSystemPrune(dialog);
      },
      function (dialog) {
        runSystemPrune(dialog);
      }
    );
  }

  /* The dry run. Until it answers there is nothing to approve, so the confirm
   * button stays disabled: a "Remove" that fires against an unknown list is
   * the exact failure this dialog exists to prevent. */
  async function previewSystemPrune(dialog) {
    var state = qs("[data-fs-prune-state]", dialog);
    var rows = qs("[data-fs-prune-rows]", dialog);
    var total = qs("[data-fs-prune-total]", dialog);
    var submit = qs("[data-fs-prune-submit]", dialog);
    if (submit) {
      submit.disabled = true;
    }
    if (rows) {
      rows.textContent = "";
    }
    if (total) {
      total.textContent = "";
    }
    if (state) {
      state.textContent = "counting…";
    }

    var record;
    try {
      record = await streamAction(
        SYSTEM_PRUNE_URL,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            Accept: "application/json"
          },
          body: JSON.stringify(pruneRequest(dialog, true))
        },
        function () {}
      );
    } catch (error) {
      record = errorEnvelope(error);
    }

    if (isErrorEnvelope(record)) {
      if (state) {
        state.textContent = record.error.message;
      }
      return;
    }
    var payload = prunePayload(record);
    if (!payload) {
      if (state) {
        state.textContent = "the preview did not answer with a removal list";
      }
      return;
    }
    renderPrunePreview(dialog, payload);
    if (submit) {
      submit.disabled = false;
    }
  }

  function renderPrunePreview(dialog, payload) {
    var state = qs("[data-fs-prune-state]", dialog);
    var rows = qs("[data-fs-prune-rows]", dialog);
    var total = qs("[data-fs-prune-total]", dialog);
    var removed = payload.removed || [];

    if (state) {
      state.textContent =
        removed.length === 1
          ? "1 item would be removed"
          : removed.length + " items would be removed";
    }
    if (rows) {
      rows.textContent = "";
      removed.forEach(function (item) {
        var row = el("div", "fs-prune-row");
        row.setAttribute("data-fs-prune-kind", item.kind || "");
        row.appendChild(el("span", "fs-prune-row__kind", item.kind || "?"));
        row.appendChild(el("span", "fs-prune-row__id", item.id || ""));
        row.appendChild(
          el("span", "fs-prune-row__bytes", formatByteSize(item.bytes || 0))
        );
        rows.appendChild(row);
      });
    }
    if (total) {
      total.textContent =
        formatByteSize(payload.reclaimed_bytes || 0) + " would be reclaimed";
    }
  }

  async function runSystemPrune(dialog) {
    var record;
    try {
      record = await streamAction(
        SYSTEM_PRUNE_URL,
        {
          method: "POST",
          headers: {
            "Content-Type": "application/json",
            Accept: "application/x-ndjson"
          },
          body: JSON.stringify(pruneRequest(dialog, false))
        },
        function () {}
      );
    } catch (error) {
      record = errorEnvelope(error);
    }

    if (isErrorEnvelope(record)) {
      toastFailed("disk space", "prune", record.error.message);
      return;
    }
    var payload = prunePayload(record) || {};
    var removed = payload.removed ? payload.removed.length : 0;
    toastDone(
      removed === 1 ? "removed 1 item" : "removed " + removed + " items",
      formatByteSize(payload.reclaimed_bytes || 0) + " reclaimed"
    );
    refreshFromServer();
  }

  /* ----------------------------------------------------------- listeners -- */

  /* Bubble phase, and after every handler above: a command button inside a
   * row must not also navigate the row, and the capture-phase listener at the
   * top of this file already stops the ones it owns. */
  document.addEventListener("click", function (event) {
    if (event.defaultPrevented) {
      return;
    }

    var snapshotButton = event.target.closest("[data-fs-snapshot-action]");
    if (snapshotButton) {
      event.preventDefault();
      event.stopPropagation();
      var kind = snapshotButton.getAttribute("data-fs-snapshot-action");
      var machine = snapshotButton.getAttribute("data-fs-machine");
      var snapshot = snapshotButton.getAttribute("data-fs-snapshot");
      if (kind === "create") {
        openSnapshotDialog(
          snapshotButton,
          machine,
          snapshotButton.getAttribute("data-fs-snapshot-kind") === "warm"
        );
      } else if (kind === "restore") {
        openRestoreDialog(
          snapshotButton,
          machine,
          snapshot,
          snapshotButton.getAttribute("data-fs-snapshot-kind") === "warm",
          snapshotButton.getAttribute("data-fs-running") === "true"
        );
      } else if (kind === "delete") {
        openSnapshotDeleteDialog(snapshotButton, machine, snapshot);
      }
      return;
    }

    var cloneButton = event.target.closest("[data-fs-clone]");
    if (cloneButton && !cloneButton.disabled) {
      event.preventDefault();
      event.stopPropagation();
      closeRowMenus(null);
      openCloneDialog(cloneButton, cloneButton.getAttribute("data-fs-clone"));
      return;
    }

    var imageButton = event.target.closest("[data-fs-image-action]");
    if (imageButton) {
      event.preventDefault();
      event.stopPropagation();
      openImageDeleteDialog(
        imageButton,
        imageButton.getAttribute("data-fs-image"),
        imageButton.getAttribute("data-fs-image-ref")
      );
      return;
    }

    var pruneButton = event.target.closest("[data-fs-prune]");
    if (pruneButton) {
      event.preventDefault();
      event.stopPropagation();
      if (pruneButton.getAttribute("data-fs-prune") === "system") {
        openSystemPruneDialog();
      } else {
        openImagePruneDialog(pruneButton);
      }
      return;
    }

    var paletteAction = event.target.closest("[data-fs-palette-action]");
    if (paletteAction) {
      event.preventDefault();
      runPaletteAction(
        paletteAction.getAttribute("data-fs-palette-action"),
        paletteAction.getAttribute("data-fs-machine"),
        paletteAction.getAttribute("data-fs-running") === "true"
      );
      return;
    }

    /* A menu left open over a table that repaints every five seconds is a
     * click target that moves, so any click outside one closes it. */
    closeRowMenus(event.target.closest("[data-fs-row-menu]"));
  });

  function closeRowMenus(keep) {
    var menus = document.querySelectorAll("[data-fs-row-menu][open]");
    Array.prototype.forEach.call(menus, function (menu) {
      if (menu !== keep) {
        menu.open = false;
      }
    });
  }

  /* Every palette command opens the same dialog, or navigates to the same
   * page, the screen's own control opens. Nothing here writes by itself, and
   * nothing here is reachable only from the palette. */
  function runPaletteAction(kind, machine, running) {
    if (kind === "new-machine") {
      openDialogFragment("/ui/machines/new");
    } else if (kind === "prune-images") {
      openImagePruneDialog(null);
    } else if (kind === "prune-system") {
      openSystemPruneDialog();
    } else if (kind === "snapshot" && machine) {
      openSnapshotDialog(null, machine, running);
    } else if (kind === "clone" && machine) {
      openCloneDialog(null, machine);
    } else if (kind === "edit" && machine) {
      openDialogFragment(
        "/ui/machines/" + encodeURIComponent(machine) + "/edit"
      );
    } else if (kind === "terminal" && machine) {
      window.location.assign(
        "/machines/" + encodeURIComponent(machine) + "/terminal"
      );
    }
  }

  /* Changing a tier re-runs the preview, because the list the operator is
   * about to approve just changed. */
  document.addEventListener("change", function (event) {
    var option = event.target.closest("[data-fs-prune-option]");
    if (!option) {
      return;
    }
    var dialog = option.closest("dialog");
    if (dialog) {
      previewSystemPrune(dialog);
    }
  });

  window.firestoneFeatures = {
    prunePayload: prunePayload,
    pruneRequest: pruneRequest
  };
})();
