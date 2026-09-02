// Auto-scroll for the run output pane (the Output card on a run page).
//
// The pane replaces itself every poll (hx-swap="outerHTML"), and a freshly
// inserted <pre> starts scrolled to the top — so while a run streams you would
// be pinned to the oldest line of the tail, exactly where you don't want to
// be. With the checkbox on, this re-pins the <pre> to the bottom after every
// swap; with it off, it restores the scroll offset the outgoing <pre> had, so
// the swap does not yank you back to the top while you read.
//
// A real file rather than hx-on:* attributes because the page CSP forbids
// inline script. The checkbox itself lives outside #log-pane in the markup,
// so a swap can never destroy a click mid-press or steal its focus.

(() => {
  const STORAGE_KEY = "autoscroll";

  // Elements are looked up on every use: the pane is replaced every poll, so a
  // cached reference would describe swap N-1 forever.
  const pane = () => document.getElementById("log-pane");
  const checkbox = () => document.getElementById("autoscroll");
  const log = () => pane()?.querySelector("pre.log") ?? null;

  // Scroll offset of the <pre> that a swap is about to replace.
  let savedScrollTop = 0;

  function scrollToBottom() {
    const pre = log();
    if (pre) pre.scrollTop = pre.scrollHeight;
  }

  function enabled() {
    return checkbox()?.checked ?? false;
  }

  function init() {
    // The server renders the checkbox checked; storage overrides for viewers
    // who turned it off on an earlier run. It can throw when the browser has
    // site storage switched off.
    const box = checkbox();
    if (box) {
      try {
        box.checked = localStorage.getItem(STORAGE_KEY) !== "off";
      } catch {
        /* keep the default */
      }
    }
    if (enabled()) scrollToBottom();

    document.addEventListener("change", (event) => {
      if (event.target !== box) return;
      try {
        localStorage.setItem(STORAGE_KEY, box.checked ? "on" : "off");
      } catch {
        /* preference just won't persist */
      }
      // Immediate: don't make the reader wait for the next poll to land.
      if (box.checked) scrollToBottom();
    });

    // htmx events bubble to document, and both fire while the old pane is
    // still reachable: beforeSwap just prior to replacement, afterSwap once
    // the new one is in (and laid out, so scrollHeight is already correct).
    document.addEventListener("htmx:beforeSwap", (event) => {
      if (event.target !== pane()) return;
      savedScrollTop = log()?.scrollTop ?? 0;
    });
    document.addEventListener("htmx:afterSwap", (event) => {
      if (event.target !== pane()) return;
      if (enabled()) {
        scrollToBottom();
      } else {
        const pre = log();
        if (pre) pre.scrollTop = savedScrollTop;
      }
    });
  }

  init();
})();
