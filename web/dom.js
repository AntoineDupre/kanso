// Small DOM helpers shared by the shell, board, panel and forms.

const SVG_NS = "http://www.w3.org/2000/svg";

const ICONS = {
  plus: [["path", { d: "M12 5v14M5 12h14" }]],
  search: [
    ["circle", { cx: "11", cy: "11", r: "7" }],
    ["path", { d: "m20 20-3.5-3.5" }]
  ],
  close: [["path", { d: "M6 6l12 12M18 6 6 18" }]],
  chevronDown: [["path", { d: "m6 9 6 6 6-6" }]],
  chevronLeft: [["path", { d: "m15 18-6-6 6-6" }]],
  chevronRight: [["path", { d: "m9 18 6-6-6-6" }]],
  gear: [
    ["circle", { cx: "12", cy: "12", r: "3" }],
    [
      "path",
      {
        d: "M19.4 15a1.7 1.7 0 0 0 .3 1.9l.1.1-2 3.4-.2-.1a1.7 1.7 0 0 0-1.8.2 1.7 1.7 0 0 0-.8 1.6V22h-4v-.1a1.7 1.7 0 0 0-1-1.5 1.7 1.7 0 0 0-1.8.2l-.2.1-2-3.4.1-.1a1.7 1.7 0 0 0 .3-1.9 1.7 1.7 0 0 0-1.5-1H4v-4h.1a1.7 1.7 0 0 0 1.5-1 1.7 1.7 0 0 0-.2-1.8l-.1-.2 2-3.4.2.1a1.7 1.7 0 0 0 1.8.2A1.7 1.7 0 0 0 10.1 2V2h4v.1a1.7 1.7 0 0 0 .8 1.5 1.7 1.7 0 0 0 1.9-.2l.2-.1 2 3.4-.1.2a1.7 1.7 0 0 0-.3 1.8 1.7 1.7 0 0 0 1.5 1h.1v4h-.1a1.7 1.7 0 0 0-1.5 1Z"
      }
    ]
  ],
  bell: [
    ["path", { d: "M18 8a6 6 0 1 0-12 0c0 7-3 7-3 9h18c0-2-3-2-3-9" }],
    ["path", { d: "M10 21h4" }]
  ],
  logout: [
    ["path", { d: "M15 3h4a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2h-4" }],
    ["path", { d: "m10 17 5-5-5-5M15 12H3" }]
  ],
  check: [["path", { d: "m5 12 4 4L19 6" }]],
  trash: [
    ["path", { d: "M4 7h16M9 7V4h6v3M6 7l1 14h10l1-14" }],
    ["path", { d: "M10 11v6M14 11v6" }]
  ],
  arrowUp: [["path", { d: "m6 15 6-6 6 6" }]],
  arrowDown: [["path", { d: "m6 9 6 6 6-6" }]],
  lock: [
    ["rect", { x: "5", y: "11", width: "14", height: "10", rx: "2" }],
    ["path", { d: "M8 11V8a4 4 0 0 1 8 0v3" }]
  ],
  alert: [
    ["circle", { cx: "12", cy: "12", r: "9" }],
    ["path", { d: "M12 8v5M12 16h.01" }]
  ],
  menu: [
    ["circle", { cx: "5", cy: "12", r: "1" }],
    ["circle", { cx: "12", cy: "12", r: "1" }],
    ["circle", { cx: "19", cy: "12", r: "1" }]
  ],
  copy: [
    ["rect", { x: "9", y: "9", width: "11", height: "11", rx: "2" }],
    ["path", { d: "M5 15V5a2 2 0 0 1 2-2h10" }]
  ],
  inbox: [
    ["path", { d: "M4 4h16v16H4z" }],
    ["path", { d: "M4 14h5l2 3h2l2-3h5" }]
  ],
  board: [
    ["rect", { x: "3", y: "4", width: "5", height: "16", rx: "1" }],
    ["rect", { x: "10", y: "4", width: "5", height: "16", rx: "1" }],
    ["rect", { x: "17", y: "4", width: "4", height: "16", rx: "1" }]
  ],
  globe: [
    ["circle", { cx: "12", cy: "12", r: "9" }],
    ["path", { d: "M3 12h18M12 3a15 15 0 0 1 0 18M12 3a15 15 0 0 0 0 18" }]
  ],
  user: [
    ["circle", { cx: "12", cy: "8", r: "4" }],
    ["path", { d: "M4 21a8 8 0 0 1 16 0" }]
  ],
  key: [
    ["circle", { cx: "8", cy: "15", r: "4" }],
    ["path", { d: "m11 12 9-9M17 6l3 3" }]
  ],
  link: [
    ["path", { d: "M10 13a5 5 0 0 0 7.5.5l3-3a5 5 0 0 0-7-7l-1.7 1.7" }],
    ["path", { d: "M14 11a5 5 0 0 0-7.5-.5l-3 3a5 5 0 0 0 7 7l1.7-1.7" }]
  ]
};

export function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props || {})) {
    if (value === null || value === undefined || value === false) continue;
    if (key === "class") {
      node.className = value;
    } else if (key === "text") {
      node.textContent = String(value);
    } else if (key === "dataset") {
      for (const [name, data] of Object.entries(value)) {
        if (data !== null && data !== undefined) node.dataset[name] = String(data);
      }
    } else if (key === "style") {
      for (const [name, styleValue] of Object.entries(value)) {
        if (name.startsWith("--")) node.style.setProperty(name, styleValue);
        else node.style[name] = styleValue;
      }
    } else if (key === "value") {
      node.value = value;
    } else if (key === "checked" || key === "selected" || key === "disabled" || key === "required") {
      node[key] = Boolean(value);
    } else if (key.startsWith("on") && typeof value === "function") {
      node.addEventListener(key.slice(2).toLowerCase(), value);
    } else if (value === true) {
      node.setAttribute(key, "");
    } else {
      node.setAttribute(key, String(value));
    }
  }
  append(node, children);
  return node;
}

export function append(node, children) {
  const list = Array.isArray(children) ? children : [children];
  for (const child of list) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

export function icon(name, { size = 16, label = "" } = {}) {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "2");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  if (label) {
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", label);
  } else {
    svg.setAttribute("aria-hidden", "true");
    svg.setAttribute("focusable", "false");
  }
  const shapes = ICONS[name] || ICONS.alert;
  for (const [tag, attributes] of shapes) {
    const shape = document.createElementNS(SVG_NS, tag);
    for (const [key, value] of Object.entries(attributes)) shape.setAttribute(key, value);
    svg.append(shape);
  }
  return svg;
}

export function button(label, { variant = "secondary", iconName = "", iconOnly = false, ...rest } = {}) {
  const classes = ["button", `button-${variant}`];
  if (iconOnly) classes.push("button-icon");
  return el(
    "button",
    { type: "button", class: classes.join(" "), "aria-label": iconOnly ? label : null, ...rest },
    [iconName ? icon(iconName, { size: 16 }) : null, iconOnly ? null : el("span", { text: label })]
  );
}

export function iconButton(label, name, props = {}) {
  return el(
    "button",
    { type: "button", class: "button button-ghost button-icon", "aria-label": label, title: label, ...props },
    [icon(name, { size: 17 })]
  );
}

export function spacer() {
  return el("div", { class: "spacer" });
}

export function debounce(fn, wait = 200) {
  let timer = null;
  return (...args) => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  };
}

export function isTyping(target) {
  if (!target || !(target instanceof Element)) return false;
  const tag = target.tagName;
  return (
    tag === "INPUT" ||
    tag === "TEXTAREA" ||
    tag === "SELECT" ||
    target.isContentEditable ||
    target.closest("[contenteditable='true']") !== null
  );
}

export function relativeTime(millis, t) {
  if (!millis) return "";
  const seconds = Math.max(0, Math.round((Date.now() - millis) / 1000));
  if (seconds < 60) return t("time.justNow");
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return t("time.minutesAgo", { n: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t("time.hoursAgo", { n: hours });
  const days = Math.floor(hours / 24);
  if (days < 14) return t("time.daysAgo", { n: days });
  return new Date(millis).toLocaleDateString();
}

export function todayISO() {
  const now = new Date();
  const pad = (value) => String(value).padStart(2, "0");
  return `${now.getFullYear()}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`;
}

export function isOverdue(iso) {
  return Boolean(iso) && iso < todayISO();
}

export function formatDue(iso) {
  if (!iso) return "";
  const [year, month, day] = iso.split("-").map(Number);
  if (!year || !month || !day) return iso;
  const date = new Date(year, month - 1, day);
  const options = { month: "short", day: "numeric" };
  if (year !== new Date().getFullYear()) options.year = "numeric";
  return date.toLocaleDateString(undefined, options);
}

// ---------------------------------------------------------------------------
// Markdown

const DROP_WITH_CONTENT = new Set([
  "script",
  "style",
  "iframe",
  "object",
  "embed",
  "svg",
  "math",
  "form",
  "input",
  "button",
  "select",
  "textarea",
  "video",
  "audio",
  "source",
  "link",
  "meta",
  "base",
  "template",
  "canvas"
]);

const ALLOWED_TAGS = new Set([
  "a",
  "blockquote",
  "br",
  "code",
  "del",
  "em",
  "h1",
  "h2",
  "h3",
  "h4",
  "h5",
  "h6",
  "hr",
  "li",
  "ol",
  "p",
  "pre",
  "s",
  "strong",
  "sub",
  "sup",
  "table",
  "tbody",
  "td",
  "th",
  "thead",
  "tr",
  "ul"
]);

const ALLOWED_ATTRS = {
  a: new Set(["href", "title"]),
  code: new Set(["class"]),
  ol: new Set(["start"]),
  td: new Set(["align"]),
  th: new Set(["align"])
};

function safeHref(raw) {
  const value = String(raw).trim();
  if (!value) return null;
  if (value.startsWith("#") || value.startsWith("/") || value.startsWith("./") || value.startsWith("../")) {
    return value;
  }
  try {
    const url = new URL(value, window.location.origin);
    if (url.protocol === "http:" || url.protocol === "https:" || url.protocol === "mailto:") {
      return value;
    }
  } catch {
    return null;
  }
  return null;
}

function sanitizeNode(root) {
  let child = root.firstChild;
  while (child) {
    const next = child.nextSibling;
    if (child.nodeType === Node.TEXT_NODE) {
      child = next;
      continue;
    }
    if (child.nodeType !== Node.ELEMENT_NODE) {
      child.remove();
      child = next;
      continue;
    }
    const tag = child.localName.toLowerCase();
    if (tag === "img") {
      const alt = child.getAttribute("alt");
      if (alt) {
        child.replaceWith(document.createTextNode(alt));
      } else {
        child.remove();
      }
      child = next;
      continue;
    }
    if (DROP_WITH_CONTENT.has(tag)) {
      child.remove();
      child = next;
      continue;
    }
    if (!ALLOWED_TAGS.has(tag)) {
      while (child.firstChild) root.insertBefore(child.firstChild, child);
      child.remove();
      // Restart: the unwrapped children still need sanitizing.
      child = root.firstChild;
      continue;
    }

    const allowed = ALLOWED_ATTRS[tag] || new Set();
    for (const attribute of Array.from(child.attributes)) {
      const name = attribute.name.toLowerCase();
      if (name.startsWith("on") || !allowed.has(name)) {
        child.removeAttribute(attribute.name);
      }
    }
    if (tag === "a") {
      const href = child.getAttribute("href");
      if (href === null) {
        // Keep the link text, drop the node's link semantics.
        while (child.firstChild) root.insertBefore(child.firstChild, child);
        child.remove();
        child = root.firstChild;
        continue;
      }
      const safe = safeHref(href);
      if (safe === null) {
        child.removeAttribute("href");
      } else {
        child.setAttribute("href", safe);
        child.setAttribute("target", "_blank");
        child.setAttribute("rel", "noopener noreferrer");
      }
    }
    if (tag === "code") {
      const className = child.getAttribute("class");
      if (className && !/^language-[a-z0-9_+-]+$/i.test(className)) {
        child.removeAttribute("class");
      }
    }
    sanitizeNode(child);
    child = next;
  }
}

function configureMarked() {
  const marked = window.marked;
  if (!marked || typeof marked.parse !== "function") return null;
  if (!marked.__kansoConfigured) {
    const renderer = new marked.Renderer();
    // Do not interpret raw HTML: show it as text instead.
    renderer.html = (token) => {
      const text = typeof token === "string" ? token : token && token.text ? token.text : "";
      return String(text)
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;");
    };
    marked.use({
      renderer,
      gfm: true,
      breaks: true,
      mangle: false,
      headerIds: false,
      async: false
    });
    marked.__kansoConfigured = true;
  }
  return marked;
}

/// Render Markdown into `container`; the only innerHTML in the UI, guarded by
/// the allow-list sanitizer above.
export function renderMarkdown(container, text) {
  const source = text || "";
  const marked = configureMarked();
  if (!marked) {
    container.textContent = source;
    return;
  }
  let html = "";
  try {
    html = marked.parse(source);
  } catch {
    container.textContent = source;
    return;
  }
  const template = document.createElement("template");
  template.innerHTML = html;
  sanitizeNode(template.content);
  container.replaceChildren(...template.content.childNodes);
}

// ---------------------------------------------------------------------------
// Toasts

let toastCounter = 0;

export function toast(message, tone = "info", { timeout = 4200 } = {}) {
  const host = document.getElementById("toasts");
  if (!host) return;
  const item = el("div", { class: `toast toast-${tone}`, role: "status" }, [
    el("span", { class: "toast-message", text: message })
  ]);
  item.dataset.toastId = String(++toastCounter);
  host.append(item);
  while (host.children.length > 5) host.firstElementChild.remove();
  window.setTimeout(() => {
    item.classList.add("toast-leaving");
    window.setTimeout(() => item.remove(), 180);
  }, timeout);
}

export function toastForError(error, t) {
  const code = error && error.code;
  const known = [
    "already_claimed",
    "dependency_unmet",
    "not_claimable",
    "not_proposed",
    "not_claimed",
    "not_active",
    "forbidden"
  ];
  if (code && known.includes(code)) {
    toast(t(`toast.${code}`), "warning");
    return;
  }
  if (error && error.status === 401) return;
  toast((error && (error.message || error.userMessage)) || t("toast.error"), "error");
}
