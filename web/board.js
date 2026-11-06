// Board rendering and pointer-event drag & drop.
//
// No library, no HTML strings: every card is built with DOM nodes. The drag
// interaction uses pointer events only, with a long press for touch.

import { t } from "./i18n/index.js";
import { el, icon, isOverdue, formatDue } from "./dom.js";

export const STATUSES = ["backlog", "todo", "in_progress", "in_review", "blocker", "done"];
const ACTIVE_STATUSES = ["in_progress", "in_review", "blocker"];

const DRAG_THRESHOLD = 5;
const TOUCH_LONG_PRESS = 250;
const AUTOSCROLL_EDGE = 72;
const AUTOSCROLL_MAX = 20;

export function matchesSearch(card, search) {
  const needle = search.trim().toLowerCase();
  if (!needle) return true;
  if (card.reference.toLowerCase().includes(needle)) return true;
  if (card.title.toLowerCase().includes(needle)) return true;
  if ((card.claimed_by || "").toLowerCase().includes(needle)) return true;
  return (card.labels || []).some((label) => label.name.toLowerCase().includes(needle));
}

function cardData(card, project) {
  return { ...card, projectId: project.id, projectKey: project.key, projectName: project.name };
}

function priorityDot(priority) {
  const label = t(`priority.${priority}`) || priority;
  return el("span", {
    class: `priority-dot priority-${priority}`,
    title: t("card.priorityLabel", { priority: label }),
    "aria-label": t("card.priorityLabel", { priority: label })
  });
}

function dueChip(card) {
  if (!card.due_date) return null;
  const overdue = isOverdue(card.due_date);
  return el("span", {
    class: `due-chip${overdue ? " overdue" : ""}`,
    title: overdue
      ? t("card.overdue", { date: formatDue(card.due_date) })
      : t("card.due", { date: formatDue(card.due_date) })
  }, [formatDue(card.due_date)]);
}

function checklistChip(card) {
  const total = Number(card.checklist_total || 0);
  if (!total) return null;
  const done = Number(card.checklist_done || 0);
  return el("span", {
    class: `checklist-chip${done === total ? " complete" : ""}`,
    title: t("card.checklistTitle", { done, total })
  }, [icon("check", { size: 12 }), `${done}/${total}`]);
}

function claimChip(card) {
  if (!card.claimed_by) return null;
  const name = card.claimed_by === "ui" ? t("activity.you") : card.claimed_by;
  return el("span", {
    class: "claim-chip",
    title: t("card.claimedBy", { name })
  }, [icon("user", { size: 12 }), name]);
}

function renderCard(card, project, { selectedRef, onSelect, onMoveMenu }) {
  const icons = el("div", { class: "card-icons" });
  if (card.blocked || card.status === "blocker") {
    const fallback = card.blocked_from
      ? t("card.inBlocker", { status: t(`status.${card.blocked_from}`) })
      : t("card.blocked");
    const title =
      card.status === "blocker" && card.blocked_reason
        ? card.blocked_reason
        : fallback;
    icons.append(el("span", { class: "card-flag flag-lock", title, "aria-label": title }, [icon("lock", { size: 13 })]));
  }
  if (card.attention) {
    icons.append(
      el("span", { class: "card-flag flag-attention", title: t("card.attention"), "aria-label": t("card.attention") }, [
        icon("alert", { size: 13 })
      ])
    );
  }

  const labels = el("div", { class: "card-labels" });
  for (const label of card.labels || []) {
    labels.append(
      el("span", { class: "label-chip", style: { "--label-color": label.color } }, [label.name])
    );
  }

  const meta = el("div", { class: "card-meta" }, [
    priorityDot(card.priority),
    dueChip(card),
    checklistChip(card),
    claimChip(card)
  ]);

  const article = el(
    "article",
    {
      class: `card priority-border-${card.priority}${selectedRef === card.reference ? " selected" : ""}`,
      tabindex: "0",
      role: "group",
      "aria-label": t("card.openNamed", { ref: card.reference, title: card.title }),
      dataset: {
        taskId: card.id,
        reference: card.reference,
        status: card.status,
        projectId: project.id,
        projectKey: project.key
      },
      title: t("card.dragHint")
    },
    [
      el("div", { class: "card-top" }, [
        el("span", { class: "card-ref", text: card.reference }),
        icons
      ]),
      el("div", { class: "card-title", text: card.title }),
      card.status === "blocker" && card.blocked_reason
        ? el("div", {
            class: "card-blocked-reason",
            text: card.blocked_reason,
            title: card.blocked_reason
          })
        : null,
      (card.labels || []).length ? labels : null,
      meta
    ]
  );

  const menu = el(
    "button",
    {
      type: "button",
      class: "button button-ghost button-icon card-menu",
      "aria-label": t("card.moveTo"),
      title: t("card.moveTo"),
      dataset: { action: "move" }
    },
    [icon("menu", { size: 16 })]
  );
  meta.append(menu);

  article.addEventListener("click", (event) => {
    if (article.dataset.suppressClick === "1") {
      article.dataset.suppressClick = "";
      return;
    }
    if (event.target.closest("[data-action='move']")) {
      event.stopPropagation();
      onMoveMenu(cardData(card, project));
      return;
    }
    onSelect(cardData(card, project));
  });
  article.addEventListener("keydown", (event) => {
    // Let Enter/Space on the nested "Move to…" button do its own thing.
    if (event.target !== article) return;
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      onSelect(cardData(card, project));
    } else if (event.key.toLowerCase() === "m") {
      event.preventDefault();
      onMoveMenu(cardData(card, project));
    }
  });

  return article;
}

function quickAdd(project, status, onQuickAdd) {
  const trigger = el(
    "button",
    { type: "button", class: "quick-add-trigger" },
    [icon("plus", { size: 14 }), el("span", { text: t("board.quickAdd") })]
  );
  trigger.addEventListener("click", () => {
    const focusInput = trigger.dataset.focusOnOpen !== "0";
    const input = el("input", {
      class: "input quick-add-input",
      type: "text",
      placeholder: t("board.quickAddPlaceholder"),
      "aria-label": t("board.quickAddPlaceholder")
    });
    const form = el("form", { class: "quick-add-form" }, [input]);
    const column = trigger.closest(".column");
    const close = () => {
      if (form.isConnected) form.replaceWith(trigger);
    };
    let submitting = false;
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      if (submitting) return;
      const title = input.value.trim();
      if (!title) return;
      submitting = true;
      // Close before the request: a successful create re-renders the board
      // inside onQuickAdd, and a still-open draft would be replayed.
      close();
      const result = onQuickAdd({ projectId: project.id, status, title });
      const finish = (ok) => {
        submitting = false;
        if (ok === false) {
          const liveColumn =
            column && column.isConnected
              ? column
              : Array.from(document.querySelectorAll(".board-scroll .column")).find(
                  (node) =>
                    node.dataset.projectId === project.id && node.dataset.status === status
                );
          const liveTrigger = liveColumn
            ? liveColumn.querySelector(".quick-add-trigger")
            : null;
          if (liveTrigger) liveTrigger.click();
          const again = liveColumn ? liveColumn.querySelector(".quick-add-input") : null;
          if (again) {
            again.value = title;
            again.focus();
            again.setSelectionRange(title.length, title.length);
          }
        }
      };
      if (result && typeof result.then === "function") {
        result.then(finish).catch(() => finish(false));
      } else {
        finish(true);
      }
    });
    input.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        event.stopPropagation();
        close();
        trigger.focus();
      }
    });
    trigger.replaceWith(form);
    if (focusInput) input.focus();
  });
  return trigger;
}

function renderColumn(project, status, cards, options, mode) {
  const body = el("div", {
    class: "column-cards",
    dataset: { status, projectId: project.id },
    "aria-label": `${t(`status.${status}`)} — ${project.name}`
  });

  for (const card of cards) {
    body.append(renderCard(card, project, options));
  }
  if (!cards.length) {
    body.append(
      el("p", {
        class: "column-empty",
        text: options.search ? t("board.noMatches") : t("board.emptyColumn")
      })
    );
  }

  const footer =
    mode === "project" || cards.length
      ? el("div", { class: "column-footer" }, [quickAdd(project, status, options.onQuickAdd)])
      : null;

  return el("section", { class: "column", dataset: { status, projectId: project.id } }, [
    el("header", { class: "column-header" }, [
      el("span", { class: "column-title", text: t(`status.${status}`) }),
      el("span", { class: "column-count", text: String(cards.length) })
    ]),
    body,
    footer
  ]);
}

export function createBoardView(root, handlers) {
  root.classList.add("board-root");
  const scroll = el("div", { class: "board-scroll", tabindex: "-1" });
  root.append(scroll);

  let current = { boards: [], options: { search: "", showAll: false, selectedRef: null, mode: "project" } };
  let lastSignature = "";
  let press = null;
  let drag = null;
  let autoscrollFrame = null;
  let suppressClickUntil = 0;
  let destroyed = false;

  function render(boards, options = {}) {
    current = { boards: boards || [], options: { ...current.options, ...options } };
    const mode = current.options.mode === "global" ? "global" : "project";
    const signature = `${mode}:${current.boards.map((board) => board.project.id).join(",")}`;
    const preserveScroll = signature === lastSignature;
    const scrollLeft = scroll.scrollLeft;
    const scrollTop = scroll.scrollTop;
    const active = document.activeElement;
    const focusedRef =
      active && scroll.contains(active) && active.closest
        ? active.closest(".card")?.dataset.reference || null
        : null;
    const columnScroll = new Map();
    for (const body of scroll.querySelectorAll(".column-cards")) {
      const column = body.closest(".column");
      if (column) {
        columnScroll.set(`${column.dataset.projectId}:${column.dataset.status}`, body.scrollTop);
      }
    }
    const quickDrafts = Array.from(scroll.querySelectorAll(".quick-add-form")).map((form) => {
      const input = form.querySelector(".quick-add-input");
      const column = form.closest(".column");
      return {
        projectId: column ? column.dataset.projectId : "",
        status: column ? column.dataset.status : "",
        value: input ? input.value : ""
      };
    });
    const quickFocus =
      active && active.classList && active.classList.contains("quick-add-input")
        ? {
            projectId: active.closest(".column")?.dataset.projectId || "",
            status: active.closest(".column")?.dataset.status || ""
          }
        : null;
    scroll.replaceChildren();
    scroll.dataset.mode = mode;
    scroll.classList.toggle("filtered", Boolean(current.options.search.trim()));

    for (const board of current.boards) {
      const project = board.project;
      const columns = board.columns || {};
      const columnNodes = el("div", { class: "columns" });

      for (const status of STATUSES) {
        if (mode === "global" && !current.options.showAll && !ACTIVE_STATUSES.includes(status)) {
          continue;
        }
        const cards = (columns[status] || []).filter((card) => matchesSearch(card, current.options.search));
        columnNodes.append(
          renderColumn(project, status, cards, { ...current.options, ...handlers }, mode)
        );
      }

      if (mode === "global") {
        const total = STATUSES.reduce(
          (sum, status) =>
            (columns[status] || []).filter((card) => matchesSearch(card, current.options.search)).length + sum,
          0
        );
        scroll.append(
          el("section", { class: "project-group", dataset: { projectId: project.id } }, [
            el("header", { class: "project-group-header" }, [
              el("span", { class: "project-dot", style: { "--project-color": project.color } }),
              el("h2", { class: "project-group-title", text: project.name }),
              el("span", { class: "project-group-key", text: project.key }),
              el("span", { class: "project-group-count", text: String(total) })
            ]),
            columnNodes
          ])
        );
      } else {
        scroll.append(columnNodes);
      }
    }

    lastSignature = signature;
    if (preserveScroll) {
      scroll.scrollLeft = scrollLeft;
      scroll.scrollTop = scrollTop;
      for (const body of scroll.querySelectorAll(".column-cards")) {
        const column = body.closest(".column");
        if (!column) continue;
        const key = `${column.dataset.projectId}:${column.dataset.status}`;
        if (columnScroll.has(key)) body.scrollTop = columnScroll.get(key);
      }
    } else {
      scroll.scrollLeft = 0;
      scroll.scrollTop = 0;
    }
    if (focusedRef) {
      const restored = Array.from(scroll.querySelectorAll(".card")).find(
        (card) => card.dataset.reference === focusedRef
      );
      if (restored) restored.focus({ preventScroll: true });
    }
    for (const draft of quickDrafts) {
      const column = Array.from(scroll.querySelectorAll(".column")).find(
        (node) =>
          node.dataset.projectId === draft.projectId && node.dataset.status === draft.status
      );
      const trigger = column ? column.querySelector(".quick-add-trigger") : null;
      if (!trigger) continue;
      trigger.dataset.focusOnOpen = "0";
      trigger.click();
      delete trigger.dataset.focusOnOpen;
      const input = column.querySelector(".quick-add-input");
      if (!input) continue;
      input.value = draft.value;
      if (
        quickFocus &&
        quickFocus.projectId === draft.projectId &&
        quickFocus.status === draft.status
      ) {
        input.focus();
      }
    }
  }

  function visibleCards() {
    return Array.from(scroll.querySelectorAll(".card")).filter(
      (card) => !card.classList.contains("dragging")
    );
  }

  // -------------------------------------------------------------------------
  // Drag & drop

  function pointerPosition(event) {
    return { x: event.clientX, y: event.clientY };
  }

  function clearPress() {
    if (press && press.timer) window.clearTimeout(press.timer);
    press = null;
  }

  function startDrag(activePress) {
    if (drag || !activePress) return;
    const card = activePress.card;
    if (!card.isConnected) {
      clearPress();
      return;
    }
    activePress.timer = null;
    activePress.active = true;
    const rect = card.getBoundingClientRect();
    const ghost = card.cloneNode(true);
    ghost.classList.add("drag-ghost");
    ghost.removeAttribute("tabindex");
    ghost.removeAttribute("role");
    ghost.style.width = `${rect.width}px`;
    ghost.style.height = `${rect.height}px`;
    ghost.style.left = "0";
    ghost.style.top = "0";
    document.body.append(ghost);

    const originColumn = card.closest(".column");
    const originIndex = originColumn
      ? Array.from(originColumn.querySelectorAll(".column-cards > .card")).indexOf(card)
      : -1;

    const placeholder = el("div", { class: "card-placeholder" });
    placeholder.style.height = `${rect.height}px`;
    card.after(placeholder);
    card.classList.add("dragging");
    document.body.classList.add("dragging-card");

    drag = {
      card,
      ghost,
      placeholder,
      pointerId: activePress.pointerId,
      offsetX: activePress.lastX - rect.left,
      offsetY: activePress.lastY - rect.top,
      x: activePress.lastX,
      y: activePress.lastY,
      projectId: card.dataset.projectId,
      originStatus: card.dataset.status,
      originColumn,
      originIndex,
      started: true
    };
    try {
      card.setPointerCapture(activePress.pointerId);
    } catch {
      // Pointer capture is best-effort; document listeners still work.
    }
    moveGhost();
    updatePlaceholder();
    autoScrollLoop();
  }

  function moveGhost() {
    if (!drag) return;
    drag.ghost.style.transform = `translate(${drag.x - drag.offsetX}px, ${drag.y - drag.offsetY}px)`;
  }

  function updatePlaceholder() {
    if (!drag) return;
    const under = document.elementFromPoint(drag.x, drag.y);
    const column = under && under.closest ? under.closest(".column") : null;
    if (!column || column.dataset.projectId !== drag.projectId) return;
    const body = column.querySelector(".column-cards");
    if (!body) return;
    const cards = Array.from(body.children).filter(
      (node) => node.classList.contains("card") && !node.classList.contains("dragging")
    );
    let index = cards.findIndex((card) => {
      const rect = card.getBoundingClientRect();
      return drag.y < rect.top + rect.height / 2;
    });
    if (index === -1) index = cards.length;
    const reference = cards[index] || null;
    if (reference) {
      body.insertBefore(drag.placeholder, reference);
    } else {
      body.append(drag.placeholder);
    }
  }

  function autoScrollStep() {
    if (!drag) return;
    const rect = scroll.getBoundingClientRect();
    let delta = 0;
    if (drag.x < rect.left + AUTOSCROLL_EDGE) {
      delta = -AUTOSCROLL_MAX * (1 - (drag.x - rect.left) / AUTOSCROLL_EDGE);
    } else if (drag.x > rect.right - AUTOSCROLL_EDGE) {
      delta = AUTOSCROLL_MAX * (1 - (rect.right - drag.x) / AUTOSCROLL_EDGE);
    }
    if (delta !== 0) {
      scroll.scrollLeft += Math.max(-AUTOSCROLL_MAX, Math.min(AUTOSCROLL_MAX, delta));
      updatePlaceholder();
    }
    autoscrollFrame = window.requestAnimationFrame(autoScrollStep);
  }

  function autoScrollLoop() {
    if (autoscrollFrame !== null) window.cancelAnimationFrame(autoscrollFrame);
    autoscrollFrame = window.requestAnimationFrame(autoScrollStep);
  }

  function cleanupDrag() {
    if (!drag) return;
    clearPress();
    const { card, ghost, placeholder } = drag;
    try {
      card.releasePointerCapture(drag.pointerId);
    } catch {
      // Capture may already be released.
    }
    card.classList.remove("dragging");
    card.dataset.suppressClick = "1";
    ghost.remove();
    placeholder.remove();
    document.body.classList.remove("dragging-card");
    if (autoscrollFrame !== null) window.cancelAnimationFrame(autoscrollFrame);
    autoscrollFrame = null;
    drag = null;
    suppressClickUntil = Date.now() + 300;
    if (handlers.onDragEnd && !destroyed) handlers.onDragEnd();
  }

  function finishDrag(commit) {
    if (!drag) return;
    const { card, placeholder, originStatus } = drag;
    const body = placeholder.parentElement;
    const targetColumn = body ? body.closest(".column") : null;
    const targetStatus = targetColumn ? targetColumn.dataset.status : originStatus;
    let targetIndex = 0;
    if (body) {
      for (const node of body.children) {
        if (node === placeholder) break;
        if (node.classList.contains("card") && !node.classList.contains("dragging")) targetIndex += 1;
      }
    }
    let before = null;
    let after = null;
    if (body) {
      const targetCards = Array.from(body.children).filter(
        (node) => node.classList.contains("card") && !node.classList.contains("dragging")
      );
      before = targetCards[targetIndex] ? targetCards[targetIndex].dataset.reference : null;
      after = targetIndex > 0 ? targetCards[targetIndex - 1].dataset.reference : null;
    }

    const originIndex = drag.originIndex;
    const reference = card.dataset.reference;
    const from = originStatus;
    const projectId = drag.projectId;
    const changed =
      targetStatus !== originStatus || targetIndex !== Math.max(originIndex, 0);

    cleanupDrag();
    if (commit && changed) {
      handlers.onMove({ reference, from, to: targetStatus, before, after, projectId });
    }
  }

  function cancelDrag() {
    if (drag) finishDrag(false);
    clearPress();
  }

  function onPointerDown(event) {
    if (drag) return;
    // A filtered column's DOM no longer contains hidden neighbours, so a drop
    // could send a non-adjacent before/after pair. Use "Move to…" while
    // searching instead.
    if (current.options.search.trim()) return;
    if (event.pointerType === "mouse" && event.button !== 0) return;
    const card = event.target.closest ? event.target.closest(".card") : null;
    if (!card || !scroll.contains(card)) return;
    if (event.target.closest("button, a, input, textarea, select, label")) return;
    clearPress();
    press = {
      card,
      pointerId: event.pointerId,
      pointerType: event.pointerType,
      startX: event.clientX,
      startY: event.clientY,
      lastX: event.clientX,
      lastY: event.clientY,
      active: false,
      timer: null
    };
    if (event.pointerType === "touch") {
      const captured = press;
      press.timer = window.setTimeout(() => {
        if (press === captured && !press.active) startDrag(press);
      }, TOUCH_LONG_PRESS);
    }
  }

  function onPointerMove(event) {
    if (press && !press.active) {
      press.lastX = event.clientX;
      press.lastY = event.clientY;
      const distance = Math.hypot(event.clientX - press.startX, event.clientY - press.startY);
      if (press.pointerType === "touch") {
        if (distance > 8) clearPress();
        return;
      }
      if (distance >= DRAG_THRESHOLD) {
        const active = press;
        startDrag(active);
      }
    }
    if (!drag) return;
    event.preventDefault();
    drag.x = event.clientX;
    drag.y = event.clientY;
    moveGhost();
    updatePlaceholder();
  }

  function onPointerUp(event) {
    if (press && !press.active) {
      clearPress();
      return;
    }
    if (drag && (!event.pointerId || event.pointerId === drag.pointerId)) {
      finishDrag(true);
    }
  }

  function onPointerCancel() {
    if (drag) finishDrag(false);
    clearPress();
  }

  function onTouchMove(event) {
    if (drag) event.preventDefault();
  }

  function onClickCapture(event) {
    if (Date.now() < suppressClickUntil) {
      event.stopPropagation();
      event.preventDefault();
    }
  }

  function onKeyDown(event) {
    if (event.key === "Escape" && drag) {
      event.preventDefault();
      event.stopPropagation();
      finishDrag(false);
    }
  }

  scroll.addEventListener("pointerdown", onPointerDown);
  scroll.addEventListener("touchmove", onTouchMove, { passive: false });
  scroll.addEventListener("click", onClickCapture, true);
  document.addEventListener("pointermove", onPointerMove, { passive: false });
  document.addEventListener("pointerup", onPointerUp);
  document.addEventListener("pointercancel", onPointerCancel);
  document.addEventListener("keydown", onKeyDown, true);

  function destroy() {
    destroyed = true;
    cancelDrag();
    scroll.removeEventListener("pointerdown", onPointerDown);
    scroll.removeEventListener("touchmove", onTouchMove);
    scroll.removeEventListener("click", onClickCapture, true);
    document.removeEventListener("pointermove", onPointerMove);
    document.removeEventListener("pointerup", onPointerUp);
    document.removeEventListener("pointercancel", onPointerCancel);
    document.removeEventListener("keydown", onKeyDown, true);
    root.replaceChildren();
  }

  return {
    render,
    cancelDrag,
    destroy,
    isDragging: () => drag !== null,
    visibleCards,
    scroll,
    element: root
  };
}
