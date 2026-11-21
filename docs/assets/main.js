/* webhookr site: theme toggle, copy buttons and code blocks, mobile nav,
   docs sidebar scrollspy, scroll reveal. Plain JS, no dependencies.
   The pages work without it; the theme is first set by an inline script
   in <head> so there is no flash of the wrong one. */
(function () {
  "use strict";

  var root = document.documentElement;
  var reduceMotion = window.matchMedia && window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* ---------- Theme toggle ---------- */

  var THEME_KEY = "webhookr-theme";
  var themeMeta = document.querySelector('meta[name="theme-color"]');
  var themeButtons = document.querySelectorAll(".theme-toggle");
  var lightQuery = window.matchMedia ? window.matchMedia("(prefers-color-scheme: light)") : null;

  function systemTheme() {
    return lightQuery && lightQuery.matches ? "light" : "dark";
  }

  function currentTheme() {
    return root.getAttribute("data-theme") || systemTheme();
  }

  function applyTheme(theme) {
    root.setAttribute("data-theme", theme);
    if (themeMeta) themeMeta.setAttribute("content", theme === "light" ? "#ffffff" : "#09090b");
    themeButtons.forEach(function (button) {
      button.setAttribute("aria-label", theme === "light" ? "Switch to dark theme" : "Switch to light theme");
      button.setAttribute("title", button.getAttribute("aria-label"));
    });
  }

  applyTheme(currentTheme());

  themeButtons.forEach(function (button) {
    button.addEventListener("click", function () {
      var next = currentTheme() === "light" ? "dark" : "light";
      applyTheme(next);
      try { localStorage.setItem(THEME_KEY, next); } catch (e) { /* storage unavailable */ }
    });
  });

  // Follow the OS while the user has not picked a theme.
  if (lightQuery) {
    var onSystemChange = function () {
      var stored = null;
      try { stored = localStorage.getItem(THEME_KEY); } catch (e) {}
      if (stored !== "light" && stored !== "dark") applyTheme(systemTheme());
    };
    if (lightQuery.addEventListener) lightQuery.addEventListener("change", onSystemChange);
    else if (lightQuery.addListener) lightQuery.addListener(onSystemChange);
  }

  /* ---------- Header shadow once scrolled ---------- */

  var header = document.querySelector(".site-header");
  if (header) {
    var onScrollHeader = function () {
      header.classList.toggle("is-scrolled", window.scrollY > 8);
    };
    onScrollHeader();
    window.addEventListener("scroll", onScrollHeader, { passive: true });
  }

  /* ---------- Copy buttons ---------- */

  var COPY_ICONS =
    '<svg class="icon i-copy" viewBox="0 0 24 24" aria-hidden="true"><rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/></svg>' +
    '<svg class="icon i-check" viewBox="0 0 24 24" aria-hidden="true"><path d="M5 12.5l4.5 4.5L19 7"/></svg>';

  function copyText(text) {
    if (navigator.clipboard && window.isSecureContext) {
      return navigator.clipboard.writeText(text);
    }
    // Fallback for http:// and file:// pages.
    return new Promise(function (resolve, reject) {
      var ta = document.createElement("textarea");
      ta.value = text;
      ta.setAttribute("readonly", "");
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy") ? resolve() : reject(new Error("copy failed"));
      } catch (err) {
        reject(err);
      } finally {
        document.body.removeChild(ta);
      }
    });
  }

  function labelOf(button) {
    var label = button.querySelector(".copy-label");
    if (!label) {
      label = document.createElement("span");
      label.className = "copy-label";
      label.textContent = button.textContent.trim() || "Copy";
      button.textContent = "";
      button.insertAdjacentHTML("beforeend", COPY_ICONS);
      button.appendChild(label);
    }
    return label;
  }

  function flash(button, text, ok) {
    var label = labelOf(button);
    if (!button.dataset.label) button.dataset.label = label.textContent;
    label.textContent = text;
    if (ok) button.dataset.copied = "true";
    window.clearTimeout(button._timer);
    button._timer = window.setTimeout(function () {
      label.textContent = button.dataset.label;
      delete button.dataset.copied;
    }, 1700);
  }

  function wireCopy(button, getText) {
    labelOf(button).setAttribute("aria-live", "polite");
    button.addEventListener("click", function () {
      copyText(getText()).then(
        function () { flash(button, "Copied", true); },
        function () { flash(button, "Press Ctrl+C", false); }
      );
    });
  }

  // Explicit buttons: <button class="copy-btn" data-copy-target="#id">
  document.querySelectorAll("[data-copy-target]").forEach(function (button) {
    var target = document.querySelector(button.getAttribute("data-copy-target"));
    if (target) {
      wireCopy(button, function () { return target.textContent.trim(); });
    }
  });

  /* ---------- Code blocks: language label, copy button, light highlighting ---------- */

  function esc(text) {
    return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function span(cls, text) {
    return '<span class="' + cls + '">' + esc(text) + "</span>";
  }

  var CMD_RE = /^(\s*)(\$\s+)?(webhookr|sudo|export|mise|curl|\.\/deploy\/install\.sh)(?![\w-])/;
  var TOKEN_RE = /("[^"]*"|'[^']*')|(<[^>\s]+>)|((?:^|\s)--?[A-Za-z][\w-]*)/g;

  function highlightLine(line) {
    var comment = "";
    var cm = /(?:^|\s)(#.*)$/.exec(line);
    if (cm) {
      comment = cm[1];
      line = line.slice(0, cm.index + cm[0].length - comment.length);
    }

    var out = "";
    var rest = line;
    var head = CMD_RE.exec(line);
    if (head) {
      out += esc(head[1]);
      if (head[2]) out += span("tok-d", head[2]);
      out += span("tok-k", head[3]);
      rest = line.slice(head[0].length);
    }

    var last = 0;
    var m;
    TOKEN_RE.lastIndex = 0;
    while ((m = TOKEN_RE.exec(rest))) {
      out += esc(rest.slice(last, m.index));
      if (m[1]) out += span("tok-s", m[1]);
      else if (m[2]) out += span("tok-p", m[2]);
      else {
        var lead = /^\s/.test(m[3]) ? m[3].charAt(0) : "";
        out += esc(lead) + span("tok-f", m[3].slice(lead.length));
      }
      last = m.index + m[0].length;
    }
    out += esc(rest.slice(last));
    if (comment) out += span("tok-c", comment);
    return out;
  }

  function highlight(code, lang) {
    if (lang !== "sh" && lang !== "console") return;
    var html = code.textContent.split("\n").map(highlightLine).join("\n");
    code.innerHTML = html; // textContent is unchanged, so copying still yields the original text.
  }

  document.querySelectorAll("pre").forEach(function (pre) {
    if (pre.closest(".code-block") || pre.hasAttribute("data-no-copy")) return;
    var code = pre.querySelector("code");
    var lang = pre.getAttribute("data-lang") || "text";
    var text = pre.textContent.replace(/\n$/, "");

    if (code) highlight(code, lang);

    var wrapper = document.createElement("div");
    wrapper.className = "code-block";
    var head = document.createElement("div");
    head.className = "code-head";
    var label = document.createElement("span");
    label.className = "code-lang";
    label.textContent = lang;
    var button = document.createElement("button");
    button.type = "button";
    button.className = "copy-btn";
    button.setAttribute("aria-label", "Copy " + lang + " code to clipboard");
    button.innerHTML = COPY_ICONS + '<span class="copy-label">Copy</span>';
    head.appendChild(label);
    head.appendChild(button);

    pre.parentNode.insertBefore(wrapper, pre);
    wrapper.appendChild(head);
    wrapper.appendChild(pre);
    pre.setAttribute("tabindex", "0");
    wireCopy(button, function () { return text; });
  });

  /* ---------- Mobile nav ---------- */

  var toggle = document.querySelector(".nav-toggle");
  var nav = document.getElementById("site-nav");

  function setNav(open) {
    nav.classList.toggle("is-open", open);
    toggle.setAttribute("aria-expanded", String(open));
  }

  if (toggle && nav) {
    toggle.addEventListener("click", function () {
      setNav(!nav.classList.contains("is-open"));
    });
    nav.addEventListener("click", function (event) {
      if (event.target.closest("a")) setNav(false);
    });
    document.addEventListener("keydown", function (event) {
      if (event.key === "Escape" && nav.classList.contains("is-open")) {
        setNav(false);
        toggle.focus();
      }
    });
    document.addEventListener("click", function (event) {
      if (nav.classList.contains("is-open") && !event.target.closest(".site-header")) setNav(false);
    });
    window.addEventListener("resize", function () {
      if (window.innerWidth > 720) setNav(false);
    });
  }

  /* ---------- Docs: heading anchors ---------- */

  document.querySelectorAll(".doc-content > section[id] > h2").forEach(function (h2) {
    var link = document.createElement("a");
    link.className = "anchor";
    link.href = "#" + h2.parentNode.id;
    link.setAttribute("aria-label", "Link to this section");
    link.textContent = "#";
    h2.appendChild(link);
  });

  /* ---------- Docs: sidebar scrollspy with sliding indicator ---------- */

  var toc = document.querySelector(".toc");
  var tocLinks = Array.prototype.slice.call(document.querySelectorAll(".toc a[href^='#']"));

  if (toc && tocLinks.length) {
    var list = toc.querySelector(".toc-list");
    var items = [];
    tocLinks.forEach(function (link) {
      var section = document.getElementById(decodeURIComponent(link.getAttribute("href").slice(1)));
      if (section) items.push({ link: link, section: section });
    });

    var indicator = document.createElement("span");
    indicator.className = "toc-indicator";
    indicator.setAttribute("aria-hidden", "true");
    list.appendChild(indicator);

    var tocToggle = toc.querySelector(".toc-toggle");
    if (tocToggle) {
      tocToggle.addEventListener("click", function () {
        var open = toc.classList.toggle("is-open");
        tocToggle.setAttribute("aria-expanded", String(open));
        place();
      });
      toc.addEventListener("click", function (event) {
        if (window.innerWidth <= 860 && event.target.closest(".toc-body a")) {
          toc.classList.remove("is-open");
          tocToggle.setAttribute("aria-expanded", "false");
        }
      });
    }

    var active = null;

    function place() {
      if (!active || !active.link.getClientRects().length) {
        indicator.style.opacity = "0";
        return;
      }
      var lr = active.link.getBoundingClientRect();
      var pr = list.getBoundingClientRect();
      indicator.style.height = lr.height + "px";
      indicator.style.transform = "translateY(" + (lr.top - pr.top) + "px)";
      indicator.style.opacity = "1";

      // Keep the active link visible inside the sticky sidebar.
      if (toc.scrollHeight > toc.clientHeight + 2 && window.innerWidth > 860) {
        var tr = toc.getBoundingClientRect();
        if (lr.top < tr.top + 48) toc.scrollTop -= tr.top + 48 - lr.top;
        else if (lr.bottom > tr.bottom - 48) toc.scrollTop += lr.bottom - (tr.bottom - 48);
      }
    }

    function setActive(item) {
      if (item === active) return;
      active = item;
      items.forEach(function (it) {
        var on = it === item;
        it.link.classList.toggle("active", on);
        if (on) it.link.setAttribute("aria-current", "true");
        else it.link.removeAttribute("aria-current");
      });
      place();
    }

    var ticking = false;
    function update() {
      ticking = false;
      var line = (header ? header.offsetHeight : 0) + 32;
      var current = items[0];
      for (var i = 0; i < items.length; i++) {
        if (items[i].section.getBoundingClientRect().top - line <= 0) current = items[i];
      }
      if (window.innerHeight + window.scrollY >= document.documentElement.scrollHeight - 4) {
        current = items[items.length - 1];
      }
      setActive(current);
    }

    function schedule() {
      if (!ticking) {
        ticking = true;
        window.requestAnimationFrame(update);
      }
    }

    window.addEventListener("scroll", schedule, { passive: true });
    window.addEventListener("resize", function () { schedule(); place(); });
    update();
  }

  /* ---------- Scroll reveal and card spotlight ---------- */

  var reveals = Array.prototype.slice.call(document.querySelectorAll(".reveal"));
  if (reveals.length) {
    reveals.forEach(function (el) {
      var siblings = Array.prototype.filter.call(el.parentNode.children, function (c) {
        return c.classList.contains("reveal");
      });
      el.style.setProperty("--i", String(siblings.indexOf(el) % 4));
    });

    if ("IntersectionObserver" in window && !reduceMotion) {
      var revealObserver = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (entry) {
            if (entry.isIntersecting) {
              entry.target.classList.add("is-visible");
              revealObserver.unobserve(entry.target);
            }
          });
        },
        { threshold: 0.1, rootMargin: "0px 0px -5% 0px" }
      );
      reveals.forEach(function (el) { revealObserver.observe(el); });
    } else {
      reveals.forEach(function (el) { el.classList.add("is-visible"); });
    }
  }

  document.querySelectorAll(".card").forEach(function (card) {
    card.addEventListener("pointermove", function (event) {
      var rect = card.getBoundingClientRect();
      card.style.setProperty("--mx", event.clientX - rect.left + "px");
      card.style.setProperty("--my", event.clientY - rect.top + "px");
    });
  });
})();
