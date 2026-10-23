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
