// The right panel (layout B): card details, editors, checklist, comments,
// dependencies, activity and lifecycle actions.

import { t } from "./i18n/index.js";
import { api } from "./api.js";
import {
  el,
  icon,
  iconButton,
  renderMarkdown,
  relativeTime,
  formatDue,
  isOverdue,
  toast,
  toastForError
} from "./dom.js";
import {
  confirmDialog,
  copyText,
  openDispatchDialog,
  openLabelForm,
  openMoveMenu
} from "./forms.js";

const STATUSES = ["backlog", "todo", "in_progress", "in_review", "blocker", "done"];
const PRIORITIES = ["low", "normal", "high"];

function statusLabel(status) {
  return t(`status.${status}`);
}

function latestReply(comments) {
  const replies = (comments || []).filter((comment) => comment.delivery_status);
  return replies.length ? replies[replies.length - 1] : null;
}

function replyToast(status) {
  if (status === "sent") return t("toast.replySent");
  if (status === "unknown") return t("toast.replyUnknown");
  return t("toast.replyFailed");
}

function selectInput(options, value, attributes = {}) {
  const node = el("select", { class: "input input-compact", ...attributes });
  for (const option of options) {
    node.append(
      el("option", {
        value: option.value,
        text: option.label,
        selected: option.value === value
      })
    );
  }
  return node;
}

export class Panel {
  constructor(root, options = {}) {
    this.root = root;
    this.options = options;
    this.me = options.me || { kind: "ui", name: "ui", scope: "human" };
    this.labels = options.labels || [];
    this.projects = options.projects || [];
    this.integration = options.integration || null;
    this.detail = null;
    this.reference = null;
    this.busy = false;
    this.editingDescription = false;
    this.editingItem = null;
    this.sessionLinkOpen = false;
    this.pendingReload = false;
    this.pendingReloadTimer = null;
    this.drafts = new Map();
    this.secretRequests = [];
    this.secretLoadFailed = false;
    this.lastRenderedReference = null;
    this.width = options.width || 420;
    this.collapsed = Boolean(options.collapsed);
    this.root.classList.add("panel");
    this.applyWidth();
    this.root.classList.toggle("collapsed", this.collapsed);
  }

  isHuman() {
    return this.me && this.me.scope === "human";
  }

  setIdentity(me) {
    this.me = me;
    if (this.detail) this.render();
  }

  setLabels(labels) {
    this.labels = labels || [];
    if (this.detail && !this.editingGuard()) this.render();
  }

  setProjects(projects) {
    this.projects = projects || [];
    if (this.detail && !this.editingGuard()) this.render();
  }

  setIntegration(integration) {
    this.integration = integration || null;
    if (this.detail && !this.editingGuard()) this.render();
  }

  projectFor(detail) {
    return this.projects.find((project) => project.id === detail.project_id) || null;
  }

  editingGuard() {
    const active = document.activeElement;
    const typingInPanel =
      active &&
      this.root.contains(active) &&
      ["INPUT", "TEXTAREA", "SELECT"].includes(active.tagName);
    return Boolean(
      this.busy || this.editingDescription || this.editingItem || typingInPanel
    );
  }

  setCollapsed(collapsed) {
    this.collapsed = Boolean(collapsed);
    this.root.classList.toggle("collapsed", this.collapsed);
    this.render();
    if (this.options.onCollapsedChange) this.options.onCollapsedChange(this.collapsed);
  }

  toggleCollapsed() {
    this.setCollapsed(!this.collapsed);
  }

  setWidth(width) {
    const clamped = Math.max(320, Math.min(640, Number(width) || 420));
    this.width = clamped;
    this.applyWidth();
    if (this.options.onWidthChange) this.options.onWidthChange(clamped);
  }

  applyWidth() {
    this.root.style.setProperty("--panel-width", `${this.width}px`);
    this.root.style.width = `${this.width}px`;
  }

  get currentTaskId() {
    return this.detail ? this.detail.id : null;
  }

  get currentReference() {
    return this.reference;
  }

  async open(reference, { force = false } = {}) {
    if (!reference) {
      this.hide();
      return;
    }
    if (!force && this.reference === reference && this.detail) {
      this.root.classList.add("open");
      return;
    }
    this.reference = reference;
    this.detail = null;
    this.secretRequests = [];
    this.editingDescription = false;
    this.editingItem = null;
    this.sessionLinkOpen = false;
    this.clearDrafts();
    this.clearPendingReload();
    this.root.classList.add("open");
    this.renderLoading();
    try {
      const detail = await api.task(reference);
      if (this.reference !== reference) return;
      const secrets = detail.status === "blocker" ? await api.secretRequests(reference).catch(() => null) : [];
      if (this.reference !== reference) return;
      this.secretLoadFailed = secrets === null;
      this.secretRequests = secrets || [];
      this.detail = detail;
      this.render({ resetScroll: true });
    } catch (error) {
      if (this.reference !== reference) return;
      if (error.status === 404) {
        if (this.options.onNotFound) this.options.onNotFound(reference);
        this.hide();
      } else {
        this.renderError(error);
      }
    }
  }

  hide() {
    this.reference = null;
    this.detail = null;
    this.secretRequests = [];
    this.editingDescription = false;
    this.editingItem = null;
    this.sessionLinkOpen = false;
    this.clearDrafts();
    this.clearPendingReload();
    this.root.classList.remove("open");
    this.root.replaceChildren();
    this.scrollEl = null;
  }

  captureDrafts() {
    for (const node of this.root.querySelectorAll("[data-draft]")) {
      this.drafts.set(node.dataset.draft, node.value);
    }
  }

  applyDrafts() {
    for (const node of this.root.querySelectorAll("[data-draft]")) {
      if (this.drafts.has(node.dataset.draft)) {
        node.value = this.drafts.get(node.dataset.draft);
      }
    }
  }

  clearDrafts() {
    this.drafts.clear();
  }

  clearDraft(key) {
    for (const node of this.root.querySelectorAll("[data-draft]")) {
      if (node.dataset.draft === key) {
        node.value = "";
        this.drafts.set(key, "");
        return;
      }
    }
    this.drafts.delete(key);
  }

  clearPendingReload() {
    if (this.pendingReloadTimer !== null) {
      window.clearTimeout(this.pendingReloadTimer);
      this.pendingReloadTimer = null;
    }
    this.pendingReload = false;
  }

  schedulePendingReload() {
    this.pendingReload = true;
    if (this.pendingReloadTimer !== null) return;
    this.pendingReloadTimer = window.setTimeout(() => {
      this.pendingReloadTimer = null;
      if (!this.pendingReload) return;
      if (!this.reference) {
        this.pendingReload = false;
        return;
      }
      if (this.editingGuard()) {
        this.schedulePendingReload();
        return;
      }
      this.pendingReload = false;
      this.reload();
    }, 600);
  }

  async reload({ force = false } = {}) {
    const reference = this.reference;
    if (!reference) return;
    if (!force && this.editingGuard()) {
      this.schedulePendingReload();
      return;
    }
    this.clearPendingReload();
    try {
      const detail = await api.task(reference);
      if (this.reference !== reference) return;
      const secrets = detail.status === "blocker" ? await api.secretRequests(reference).catch(() => null) : [];
      if (this.reference !== reference) return;
      this.secretLoadFailed = secrets === null;
      this.secretRequests = secrets || [];
      this.detail = detail;
      this.render();
    } catch (error) {
      if (this.reference !== reference) return;
      if (error.status === 404 && this.options.onNotFound) {
        this.options.onNotFound(reference);
        this.hide();
      }
    }
  }

  applyDetail(detail, message) {
    if (!detail) return;
    if (this.reference && detail.reference !== this.reference) return;
    const descriptionDraft = this.editingDescription
      ? this.root.querySelector("textarea[data-description-editor]")?.value
      : null;
    const itemDraft = this.editingItem
      ? this.root.querySelector("input.checklist-edit")?.value
      : null;
    this.detail = detail;
    this.clearPendingReload();
    this.render({ descriptionDraft, itemDraft });
    if (this.options.onChanged) this.options.onChanged(detail);
    if (message) toast(message, "success");
  }

  async saveDescription(detail, value) {
    const reference = this.reference;
    this.busy = true;
    try {
      const updated = await api.updateTask(detail.reference, { description: value });
      if (this.reference !== reference) return;
      this.editingDescription = false;
      this.applyDetail(updated, t("toast.saved"));
      this.drafts.delete("description");
    } catch (error) {
      if (this.reference !== reference) return;
      if (error.status === 404) {
        this.reload({ force: true });
      } else {
        toastForError(error, t);
        this.render({ descriptionDraft: value });
      }
    } finally {
      this.busy = false;
    }
  }

  async saveChecklistText(detail, item, value) {
    const reference = this.reference;
    this.busy = true;
    try {
      await api.updateChecklist(detail.reference, item.id, { text: value });
      if (this.reference !== reference) return;
      const updated = await api.task(reference);
      if (this.reference !== reference) return;
      this.editingItem = null;
      this.applyDetail(updated, t("toast.saved"));
      this.drafts.delete(`checklist:${item.id}`);
    } catch (error) {
      if (this.reference !== reference) return;
      if (error.status === 404) {
        this.reload({ force: true });
      } else {
        toastForError(error, t);
        this.render({ itemDraft: value });
      }
    } finally {
      this.busy = false;
    }
  }

  async run(promise, message, { taskMissing = true } = {}) {
    const reference = this.reference;
    this.busy = true;
    try {
      const detail = await promise;
      if (!reference || this.reference !== reference) return null;
      this.applyDetail(detail, message);
      return detail;
    } catch (error) {
      if (!reference || this.reference !== reference) return null;
      toastForError(error, t);
      if (error.status === 404) {
        if (taskMissing && this.options.onNotFound) {
          this.options.onNotFound(reference);
        } else {
          this.reload({ force: true });
        }
      } else {
        // Reset optimistic control values to the last known detail.
        this.render();
      }
      return null;
    } finally {
      this.busy = false;
    }
  }

  async refreshAfter(promise, message, clearDraftKey = null, { taskMissing = true } = {}) {
    const reference = this.reference;
    let mutated = false;
    this.busy = true;
    try {
      await promise;
      mutated = true;
      if (!reference || this.reference !== reference) return null;
      if (clearDraftKey) this.clearDraft(clearDraftKey);
      const detail = await api.task(reference);
      if (this.reference !== reference) return null;
      this.applyDetail(detail, message);
      return detail;
    } catch (error) {
      if (!reference || this.reference !== reference) return null;
      if (mutated) {
        toast(t("toast.refreshFailed"), "warning");
        this.render();
      } else {
        toastForError(error, t);
        if (error.status === 404 && !taskMissing) {
          this.reload({ force: true });
        } else if (error.status === 404 && this.options.onNotFound) {
          this.options.onNotFound(reference);
        } else {
          this.render();
        }
      }
      return null;
    } finally {
      this.busy = false;
    }
  }

  async sendReply() {
    const textarea = this.root.querySelector('textarea[data-draft="reply"]');
    if (!textarea || this.busy) return;
    const body = textarea.value.trim();
    if (!body) {
      toast(t("panel.replyEmpty"), "warning");
      return;
    }
    const reference = this.reference;
    let created = false;
    this.busy = true;
    try {
      const result = await api.replyTask(reference, body);
      created = true;
      if (this.reference !== reference) return;
      this.clearDraft("reply");
      const updated = await api.task(reference);
      if (this.reference !== reference) return;
      this.busy = false;
      const status = result.delivery ? result.delivery.status : null;
      this.applyDetail(updated);
      toast(replyToast(status), status === "sent" ? "success" : "warning");
    } catch (error) {
      this.busy = false;
      if (this.reference !== reference) return;
      if (created) {
        toast(t("toast.refreshFailed"), "warning");
        this.render();
      } else {
        toastForError(error, t);
        this.render();
      }
    } finally {
      this.busy = false;
    }
  }

  async retryReply(comment) {
    if (this.busy) return;
    const reference = this.reference;
    this.busy = true;
    try {
      const result = await api.retryReply(reference, comment.id);
      if (this.reference !== reference) return;
      const updated = await api.task(reference);
      if (this.reference !== reference) return;
      this.busy = false;
      const status = result.delivery ? result.delivery.status : null;
      this.applyDetail(updated);
      toast(replyToast(status), status === "sent" ? "success" : "warning");
    } catch (error) {
      this.busy = false;
      if (this.reference !== reference) return;
      toastForError(error, t);
      this.render();
    } finally {
      this.busy = false;
    }
  }

  renderLoading() {
    const scrollTop = this.scrollEl ? this.scrollEl.scrollTop : 0;
    this.root.replaceChildren();
    if (this.collapsed) {
      this.renderRail();
      return;
    }
    const body = el("div", { class: "panel-body" }, [
      el("div", { class: "panel-loading" }, [
        el("span", { class: "spinner", "aria-hidden": "true" }),
        el("span", { text: t("panel.loading") })
      ])
    ]);
    this.root.append(
      el("div", { class: "panel-shell" }, [this.renderHeader(), body]),
      this.renderResizer()
    );
    this.scrollEl = body;
    this.scrollEl.scrollTop = scrollTop;
  }

  renderError(error) {
    this.root.replaceChildren();
    if (this.collapsed) {
      this.renderRail();
      return;
    }
    const body = el("div", { class: "panel-body" }, [
      el("div", { class: "panel-empty" }, [
        el("p", { text: t("panel.loadError") }),
        el("p", { class: "muted", text: (error && error.message) || "" })
      ])
    ]);
    this.root.append(
      el("div", { class: "panel-shell" }, [this.renderHeader(), body]),
      this.renderResizer()
    );
    this.scrollEl = body;
  }

  renderRail() {
    const expand = iconButton(t("board.expandPanel"), "chevronLeft", {
      onClick: () => this.setCollapsed(false)
    });
    const rail = el("div", { class: "panel-rail" }, [
      expand,
      el("span", { class: "panel-rail-ref", text: this.reference || "" })
    ]);
    this.root.replaceChildren(rail);
  }

  renderHeader() {
    const collapse = iconButton(
      this.collapsed ? t("board.expandPanel") : t("board.collapsePanel"),
      this.collapsed ? "chevronLeft" : "chevronRight",
      { onClick: () => this.toggleCollapsed() }
    );
    const close = iconButton(t("panel.close"), "close", {
      onClick: () => {
        if (this.options.onClose) this.options.onClose();
      }
    });
    const reference = this.detail ? this.detail.reference : this.reference || "";
    const flags = [];
    if (this.detail && this.detail.attention) {
      flags.push(
        el("span", { class: "card-flag flag-attention", title: t("card.attention") }, [
          icon("alert", { size: 14 })
        ])
      );
    }
    return el("header", { class: "panel-header" }, [
      el("div", { class: "panel-header-ref" }, [
        el("span", { class: "panel-ref", text: reference }),
        ...flags
      ]),
      el("div", { class: "panel-header-actions" }, [collapse, close])
    ]);
  }

  renderResizer() {
    const handle = el("div", {
      class: "panel-resizer",
      role: "separator",
      "aria-orientation": "vertical",
      "aria-label": t("board.resizePanel"),
      title: t("board.resizePanel"),
      tabindex: "0"
    });
    let active = null;
    const move = (event) => {
      if (!active) return;
      this.setWidth(active.startWidth + (active.startX - event.clientX));
    };
    const stop = () => {
      if (!active) return;
      active = null;
      document.body.classList.remove("resizing-panel");
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    handle.addEventListener("pointerdown", (event) => {
      if (event.button !== 0) return;
      event.preventDefault();
      active = { startX: event.clientX, startWidth: this.width };
      try {
        handle.setPointerCapture(event.pointerId);
      } catch {
        // Best effort.
      }
      document.body.classList.add("resizing-panel");
      // Window listeners survive a panel re-render replacing this handle.
      window.addEventListener("pointermove", move);
      window.addEventListener("pointerup", stop);
      window.addEventListener("pointercancel", stop);
    });
    handle.addEventListener("keydown", (event) => {
      if (event.key === "ArrowLeft") {
        event.preventDefault();
        this.setWidth(this.width + 20);
      } else if (event.key === "ArrowRight") {
        event.preventDefault();
        this.setWidth(this.width - 20);
      }
    });
    return handle;
  }

  render({ resetScroll = false, descriptionDraft = null, itemDraft = null } = {}) {
    const previousScroll = resetScroll ? 0 : this.scrollEl ? this.scrollEl.scrollTop : 0;
    if (resetScroll) this.lastRenderedReference = this.reference;
    const active = document.activeElement;
    const focusKey =
      active && this.root.contains(active)
        ? {
            key: active.dataset ? active.dataset.focusKey || "" : "",
            aria: active.getAttribute("aria-label"),
            text: active.tagName === "BUTTON" ? active.textContent.trim() : "",
            className: active.className
          }
        : null;
    this.captureDrafts();
    this.root.replaceChildren();
    if (!this.detail) {
      if (this.collapsed) {
        this.renderRail();
      } else {
        this.renderLoading();
      }
      return;
    }
    if (this.collapsed) {
      this.renderRail();
      return;
    }

    const detail = this.detail;
    const body = el("div", { class: "panel-body" }, [
      this.renderSummary(detail),
      this.renderSecrets(detail),
      this.renderAgentQuestion(detail),
      this.renderDescription(detail, descriptionDraft),
      this.renderChecklist(detail, itemDraft),
      this.renderComments(detail),
      this.renderDependencies(detail),
      this.renderSession(detail),
      this.renderActivity(detail),
      this.renderActions(detail)
    ]);
    this.root.append(
      el("div", { class: "panel-shell" }, [this.renderHeader(), body]),
      this.renderResizer()
    );
    this.scrollEl = body;
    this.scrollEl.scrollTop = previousScroll;
    this.applyDrafts();
    let focused = false;
    if (focusKey) {
      const candidates = Array.from(this.root.querySelectorAll("button, input, select, textarea, [tabindex]"));
      let match = null;
      const usable = (node) =>
        !(node.disabled || node.getAttribute("aria-disabled") === "true");
      if (focusKey.key) {
        match = candidates.find(
          (node) => node.dataset && node.dataset.focusKey === focusKey.key && usable(node)
        );
      }
      if (!match && focusKey.aria) {
        match = candidates.find(
          (node) => node.getAttribute("aria-label") === focusKey.aria && usable(node)
        );
      }
      if (!match && focusKey.text) {
        match = candidates.find(
          (node) =>
            node.tagName === "BUTTON" &&
            node.className === focusKey.className &&
            node.textContent.trim() === focusKey.text &&
            usable(node)
        );
      }
      if (match) {
        match.focus({ preventScroll: true });
        focused = document.activeElement === match;
      }
    }
    if (!focused) {
      const focus = this.root.querySelector("[data-autofocus]");
      if (focus) focus.focus();
    }
  }

  // -------------------------------------------------------------------------
  // Sections

  renderSummary(detail) {
    const agentStatuses = ["todo", "in_progress", "in_review", "blocker"];
    const statusValues = this.isHuman()
      ? STATUSES
      : STATUSES.filter(
          (value) => value === detail.status || agentStatuses.includes(value)
        );
    const status = selectInput(
      statusValues.map((value) => ({ value, label: statusLabel(value) })),
      detail.status,
      { "aria-label": t("panel.status") }
    );
    status.disabled = detail.state !== "active";
    status.addEventListener("change", () => {
      this.run(api.moveTask(detail.reference, { status: status.value }), t("toast.moved"));
    });

    const priority = selectInput(
      PRIORITIES.map((value) => ({ value, label: t(`priority.${value}`) })),
      detail.priority,
      { "aria-label": t("panel.priority") }
    );
    priority.disabled = !this.isHuman();
    priority.addEventListener("change", () => {
      this.run(api.updateTask(detail.reference, { priority: priority.value }), t("toast.saved"));
    });

    const due = el("input", {
      class: "input input-compact",
      type: "date",
      value: detail.due_date || "",
      "aria-label": t("panel.due")
    });
    due.disabled = !this.isHuman();
    due.addEventListener("change", () => {
      this.run(api.updateTask(detail.reference, { due_date: due.value || null }), t("toast.saved"));
    });
    const clearDue = iconButton(t("panel.clearDue"), "close", {
      onClick: () => {
        this.run(api.updateTask(detail.reference, { due_date: null }), t("toast.saved"));
      }
    });
    clearDue.hidden = !detail.due_date || !this.isHuman();

    const meta = el("div", { class: "panel-meta" }, [
      el("label", { class: "meta-field" }, [
        el("span", { class: "field-label", text: t("panel.status") }),
        status
      ]),
      el("label", { class: "meta-field" }, [
        el("span", { class: "field-label", text: t("panel.priority") }),
        priority
      ]),
      el("label", { class: "meta-field" }, [
        el("span", { class: "field-label", text: t("panel.due") }),
        el("span", { class: "due-field" }, [due, clearDue])
      ])
    ]);

    const badges = el("div", { class: "panel-badges" });
    if (detail.state === "archived") {
      badges.append(el("span", { class: "badge badge-warning", text: t("panel.archived") }));
    }
    if (detail.state === "proposed") {
      badges.append(el("span", { class: "badge badge-info", text: t("panel.proposed") }));
    }
    if (detail.due_date && isOverdue(detail.due_date)) {
      badges.append(
        el("span", {
          class: "due-chip overdue",
          text: t("card.overdue", { date: formatDue(detail.due_date) })
        })
      );
    }
    if (detail.attention) {
      badges.append(el("span", { class: "badge badge-warning", text: t("card.attention") }));
    }
    if (detail.session_status) {
      badges.append(
        el("span", { class: "badge badge-neutral", text: t("panel.sessionStatus", { status: detail.session_status }) })
      );
    }

    const claim = this.renderClaim(detail);

    return el("section", { class: "panel-section panel-summary" }, [
      el("h2", { class: "panel-title", text: detail.title }),
      badges,
      meta,
      this.renderLabels(detail),
      claim
    ]);
  }

  renderAgentQuestion(detail) {
    if (detail.state !== "active" || detail.status !== "blocker") return null;

    const question = el("div", { class: "markdown agent-question-reason" });
    if (detail.blocked_reason) {
      renderMarkdown(question, detail.blocked_reason);
    } else {
      question.append(
        el("p", { class: "muted", text: t("panel.agentQuestionEmpty") })
      );
    }

    const children = [
      question,
      el("p", { class: "agent-question-warning", text: t("secret.replyWarning") }),
      el("p", {
        class: "muted agent-question-session",
        text: t("panel.sessionStatus", {
          status: detail.session_status || t("common.none")
        })
      })
    ];

    const integrationEnabled = Boolean(this.integration && this.integration.enabled);
    const hasSession = Boolean(detail.session_id);

    const lastReply = latestReply(detail.comments);
    if (lastReply && lastReply.delivery_status === "sent") {
      children.push(
        el("p", {
          class: "muted agent-question-last",
          text: t("panel.replySentAt", {
            when: relativeTime(lastReply.created_at, t)
          })
        })
      );
    } else if (lastReply) {
      const failed = lastReply.delivery_status === "failed";
      const error = lastReply.delivery_error || t("common.error");
      children.push(
        el(
          "div",
          {
            class: `reply-banner ${failed ? "reply-banner-failed" : "reply-banner-unknown"}`,
            role: "status"
          },
          [
            el("p", {
              class: "reply-banner-text",
              text: failed
                ? t("panel.replyFailedBanner", { error })
                : t("panel.replyUnknownBanner", { error })
            }),
            el("button", {
              type: "button",
              class: "button button-secondary button-small",
              disabled: this.busy,
              text: failed ? t("panel.retry") : t("panel.retryAnyway"),
              onClick: () => this.retryReply(lastReply)
            })
          ]
        )
      );
    }

    if (!hasSession) {
      children.push(
        el("p", { class: "agent-question-hint", text: t("panel.replyNoSession") })
      );
    } else if (!integrationEnabled) {
      children.push(
        el("p", {
          class: "agent-question-hint",
          text: t("panel.replyIntegrationDisabled")
        })
      );
    } else {
      // A busy session is fine: the prompt queues. A failed or offline session
      // may still be resumed, so warn instead of blocking the human.
      if (["failed", "offline"].includes(detail.session_status)) {
        children.push(
          el("p", {
            class: "agent-question-warning",
            text: t("panel.replySessionWarning", {
              status: detail.session_status
            })
          })
        );
      }
      const textarea = el("textarea", {
        class: "input textarea",
        rows: "3",
        "data-draft": "reply",
        placeholder: t("panel.replyPlaceholder"),
        "aria-label": t("panel.replyPlaceholder")
      });
      const send = el("button", {
        type: "submit",
        class: "button button-primary button-small",
        disabled: this.busy,
        text: t("panel.sendReply")
      });
      const form = el("form", { class: "reply-form" }, [textarea, send]);
      form.addEventListener("submit", (event) => {
        event.preventDefault();
        this.sendReply();
      });
      children.push(form);
    }

    return this.section(t("panel.agentQuestion"), children, []);
  }

  renderSecrets(detail) {
    if (detail.status !== "blocker" || detail.state !== "active") return null;
    if (this.secretLoadFailed) return this.section(t("secret.title"), [el("p", { text: t("secret.loadFailed") })], []);
    if (!this.secretRequests.length) return null;
    const request = this.secretRequests[0];
    const lines = [
      el("p", { text: request.label }),
      el("p", { class: "muted", text: request.purpose }),
      el("p", { class: "muted", text: t("secret.state", { state: request.state }) }),
      el("p", { class: "muted", text: t("secret.deadline", { date: new Date(request.deadline).toLocaleString() }) }),
      request.value_deadline ? el("p", { class: "muted", text: t("secret.valueDeadline", { date: new Date(request.value_deadline).toLocaleString() }) }) : null,
      request.invalidation_reason === "restart" ? el("p", { class: "agent-question-warning", text: t("secret.restart") }) : null,
      el("p", { class: "agent-question-warning", text: t("secret.guidance") }),
      el("p", { class: "muted", text: t("secret.delivery", { status: request.delivery_status || t("secret.notSent") }) })
    ];
    if (request.delivery_error) lines.push(el("p", { class: "muted", text: request.delivery_error }));
    const perform = async (operation, input = null) => {
      if (this.busy) return;
      this.busy = true;
      try {
        await operation();
        if (input) input.value = "";
        this.busy = false;
        await this.reload({ force: true });
      } catch {
        if (input) input.value = "";
        this.busy = false;
        // Never display arbitrary upstream error bodies for secret operations.
        toast(t("secret.operationFailed"), "warning");
        await this.reload({ force: true });
      }
    };
    const notificationLive = ["ready", "declined"].includes(request.state) &&
      !request.invalidation_reason && request.deadline > Date.now() &&
      (!request.value_deadline || request.value_deadline > Date.now());
    if (this.isHuman() && notificationLive && ["failed", "unknown"].includes(request.delivery_status)) {
      lines.push(el("button", { type: "button", class: "button button-secondary button-small", text: request.delivery_status === "unknown" ? t("panel.retryAnyway") : t("panel.retry"), onClick: () => perform(() => api.retrySecret(request.id)) }));
    }
    if (this.isHuman() && request.state === "waiting") {
      const field = el("textarea", { class: "input textarea secret-input", rows: "4", autocomplete: "off", spellcheck: "false", "aria-label": t("secret.value"), "data-secret-input": "true" });
      const mask = el("div", { class: "secret-mask", "aria-hidden": "true" });
      const wrapper = el("div", { class: "secret-input-wrap" }, [field, mask]);
      let revealed = false;
      const updateMask = () => { mask.textContent = Array.from(field.value, (c) => c === "\n" ? "\n" : "•").join(""); };
      field.addEventListener("input", updateMask);
      field.addEventListener("scroll", () => { mask.scrollTop = field.scrollTop; mask.scrollLeft = field.scrollLeft; });
      const reveal = el("button", { type: "button", class: "button button-secondary button-small", text: t("secret.reveal"), "aria-pressed": "false", onClick: () => {
        revealed = !revealed;
        wrapper.classList.toggle("revealed", revealed);
        reveal.setAttribute("aria-pressed", String(revealed));
        reveal.textContent = t(revealed ? "secret.hide" : "secret.reveal");
      } });
      const submit = el("button", { type: "button", class: "button button-primary button-small", text: t("secret.submit"), onClick: () => {
        if (!field.value) { toast(t("secret.empty"), "warning"); return; }
        perform(() => api.submitSecret(request.id, field.value), field);
      } });
      const explanation = el("textarea", { class: "input textarea", rows: "2", maxlength: "2000", placeholder: t("secret.explanation"), "aria-label": t("secret.explanation") });
      const decline = el("button", { type: "button", class: "button button-secondary button-small", text: t("secret.decline"), onClick: () => perform(() => api.declineSecret(request.id, explanation.value), field) });
      lines.push(wrapper, el("div", { class: "form-actions" }, [reveal, submit]), explanation, decline);
    }
    if (this.isHuman() && ["waiting", "ready"].includes(request.state)) {
      lines.push(el("button", { type: "button", class: "button button-secondary button-small", text: t("secret.cancel"), onClick: () => {
        const field = this.root.querySelector("[data-secret-input]");
        if (field) field.value = "";
        perform(() => api.cancelSecret(request.id), field);
      } }));
    }
    return this.section(t("secret.title"), lines, []);
  }

  renderLabels(detail) {
    const chips = el("div", { class: "panel-label-chips" });
    const current = detail.labels || [];
    for (const label of current) {
      const chip = el(
        "span",
        { class: "label-chip label-chip-removable", style: { "--label-color": label.color } },
        [label.name]
      );
      if (this.isHuman()) {
        chip.append(
          iconButton(t("panel.removeLabel", { name: label.name }), "close", {
            class: "chip-remove",
            onClick: () => {
              const ids = current.filter((item) => item.id !== label.id).map((item) => item.id);
              this.run(api.updateTask(detail.reference, { labels: ids }), t("toast.labelRemoved"));
            }
          })
        );
      }
      chips.append(chip);
    }
    if (!current.length) chips.append(el("span", { class: "muted", text: t("panel.noLabels") }));

    const actions = [];
    const available = this.labels.filter(
      (label) => !current.some((item) => item.id === label.id || item.name === label.name)
    );
    if (this.isHuman() && available.length) {
      const add = selectInput(
        [
          { value: "", label: t("panel.addLabel") },
          ...available.map((label) => ({ value: label.id, label: label.name }))
        ],
        "",
        { "aria-label": t("panel.addLabel") }
      );
      add.addEventListener("change", () => {
        if (!add.value) return;
        const ids = current.map((item) => item.id);
        if (!ids.includes(add.value)) ids.push(add.value);
        this.run(api.updateTask(detail.reference, { labels: ids }), t("toast.labelAdded"));
      });
      actions.push(add);
    }
    if (this.isHuman()) {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-ghost button-small",
            onClick: () => {
              openLabelForm({
                onCreated: (label) => {
                  this.labels.push(label);
                  if (this.options.onLabelsChanged) this.options.onLabelsChanged(this.labels);
                  const ids = current.map((item) => item.id);
                  ids.push(label.id);
                  this.run(api.updateTask(detail.reference, { labels: ids }), t("toast.labelAdded"));
                }
              });
            }
          },
          [icon("plus", { size: 13 }), el("span", { text: t("panel.newLabel") })]
        )
      );
    }

    return el("div", { class: "panel-labels" }, [
      el("span", { class: "field-label", text: t("panel.labels") }),
      chips,
      actions.length ? el("div", { class: "panel-label-actions" }, actions) : null
    ]);
  }

  renderClaim(detail) {
    if (!detail.claimed_by) {
      if (!this.isHuman() || !["backlog", "todo"].includes(detail.status) || detail.state !== "active") {
        return null;
      }
      const claim = el(
        "button",
        {
          type: "button",
          class: "button button-secondary button-small",
          onClick: () => this.run(api.claimTask(detail.reference), t("toast.claimed"))
        },
        [icon("user", { size: 14 }), el("span", { text: t("panel.claim") })]
      );
      return el("div", { class: "panel-claim" }, [
        el("span", { class: "muted", text: t("panel.unclaimed") }),
        claim
      ]);
    }
    const canRelease = this.isHuman() || this.me.name === detail.claimed_by;
    return el("div", { class: "panel-claim" }, [
      el("span", { class: "claim-chip" }, [
        icon("user", { size: 13 }),
        t("panel.claimed", {
          name: detail.claimed_by === "ui" ? t("activity.you") : detail.claimed_by
        })
      ]),
      canRelease
        ? el(
            "button",
            {
              type: "button",
              class: "button button-ghost button-small",
              onClick: () => this.run(api.releaseTask(detail.reference), t("toast.released"))
            },
            [el("span", { text: t("panel.release") })]
          )
        : null
    ]);
  }

  renderDescription(detail, draft = null) {
    const actions = [];
    if (this.isHuman() && !this.editingDescription) {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-ghost button-small",
            onClick: () => {
              this.editingDescription = true;
              this.render();
            }
          },
          [el("span", { text: t("panel.edit") })]
        )
      );
    }

    let content;
    if (this.editingDescription) {
      const textarea = el("textarea", {
        class: "input textarea",
        rows: "8",
        value: draft !== null ? draft : detail.description,
        "data-description-editor": "true",
        "data-draft": "description",
        "data-autofocus": "true",
        "aria-label": t("panel.description")
      });
      const save = el("button", {
        type: "button",
        class: "button button-primary button-small",
        text: t("panel.save")
      });
      const cancel = el("button", {
        type: "button",
        class: "button button-secondary button-small",
        text: t("panel.cancel")
      });
      save.addEventListener("click", () => {
        this.saveDescription(detail, textarea.value);
      });
      cancel.addEventListener("click", () => {
        this.editingDescription = false;
        this.render();
        this.drafts.delete("description");
      });
      content = el("div", { class: "form-stack" }, [
        textarea,
        el("div", { class: "form-actions" }, [cancel, save])
      ]);
    } else {
      const markdown = el("div", { class: "markdown" });
      if (detail.description) {
        renderMarkdown(markdown, detail.description);
      } else {
        markdown.append(el("p", { class: "muted", text: t("panel.descriptionEmpty") }));
      }
      content = markdown;
    }

    return this.section(t("panel.description"), [content], actions);
  }

  renderChecklist(detail, draft = null) {
    const items = detail.checklist || [];
    const done = items.filter((item) => item.done).length;
    const total = items.length;
    const progress = el("div", { class: "checklist-progress" }, [
      el("div", { class: "progress-track" }, [
        el("div", {
          class: "progress-fill",
          style: { width: total ? `${Math.round((done / total) * 100)}%` : "0%" }
        })
      ]),
      el("span", { class: "muted", text: `${done}/${total}` })
    ]);

    const list = el("ul", { class: "checklist" });
    items.forEach((item, index) => {
      const checkbox = el("input", {
        type: "checkbox",
        checked: item.done,
        "aria-label": item.text,
        dataset: { focusKey: `checklist-${item.id}-toggle` }
      });
      checkbox.addEventListener("change", () => {
        this.refreshAfter(
          api.updateChecklist(detail.reference, item.id, { done: checkbox.checked }),
          null,
          null,
          { taskMissing: false }
        );
      });

      let text;
      if (this.editingItem === item.id) {
        const input = el("input", {
          class: "input input-compact checklist-edit",
          type: "text",
          value: draft !== null ? draft : item.text,
          maxlength: "500",
          "data-draft": `checklist:${item.id}`,
          "data-autofocus": "true",
          "aria-label": t("panel.editItem"),
          dataset: { focusKey: `checklist-${item.id}-input` }
        });
        const save = iconButton(t("panel.save"), "check", {
          onClick: () => {
            const value = input.value.trim();
            if (!value) return;
            this.saveChecklistText(detail, item, value);
          }
        });
        const cancel = iconButton(t("panel.cancel"), "close", {
          onClick: () => {
            this.editingItem = null;
            this.render();
            this.drafts.delete(`checklist:${item.id}`);
          }
        });
        input.addEventListener("keydown", (event) => {
          if (event.key === "Enter") {
            event.preventDefault();
            save.click();
          } else if (event.key === "Escape") {
            event.stopPropagation();
            this.editingItem = null;
            this.render();
            this.drafts.delete(`checklist:${item.id}`);
          }
        });
        text = el("span", { class: "checklist-edit-row" }, [input, save, cancel]);
      } else {
        text = el(
          "button",
          {
            type: "button",
            class: `checklist-text${item.done ? " done" : ""}`,
            dataset: { focusKey: `checklist-${item.id}-text` },
            onClick: () => {
              this.editingItem = item.id;
              this.render();
            }
          },
          [item.text]
        );
      }

      const up = iconButton(t("panel.moveUp"), "arrowUp", {
        disabled: index === 0,
        dataset: { focusKey: `checklist-${item.id}-up` },
        onClick: () => this.reorderChecklist(detail, items, index, -1)
      });
      const down = iconButton(t("panel.moveDown"), "arrowDown", {
        disabled: index === items.length - 1,
        dataset: { focusKey: `checklist-${item.id}-down` },
        onClick: () => this.reorderChecklist(detail, items, index, 1)
      });
      const remove = this.isHuman()
        ? iconButton(t("panel.remove"), "trash", {
            dataset: { focusKey: `checklist-${item.id}-remove` },
            onClick: () => {
              this.refreshAfter(
                api.deleteChecklist(detail.reference, item.id),
                t("toast.removed"),
                null,
                { taskMissing: false }
              );
            }
          })
        : null;

      list.append(
        el("li", { class: `checklist-item${item.done ? " done" : ""}` }, [
          checkbox,
          text,
          el("span", { class: "checklist-buttons" }, [up, down, remove])
        ])
      );
    });

    const input = el("input", {
      class: "input input-compact",
      type: "text",
      maxlength: "500",
      "data-draft": "new-checklist",
      placeholder: t("panel.checklistItemPlaceholder"),
      "aria-label": t("panel.addChecklist")
    });
    const add = el("button", { type: "submit", class: "button button-secondary button-small" }, [
      icon("plus", { size: 14 }),
      el("span", { text: t("panel.add") })
    ]);
    const form = el("form", { class: "inline-form" }, [input, add]);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (this.busy) return;
      const value = input.value.trim();
      if (!value) return;
      this.refreshAfter(
        api.addChecklist(detail.reference, value),
        t("toast.added"),
        "new-checklist",
        { taskMissing: false }
      );
    });

    return this.section(
      t("panel.checklist"),
      [progress, total ? list : el("p", { class: "muted", text: t("panel.checklistEmpty") }), form],
      []
    );
  }

  reorderChecklist(detail, items, index, direction) {
    const target = index + direction;
    if (target < 0 || target >= items.length) return;
    let before = null;
    let after = null;
    if (direction < 0) {
      after = items[target - 1] ? items[target - 1].id : null;
      before = items[target].id;
    } else {
      after = items[target].id;
      before = items[target + 1] ? items[target + 1].id : null;
    }
    const payload = {};
    if (before) payload.before = before;
    if (after) payload.after = after;
    this.refreshAfter(
      api.updateChecklist(detail.reference, items[index].id, payload),
      t("toast.reordered"),
      null,
      { taskMissing: false }
    );
  }

  renderComments(detail) {
    const comments = (detail.comments || []).slice().reverse();
    const list = el("div", { class: "comments" });
    if (!comments.length) {
      list.append(el("p", { class: "muted", text: t("panel.noComments") }));
    }
    for (const comment of comments) {
      const body = el("div", { class: "markdown comment-body" });
      renderMarkdown(body, comment.body);
      const byline = [
        el("span", {
          class: "comment-author",
          text: comment.author === "ui" ? t("activity.you") : comment.author
        })
      ];
      if (
        comment.delivery_status === "failed" ||
        comment.delivery_status === "unknown"
      ) {
        const failed = comment.delivery_status === "failed";
        byline.push(
          el("span", {
            class: `badge ${failed ? "badge-warning" : "badge-info"} comment-delivery`,
            title: comment.delivery_error || "",
            text: failed
              ? t("panel.deliveryFailed")
              : t("panel.deliveryUnknown")
          }),
          el("button", {
            type: "button",
            class: "button button-ghost button-small comment-retry",
            disabled: this.busy,
            text: failed ? t("panel.retry") : t("panel.retryAnyway"),
            onClick: () => this.retryReply(comment)
          })
        );
      }
      list.append(
        el("article", { class: "comment" }, [
          el("header", { class: "comment-header" }, [
            el("span", { class: "comment-byline" }, byline),
            el("time", {
              class: "muted",
              datetime: new Date(comment.created_at).toISOString(),
              title: new Date(comment.created_at).toLocaleString(),
              text: relativeTime(comment.created_at, t)
            })
          ]),
          body
        ])
      );
    }

    const textarea = el("textarea", {
      class: "input textarea",
      rows: "3",
      "data-draft": "comment",
      placeholder: t("panel.commentPlaceholder"),
      "aria-label": t("panel.commentPlaceholder")
    });
    const add = el("button", { type: "submit", class: "button button-secondary button-small" }, [
      icon("plus", { size: 14 }),
      el("span", { text: t("panel.addComment") })
    ]);
    const form = el("form", { class: "comment-form" }, [textarea, add]);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (this.busy) return;
      const body = textarea.value.trim();
      if (!body) return;
      this.refreshAfter(api.comment(detail.reference, body), t("toast.added"), "comment");
    });

    return this.section(t("panel.comments"), [list, form], []);
  }

  renderDependencies(detail) {
    const dependencies = detail.dependencies || { blocked_by: [], blocks: [] };
    const rows = el("div", { class: "dependency-groups" });

    const group = (title, list, direction) => {
      const groupNode = el("div", { class: "dependency-group" }, [
        el("h4", { class: "dependency-title", text: title })
      ]);
      if (!list.length) {
        groupNode.append(el("p", { class: "muted", text: t("panel.noDependencies") }));
      }
      for (const dependency of list) {
        const row = el("div", { class: "dependency-row" }, [
          el("button", {
            type: "button",
            class: "dependency-ref",
            text: dependency.reference,
            onClick: () => {
              if (this.options.onOpenReference) this.options.onOpenReference(dependency.reference);
            }
          }),
          el("span", { class: "dependency-name", text: dependency.title }),
          el("span", {
            class: `status-chip status-${dependency.status}`,
            text: statusLabel(dependency.status)
          })
        ]);
        if (this.isHuman()) {
          row.append(
            iconButton(t("panel.removeDependency", { ref: dependency.reference }), "close", {
              onClick: () => this.removeDependency(detail, dependency, direction)
            })
          );
        }
        groupNode.append(row);
      }
      return groupNode;
    };

    rows.append(group(t("panel.blockedBy"), dependencies.blocked_by || [], "blocked_by"));
    rows.append(group(t("panel.blocks"), dependencies.blocks || [], "blocks"));

    let addForm = null;
    if (this.isHuman()) {
      const input = el("input", {
        class: "input input-compact",
        type: "text",
        "data-draft": "new-dependency",
        placeholder: t("panel.addDependency"),
        "aria-label": t("panel.addDependency")
      });
      const add = el("button", { type: "submit", class: "button button-secondary button-small" }, [
        icon("plus", { size: 14 }),
        el("span", { text: t("panel.add") })
      ]);
      addForm = el("form", { class: "inline-form" }, [input, add]);
      addForm.addEventListener("submit", async (event) => {
        event.preventDefault();
        const reference = input.value.trim().toUpperCase();
        if (!reference) return;
        if (reference === detail.reference) return;
        const refs = (dependencies.blocked_by || []).map((item) => item.reference);
        if (!refs.includes(reference)) refs.push(reference);
        const updated = await this.run(
          api.setDependencies(detail.reference, refs),
          t("toast.dependencyAdded"),
          { taskMissing: false }
        );
        if (updated) this.clearDraft("new-dependency");
      });
    }

    return this.section(t("panel.dependencies"), [rows, addForm], []);
  }

  async removeDependency(detail, dependency, direction) {
    if (direction === "blocked_by") {
      const refs = (detail.dependencies.blocked_by || [])
        .filter((item) => item.reference !== dependency.reference)
        .map((item) => item.reference);
      this.run(api.setDependencies(detail.reference, refs), t("toast.dependencyRemoved"), {
        taskMissing: false
      });
      return;
    }
    // "blocks": remove this card from the other card's blocked_by set.
    const reference = this.reference;
    this.busy = true;
    try {
      const other = await api.task(dependency.reference);
      if (!reference || this.reference !== reference) return;
      const refs = (other.dependencies.blocked_by || [])
        .filter((item) => item.reference !== detail.reference)
        .map((item) => item.reference);
      await api.setDependencies(dependency.reference, refs);
      if (!reference || this.reference !== reference) return;
      const refreshed = await api.task(reference);
      if (this.reference !== reference) return;
      this.applyDetail(refreshed, t("toast.dependencyRemoved"));
    } catch (error) {
      if (!reference || this.reference !== reference) return;
      toastForError(error, t);
      this.render();
    } finally {
      this.busy = false;
    }
  }

  activityText(entry) {
    const data = entry.data || {};
    const vars = {};
    if (entry.kind === "moved") {
      vars.from = data.from ? statusLabel(data.from) : "";
      vars.to = data.to ? statusLabel(data.to) : "";
    } else if (entry.kind === "updated") {
      const fields = Array.isArray(data.fields) ? data.fields : [];
      vars.fields = fields
        .map((field) => {
          const key = `field.${field}`;
          const translated = t(key);
          return translated === key ? field : translated;
        })
        .join(", ");
    }
    const text = t(`activity.${entry.kind}`, vars);
    return text === `activity.${entry.kind}` ? entry.kind : text;
  }

  renderSession(detail) {
    const project = this.projectFor(detail);
    if (!detail.session_id && !(this.isHuman() && detail.state === "active")) {
      return null;
    }

    const children = [];
    if (detail.session_id) {
      const copy = el(
        "button",
        {
          type: "button",
          class: "button button-ghost button-small",
          onClick: async () => {
            const copied = await copyText(detail.session_id);
            if (copied) toast(t("form.copied"), "success");
            else toast(t("panel.copySessionFailed"), "warning");
          }
        },
        [icon("copy", { size: 13 }), el("span", { text: t("panel.copySession") })]
      );
      children.push(
        el("div", { class: "panel-session" }, [
          el("p", {
            class: "muted",
            text: t("panel.sessionStatus", {
              status: detail.session_status || t("common.none")
            })
          }),
          el("div", { class: "session-line" }, [
            el("code", { class: "session-id", text: detail.session_id }),
            copy
          ]),
          detail.session_directory
            ? el("p", { class: "muted session-directory", text: detail.session_directory })
            : null
        ])
      );
    } else {
      children.push(el("p", { class: "muted", text: t("panel.noSession") }));
    }

    if (this.isHuman() && detail.state === "active" && !detail.session_id) {
      children.push(this.renderLinkSession(detail, project));
    }
    return this.section(t("panel.session"), children, []);
  }

  renderLinkSession(detail, project) {
    const sessionId = el("input", {
      class: "input input-compact",
      type: "text",
      placeholder: t("panel.sessionIdPlaceholder"),
      "aria-label": t("panel.sessionId"),
      "data-draft": "session-id"
    });
    const directory = el("input", {
      class: "input input-compact",
      type: "text",
      value: (project && project.repo_path) || "",
      placeholder: t("form.exampleRepoPath"),
      "aria-label": t("panel.sessionDirectory"),
      "data-draft": "session-directory"
    });
    const link = el(
      "button",
      {
        type: "button",
        class: "button button-secondary button-small",
        onClick: () => {
          const session_id = sessionId.value.trim();
          const dir = directory.value.trim();
          if (!session_id || !dir) {
            toast(t("form.requiredFields"), "warning");
            return;
          }
          this.run(
            api.linkSession(detail.reference, {
              session_id,
              directory: dir,
              status: "running"
            }),
            t("toast.sessionLinked")
          );
        }
      },
      [icon("link", { size: 13 }), el("span", { text: t("panel.linkSessionAction") })]
    );
    const details = el("details", { class: "session-link", open: this.sessionLinkOpen }, [
      el("summary", { text: t("panel.linkSession") }),
      el("div", { class: "form-stack" }, [
        el("label", { class: "field" }, [
          el("span", { class: "field-label", text: t("panel.sessionId") }),
          sessionId
        ]),
        el("label", { class: "field" }, [
          el("span", { class: "field-label", text: t("panel.sessionDirectory") }),
          directory
        ]),
        link
      ])
    ]);
    details.addEventListener("toggle", () => {
      this.sessionLinkOpen = details.open;
    });
    return details;
  }

  renderActivity(detail) {
    const entries = (detail.activity || []).slice().reverse();
    const list = el("ol", { class: "activity" });
    if (!entries.length) {
      list.append(el("li", { class: "muted", text: t("panel.noActivity") }));
    }
    for (const entry of entries) {
      const actor = entry.actor === "ui" ? t("activity.you") : entry.actor;
      list.append(
        el("li", { class: "activity-item" }, [
          el("span", { class: "activity-actor", text: actor }),
          el("span", { class: "activity-text", text: this.activityText(entry) }),
          el("time", {
            class: "muted",
            datetime: new Date(entry.created_at).toISOString(),
            title: new Date(entry.created_at).toLocaleString(),
            text: relativeTime(entry.created_at, t)
          })
        ])
      );
    }
    return this.section(t("panel.activity"), [list], []);
  }

  renderActions(detail) {
    const actions = [];
    const human = this.isHuman();
    const archived = detail.state === "archived";

    if (human && detail.state === "active") {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-secondary button-small",
            onClick: () => this.openMove(detail)
          },
          [icon("board", { size: 14 }), el("span", { text: t("panel.moveTo") })]
        )
      );
    }

    const project = this.projectFor(detail);
    const canDispatch =
      human &&
      detail.state === "active" &&
      !detail.claimed_by &&
      this.integration &&
      this.integration.enabled &&
      project &&
      project.repo_path &&
      project.repo_path.trim();

    if (canDispatch) {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-primary button-small",
            onClick: () => {
              openDispatchDialog({
                reference: detail.reference,
                agent: (project.agent || "").trim() || "build",
                model: (project.model || "").trim(),
                onSubmit: async (selection) => {
                  try {
                    const updated = await api.dispatchTask(detail.reference, selection);
                    if (this.reference === detail.reference) {
                      this.applyDetail(updated, t("toast.dispatched"));
                    }
                  } catch (error) {
                    if (error.status === 404 && this.options.onNotFound) {
                      this.options.onNotFound(detail.reference);
                    }
                    throw error;
                  }
                }
              });
            }
          },
          [icon("globe", { size: 14 }), el("span", { text: t("panel.dispatch") })]
        )
      );
    }

    if (archived) {
      if (human) {
        actions.push(
          el(
            "button",
            {
              type: "button",
              class: "button button-secondary button-small",
              onClick: () => this.run(api.restoreTask(detail.reference), t("toast.restored"))
            },
            [icon("arrowUp", { size: 14 }), el("span", { text: t("panel.restore") })]
          )
        );
      }
    } else if (detail.state === "active" && human) {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-secondary button-small",
            onClick: () => this.run(api.archiveTask(detail.reference), t("toast.archived"))
          },
          [icon("inbox", { size: 14 }), el("span", { text: t("panel.archive") })]
        )
      );
    }

    if (human) {
      actions.push(
        el(
          "button",
          {
            type: "button",
            class: "button button-danger button-small",
            onClick: async () => {
              const confirmed = await confirmDialog(
                t("panel.deleteConfirm", { ref: detail.reference }),
                { title: t("panel.delete"), confirmLabel: t("panel.delete"), danger: true }
              );
              if (!confirmed) return;
              this.busy = true;
              try {
                await api.deleteTask(detail.reference);
                toast(t("toast.deleted"), "success");
                if (this.options.onDeleted) this.options.onDeleted(detail);
              } catch (error) {
                toastForError(error, t);
              } finally {
                this.busy = false;
              }
            }
          },
          [icon("trash", { size: 14 }), el("span", { text: t("panel.delete") })]
        )
      );
    }

    if (!actions.length) return null;

    return this.section(t("panel.actions"), [el("div", { class: "panel-actions" }, actions)], []);
  }

  openMove(detail) {
    openMoveMenu(detail, (payload) => {
      this.run(api.moveTask(detail.reference, payload), t("toast.moved"));
    });
  }

  section(title, children, actions = []) {
    return el("section", { class: "panel-section" }, [
      el("header", { class: "panel-section-head" }, [
        el("h3", { class: "panel-section-title", text: title }),
        actions.length ? el("div", { class: "panel-section-actions" }, actions) : null
      ]),
      ...children.filter(Boolean)
    ]);
  }
}
