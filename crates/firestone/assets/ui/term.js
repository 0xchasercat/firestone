/* Firestone browser terminal (SPEC §16.5, transports in §16.3).
 *
 * Loaded by the terminal page and by nothing else. app.js is not on this
 * page: there is no htmx here, no polling, and no mutation stream — one
 * WebSocket carrying raw bytes is the whole surface.
 *
 * Wire contract, both tabs:
 *
 *   - Binary frames are raw terminal bytes in both directions. Keystrokes go
 *     out as Binary, server bytes come in as Binary and are written to the
 *     emulator unmodified.
 *   - Text frames are JSON control messages. This client sends exactly one,
 *     {"resize":{"rows":R,"cols":C}}, and only on the shell tab: the console
 *     broker owns a fixed guest serial geometry and ignores it.
 *   - The server's close frame carries a reason ("machine stopped", "session
 *     ended", or a transport failure detail). It is shown verbatim.
 *
 * Rendering is ghostty-web: Ghostty's own VT parser compiled to WebAssembly,
 * so escape sequences are handled by the emulator the native app uses rather
 * than by a second parser written here. Instantiating it is why this page —
 * and only this page — is served `script-src 'self' 'wasm-unsafe-eval'`.
 *
 * If the emulator cannot start (no WebAssembly, a blocked module, a broken
 * bundle) the page does not go blank: a plain <pre> renderer below takes
 * over, stripping the escape sequences it cannot draw and still sending
 * keystrokes. It is a degraded read of the same stream, never a second
 * terminal implementation.
 *
 * The policy served with this page has no 'unsafe-inline': every visual
 * decision is a class in app.css and every script is a same-origin file.
 */
(function () {
  "use strict";

  var root = document.getElementById("fs-term");
  if (!root) {
    return;
  }

  var RESIZE_DEBOUNCE_MS = 120;
  var SCROLLBACK = 5000;
  /* The <pre> fallback keeps a bounded window: a boot log is unbounded and
   * this path has no scrollback buffer of its own. */
  var FALLBACK_MAX_CHARS = 120000;

  var config = {
    machine: root.getAttribute("data-fs-machine") || "",
    tab: root.getAttribute("data-fs-tab") === "shell" ? "shell" : "console",
    console: root.getAttribute("data-fs-console-url") || "",
    shell: root.getAttribute("data-fs-shell-url") || "",
    state: root.getAttribute("data-fs-state-url") || "",
    module: root.getAttribute("data-fs-module-url") || "",
    wasm: root.getAttribute("data-fs-wasm-url") || ""
  };

  var screenEl = document.getElementById("fs-term-screen");
  var stage = document.getElementById("fs-term-stage");
  var connection = document.getElementById("fs-term-connection");
  var overlay = document.getElementById("fs-term-overlay");
  var overlayTitle = document.getElementById("fs-term-overlay-title");
  var overlayHint = document.getElementById("fs-term-overlay-hint");
  var reconnect = document.getElementById("fs-term-reconnect");
  var note = document.getElementById("fs-term-note");
  var geometry = document.getElementById("fs-term-geometry");

  var encoder = new TextEncoder();

  /* The loaded ghostty-web namespace and its WASM instance, or null once the
   * page has decided the emulator is unavailable. Both are loaded once and
   * shared by every session the page opens. */
  var bundle = null;
  var ghostty = null;
  var emulatorUnavailable = false;

  /* The one live session, or null. Everything a teardown has to undo hangs
   * off this object, so switching tabs cannot leave a socket or an observer
   * behind. */
  var session = null;

  /* ------------------------------------------------------------ chrome -- */

  function setConnection(state, text) {
    if (!connection) {
      return;
    }
    connection.setAttribute("data-fs-connection", state);
    connection.textContent = text;
  }

  function showOverlay(title, hint, offerReconnect) {
    if (!overlay) {
      return;
    }
    if (overlayTitle) {
      overlayTitle.textContent = title;
    }
    if (overlayHint) {
      overlayHint.textContent = hint || "";
      overlayHint.hidden = !hint;
    }
    if (reconnect) {
      reconnect.hidden = !offerReconnect;
    }
    overlay.hidden = false;
  }

  function hideOverlay() {
    if (overlay) {
      overlay.hidden = true;
    }
  }

  function setGeometry(cols, rows) {
    if (geometry) {
      geometry.textContent = cols && rows ? cols + "×" + rows : "";
    }
  }

  function setNote(text) {
    if (note) {
      note.textContent = text;
    }
  }

  function tabUrl(tab) {
    return tab === "shell" ? config.shell : config.console;
  }

  /* A relative /v1 path becomes an absolute ws:// URL on this same origin, so
   * the handshake stays inside `connect-src 'self'`. */
  function socketUrl(path) {
    var url = new URL(path, window.location.href);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    return url.href;
  }

  /* ------------------------------------------------- <pre> fallback term -- */

  var ESC = "\u001b";
  var BEL = "\u0007";
  var DEL = "\u007f";

  /* Removes the escape sequences a plain <pre> cannot draw: CSI, OSC (both
   * terminators), the two-character ESC sequences, and the C0 controls that
   * are not whitespace. Carriage returns are applied as a last-writer-wins
   * overlay on the current line, which is how a progress line is meant to
   * read. This is deliberately not a terminal: it is a readable transcript
   * for the case where the emulator did not load. */
  function stripSequences(text) {
    var out = "";
    var index = 0;
    while (index < text.length) {
      var character = text.charAt(index);
      if (character !== ESC) {
        if (character === "\n" || character === "\t" || character === "\r") {
          out += character;
        } else if (character >= " ") {
          out += character;
        }
        index += 1;
        continue;
      }
      var next = text.charAt(index + 1);
      if (next === "[") {
        /* CSI: parameter and intermediate bytes, then one final byte. */
        index += 2;
        while (index < text.length && text.charAt(index) < "@") {
          index += 1;
        }
        index += 1;
        continue;
      }
      /* The string sequences — OSC, DCS, SOS, PM, APC — all run to BEL or to
       * ST (ESC backslash), and their payloads are never screen text. */
      if (next === "]" || next === "P" || next === "X" || next === "^" || next === "_") {
        index = skipString(text, index + 2);
        continue;
      }
      /* An ESC carrying intermediate bytes (0x20-0x2F) ends at the first
       * final byte, so a charset designation such as ESC ( B is three
       * characters and not two. */
      if (next >= " " && next <= "/") {
        index += 1;
        while (index < text.length && isIntermediate(text.charAt(index))) {
          index += 1;
        }
        index += 1;
        continue;
      }
      index += next ? 2 : 1;
    }
    return out;
  }

  function isIntermediate(character) {
    return character >= " " && character <= "/";
  }

  /* Returns the index just past the terminator of a string sequence. */
  function skipString(text, from) {
    var index = from;
    while (index < text.length) {
      var stop = text.charAt(index);
      if (stop === BEL) {
        return index + 1;
      }
      if (stop === ESC && text.charAt(index + 1) === "\\") {
        return index + 2;
      }
      index += 1;
    }
    return index;
  }

  function applyCarriageReturns(text) {
    var lines = text.split("\n");
    for (var index = 0; index < lines.length; index += 1) {
      var parts = lines[index].split("\r");
      if (parts.length === 1) {
        continue;
      }
      var line = "";
      for (var part = 0; part < parts.length; part += 1) {
        var piece = parts[part];
        line = piece + line.slice(piece.length);
      }
      lines[index] = line;
    }
    return lines.join("\n");
  }

  function createFallbackTerminal(onData) {
    var pre = document.createElement("pre");
    pre.className = "fs-term__pre";
    pre.tabIndex = 0;
    var buffered = "";
    /* Its own decoder: a multi-byte character split across chunks must not
     * carry over a teardown into the next session's first chunk. */
    var stream = new TextDecoder("utf-8", { fatal: false });
    var pending = null;

    /* Repainting a <pre> costs the whole buffer, and a boot log arrives in
     * hundreds of small chunks. Coalescing to one repaint per frame keeps a
     * busy stream from starving the main thread. */
    function render() {
      if (pending !== null) {
        return;
      }
      pending = window.requestAnimationFrame(function () {
        pending = null;
        pre.textContent = buffered;
        pre.scrollTop = pre.scrollHeight;
      });
    }

    /* Carriage returns are folded on the way in, over the trailing partial
     * line only: `\r` never reaches back past a newline, so nothing earlier
     * in the buffer can change. */
    function append(text) {
      var cut = buffered.lastIndexOf("\n") + 1;
      buffered =
        buffered.slice(0, cut) + applyCarriageReturns(buffered.slice(cut) + text);
      if (buffered.length > FALLBACK_MAX_CHARS) {
        buffered = buffered.slice(buffered.length - FALLBACK_MAX_CHARS);
      }
      render();
    }

    pre.addEventListener("keydown", function (event) {
      if (event.metaKey || event.altKey) {
        return;
      }
      var data = null;
      if (event.ctrlKey) {
        var letter = event.key.length === 1 ? event.key.toLowerCase() : "";
        if (letter >= "a" && letter <= "z") {
          data = String.fromCharCode(letter.charCodeAt(0) - 96);
        }
      } else if (event.key === "Enter") {
        data = "\r";
      } else if (event.key === "Backspace") {
        data = DEL;
      } else if (event.key === "Tab") {
        data = "\t";
      } else if (event.key === "Escape") {
        data = ESC;
      } else if (event.key === "ArrowUp") {
        data = ESC + "[A";
      } else if (event.key === "ArrowDown") {
        data = ESC + "[B";
      } else if (event.key === "ArrowRight") {
        data = ESC + "[C";
      } else if (event.key === "ArrowLeft") {
        data = ESC + "[D";
      } else if (event.key.length === 1) {
        data = event.key;
      }
      if (data === null) {
        return;
      }
      event.preventDefault();
      onData(data);
    });

    return {
      element: pre,
      degraded: true,
      open: function (parent) {
        parent.appendChild(pre);
      },
      write: function (bytes) {
        append(stripSequences(stream.decode(bytes, { stream: true })));
      },
      focus: function () {
        pre.focus();
      },
      resize: function () {},
      dimensions: function () {
        return null;
      },
      dispose: function () {
        if (pending !== null) {
          window.cancelAnimationFrame(pending);
          pending = null;
        }
        buffered = "";
        pre.textContent = "";
      }
    };
  }

  /* ---------------------------------------------------- ghostty terminal -- */

  /* Loads the emulator once. `Ghostty.load` is handed the same-origin wasm
   * URL explicitly: called with no argument the bundle would fetch its own
   * inlined `data:` copy, which `connect-src 'self'` does not allow. */
  function loadEmulator() {
    if (ghostty) {
      return Promise.resolve(ghostty);
    }
    if (emulatorUnavailable) {
      return Promise.reject(new Error("the terminal emulator is unavailable"));
    }
    if (typeof WebAssembly !== "object" || !config.module || !config.wasm) {
      emulatorUnavailable = true;
      return Promise.reject(new Error("this browser has no WebAssembly"));
    }
    return import(config.module)
      .then(function (loaded) {
        bundle = loaded;
        return loaded.Ghostty.load(config.wasm);
      })
      .then(function (loadedGhostty) {
        ghostty = loadedGhostty;
        return ghostty;
      })
      .catch(function (error) {
        emulatorUnavailable = true;
        throw error;
      });
  }

  function createGhosttyTerminal(onData) {
    var terminal = new bundle.Terminal({
      ghostty: ghostty,
      scrollback: SCROLLBACK,
      cursorBlink: true,
      fontFamily: "IBM Plex Mono, ui-monospace, SFMono-Regular, Menlo, monospace",
      fontSize: 13,
      convertEol: false
    });
    var fit = new bundle.FitAddon();
    terminal.loadAddon(fit);
    terminal.onData(onData);

    return {
      element: null,
      degraded: false,
      open: function (parent) {
        terminal.open(parent);
        this.element = terminal.element || null;
        fit.fit();
      },
      write: function (bytes) {
        terminal.write(bytes);
      },
      focus: function () {
        terminal.focus();
      },
      resize: function () {
        fit.fit();
      },
      dimensions: function () {
        return { cols: terminal.cols, rows: terminal.rows };
      },
      dispose: function () {
        try {
          fit.dispose();
        } catch (error) {
          /* An addon that never activated has nothing to release. */
        }
        try {
          terminal.dispose();
        } catch (error) {
          /* Disposing twice is not an error worth surfacing. */
        }
      }
    };
  }

  /* ---------------------------------------------------------- diagnosis -- */

  /* A browser never exposes the status of a failed WebSocket handshake: a
   * refused upgrade and a dropped connection both arrive as close code 1006
   * with no reason. So when a socket dies before it ever opened, the page
   * re-reads the machine through the documented GET /v1/machines/{name} and
   * names the cause from the server's own answer. A machine that is not
   * running explains itself; a running machine whose console would not open
   * is the single-client broker already being held, which is exactly what the
   * 409 the browser hid would have said. */
  function diagnose(tab) {
    return fetch(config.state, {
      headers: { Accept: "application/json" },
      credentials: "same-origin"
    })
      .then(function (response) {
        if (!response.ok) {
          return null;
        }
        return response.json();
      })
      .then(function (view) {
        var status = view && view.state ? view.state.status : null;
        if (status && status !== "running") {
          return {
            title: "machine stopped",
            hint:
              "`" +
              config.machine +
              "` is " +
              status +
              ". Start it from the machine page, then reconnect."
          };
        }
        if (tab === "console") {
          return {
            title: "another console client is attached",
            hint:
              "the console broker is single-client — detach the other client with Ctrl-] " +
              "and reconnect, or try `firestone console " +
              config.machine +
              "`"
          };
        }
        return {
          title: "the shell session could not start",
          hint:
            "check that the machine finished provisioning, then reconnect, or try " +
            "`firestone shell " +
            config.machine +
            "`"
        };
      })
      .catch(function () {
        return {
          title: "the connection could not be opened",
          hint: "check that firestone serve is still running, then reconnect"
        };
      });
  }

  /* ----------------------------------------------------------- sessions -- */

  function teardown() {
    if (!session) {
      return;
    }
    var closing = session;
    session = null;
    closing.discarded = true;

    if (closing.observer) {
      closing.observer.disconnect();
    }
    if (closing.resizeTimer) {
      window.clearTimeout(closing.resizeTimer);
    }
    if (closing.socket) {
      closing.socket.onopen = null;
      closing.socket.onmessage = null;
      closing.socket.onerror = null;
      closing.socket.onclose = null;
      if (
        closing.socket.readyState === WebSocket.OPEN ||
        closing.socket.readyState === WebSocket.CONNECTING
      ) {
        closing.socket.close(1000, "client closed");
      }
    }
    if (closing.terminal) {
      closing.terminal.dispose();
    }
    while (screenEl.firstChild) {
      screenEl.removeChild(screenEl.firstChild);
    }
    setGeometry(null, null);
  }

  function send(current, data) {
    if (!current.socket || current.socket.readyState !== WebSocket.OPEN) {
      return;
    }
    current.socket.send(encoder.encode(data));
  }

  /* The only control message this client sends, and only on the shell tab.
   * The console ignores it; sending it there would be noise on a stream whose
   * geometry the guest owns. */
  function sendResize(current) {
    if (current.tab !== "shell" || !current.terminal) {
      return;
    }
    var size = current.terminal.dimensions();
    if (!size || !size.cols || !size.rows) {
      return;
    }
    if (current.socket && current.socket.readyState === WebSocket.OPEN) {
      current.socket.send(
        JSON.stringify({ resize: { rows: size.rows, cols: size.cols } })
      );
    }
  }

  function observeResize(current) {
    if (typeof ResizeObserver !== "function" || !stage) {
      return;
    }
    current.observer = new ResizeObserver(function () {
      if (current.discarded) {
        return;
      }
      if (current.resizeTimer) {
        window.clearTimeout(current.resizeTimer);
      }
      current.resizeTimer = window.setTimeout(function () {
        current.resizeTimer = null;
        if (current.discarded || !current.terminal) {
          return;
        }
        current.terminal.resize();
        var size = current.terminal.dimensions();
        if (size) {
          setGeometry(size.cols, size.rows);
        }
        sendResize(current);
      }, RESIZE_DEBOUNCE_MS);
    });
    current.observer.observe(stage);
  }

  function connect(tab) {
    teardown();
    hideOverlay();
    setConnection("connecting", "connecting…");

    var current = {
      tab: tab,
      socket: null,
      terminal: null,
      observer: null,
      resizeTimer: null,
      opened: false,
      discarded: false
    };
    session = current;

    buildTerminal(current)
      .then(function () {
        if (current.discarded) {
          return;
        }
        openSocket(current);
      })
      .catch(function (error) {
        if (current.discarded) {
          return;
        }
        setConnection("failed", "failed");
        showOverlay(
          "the terminal could not start",
          error && error.message ? error.message : String(error),
          true
        );
      });
  }

  /* Paste and IME text never reach the emulator's key handler, so the screen
   * host forwards them to whichever session is live. Attached once: the
   * listeners survive reconnects and always resolve the current session.
   * Newlines become carriage returns because the guest line discipline
   * expects CR from a terminal. */
  screenEl.addEventListener("paste", function (event) {
    if (!session || !event.clipboardData) {
      return;
    }
    var text = event.clipboardData.getData("text");
    if (!text) {
      return;
    }
    event.preventDefault();
    send(session, text.replace(/\r?\n/g, "\r"));
  });
  screenEl.addEventListener("beforeinput", function (event) {
    if (!session || event.inputType !== "insertText" || !event.data) {
      return;
    }
    event.preventDefault();
    send(session, event.data);
  });

  function buildTerminal(current) {
    return loadEmulator()
      .then(function () {
        if (current.discarded) {
          return;
        }
        current.terminal = createGhosttyTerminal(function (data) {
          send(current, data);
        });
      })
      .catch(function (error) {
        if (current.discarded) {
          return;
        }
        /* The emulator is the preferred renderer, not a requirement. Falling
         * back keeps the byte stream readable instead of showing a dead page,
         * and says so rather than pretending nothing happened. */
        if (window.console && window.console.warn) {
          window.console.warn("firestone: terminal emulator unavailable", error);
        }
        current.terminal = createFallbackTerminal(function (data) {
          send(current, data);
        });
      })
      .then(function () {
        if (current.discarded || !current.terminal) {
          return;
        }
        current.terminal.open(screenEl);
        if (current.terminal.degraded) {
          setNote(
            "plain-text fallback: the WebAssembly terminal did not load, so escape " +
              "sequences are stripped rather than drawn"
          );
        }
        var size = current.terminal.dimensions();
        if (size) {
          setGeometry(size.cols, size.rows);
        }
        observeResize(current);
        current.terminal.focus();
      });
  }

  function openSocket(current) {
    var socket;
    try {
      socket = new WebSocket(socketUrl(tabUrl(current.tab)));
    } catch (error) {
      setConnection("failed", "failed");
      showOverlay(
        "the connection could not be opened",
        error && error.message ? error.message : String(error),
        true
      );
      return;
    }
    socket.binaryType = "arraybuffer";
    current.socket = socket;

    socket.onopen = function () {
      if (current.discarded) {
        return;
      }
      current.opened = true;
      setConnection("open", current.tab === "shell" ? "shell attached" : "console attached");
      hideOverlay();
      sendResize(current);
      if (current.terminal) {
        current.terminal.focus();
      }
    };

    socket.onmessage = function (event) {
      if (current.discarded || !current.terminal) {
        return;
      }
      /* Binary frames are the byte stream. A text frame is a control message;
       * the server defines none in this direction, so it is ignored rather
       * than written into the screen. */
      if (typeof event.data === "string") {
        return;
      }
      current.terminal.write(new Uint8Array(event.data));
    };

    socket.onerror = function () {
      /* onclose always follows, and only it carries a reason worth showing. */
    };

    socket.onclose = function (event) {
      if (current.discarded) {
        return;
      }
      setConnection("closed", "disconnected");
      if (current.opened) {
        var reason = event.reason ? event.reason : "the connection closed";
        showOverlay(
          reason,
          event.code === 1000 ? "" : "close code " + event.code,
          true
        );
        return;
      }
      diagnose(current.tab).then(function (diagnosis) {
        if (current.discarded) {
          return;
        }
        showOverlay(diagnosis.title, diagnosis.hint, true);
      });
    };
  }

  /* ------------------------------------------------------------- events -- */

  function selectTab(tab) {
    if (tab !== "console" && tab !== "shell") {
      return;
    }
    config.tab = tab;
    var buttons = root.querySelectorAll("[data-fs-term-tab]");
    for (var index = 0; index < buttons.length; index += 1) {
      var button = buttons[index];
      var active = button.getAttribute("data-fs-term-tab") === tab;
      button.classList.toggle("is-active", active);
      button.setAttribute("aria-selected", active ? "true" : "false");
    }
    if (screenEl) {
      screenEl.setAttribute("aria-labelledby", "fs-term-tab-" + tab);
    }
    setNote(
      tab === "shell"
        ? "SSH over vsock on a host pseudo-terminal · resize is honoured"
        : "serial console · single client · resize is the guest's to decide"
    );
    connect(tab);
  }

  root.addEventListener("click", function (event) {
    var tab = event.target.closest ? event.target.closest("[data-fs-term-tab]") : null;
    if (tab) {
      var wanted = tab.getAttribute("data-fs-term-tab");
      if (wanted !== config.tab) {
        selectTab(wanted);
      } else if (session && session.terminal) {
        session.terminal.focus();
      }
      return;
    }
    if (reconnect && event.target === reconnect) {
      connect(config.tab);
      return;
    }
    if (stage && stage.contains(event.target) && session && session.terminal) {
      session.terminal.focus();
    }
  });

  /* A page being unloaded should close its socket rather than leave the shim
   * and the pseudo-terminal to notice later. */
  window.addEventListener("pagehide", teardown);

  selectTab(config.tab);
})();
