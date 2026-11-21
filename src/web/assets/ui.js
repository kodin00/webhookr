// Dropdown behaviour for the themed form components (views::dropdown).
//
// The component is a native <details class="select-menu"> wrapping real
// radios, so picking a value and submitting it with the form needs no script
// at all. What the browser will not do on its own is close an open menu when
// one of its radios is chosen, or when the click lands anywhere else on the
// page, or echo the chosen option back into the collapsed summary — this file
// adds exactly those three things and nothing else.
//
// A real file rather than hx-on:* attributes because the page CSP forbids
// inline script (see views::page). The listeners sit on the document, so
// they keep working for dropdowns swapped in later by htmx.

(() => {
  const openMenus = () => document.querySelectorAll("details.select-menu[open]");

  document.addEventListener("click", (event) => {
    const clicked = event.target instanceof Element
      ? event.target.closest("details.select-menu")
      : null;
    // A click on a menu's own summary is left alone so the browser's native
    // open/close toggle still runs for it.
    for (const menu of openMenus()) {
      if (menu !== clicked) menu.open = false;
    }
  });

  document.addEventListener("change", (event) => {
    const input = event.target;
    if (!(input instanceof HTMLInputElement) || input.type !== "radio") return;
    const menu = input.closest("details.select-menu");
    if (!menu) return;
    menu.open = false;
    // Echo the pick into the collapsed summary, which otherwise keeps
    // showing whatever the server last rendered.
    const label = input.closest("label");
    const text = label && label.querySelector("span");
    const value = menu.querySelector(".select-value");
    if (text && value) value.textContent = text.textContent;
  });
})();

// Highlight the topbar link matching the current path. The server cannot do
// this per page because every route renders through one shared views::page.
(() => {
  const here = window.location.pathname;
  // "/" only highlights itself; the others also match their subtrees, so
  // a run page keeps Projects/Runs lit as appropriate. Longest prefix wins
  // when two could match ("/projects/new" lights Projects, not something
  // shorter).
  const nav = document.querySelector(".topbar nav");
  if (!nav) return;
  let best = null;
  for (const link of nav.querySelectorAll("a")) {
    const path = new URL(link.href, window.location.origin).pathname;
    const matches =
      path === "/" ? here === "/" : here === path || here.startsWith(`${path}/`);
    if (matches && (!best || path.length > best.path.length)) {
      best = { link, path };
    }
  }
  if (best) best.link.classList.add("active");
})();

// Copy-to-clipboard buttons. Any element with [data-copy] names the id of
// the element holding the text; the button's own label is restored after a
// moment. The project secret, webhook URL and backup file paths are the
// values an operator actually pastes elsewhere.
(() => {
  const restore = (button, label, ms) => {
    setTimeout(() => {
      button.textContent = label;
      button.classList.remove("copied");
    }, ms);
  };

  document.addEventListener("click", (event) => {
    if (!(event.target instanceof Element)) return;
    const button = event.target.closest("[data-copy]");
    if (!button) return;
    const source = document.getElementById(button.dataset.copy);
    const text = source?.textContent ?? "";
    if (!text) return;

    const label = button.textContent;
    const done = () => {
      button.textContent = "Copied";
      button.classList.add("copied");
      restore(button, label, 1600);
    };
    // Clipboard requires a secure context; the tunnel hostnames qualify,
    // a plain loopback development port usually does too.
    if (navigator.clipboard?.writeText) {
      navigator.clipboard.writeText(text).then(done, () => {});
      return;
    }
    // Fallback for an insecure origin: the text has to come from a real
    // selection, so park it in a temporary textarea next to the button.
    const scratch = document.createElement("textarea");
    scratch.value = text;
    scratch.setAttribute("readonly", "");
    scratch.style.position = "fixed";
    scratch.style.opacity = "0";
    button.after(scratch);
    scratch.select();
    try {
      document.execCommand("copy");
      done();
    } catch {
      /* best effort only */
    }
    scratch.remove();
  });
})();

// Extra-branch rows on the project form. The server renders one row per
// stored branch plus a <template> with an `__i__` placeholder in every field
// name; adding a row clones the template under a fresh number, removing one
// just drops its fieldset (a row that is never posted is a row that is gone).
(() => {
  document.addEventListener("click", (event) => {
    if (!(event.target instanceof Element)) return;

    const remove = event.target.closest("[data-remove-branch]");
    if (remove) {
      const row = remove.closest("[data-branch-row]");
      if (row) row.remove();
      return;
    }

    if (event.target.closest("[data-add-branch]")) {
      const rows = document.getElementById("branch-rows");
      const template = document.getElementById("branch-template");
      if (!rows || !template) return;
      const next = Number(rows.dataset.next || "0");
      rows.dataset.next = String(next + 1);
      rows.insertAdjacentHTML(
        "beforeend",
        template.innerHTML.replaceAll("__i__", String(next)),
      );
    }
  });
})();
