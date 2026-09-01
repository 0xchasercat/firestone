/* Applied synchronously in <head>, before the first paint, so a dark-theme
 * viewer never sees a light flash. Kept in its own file because the served
 * Content-Security-Policy has no 'unsafe-inline'.
 *
 * Three states: "dark" and "light" are explicit choices and stamp the root
 * element; anything else (including a blocked or empty localStorage) leaves
 * the attribute off, and the stylesheet falls back to prefers-color-scheme.
 */
(function () {
  "use strict";
  try {
    var stored = window.localStorage.getItem("firestone-theme");
    if (stored === "dark" || stored === "light") {
      document.documentElement.setAttribute("data-theme", stored);
    }
  } catch (error) {
    /* Private windows and blocked site data throw on access. The system
     * preference is a correct answer, so there is nothing to recover. */
  }
})();
