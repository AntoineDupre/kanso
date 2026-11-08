// Kanso web shell: routing, store, live events and keyboard shortcuts.

import { t } from "./i18n/index.js";
import { api, setUnauthorizedHandler } from "./api.js";
import {
  el,
  icon,
  iconButton,
  isTyping,
  debounce,
  toast,
  toastForError,
  relativeTime
} from "./dom.js";
import { createBoardView } from "./board.js";
import { Panel } from "./panel.js";
import { renderImports } from "./imports.js";
import {
  openCardForm,
  openProjectForm,
  openTokenForm,
  openMoveMenu,
  confirmDialog,
  hasModal,
  closeAllModals,
  openModal
} from "./forms.js";

const LS = {
  lastProject: "kanso.lastProject",
  panelWidth: "kanso.panel.width",
  panelCollapsed: "kanso.panel.collapsed",
  notifications: "kanso.notifications"
};

const EVENT_TYPES = [
  "task.created",
  "task.updated",
  "task.moved",
  "task.claimed",
  "task.released",
  "task.archived",
  "task.restored",
  "task.deleted",
  "dependencies.changed",
  "proposal.created",
  "proposal.accepted",
  "proposal.rejected",
  "project.created",
  "project.updated",
  "project.archived",
  "comment.created",
  "comment.updated",
  "secret.updated",
  "checklist.created",
  "checklist.updated",
  "checklist.deleted",
  "label.created",
  "label.updated",
  "label.deleted"
];

const state = {
  me: null,
  projects: [],
  allProjects: [],
  labels: [],
  inbox: [],
  integration: null,
  boards: [],
  route: { name: "home" },
  routeToken: 0,
  search: "",
  showAll: false,
  selectedRef: null,
  currentProjectKey: null,
  currentProjectId: null,
  pageVisible: false,
  boardReady: false,
  eventSource: null,
  reconnectTimer: null,
  reconnectDelay: 1000,
  connectedOnce: false,
  initialLoaded: false,
  refreshOnFirstOpen: false,
  offlineNotified: false,
  notificationsEnabled: localStorage.getItem(LS.notifications) === "1",
  previousCards: new Map(),
  dom: null,
  boardView: null,
  panel: null,
  starting: false,
  loggingOut: false
};

const refreshBoardDebounced = debounce(() => {
  refreshBoard().catch(() => {});
}, 180);

// ---------------------------------------------------------------------------
// Boot and authentication

const root = document.getElementById("app-root");

setUnauthorizedHandler(() => {
  if (state.me && !state.loggingOut) handleSessionExpired();
});

async function boot() {
  root.replaceChildren(
    el("div", { class: "boot" }, [
      el("span", { class: "spinner", "aria-hidden": "true" }),
      el("span", { class: "muted", text: t("app.loading") })
    ])
  );
  try {
    const me = await api.me();
    await startApp(me);
  } catch (error) {
    if (error.status === 401) {
      renderLogin();
    } else {
      renderFatal(error);
    }
  }
}

function renderFatal(error) {
  teardownClient();
  root.replaceChildren(
    el("div", { class: "login-screen" }, [
      el("div", { class: "login-card" }, [
        el("h1", { class: "brand-large", text: t("app.name") }),
        el("p", { class: "muted", text: t("login.fatal") }),
        el("p", { class: "form-error", text: (error && error.message) || t("common.error") }),
        el(
          "button",
          {
            type: "button",
            class: "button button-primary",
            onClick: () => window.location.reload()
          },
          [el("span", { text: t("common.tryAgain") })]
        )
      ])
    ])
  );
}

function renderLogin(message = "") {
  teardownClient();
  const password = el("input", {
    class: "input",
    type: "password",
    id: "login-password",
    autocomplete: "current-password",
    required: true,
    "aria-label": t("login.password")
  });
  const error = el("p", { class: "form-error", role: "alert", hidden: !message, text: message });
  const submit = el("button", { type: "submit", class: "button button-primary" }, [
    el("span", { text: t("login.submit") })
  ]);
  const form = el("form", { class: "login-form" }, [
    el("label", { class: "field" }, [
      el("span", { class: "field-label", text: t("login.password") }),
      password
    ]),
    error,
    submit
  ]);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    submit.disabled = true;
    submit.setAttribute("aria-busy", "true");
    try {
      await api.login(password.value);
      const me = await api.me();
      await startApp(me);
    } catch (failure) {
      if (failure.status === 401) {
        error.textContent = t("login.invalid");
      } else if (failure.status === 429) {
        error.textContent = t("login.rateLimited");
      } else {
        error.textContent = failure.message || t("login.failed");
      }
      error.hidden = false;
      password.select();
    } finally {
      submit.disabled = false;
      submit.removeAttribute("aria-busy");
    }
  });

  root.replaceChildren(
    el("div", { class: "login-screen" }, [
      el("main", { class: "login-card" }, [
        el("div", { class: "login-brand" }, [
          el("span", { class: "brand-mark", text: "K" }),
          el("h1", { class: "brand-large", text: t("login.title") })
        ]),
        el("p", { class: "muted login-subtitle", text: t("login.subtitle") }),
        form
      ])
    ])
  );
  window.setTimeout(() => password.focus(), 0);
}

function handleSessionExpired() {
  state.loggingOut = true;
  renderLogin(t("toast.unauthorized"));
  state.loggingOut = false;
}

function teardownClient() {
  closeEvents();
  closeAllModals();
  if (state.panel) state.panel.hide();
  if (state.boardView && state.boardView.destroy) state.boardView.destroy();
  state.me = null;
  state.projects = [];
  state.allProjects = [];
  state.labels = [];
  state.inbox = [];
  state.integration = null;
  state.boards = [];
  state.selectedRef = null;
  state.currentProjectKey = null;
  state.currentProjectId = null;
  state.pageVisible = false;
  state.boardReady = false;
  state.routeToken += 1;
  state.route = { name: "login" };
  state.dom = null;
  state.boardView = null;
  state.panel = null;
  state.previousCards = new Map();
  state.connectedOnce = false;
  state.initialLoaded = false;
  state.refreshOnFirstOpen = false;
  state.offlineNotified = false;
  state.reconnectDelay = 1000;
  state.reconnectTimer = null;
}

async function startApp(me) {
  if (state.starting) return;
  state.starting = true;
  try {
    teardownClient();
    state.me = me;
    buildShell();
    await loadCore();
    await renderRoute();
    state.initialLoaded = true;
    connectEvents();
    if (state.refreshOnFirstOpen) {
      state.refreshOnFirstOpen = false;
      refreshAfterReconnect();
    }
  } catch (error) {
    if (error.status === 401) {
      renderLogin();
    } else {
      renderFatal(error);
    }
  } finally {
    state.starting = false;
  }
}

// ---------------------------------------------------------------------------
// Shell

function buildShell() {
  const projectSelect = el("select", {
    class: "project-switcher",
    "aria-label": t("form.project")
  });
  const tabBoard = el(
    "button",
    { type: "button", class: "tab", "aria-label": t("nav.board"), title: t("nav.board") },
    [icon("board", { size: 15 }), el("span", { class: "tab-label", text: t("nav.board") })]
  );
  const tabGlobal = el(
    "button",
    { type: "button", class: "tab", "aria-label": t("nav.global"), title: t("nav.global") },
    [icon("globe", { size: 15 }), el("span", { class: "tab-label", text: t("nav.global") })]
  );
  const inboxBadge = el("span", { class: "tab-badge", hidden: true });
  const tabInbox = el(
    "button",
    { type: "button", class: "tab", "aria-label": t("nav.inbox"), title: t("nav.inbox") },
    [
      icon("inbox", { size: 15 }),
      el("span", { class: "tab-label", text: t("nav.inbox") }),
      inboxBadge
    ]
  );

  const search = el("input", {
    class: "search-input",
    type: "search",
    placeholder: t("board.searchPlaceholder"),
    "aria-label": t("board.searchPlaceholder")
  });

  const newCard = el(
    "button",
    {
      type: "button",
      class: "button button-primary topbar-new",
      "aria-label": t("board.newCard"),
      title: t("board.newCard"),
      onClick: newCardFlow
    },
    [icon("plus", { size: 16 }), el("span", { class: "button-label", text: t("board.newCard") })]
  );

  const notifications = iconButton(t("app.notifications"), "bell", {
    onClick: toggleNotifications
  });
  notifications.classList.toggle("active", state.notificationsEnabled);

  const help = iconButton(t("app.keyboardHelp"), "help", {
    onClick: openKeyboardHelp
  });

  const settings = iconButton(t("app.settings"), "gear", {
    onClick: () => navigate("/settings/projects")
  });
  const logout = iconButton(t("app.logout"), "logout", { onClick: logoutFlow });

  const globalToggle = el("label", { class: "global-toggle", hidden: true }, [
    el("input", { type: "checkbox", checked: state.showAll }),
    el("span", { text: t("board.showAll") })
  ]);

  const topbar = el("header", { class: "topbar" }, [
    el("div", { class: "topbar-left" }, [
      el(
        "button",
        {
          type: "button",
          class: "brand",
          title: t("app.name"),
          onClick: () => navigate(returnToBoardPath())
        },
        [el("span", { class: "brand-mark", text: "K" }), el("span", { class: "brand-name", text: t("app.name") })]
      ),
      projectSelect
    ]),
    el("nav", { class: "tabs", "aria-label": t("app.name") }, [tabBoard, tabGlobal, tabInbox]),
    el("div", { class: "topbar-right" }, [
      el("div", { class: "search-box" }, [icon("search", { size: 15 }), search]),
      globalToggle,
      newCard,
      state.me?.scope === "human"
        ? iconButton(t("imports.title"), "download", { onClick: () => navigate("/imports") })
        : null,
      notifications,
      help,
      settings,
      logout
    ])
  ]);

  const boardRegion = el("section", { class: "board-region", "aria-label": t("nav.board") });
  const pageRegion = el("section", { class: "page-region", hidden: true });
  const panelRoot = el("aside", { class: "panel", id: "panel-root" });
  const main = el("main", { class: "main" }, [boardRegion, pageRegion, panelRoot]);
  const shell = el("div", { class: "app-shell" }, [topbar, main]);
  root.replaceChildren(shell);

  const boardView = createBoardView(boardRegion, {
    onSelect: selectCard,
    onMove: moveCard,
    onQuickAdd: quickAdd,
    onMoveMenu: (task) => openMoveMenu(task, (payload) => moveCardById(task, payload)),
    onDragEnd: () => {
      if (isBoardRoute()) renderBoardView();
    }
  });

  const savedWidth = Number(localStorage.getItem(LS.panelWidth)) || 420;
  const savedCollapsed = localStorage.getItem(LS.panelCollapsed) === "1";
  const panel = new Panel(panelRoot, {
    me: state.me,
    labels: state.labels,
    projects: state.projects,
    integration: state.integration,
    width: savedWidth,
    collapsed: savedCollapsed,
    onClose: () => closePanel({ navigate: true }),
    onChanged: (detail) => applyDetailToStore(detail),
    onDeleted: (detail) => {
      removeCard(detail.id);
      closePanel({ navigate: true });
    },
    onLabelsChanged: (labels) => {
      state.labels = labels;
      if (state.panel) state.panel.setLabels(labels);
    },
    onNotFound: (reference) => showTaskNotFound(reference),
    onOpenReference: (reference) => navigate(taskPath(reference)),
    onWidthChange: (width) => localStorage.setItem(LS.panelWidth, String(width)),
    onCollapsedChange: (collapsed) =>
      localStorage.setItem(LS.panelCollapsed, collapsed ? "1" : "0")
  });

  state.boardView = boardView;
  state.panel = panel;
  state.dom = {
    shell,
    topbar,
    projectSelect,
    tabBoard,
    tabGlobal,
    tabInbox,
    inboxBadge,
    search,
    newCard,
    notifications,
    help,
    settings,
    logout,
    globalToggle,
    boardRegion,
    pageRegion,
    panelRoot
  };

  projectSelect.addEventListener("change", () => {
    if (projectSelect.value === "all") {
      navigate("/global");
      return;
    }
    const project = state.projects.find((item) => item.id === projectSelect.value);
    if (project) navigate(`/p/${encodeURIComponent(project.key)}`);
  });
  tabBoard.addEventListener("click", () => navigate(boardPath()));
  tabGlobal.addEventListener("click", () => navigate("/global"));
  tabInbox.addEventListener("click", () => navigate("/inbox"));
  search.addEventListener("input", () => {
    state.search = search.value;
    if (isBoardRoute()) renderBoardView();
  });
  search.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.stopPropagation();
      search.value = "";
      state.search = "";
      search.blur();
      if (isBoardRoute()) renderBoardView();
    }
  });
  globalToggle.querySelector("input").addEventListener("change", (event) => {
    state.showAll = event.target.checked;
    renderBoardView();
  });

  document.removeEventListener("keydown", onGlobalKeyDown);
  document.addEventListener("keydown", onGlobalKeyDown);
}

async function loadCore() {
  const [projects, labels, inbox] = await Promise.all([
    api.projects(),
    api.labels(),
    api.inbox()
  ]);
  state.projects = projects;
  state.allProjects = projects;
  state.labels = labels;
  state.inbox = inbox;
  if (state.panel) {
    state.panel.setLabels(labels);
    state.panel.setProjects(projects);
  }
  renderChrome();
  // The version probe can take up to 2 s when OpenChamber is down; never block
  // the first board render on it. A null response keeps the last known value.
  api.integration().then((response) => {
    if (!response || !response.openchamber) return;
    state.integration = response.openchamber;
    if (state.panel) state.panel.setIntegration(state.integration);
  });
}

function renderChrome() {
  const dom = state.dom;
  if (!dom) return;
  const route = state.route;

  const options = [el("option", { value: "all", text: t("nav.allProjects") })];
  for (const project of state.projects) {
    options.push(
      el("option", {
        value: project.id,
        text: `${project.key} — ${project.name}`,
        selected: project.key === state.currentProjectKey
      })
    );
  }
  dom.projectSelect.replaceChildren(...options);
  dom.projectSelect.value =
    route.name === "global"
      ? "all"
      : state.projects.find((project) => project.key === state.currentProjectKey)?.id || "all";

  dom.tabBoard.classList.toggle("active", ["home", "project", "task"].includes(route.name));
  dom.tabGlobal.classList.toggle("active", route.name === "global");
  dom.tabInbox.classList.toggle("active", route.name === "inbox");
  dom.settings.classList.toggle(
    "active",
    ["settings-projects", "settings-tokens", "settings-integration", "settings-export"].includes(
      route.name
    )
  );
  dom.globalToggle.hidden = route.name !== "global";
  dom.search.value = state.search;
  dom.search.closest(".search-box").hidden = !isBoardRoute();

  const count = state.inbox.length;
  dom.inboxBadge.hidden = count === 0;
  dom.inboxBadge.textContent = String(count);
  const inboxLabel =
    count === 0
      ? t("nav.inbox")
      : count === 1
        ? t("nav.inboxWithOne")
        : t("nav.inboxWithCount", { count });
  dom.tabInbox.setAttribute("aria-label", inboxLabel);
  dom.tabInbox.setAttribute("title", inboxLabel);
  dom.notifications.classList.toggle("active", state.notificationsEnabled);
  dom.newCard.disabled = false;
}

function isBoardRoute() {
  return ["home", "project", "task", "global"].includes(state.route.name);
}

function returnToBoardPath() {
  return boardPath();
}

function boardPath() {
  if (
    state.currentProjectKey &&
    state.projects.some((project) => project.key === state.currentProjectKey)
  ) {
    return `/p/${encodeURIComponent(state.currentProjectKey)}`;
  }
  const last = localStorage.getItem(LS.lastProject);
  if (last && state.projects.some((project) => project.key === last)) {
    return `/p/${encodeURIComponent(last)}`;
  }
  return state.route.name === "global" ? "/global" : "/";
}

function taskPath(reference) {
  if (!reference) return boardPath();
  const [key, number] = reference.split("-");
  if (!key || !number) return boardPath();
  return `/p/${encodeURIComponent(key)}/task/${encodeURIComponent(number)}`;
}

// ---------------------------------------------------------------------------
// Routing

function decodePart(value) {
  try {
    return decodeURIComponent(value);
  } catch {
    return null;
  }
}

function parseRoute(pathname) {
  const path = pathname.replace(/\/+$/, "") || "/";
  if (path === "/" || path === "/index.html") return { name: "home" };
  if (path === "/global") return { name: "global" };
  if (path === "/inbox") return { name: "inbox" };
  if (path === "/imports") return { name: "imports" };
  if (path === "/settings/projects") return { name: "settings-projects" };
  if (path === "/settings/tokens") return { name: "settings-tokens" };
  if (path === "/settings/integration") return { name: "settings-integration" };
  if (path === "/settings/export") return { name: "settings-export" };
  let match = path.match(/^\/p\/([^/]+)$/);
  if (match) {
    const key = decodePart(match[1]);
    return key ? { name: "project", key } : { name: "not-found", path };
  }
  match = path.match(/^\/p\/([^/]+)\/task\/(\d+)$/);
  if (match) {
    const key = decodePart(match[1]);
    if (!key) return { name: "not-found", path };
    return {
      name: "task",
      key,
      number: Number(match[2]),
      reference: `${key.toUpperCase()}-${Number(match[2])}`
    };
  }
  return { name: "not-found", path };
}

function navigate(path, { replace = false } = {}) {
  if (!replace && path === location.pathname) {
    // Re-render without adding a duplicate entry for the active tab/brand.
    renderRoute().catch((error) => {
      if (error.status !== 401) toastForError(error, t);
    });
    return;
  }
  if (replace) history.replaceState({}, "", path);
  else history.pushState({}, "", path);
  renderRoute().catch((error) => {
    if (error.status !== 401) toastForError(error, t);
  });
}

window.addEventListener("popstate", () => {
  renderRoute().catch((error) => {
    if (error.status !== 401) toastForError(error, t);
  });
});

async function renderRoute() {
  const route = parseRoute(location.pathname);
  state.route = route;
  const token = ++state.routeToken;
  renderChrome();

  switch (route.name) {
    case "home":
      await renderHome(token);
      break;
    case "project":
      await renderProject(route.key, null, token);
      break;
    case "task":
      await renderProject(route.key, route, token);
      break;
    case "global":
      await renderGlobal(token);
      break;
    case "inbox":
      showPageRegion();
      renderInboxPage();
      renderChrome();
      break;
    case "imports":
      showPageRegion();
      await renderImports(state.dom.pageRegion, state.projects, () => state.routeToken === token, navigate);
      break;
    case "settings-projects":
      showPageRegion();
      await renderSettingsProjects(token);
      break;
    case "settings-tokens":
      showPageRegion();
      await renderSettingsTokens(token);
      break;
    case "settings-integration":
      showPageRegion();
      await renderSettingsIntegration(token);
      break;
    case "settings-export":
      showPageRegion();
      await renderSettingsExport(token);
      break;
    default:
      showPageRegion();
      showNotFoundPage(
        t("common.routeNotFoundTitle"),
        t("common.routeNotFoundBody"),
        boardPath()
      );
  }
}

async function renderHome(token) {
  const last = localStorage.getItem(LS.lastProject);
  const project =
    state.projects.find((item) => item.key === last) || state.projects[0] || null;
  if (!project) {
    showPageRegion();
    renderNoProjects();
    return;
  }
  navigate(`/p/${encodeURIComponent(project.key)}`, { replace: true });
  return token;
}

async function renderProject(key, taskRoute, token) {
  showBoardRegion();
  state.boardReady = false;
  renderLoading(t("app.loading"));
  try {
    const data = await api.board(key);
    if (state.routeToken !== token) return;
    const board = data.projects && data.projects[0];
    if (!board) {
      showProjectNotFound(key);
      return;
    }
    state.boardReady = true;
    state.boards = [board];
    state.currentProjectKey = board.project.key;
    state.currentProjectId = board.project.id;
    localStorage.setItem(LS.lastProject, board.project.key);
    state.selectedRef = taskRoute ? `${board.project.key}-${taskRoute.number}` : null;
    state.search = state.search || "";
    if (state.panel) state.panel.setLabels(state.labels);
    renderBoardView();
    renderChrome();
    rememberCards();

    if (taskRoute) {
      if (state.panel) {
        await state.panel.open(state.selectedRef, { force: true });
      }
    } else if (state.panel) {
      state.panel.hide();
    }
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status === 404) {
      showProjectNotFound(key);
    } else if (error.status !== 401) {
      showLoadError(error);
    }
  }
}

async function renderGlobal(token) {
  showBoardRegion();
  state.boardReady = false;
  renderLoading(t("app.loading"));
  try {
    const data = await api.board();
    if (state.routeToken !== token) return;
    state.boardReady = true;
    state.boards = data.projects || [];
    state.currentProjectKey = null;
    state.currentProjectId = null;
    state.selectedRef = null;
    if (state.panel) state.panel.hide();
    if (!state.boards.length) {
      showPageRegion();
      renderNoProjects();
      return;
    }
    renderBoardView();
    renderChrome();
    rememberCards();
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status !== 401) showLoadError(error);
  }
}

function showBoardRegion() {
  if (!state.dom) return;
  state.pageVisible = false;
  state.dom.boardRegion.hidden = false;
  state.dom.pageRegion.hidden = true;
}

function showPageRegion() {
  if (!state.dom) return;
  state.pageVisible = true;
  state.boardReady = false;
  state.dom.boardRegion.hidden = true;
  state.dom.pageRegion.hidden = false;
  state.selectedRef = null;
  if (state.panel) state.panel.hide();
}

function renderLoading(label) {
  if (!state.dom) return;
  // The board view owns the board region; show loading in the page region so
  // its scroll container and listeners survive the fetch.
  state.dom.boardRegion.hidden = true;
  state.dom.pageRegion.hidden = false;
  state.dom.pageRegion.replaceChildren(
    el("div", { class: "page-loading" }, [
      el("span", { class: "spinner", "aria-hidden": "true" }),
      el("span", { class: "muted", text: label })
    ])
  );
}

function renderBoardView() {
  if (!state.dom || !state.boardView) return;
  if (!isBoardRoute() || state.pageVisible || !state.boardReady) return;
  // A full re-render detaches the card being dragged; let the drop or cancel
  // finish first and render from the updated store afterwards.
  if (state.boardView.isDragging()) return;
  state.dom.boardRegion.hidden = false;
  state.dom.pageRegion.hidden = true;
  const mode = state.route.name === "global" ? "global" : "project";
  state.boardView.render(state.boards, {
    mode,
    search: state.search,
    showAll: state.showAll,
    selectedRef: state.selectedRef
  });
}

function showProjectNotFound(key) {
  showPageRegion();
  showNotFoundPage(
    t("common.projectNotFoundTitle"),
    t("common.projectNotFoundBody", { key }),
    boardPath()
  );
}

function showTaskNotFound(reference) {
  showPageRegion();
  if (state.panel) state.panel.hide();
  showNotFoundPage(
    t("common.taskNotFoundTitle"),
    t("common.taskNotFoundBody", { ref: reference }),
    state.currentProjectKey ? `/p/${encodeURIComponent(state.currentProjectKey)}` : boardPath()
  );
}

function showLoadError(error) {
  showPageRegion();
  const page = el("div", { class: "page" }, [
    el("div", { class: "page-empty" }, [
      el("h2", { text: t("common.error") }),
      el("p", { class: "muted", text: (error && error.message) || t("error.generic") }),
      el(
        "button",
        {
          type: "button",
          class: "button button-secondary",
          onClick: () => {
            renderRoute().catch(() => {});
          }
        },
        [el("span", { text: t("common.tryAgain") })]
      )
    ])
  ]);
  state.dom.pageRegion.replaceChildren(page);
}

function showNotFoundPage(title, body, backPath) {
  const page = el("div", { class: "page" }, [
    el("div", { class: "page-empty" }, [
      el("h2", { text: title }),
      el("p", { class: "muted", text: body }),
      el("a", { class: "button button-secondary", href: backPath }, [
        el("span", { text: t("common.backToBoard") })
      ])
    ])
  ]);
  state.dom.pageRegion.replaceChildren(page);
}

function renderNoProjects() {
  const page = el("div", { class: "page" }, [
    el("div", { class: "page-empty" }, [
      el("h2", { text: t("board.emptyTitle") }),
      el("p", { class: "muted", text: t("board.emptyBody") }),
      el(
        "button",
        {
          type: "button",
          class: "button button-primary",
          onClick: () => openProjectForm({ onSaved: afterProjectSaved })
        },
        [icon("plus", { size: 16 }), el("span", { text: t("board.createProject") })]
      )
    ])
  ]);
  state.dom.pageRegion.replaceChildren(page);
}

// ---------------------------------------------------------------------------
// Page: inbox

function renderInboxPage() {
  if (!state.dom) return;
  const page = el("div", { class: "page" }, [
    el("header", { class: "page-header" }, [
      el("div", {}, [
        el("h1", { class: "page-title", text: t("inbox.title") }),
        el("p", {
          class: "page-subtitle",
          text:
            state.inbox.length === 1
              ? t("inbox.showingOne")
              : t("inbox.showing", { count: state.inbox.length })
        })
      ])
    ])
  ]);

  if (!state.inbox.length) {
    page.append(
      el("div", { class: "page-empty" }, [
        el("h2", { text: t("inbox.emptyTitle") }),
        el("p", { class: "muted", text: t("inbox.emptyBody") })
      ])
    );
    state.dom.pageRegion.replaceChildren(page);
    return;
  }

  const human = state.me && state.me.scope === "human";
  const list = el("div", { class: "inbox-list" });
  for (const proposal of state.inbox) {
    const status = el("select", { class: "input input-compact", "aria-label": t("inbox.acceptStatus") });
    for (const value of ["backlog", "todo", "in_progress", "in_review", "blocker", "done"]) {
      status.append(
        el("option", { value, text: t(`status.${value}`), selected: value === "backlog" })
      );
    }
    const accept = el(
      "button",
      {
        type: "button",
        class: "button button-primary button-small",
        onClick: async () => {
          accept.disabled = true;
          try {
            await api.acceptProposal(proposal.reference, status.value);
            toast(t("inbox.accepted"), "success");
            state.inbox = state.inbox.filter((item) => item.id !== proposal.id);
            if (state.route.name === "inbox") renderInboxPage();
            renderChrome();
            refreshBoardDebounced();
          } catch (error) {
            toastForError(error, t);
          } finally {
            accept.disabled = false;
          }
        }
      },
      [icon("check", { size: 14 }), el("span", { text: t("inbox.accept") })]
    );
    const reject = el(
      "button",
      {
        type: "button",
        class: "button button-danger button-small",
        onClick: async () => {
          const confirmed = await confirmDialog(t("inbox.rejectConfirm", { ref: proposal.reference }), {
            title: t("inbox.reject"),
            confirmLabel: t("inbox.reject"),
            danger: true
          });
          if (!confirmed) return;
          try {
            await api.rejectProposal(proposal.reference);
            toast(t("inbox.rejected"), "success");
            state.inbox = state.inbox.filter((item) => item.id !== proposal.id);
            if (state.route.name === "inbox") renderInboxPage();
            renderChrome();
          } catch (error) {
            toastForError(error, t);
          }
        }
      },
      [icon("close", { size: 14 }), el("span", { text: t("inbox.reject") })]
    );

    const labels = el("div", { class: "card-labels" });
    for (const label of proposal.labels || []) {
      labels.append(
        el("span", { class: "label-chip", style: { "--label-color": label.color } }, [label.name])
      );
    }

    list.append(
      el("article", { class: "inbox-card" }, [
        el("div", { class: "inbox-top" }, [
          el("span", { class: "card-ref", text: proposal.reference }),
          el("span", { class: "muted", text: proposal.project_key }),
          el("span", {
            class: "muted",
            text: t("inbox.proposedBy", { name: proposal.created_by || proposal.source })
          }),
          el("time", {
            class: "muted",
            text: relativeTime(proposal.created_at, t),
            title: new Date(proposal.created_at).toLocaleString()
          })
        ]),
        el("h2", { class: "inbox-title", text: proposal.title }),
        proposal.description
          ? el("p", { class: "inbox-description", text: proposal.description.slice(0, 400) })
          : null,
        (proposal.labels || []).length ? labels : null,
        human
          ? el("div", { class: "inbox-actions" }, [
              el("label", { class: "inbox-status" }, [
                el("span", { class: "field-label", text: t("inbox.acceptStatus") }),
                status
              ]),
              accept,
              reject
            ])
          : el("p", { class: "field-hint", text: t("settings.humanOnly", { what: t("inbox.title") }) })
      ])
    );
  }
  page.append(list);
  state.dom.pageRegion.replaceChildren(page);
}

// ---------------------------------------------------------------------------
// Settings

function settingsHeader(active) {
  const projects = el(
    "a",
    { class: `settings-tab${active === "projects" ? " active" : ""}`, href: "/settings/projects" },
    [icon("board", { size: 15 }), el("span", { text: t("settings.projects") })]
  );
  projects.addEventListener("click", (event) => {
    event.preventDefault();
    navigate("/settings/projects");
  });
  const tokens = el(
    "a",
    { class: `settings-tab${active === "tokens" ? " active" : ""}`, href: "/settings/tokens" },
    [icon("key", { size: 15 }), el("span", { text: t("settings.tokens") })]
  );
  tokens.addEventListener("click", (event) => {
    event.preventDefault();
    navigate("/settings/tokens");
  });
  const integration = el(
    "a",
    {
      class: `settings-tab${active === "integration" ? " active" : ""}`,
      href: "/settings/integration"
    },
    [icon("globe", { size: 15 }), el("span", { text: t("settings.integration") })]
  );
  integration.addEventListener("click", (event) => {
    event.preventDefault();
    navigate("/settings/integration");
  });
  const exportTab = el(
    "a",
    {
      class: `settings-tab${active === "export" ? " active" : ""}`,
      href: "/settings/export"
    },
    [icon("download", { size: 15 }), el("span", { text: t("settings.export") })]
  );
  exportTab.addEventListener("click", (event) => {
    event.preventDefault();
    navigate("/settings/export");
  });
  return el("header", { class: "page-header" }, [
    el("div", {}, [
      el("h1", { class: "page-title", text: t("settings.title") }),
      el("nav", { class: "settings-tabs" }, [projects, tokens, integration, exportTab])
    ])
  ]);
}

async function renderSettingsProjects(token) {
  if (!state.dom || state.routeToken !== token) return;
  state.dom.pageRegion.replaceChildren(
    el("div", { class: "page-loading" }, [
      el("span", { class: "spinner", "aria-hidden": "true" }),
      el("span", { class: "muted", text: t("common.loading") })
    ])
  );
  try {
    const projects = await api.projects({ archived: true });
    if (state.routeToken !== token) return;
    state.allProjects = projects;
    state.projects = projects.filter((project) => !project.archived_at);
    renderChrome();
    const active = projects.filter((project) => !project.archived_at);
    const archived = projects.filter((project) => project.archived_at);
    const human = state.me && state.me.scope === "human";

    const page = el("div", { class: "page" }, [
      settingsHeader("projects"),
      el("p", { class: "page-subtitle", text: t("settings.projectsDescription") }),
      human
        ? el(
            "div",
            { class: "page-toolbar" },
            [
              el(
                "button",
                {
                  type: "button",
                  class: "button button-primary",
                  onClick: () =>
                    openProjectForm({
                      onSaved: () => {
                        renderSettingsProjects(token);
                        loadProjectsQuietly();
                      }
                    })
                },
                [icon("plus", { size: 16 }), el("span", { text: t("settings.newProject") })]
              )
            ]
          )
        : el("p", { class: "field-hint", text: t("settings.humanOnly", { what: t("settings.projects") }) })
    ]);

    if (!active.length) {
      page.append(
        el("div", { class: "page-empty" }, [
          el("h2", { text: t("settings.projectsEmpty") }),
          el("p", { class: "muted", text: t("board.emptyBody") })
        ])
      );
    } else {
      const list = el("div", { class: "settings-list" });
      const refresh = () => renderSettingsProjects(token);
      for (const project of active) {
        list.append(renderProjectRow(project, false, human, refresh));
      }
      page.append(list);
    }

    if (archived.length) {
      page.append(el("h2", { class: "settings-section-title", text: t("settings.archivedSection") }));
      const list = el("div", { class: "settings-list" });
      const refresh = () => renderSettingsProjects(token);
      for (const project of archived) list.append(renderProjectRow(project, true, human, refresh));
      page.append(list);
    }
    state.dom.pageRegion.replaceChildren(page);
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status !== 401) showLoadError(error);
  }
}

function renderProjectRow(project, archived, human, refresh) {
  const actions = [];
  const githubPanel = el("div", { class: "github-repositories" });
  let githubGeneration = 0;
  if (human) {
    actions.push(
      el(
        "button",
        {
          type: "button",
          class: "button button-ghost button-small",
          onClick: () => {
            openProjectForm({
              project,
              onSaved: () => {
                refresh();
                loadProjectsQuietly();
              }
            });
          }
        },
        [el("span", { text: t("settings.edit") })]
      )
    );
    if (!archived) {
      const githubToggle = el("button", { type: "button", class: "button button-ghost button-small", text: t("github.repositories"), onClick: async () => {
        const generation = ++githubGeneration;
        if (githubPanel.childNodes.length) { githubPanel.replaceChildren(); return; }
        githubPanel.replaceChildren(el("p", { text: t("common.loading") }));
        try {
          const repositories = await api.githubRepositories(project.id);
          if (generation === githubGeneration) renderGithubRepositories(githubPanel, project, repositories, () => generation === githubGeneration, (saving) => { githubToggle.disabled = saving; });
        } catch (error) { if (generation === githubGeneration) githubPanel.replaceChildren(el("p", { class: "form-error", role: "alert", text: error.message })); }
      } });
      actions.push(githubToggle);
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-ghost button-small",
            onClick: async () => {
              const confirmed = await confirmDialog(t("settings.archiveConfirm", { name: project.name }), {
                title: t("settings.archive"),
                confirmLabel: t("settings.archive")
              });
              if (!confirmed) return;
              try {
                await api.archiveProject(project.id);
                state.projects = state.projects.filter((item) => item.id !== project.id);
                if (state.currentProjectId === project.id) {
                  state.currentProjectKey = null;
                  state.currentProjectId = null;
                }
                toast(t("toast.archived"), "success");
                refresh();
                loadProjectsQuietly();
              } catch (error) {
                toastForError(error, t);
              }
            }
          },
          [el("span", { text: t("settings.archive") })]
        )
      );
    }
  }
  return el("article", { class: "settings-row" }, [
    el("span", { class: "project-dot", style: { "--project-color": project.color } }),
    el("div", { class: "settings-row-main" }, [
      el("div", { class: "settings-row-title" }, [
        el("span", { class: "card-ref", text: project.key }),
        el("span", { class: "settings-row-name", text: project.name }),
        archived ? el("span", { class: "badge badge-neutral", text: t("settings.archived") }) : null
      ]),
      project.description
        ? el("p", { class: "muted settings-row-description", text: project.description })
        : null,
      project.repo_path ? el("code", { class: "settings-row-repo", text: project.repo_path }) : null,
      githubPanel
    ]),
    actions.length ? el("div", { class: "settings-row-actions" }, actions) : null
  ]);
}

function renderGithubRepositories(panel, project, initial, current, setSaving) {
  const list = el("div");
  const error = el("p", { class: "form-error", role: "alert" });
  const rows = [];
  function add(item = { repository: "", issues: true, review: true, follow: true }) {
    const repository = el("input", { class: "input", value: item.repository, placeholder: t("github.repositoryPlaceholder"), "aria-label": t("github.repository") });
    const flags = Object.fromEntries(["issues", "review", "follow"].map((key) => {
      const label = t(`github.${key}`);
      const input = el("input", { type: "checkbox", checked: item[key], "aria-label": label });
      return [key, { input, label }];
    }));
    const row = el("div", { class: "github-repository" }, [repository,
      ...Object.values(flags).map(({ input, label }) => el("label", { class: "field-hint" }, [input, ` ${label} `])),
      el("button", { type: "button", class: "button button-ghost button-small", text: t("github.removeRepository"), onClick: () => { row.remove(); rows.splice(rows.indexOf(record), 1); } })]);
    const record = { repository, flags }; rows.push(record); list.append(row);
  }
  initial.forEach(add);
  panel.replaceChildren(el("h3", { text: t("github.repositories") }),
    el("p", { class: "field-hint", text: t("github.settingsHint") }), list,
    el("button", { type: "button", class: "button button-ghost button-small", text: t("github.addRepository"), onClick: () => add() }),
    el("button", { type: "button", class: "button button-primary button-small", text: t("github.saveRepositories"), onClick: async (event) => {
      error.textContent = "";
      const saveButton = event.currentTarget;
      saveButton.disabled = true;
      setSaving(true);
      try {
        const saved = await api.saveGithubRepositories(project.id, rows.map(({ repository, flags }) => ({
          repository: repository.value.trim(), issues: flags.issues.input.checked, review: flags.review.input.checked, follow: flags.follow.input.checked
        })));
        if (!current()) return;
        renderGithubRepositories(panel, project, saved, current, setSaving);
        toast(t("github.saved"), "success");
      } catch (failure) { if (current()) error.textContent = failure.message; }
      finally { saveButton.disabled = false; setSaving(false); }
    } }), error);
}

async function renderSettingsTokens(token) {
  if (!state.dom || state.routeToken !== token) return;
  const human = state.me && state.me.scope === "human";
  const page = el("div", { class: "page" }, [settingsHeader("tokens")]);
  if (!human) {
    page.append(
      el("p", { class: "field-hint", text: t("settings.humanOnly", { what: t("settings.tokens") }) })
    );
    state.dom.pageRegion.replaceChildren(page);
    return;
  }
  state.dom.pageRegion.replaceChildren(
    el("div", { class: "page-loading" }, [
      el("span", { class: "spinner", "aria-hidden": "true" }),
      el("span", { class: "muted", text: t("common.loading") })
    ])
  );
  try {
    const tokens = await api.tokens();
    if (state.routeToken !== token) return;
    page.append(
      el("p", { class: "page-subtitle", text: t("settings.tokensDescription") }),
      el(
        "div",
        { class: "page-toolbar" },
        [
          el(
            "button",
            {
              type: "button",
              class: "button button-primary",
              onClick: () =>
                openTokenForm({
                  onCreated: () => renderSettingsTokens(token)
                })
            },
            [icon("plus", { size: 16 }), el("span", { text: t("settings.createToken") })]
          )
        ]
      )
    );
    if (!tokens.length) {
      page.append(
        el("div", { class: "page-empty" }, [
          el("h2", { text: t("settings.tokensEmpty") }),
          el("p", { class: "muted", text: t("settings.tokensDescription") })
        ])
      );
      state.dom.pageRegion.replaceChildren(page);
      return;
    }
    const list = el("div", { class: "settings-list" });
    for (const tokenInfo of tokens) {
      const revoked = Boolean(tokenInfo.revoked_at);
      const revoke = el(
        "button",
        {
          type: "button",
          class: "button button-ghost button-small",
          disabled: revoked,
          onClick: async () => {
            const confirmed = await confirmDialog(
              t("settings.tokenRevokeConfirm", { name: tokenInfo.name }),
              { title: t("settings.tokenRevoke"), confirmLabel: t("settings.tokenRevoke"), danger: true }
            );
            if (!confirmed) return;
            try {
              await api.revokeToken(tokenInfo.id);
              toast(t("toast.tokenRevoked"), "success");
              renderSettingsTokens(token);
            } catch (error) {
              toastForError(error, t);
            }
          }
        },
        [el("span", { text: t("settings.tokenRevoke") })]
      );
      list.append(
        el("article", { class: "settings-row" }, [
          icon("key", { size: 18 }),
          el("div", { class: "settings-row-main" }, [
            el("div", { class: "settings-row-title" }, [
              el("span", { class: "settings-row-name", text: tokenInfo.name }),
              el("span", {
                class: `badge ${tokenInfo.scope === "human" ? "badge-info" : "badge-neutral"}`,
                text: tokenInfo.scope === "human" ? t("form.scopeHuman") : t("form.scopeAgent")
              }),
              revoked ? el("span", { class: "badge badge-warning", text: t("settings.tokenRevoked") }) : null
            ]),
            el("p", { class: "muted settings-token-meta" }, [
              `${t("settings.tokenCreated")}: ${new Date(tokenInfo.created_at).toLocaleString()}`,
              " · ",
              `${t("settings.tokenLastUsed")}: ${
                tokenInfo.last_used_at
                  ? relativeTime(tokenInfo.last_used_at, t)
                  : t("settings.tokenNever")
              }`
            ])
          ]),
          el("div", { class: "settings-row-actions" }, [revoke])
        ])
      );
    }
    page.append(list);
    state.dom.pageRegion.replaceChildren(page);
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status !== 401) showLoadError(error);
  }
}

function integrationRow(label, value) {
  return el("article", { class: "settings-row" }, [
    el("div", { class: "settings-row-main" }, [
      el("div", { class: "settings-row-title" }, [
        el("span", { class: "settings-row-name", text: label })
      ]),
      value
    ])
  ]);
}

async function renderSettingsIntegration(token) {
  if (!state.dom || state.routeToken !== token) return;
  const human = state.me && state.me.scope === "human";
  const page = el("div", { class: "page" }, [settingsHeader("integration")]);
  if (!human) {
    page.append(
      el("p", {
        class: "field-hint",
        text: t("settings.humanOnly", { what: t("settings.integration") })
      })
    );
    state.dom.pageRegion.replaceChildren(page);
    return;
  }
  state.dom.pageRegion.replaceChildren(
    el("div", { class: "page-loading" }, [
      el("span", { class: "spinner", "aria-hidden": "true" }),
      el("span", { class: "muted", text: t("common.loading") })
    ])
  );
  try {
    const response = await api.integration();
    if (state.routeToken !== token) return;
    if (!response || !response.openchamber) {
      page.append(
        el("div", { class: "settings-list" }, [
          integrationRow(
            t("settings.integrationStatus"),
            el("div", { class: "settings-row-main" }, [
              el("span", {
                class: "badge badge-warning",
                text: t("settings.integrationUnavailable")
              }),
              el("p", {
                class: "field-hint",
                text: t("settings.integrationUnavailableHint")
              })
            ])
          )
        ])
      );
      state.dom.pageRegion.replaceChildren(page);
      return;
    }
    const info = response.openchamber;
    state.integration = info;
    if (state.panel) state.panel.setIntegration(info);

    const enabled = Boolean(info.enabled);
    page.append(
      el("p", { class: "page-subtitle", text: t("settings.integrationDescription") }),
      el("div", { class: "settings-list" }, [
        el("article", { class: "settings-row" }, [
          icon(enabled ? "globe" : "close", { size: 18 }),
          el("div", { class: "settings-row-main" }, [
            el("div", { class: "settings-row-title" }, [
              el("span", { class: "settings-row-name", text: t("settings.integrationStatus") }),
              el("span", {
                class: `badge ${enabled ? "badge-success" : "badge-neutral"}`,
                text: enabled
                  ? t("settings.integrationEnabled")
                  : t("settings.integrationDisabled")
              })
            ]),
            el("p", {
              class: "muted settings-row-description",
              text: enabled
                ? t("settings.integrationEnabledHint")
                : t("settings.integrationDisabledHint")
            })
          ])
        ])
      ])
    );

    if (enabled) {
      const rows = el("div", { class: "settings-list" });
      rows.append(
        integrationRow(
          t("settings.integrationUrl"),
          el("code", { class: "settings-row-repo", text: info.url || t("common.none") })
        ),
        integrationRow(
          t("settings.integrationReachable"),
          el("span", {
            class: `badge ${info.reachable ? "badge-success" : "badge-warning"}`,
            text: info.reachable
              ? t("settings.integrationReachableYes")
              : t("settings.integrationReachableNo")
          })
        ),
        integrationRow(
          t("settings.integrationVersion"),
          el("span", {
            class: "settings-row-repo",
            text: info.version || t("common.none")
          })
        )
      );
      page.append(rows);
    } else {
      page.append(
        el("div", { class: "settings-list" }, [
          integrationRow(
            t("settings.integrationEnv"),
            el("div", { class: "settings-env" }, [
              el("code", { class: "settings-row-repo", text: "KANSO_OPENCHAMBER_URL" }),
              el("code", { class: "settings-row-repo", text: "KANSO_OPENCHAMBER_TOKEN" }),
              el("p", {
                class: "field-hint",
                text: t("settings.integrationEnvHint")
              })
            ])
          )
        ])
      );
    }
    state.dom.pageRegion.replaceChildren(page);
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status !== 401) showLoadError(error);
  }
}

function downloadBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  const link = el("a", { href: url, download: filename, hidden: true });
  document.body.append(link);
  link.click();
  link.remove();
  // Some browsers fetch the Blob after their download prompt is confirmed.
  window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

function notificationPermission() {
  if (!("Notification" in window)) return "unsupported";
  const permission = Notification.permission;
  return ["default", "granted", "denied"].includes(permission) ? permission : "default";
}

function notificationsSettings() {
  const permission = notificationPermission();
  const supported = permission !== "unsupported";
  const granted = permission === "granted";
  const tone =
    permission === "granted" ? "success" : permission === "denied" || !supported ? "warning" : "neutral";
  const hints = {
    unsupported: "settings.notificationsUnsupportedHint",
    denied: "settings.notificationsDeniedHint",
    granted: state.notificationsEnabled
      ? "settings.notificationsGrantedHint"
      : "settings.notificationsOffHint",
    default: "settings.notificationsDefaultHint"
  };

  const toggle = el("input", {
    type: "checkbox",
    checked: state.notificationsEnabled && granted,
    disabled: !supported || permission === "denied",
    "aria-label": t("settings.notificationsEnable")
  });
  toggle.addEventListener("change", () => {
    setNotificationsFromSettings(toggle.checked).catch((error) => {
      if (error.status !== 401) toastForError(error, t);
    });
  });

  const rows = [
    el("article", { class: "settings-row" }, [
      icon("bell", { size: 18 }),
      el("div", { class: "settings-row-main" }, [
        el("div", { class: "settings-row-title" }, [
          el("span", { class: "settings-row-name", text: t("settings.notificationsPermission") }),
          el("span", {
            class: `badge badge-${tone}`,
            text: t(`settings.notificationPermission.${permission}`)
          })
        ]),
        el("p", { class: "muted settings-row-description", text: t(hints[permission]) })
      ]),
      supported
        ? el("div", { class: "settings-row-actions" }, [
            el("label", { class: "notification-switch" }, [
              toggle,
              el("span", { text: t("settings.notificationsEnable") })
            ])
          ])
        : null
    ])
  ];
  if (granted) {
    rows.push(
      el("article", { class: "settings-row" }, [
        icon("alert", { size: 18 }),
        el("div", { class: "settings-row-main" }, [
          el("div", { class: "settings-row-title" }, [
            el("span", { class: "settings-row-name", text: t("settings.notificationsTest") })
          ]),
          el("p", {
            class: "muted settings-row-description",
            text: t("settings.notificationsTestHint")
          })
        ]),
        el("div", { class: "settings-row-actions" }, [
          el(
            "button",
            {
              type: "button",
              class: "button button-secondary button-small",
              onClick: sendTestNotification
            },
            [icon("bell", { size: 14 }), el("span", { text: t("settings.notificationsTest") })]
          )
        ])
      ])
    );
  }

  return el("section", { class: "settings-notifications" }, [
    el("h2", { class: "settings-section-title", tabindex: "-1", text: t("settings.notifications") }),
    el("p", { class: "page-subtitle", text: t("settings.notificationsDescription") }),
    el("div", { class: "settings-list" }, rows)
  ]);
}

function refreshNotificationsSettings({ focusToggle = false } = {}) {
  if (state.route.name !== "settings-export" || !state.dom) return;
  const current = state.dom.pageRegion.querySelector(".settings-notifications");
  if (!current) return;
  const replacement = notificationsSettings();
  current.replaceWith(replacement);
  if (!focusToggle) return;
  const target =
    replacement.querySelector(".notification-switch input:not([disabled])") ||
    replacement.querySelector(".settings-section-title");
  if (target) target.focus();
}

async function setNotificationsFromSettings(enabled) {
  if (!enabled) {
    state.notificationsEnabled = false;
    localStorage.removeItem(LS.notifications);
    renderChrome();
    refreshNotificationsSettings({ focusToggle: true });
    return;
  }

  const permission = notificationPermission();
  if (permission === "unsupported") {
    toast(t("app.notificationsUnsupported"), "warning");
    refreshNotificationsSettings({ focusToggle: true });
    return;
  }
  if (permission === "denied") {
    toast(t("app.notificationsBlocked"), "warning");
    refreshNotificationsSettings({ focusToggle: true });
    return;
  }

  let result = permission;
  if (permission === "default") {
    try {
      result = await Notification.requestPermission();
    } catch {
      result = "denied";
    }
  }

  if (result === "granted") {
    state.notificationsEnabled = true;
    localStorage.setItem(LS.notifications, "1");
    toast(t("app.notificationsOn"), "success");
  } else {
    state.notificationsEnabled = false;
    localStorage.removeItem(LS.notifications);
    toast(t("app.notificationsBlocked"), "warning");
  }
  renderChrome();
  refreshNotificationsSettings({ focusToggle: true });
}

function sendTestNotification() {
  if (notificationPermission() !== "granted") {
    toast(t("settings.notificationsTestNeedsPermission"), "warning");
    return;
  }
  try {
    new Notification(t("settings.notificationsTestTitle"), {
      body: t("settings.notificationsTestBody")
    });
    toast(t("settings.notificationsTestSent"), "success");
  } catch {
    toast(t("settings.notificationsTestFailed"), "warning");
  }
}

async function renderSettingsExport(token) {
  if (!state.dom || state.routeToken !== token) return;
  const human = state.me && state.me.scope === "human";
  const page = el("div", { class: "page" }, [settingsHeader("export")]);
  if (!human) {
    page.append(
      el("p", {
        class: "field-hint",
        text: t("settings.humanOnly", { what: t("settings.export") })
      })
    );
    state.dom.pageRegion.replaceChildren(page);
    return;
  }

  const summary = el("p", { class: "field-hint", role: "status", hidden: true });
  const button = el(
    "button",
    { type: "button", class: "button button-primary" },
    [icon("download", { size: 16 }), el("span", { text: t("settings.exportDownload") })]
  );
  button.addEventListener("click", async () => {
    const label = button.querySelector("span");
    button.disabled = true;
    button.setAttribute("aria-busy", "true");
    if (label) label.textContent = t("settings.exporting");
    try {
      const result = await api.exportDump();
      const data = JSON.parse(result.text);
      downloadBlob(result.blob, result.filename);
      const projects = Array.isArray(data.projects) ? data.projects.length : 0;
      const tasks = Array.isArray(data.tasks) ? data.tasks.length : 0;
      const comments = Array.isArray(data.comments) ? data.comments.length : 0;
      const count = (value, oneKey, manyKey) =>
        `${value} ${t(value === 1 ? oneKey : manyKey)}`;
      summary.hidden = false;
      summary.textContent = t("settings.exportSummary", {
        version: data.version,
        projects: count(projects, "settings.exportProjectOne", "settings.exportProjects"),
        tasks: count(tasks, "settings.exportCardOne", "settings.exportCards"),
        comments: count(comments, "settings.exportCommentOne", "settings.exportComments")
      });
      toast(t("settings.exportReady"), "success");
    } catch (error) {
      if (error.status !== 401) toastForError(error, t);
    } finally {
      button.disabled = false;
      button.removeAttribute("aria-busy");
      if (label) label.textContent = t("settings.exportDownload");
    }
  });

  page.append(
    el("p", { class: "page-subtitle", text: t("settings.exportDescription") }),
    el("div", { class: "page-toolbar" }, [button]),
    summary,
    notificationsSettings()
  );
  state.dom.pageRegion.replaceChildren(page);
}

async function loadProjectsQuietly() {
  try {
    const [projects, labels] = await Promise.all([api.projects(), api.labels()]);
    state.projects = projects;
    state.labels = labels;
    if (state.panel) {
      state.panel.setLabels(labels);
      state.panel.setProjects(projects);
    }
    renderChrome();
  } catch {
    // A transient failure leaves the last known lists; SSE will resync.
  }
}

function afterProjectSaved(project) {
  loadProjectsQuietly().then(() => {
    if (project && (state.route.name === "home" || (isBoardRoute() && state.pageVisible))) {
      navigate(`/p/${encodeURIComponent(project.key)}`, { replace: true });
      return;
    }
    if (isBoardRoute()) refreshBoardDebounced();
  });
}

// ---------------------------------------------------------------------------
// Cards: selection, panel, moves

function selectCard(card) {
  state.selectedRef = card.reference;
  const path = taskPath(card.reference);
  if (location.pathname === path) {
    if (state.panel) state.panel.open(card.reference);
    return;
  }
  navigate(path);
}

function closePanel({ navigate: shouldNavigate = false } = {}) {
  if (state.panel) state.panel.hide();
  state.selectedRef = null;
  if (shouldNavigate && state.route.name === "task") {
    const route = state.route;
    history.replaceState({}, "", `/p/${encodeURIComponent(route.key)}`);
    renderRoute().catch(() => {});
  }
}

function cardFromDetail(detail) {
  return {
    id: detail.id,
    reference: detail.reference,
    title: detail.title,
    status: detail.status,
    priority: detail.priority,
    due_date: detail.due_date,
    labels: detail.labels || [],
    claimed_by: detail.claimed_by,
    attention: detail.attention,
    session_status: detail.session_status,
    blocked_from: detail.blocked_from,
    blocked_reason: detail.blocked_reason,
    checklist_done: (detail.checklist || []).filter((item) => item.done).length,
    checklist_total: (detail.checklist || []).length,
    blocked: (detail.dependencies?.blocked_by || []).some((item) => item.status !== "done")
  };
}

function findBoardByProjectId(projectId) {
  return state.boards.find((board) => board.project.id === projectId) || null;
}

function findCardLocation(id) {
  for (const board of state.boards) {
    for (const status of Object.keys(board.columns)) {
      const index = board.columns[status].findIndex((card) => card.id === id);
      if (index >= 0) return { board, status, index, card: board.columns[status][index] };
    }
  }
  return null;
}

function removeCard(id) {
  const location = findCardLocation(id);
  if (!location) return null;
  location.board.columns[location.status].splice(location.index, 1);
  return location.card;
}

function applyDetailToStore(detail, { render = true } = {}) {
  if (!detail) return;
  const location = findCardLocation(detail.id);
  const card = cardFromDetail(detail);
  if (detail.state === "active") {
    if (location) {
      if (location.status === detail.status) {
        location.board.columns[location.status][location.index] = card;
      } else {
        location.board.columns[location.status].splice(location.index, 1);
        const target = location.board.columns[detail.status];
        if (target) target.push(card);
      }
    } else {
      const board = findBoardByProjectId(detail.project_id);
      if (board && board.columns[detail.status]) board.columns[detail.status].push(card);
    }
  } else if (location) {
    location.board.columns[location.status].splice(location.index, 1);
  }
  state.previousCards.set(detail.id, { attention: detail.attention, status: detail.status });
  if (render) {
    renderBoardView();
    renderChrome();
  }
}

function rememberCards() {
  const map = new Map();
  for (const board of state.boards) {
    for (const status of Object.keys(board.columns)) {
      for (const card of board.columns[status]) {
        map.set(card.id, { attention: card.attention, status: card.status });
      }
    }
  }
  state.previousCards = map;
}

async function moveCard({ reference, from, to, before, after, projectId }) {
  const board = findBoardByProjectId(projectId);
  if (!board) return;
  const location = findCardLocation(
    board.columns[from]?.find((card) => card.reference === reference)?.id || ""
  );
  if (!location) {
    refreshBoardDebounced();
    return;
  }
  // Optimistic move.
  const [card] = board.columns[from].splice(location.index, 1);
  card.status = to;
  const target = board.columns[to];
  let index = target.length;
  if (before) {
    const found = target.findIndex((item) => item.reference === before);
    if (found >= 0) index = found;
  } else if (after) {
    const found = target.findIndex((item) => item.reference === after);
    if (found >= 0) index = found + 1;
  }
  target.splice(index, 0, card);
  renderBoardView();

  try {
    const detail = await api.moveTask(reference, { status: to, before, after });
    applyDetailToStore(detail);
  } catch (error) {
    toastForError(error, t);
    await refreshBoard();
  }
}

function moveCardById(task, payload) {
  const board = findBoardByProjectId(task.projectId || task.project_id);
  if (!board) return;
  const location = findCardLocation(task.id);
  if (!location) return;
  const from = location.status;
  const reference = task.reference;
  const target = payload.restore ? task.blocked_from : payload.status;
  if (!target) return;
  moveCard({
    reference,
    from,
    to: target,
    before: null,
    after: null,
    projectId: board.project.id
  }).catch((error) => toastForError(error, t));
}

async function quickAdd({ projectId, status, title }) {
  try {
    const detail = await api.createTask({ project_id: projectId, title, status });
    if (detail.state === "active") {
      applyDetailToStore(detail);
      toast(t("toast.created"), "success");
    } else {
      state.inbox.unshift(detail);
      renderChrome();
    }
    return true;
  } catch (error) {
    toastForError(error, t);
    return false;
  }
}

function newCardFlow() {
  if (!state.me) return;
  if (!state.projects.length) {
    openProjectForm({ onSaved: afterProjectSaved });
    return;
  }
  const project =
    state.projects.find((item) => item.key === state.currentProjectKey) || state.projects[0];
  openCardForm({
    project,
    projects: state.projects,
    labels: state.labels,
    initialStatus: "backlog",
    onCreated: (detail) => {
      applyDetailToStore(detail);
      if (detail.project_key) {
        navigate(`/p/${encodeURIComponent(detail.project_key)}/task/${detail.number}`);
      }
    }
  });
}

async function refreshBoard() {
  if (!isBoardRoute() || state.pageVisible) return;
  const route = state.route;
  const token = state.routeToken;
  try {
    let data;
    if (route.name === "global") {
      data = await api.board();
    } else if (route.key) {
      data = await api.board(route.key);
    } else {
      return;
    }
    if (state.routeToken !== token || state.route !== route) return;
    state.boards = data.projects || [];
    if (route.name === "global" && !state.boards.length) {
      showPageRegion();
      renderNoProjects();
      renderChrome();
      return;
    }
    state.boardReady = true;
    if (route.name !== "global" && state.boards[0]) {
      state.currentProjectKey = state.boards[0].project.key;
      state.currentProjectId = state.boards[0].project.id;
    }
    rememberCards();
    renderBoardView();
    renderChrome();
  } catch (error) {
    if (state.routeToken !== token) return;
    if (error.status !== 401) toastForError(error, t);
  }
}

// ---------------------------------------------------------------------------
// Live events

function connectEvents() {
  closeEvents();
  const source = new EventSource("/api/events");
  state.eventSource = source;

  source.onopen = () => {
    if (state.eventSource !== source) return;
    const wasOffline = state.offlineNotified;
    state.connectedOnce = true;
    state.reconnectDelay = 1000;
    state.offlineNotified = false;
    if (wasOffline) toast(t("toast.online"), "success");
    // Refetch after the stream opens too: events emitted between the initial
    // board fetch and this open (or after an earlier failed attempt) would
    // otherwise be lost.
    if (state.initialLoaded) refreshAfterReconnect();
    else state.refreshOnFirstOpen = true;
  };

  source.onerror = () => {
    if (state.eventSource !== source) return;
    source.close();
    state.eventSource = null;
    reconnectOrLogin();
  };

  for (const type of EVENT_TYPES) {
    source.addEventListener(type, (message) => {
      let envelope;
      try {
        envelope = JSON.parse(message.data);
      } catch {
        return;
      }
      applyEvent(type, envelope.data || {});
    });
  }
}

async function reconnectOrLogin() {
  // The stream can end because the server restarted, because the 30-minute
  // stream lifetime elapsed (a clean close), or because the session was
  // revoked. Probe /api/me: a healthy server reconnects silently, a dead one
  // shows the offline toast, and 401 goes to the login screen.
  let probeFailed = false;
  try {
    await api.me();
  } catch (error) {
    if (error.status === 401) {
      handleSessionExpired();
      return;
    }
    probeFailed = true;
  }
  if (probeFailed && !state.offlineNotified) {
    state.offlineNotified = true;
    toast(t("toast.offline"), "warning");
  }
  scheduleReconnect();
}

function scheduleReconnect() {
  if (state.reconnectTimer !== null) return;
  const delay = state.reconnectDelay;
  state.reconnectDelay = Math.min(state.reconnectDelay * 2, 30000);
  state.reconnectTimer = window.setTimeout(() => {
    state.reconnectTimer = null;
    if (state.me) connectEvents();
  }, delay);
}

function closeEvents() {
  if (state.reconnectTimer !== null) {
    window.clearTimeout(state.reconnectTimer);
    state.reconnectTimer = null;
  }
  if (state.eventSource) {
    state.eventSource.close();
    state.eventSource = null;
  }
}

async function refreshAfterReconnect() {
  try {
    await loadCore();
    if (isBoardRoute() && state.pageVisible) {
      await renderRoute();
      return;
    }
    if (isBoardRoute()) await refreshBoard();
    if (state.route.name === "inbox") renderInboxPage();
    if (state.route.name === "settings-projects") await renderSettingsProjects(state.routeToken);
    if (state.route.name === "settings-tokens") await renderSettingsTokens(state.routeToken);
    if (state.route.name === "settings-integration") await renderSettingsIntegration(state.routeToken);
    if (state.route.name === "settings-export") await renderSettingsExport(state.routeToken);
    // Refetch an open card too, but do not discard an active edit or draft.
    if (state.panel && state.panel.currentReference) await state.panel.reload();
    renderChrome();
  } catch (error) {
    if (error.status !== 401) toastForError(error, t);
  }
}

function boardCardFromTask(task, previous) {
  return {
    id: task.id,
    reference: task.reference,
    title: task.title,
    status: task.status,
    priority: task.priority,
    due_date: task.due_date,
    labels: task.labels || [],
    claimed_by: task.claimed_by,
    attention: task.attention,
    session_status: task.session_status,
    blocked_from: task.blocked_from,
    blocked_reason: task.blocked_reason,
    checklist_done: previous ? previous.checklist_done : 0,
    checklist_total: previous ? previous.checklist_total : 0,
    blocked: previous ? previous.blocked : false
  };
}

function upsertTask(task) {
  const existing = findCardLocation(task.id);
  const card = boardCardFromTask(task, existing ? existing.card : null);
  if (task.state !== "active") {
    if (existing) existing.board.columns[existing.status].splice(existing.index, 1);
    return card;
  }
  const board = findBoardByProjectId(task.project_id);
  if (!board || !board.columns[task.status]) return card;
  if (existing) {
    if (existing.status === task.status) {
      board.columns[task.status][existing.index] = card;
    } else {
      existing.board.columns[existing.status].splice(existing.index, 1);
      board.columns[task.status].push(card);
    }
  } else {
    board.columns[task.status].push(card);
  }
  return card;
}

function applyTaskColumn(board, status, column, fallbackCard) {
  if (!board || !board.columns[status]) return;
  const known = new Map();
  for (const columnName of Object.keys(board.columns)) {
    for (const card of board.columns[columnName]) known.set(card.id, card);
  }
  const ordered = [];
  for (const entry of column || []) {
    const card = known.get(entry.id) || (fallbackCard && fallbackCard.id === entry.id ? fallbackCard : null);
    if (!card) {
      refreshBoardDebounced();
      continue;
    }
    card.status = status;
    ordered.push(card);
  }
  if (fallbackCard && !ordered.some((card) => card.id === fallbackCard.id)) {
    ordered.push(fallbackCard);
  }
  board.columns[status] = ordered;
}

function panelNeedsRefresh(taskId, changedDependencies = null) {
  const panel = state.panel;
  if (!panel || !panel.detail) return false;
  if (panel.currentTaskId === taskId) return true;
  if (
    changedDependencies &&
    [...(changedDependencies.blocked_by || []), ...(changedDependencies.blocks || [])].some(
      (item) => item.id === panel.currentTaskId
    )
  ) {
    return true;
  }
  const dependencies = panel.detail.dependencies || { blocked_by: [], blocks: [] };
  return [...(dependencies.blocked_by || []), ...(dependencies.blocks || [])].some(
    (item) => item.id === taskId
  );
}

function applyEvent(type, data) {
  if (type === "secret.updated") {
    if (state.panel && state.panel.currentTaskId === data.task_id) state.panel.reload({ force: true });
    return;
  }
  const task = data.task || null;
  let oldState = null;
  if (task && type !== "proposal.accepted") oldState = state.previousCards.get(task.id) || null;

  if (type.startsWith("task.") && task) {
    if (task.state === "proposed") {
      const index = state.inbox.findIndex((item) => item.id === task.id);
      if (type === "task.deleted" || type === "task.archived") {
        state.inbox = state.inbox.filter((item) => item.id !== task.id);
      } else if (index >= 0) {
        state.inbox[index] = task;
      } else {
        state.inbox.unshift(task);
      }
      if (state.route.name === "inbox") renderInboxPage();
      renderChrome();
      return;
    }
    if (type === "task.moved") {
      const card = upsertTask(task);
      const board = findBoardByProjectId(task.project_id);
      if (board && data.to) applyTaskColumn(board, data.to, data.column, card);
    } else if (type === "task.archived" || type === "task.deleted") {
      removeCard(task.id);
      state.previousCards.delete(task.id);
    } else if (type === "task.restored") {
      const card = upsertTask(task);
      const board = findBoardByProjectId(task.project_id);
      if (board && task.status) applyTaskColumn(board, task.status, data.column, card);
    } else if (type === "task.created") {
      upsertTask(task);
    } else {
      upsertTask(task);
    }

    if (
      task.attention ||
      task.status === "in_review" ||
      (task.status === "blocker" && (task.blocked_reason || task.session_id))
    ) {
      notifyTask(task, oldState);
    }
    if (task) state.previousCards.set(task.id, { attention: task.attention, status: task.status });

    if (panelNeedsRefresh(task.id)) {
      state.panel.reload();
    }
    // Derived card fields (blocked lock, checklist counters) are not part of
    // every event payload. Refetch when a move crosses done or a card appears
    // or disappears, so dependent locks and restored progress stay correct.
    if (
      (type === "task.moved" && (data.from === "done" || data.to === "done")) ||
      type === "task.archived" ||
      type === "task.deleted" ||
      type === "task.restored"
    ) {
      refreshBoardDebounced();
    }
    if (!state.boardView || !state.boardView.isDragging()) {
      renderBoardView();
      renderChrome();
    }
    return;
  }

  if (type === "dependencies.changed" && task) {
    const location = findCardLocation(task.id);
    if (location) {
      location.card.blocked = (data.dependencies?.blocked_by || []).some(
        (item) => item.status !== "done"
      );
    }
    if (panelNeedsRefresh(task.id, data.dependencies)) state.panel.reload();
    if (!state.boardView || !state.boardView.isDragging()) renderBoardView();
    return;
  }

  if (type.startsWith("proposal.")) {
    if (type === "proposal.created" && task) {
      if (!state.inbox.some((item) => item.id === task.id)) state.inbox.unshift(task);
    } else if (task) {
      state.inbox = state.inbox.filter((item) => item.id !== task.id);
      if (type === "proposal.accepted") refreshBoardDebounced();
    }
    if (state.route.name === "inbox") renderInboxPage();
    renderChrome();
    return;
  }

  if (type.startsWith("project.")) {
    const project = data.project;
    if (project) {
      if (type === "project.created") {
        if (!state.projects.some((item) => item.id === project.id)) state.projects.push(project);
        if (!state.allProjects.some((item) => item.id === project.id)) state.allProjects.push(project);
        state.projects.sort((a, b) => a.position - b.position || a.id.localeCompare(b.id));
        state.allProjects.sort((a, b) => a.position - b.position || a.id.localeCompare(b.id));
        if (state.pageVisible && (state.route.name === "global" || state.route.name === "home")) {
          renderRoute();
          return;
        }
      } else if (type === "project.updated") {
        state.projects = state.projects.map((item) => (item.id === project.id ? project : item));
        state.allProjects = state.allProjects.map((item) => (item.id === project.id ? project : item));
      } else if (type === "project.archived") {
        state.projects = state.projects.filter((item) => item.id !== project.id);
        state.allProjects = state.allProjects.map((item) =>
          item.id === project.id ? project : item
        );
        const wasCurrent = state.currentProjectId === project.id;
        if (wasCurrent) {
          state.currentProjectKey = null;
          state.currentProjectId = null;
        }
        if (wasCurrent && (state.route.name === "project" || state.route.name === "task")) {
          navigate("/", { replace: true });
          return;
        }
      }
      const board = findBoardByProjectId(project.id);
      if (board) board.project = project;
      if (state.currentProjectId === project.id) {
        state.currentProjectKey = project.key;
        if (
          (state.route.name === "project" || state.route.name === "task") &&
          project.key !== state.route.key
        ) {
          navigate(`/p/${encodeURIComponent(project.key)}`, { replace: true });
          return;
        }
      }
    }
    renderChrome();
    if (isBoardRoute()) refreshBoardDebounced();
    if (state.route.name === "settings-projects") renderSettingsProjects(state.routeToken);
    return;
  }

  if (type.startsWith("comment.") || type.startsWith("checklist.")) {
    if (state.panel && state.panel.currentTaskId === data.task_id) state.panel.reload();
    refreshBoardDebounced();
    return;
  }

  if (type.startsWith("label.")) {
    const label = data.label;
    if (type === "label.created" && label && !state.labels.some((item) => item.id === label.id)) {
      state.labels.push(label);
    }
    if (type === "label.updated" && label) {
      state.labels = state.labels.map((item) => (item.id === label.id ? label : item));
    }
    if (type === "label.deleted" && label) {
      state.labels = state.labels.filter((item) => item.id !== label.id);
    }
    state.labels.sort((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id));
    if (state.panel) state.panel.setLabels(state.labels);
    renderBoardView();
  }
}

function truncateText(value, limit) {
  const text = String(value || "").trim();
  if (text.length <= limit) return text;
  return `${text.slice(0, limit - 1).trimEnd()}…`;
}

function notifyTask(task, previous) {
  if (!state.notificationsEnabled) return;
  if (document.visibilityState !== "hidden") return;
  if (!("Notification" in window) || Notification.permission !== "granted") return;
  const enteredBlocker =
    task.status === "blocker" &&
    !(previous && previous.status === "blocker") &&
    Boolean(task.blocked_reason || task.session_id);
  if (enteredBlocker) {
    const body = truncateText(
      task.blocked_reason || t("notification.blockerBody"),
      120
    );
    try {
      new Notification(t("notification.taskBlocked", { ref: task.reference }), {
        body,
        tag: `kanso-${task.id}`
      });
    } catch {
      // Browsers may reject notifications in some contexts.
    }
    return;
  }
  const gainedAttention = task.attention && !(previous && previous.attention);
  const enteredReview = task.status === "in_review" && !(previous && previous.status === "in_review");
  if (!gainedAttention && !enteredReview) return;
  const title = gainedAttention
    ? t("notification.taskAttention", { ref: task.reference })
    : t("notification.taskReview", { ref: task.reference });
  try {
    new Notification(title, { body: t("notification.body"), tag: `kanso-${task.id}` });
  } catch {
    // Browsers may reject notifications in some contexts.
  }
}

// ---------------------------------------------------------------------------
// Top-bar actions

async function toggleNotifications() {
  if (state.notificationsEnabled) {
    state.notificationsEnabled = false;
    localStorage.removeItem(LS.notifications);
    renderChrome();
    refreshNotificationsSettings();
    return;
  }
  if (!("Notification" in window)) {
    toast(t("app.notificationsUnsupported"), "warning");
    return;
  }
  try {
    const permission = await Notification.requestPermission();
    if (permission === "granted") {
      state.notificationsEnabled = true;
      localStorage.setItem(LS.notifications, "1");
      toast(t("app.notificationsOn"), "success");
    } else {
      toast(t("app.notificationsBlocked"), "warning");
    }
  } catch {
    toast(t("app.notificationsBlocked"), "warning");
  }
  renderChrome();
  refreshNotificationsSettings();
}

async function logoutFlow() {
  state.loggingOut = true;
  try {
    await api.logout();
  } catch (error) {
    if (error.status !== 401) {
      state.loggingOut = false;
      toast(t("toast.logoutFailed"), "error");
      return;
    }
    // 401 means the session is already gone; the local sign-out is still real.
  }
  state.loggingOut = false;
  renderLogin();
}

// ---------------------------------------------------------------------------
// Keyboard shortcuts

function openKeyboardHelp() {
  const shortcuts = [
    ["n", t("help.newCard")],
    ["/", t("help.search")],
    ["j", t("help.nextCard")],
    ["k", t("help.previousCard")],
    ["m", t("help.moveMenu")],
    ["Esc", t("help.close")],
    ["?", t("help.help")]
  ];
  const list = el(
    "dl",
    { class: "shortcut-list" },
    shortcuts.flatMap(([key, label]) => [
      el("dt", {}, [el("kbd", { text: key })]),
      el("dd", { text: label })
    ])
  );
  openModal({
    title: t("help.title"),
    size: "small",
    body: el("div", { class: "shortcut-help" }, [
      el("p", { class: "field-hint", text: t("help.description") }),
      list
    ])
  });
}

function onGlobalKeyDown(event) {
  if (hasModal()) return;
  if (event.defaultPrevented) return;
  if (event.metaKey || event.ctrlKey || event.altKey) return;
  if (isTyping(event.target)) return;

  if (event.key === "Escape") {
    if (state.panel && state.panel.currentReference) {
      event.preventDefault();
      closePanel({ navigate: true });
    }
    return;
  }
  if (!state.me) return;

  if (event.key === "?") {
    event.preventDefault();
    openKeyboardHelp();
    return;
  }
  if (event.key === "n") {
    if (isBoardRoute() || state.route.name === "inbox") {
      event.preventDefault();
      newCardFlow();
    }
    return;
  }
  if (event.key === "/") {
    if (!isBoardRoute()) return;
    event.preventDefault();
    if (state.dom) state.dom.search.focus();
    return;
  }
  if (event.key === "j" || event.key === "k") {
    if (!isBoardRoute()) return;
    event.preventDefault();
    selectRelative(event.key === "j" ? 1 : -1);
    return;
  }
  if (event.key === "m") {
    if (!isBoardRoute()) return;
    const task = selectedTask();
    if (task) {
      event.preventDefault();
      openMoveMenu(task, (payload) => moveCardById(task, payload));
    }
  }
}

function selectedTask() {
  if (state.panel && state.panel.detail) return state.panel.detail;
  if (!state.selectedRef) return null;
  for (const board of state.boards) {
    for (const status of Object.keys(board.columns)) {
      const card = board.columns[status].find((item) => item.reference === state.selectedRef);
      if (card) return { ...card, projectId: board.project.id };
    }
  }
  return null;
}

function selectRelative(delta) {
  if (!state.dom) return;
  const cards = Array.from(
    state.dom.boardRegion.querySelectorAll(".card:not(.dragging)")
  ).filter((card) => card.offsetParent !== null);
  if (!cards.length) return;
  let index = cards.findIndex((card) => card.dataset.reference === state.selectedRef);
  if (index === -1) index = delta > 0 ? -1 : cards.length;
  const next = Math.max(0, Math.min(cards.length - 1, index + delta));
  const element = cards[next];
  const board = state.boards.find((item) => item.project.id === element.dataset.projectId);
  if (!board) return;
  const card = board.columns[element.dataset.status]?.find(
    (item) => item.reference === element.dataset.reference
  );
  if (card) {
    element.scrollIntoView({ block: "nearest", inline: "nearest" });
    element.focus({ preventScroll: true });
    selectCard({ ...card, projectId: board.project.id, projectKey: board.project.key });
  }
}

// ---------------------------------------------------------------------------
// Start

boot();
