// Modal dialogs and the create/edit forms.

import { t } from "./i18n/index.js";
import { api } from "./api.js";
import { el, icon, iconButton, toast, toastForError } from "./dom.js";

const modalStack = [];

function modalRoot() {
  return document.getElementById("modal-root");
}

export function openModal({ title, body, size = "medium", onClose = null, label = null }) {
  const host = modalRoot();
  const appRoot = document.getElementById("app-root");
  const sheetId = `modal-title-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
  const previousFocus = document.activeElement;

  const closeButton = iconButton(t("form.close"), "close");
  const dialog = el(
    "div",
    {
      class: `modal modal-${size}`,
      role: "dialog",
      "aria-modal": "true",
      "aria-labelledby": sheetId
    },
    [
      el("header", { class: "modal-header" }, [
        el("h2", { class: "modal-title", id: sheetId, text: title }),
        closeButton
      ]),
      el("div", { class: "modal-body" }, body)
    ]
  );
  const backdrop = el("div", { class: "modal-backdrop" }, [dialog]);

  let closed = false;
  function close(value) {
    if (closed) return;
    closed = true;
    const index = modalStack.indexOf(handle);
    if (index >= 0) modalStack.splice(index, 1);
    document.removeEventListener("keydown", onKeyDown, true);
    backdrop.remove();
    if (!modalStack.length && appRoot) appRoot.inert = false;
    if (onClose) onClose(value);
    if (previousFocus && previousFocus.focus && previousFocus.isConnected) {
      previousFocus.focus({ preventScroll: true });
    }
  }

  function onKeyDown(event) {
    if (modalStack[modalStack.length - 1] !== handle) return;
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close(undefined);
      return;
    }
    if (event.key !== "Tab") return;
    const focusable = Array.from(
      dialog.querySelectorAll(
        'a[href], button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex="-1"])'
      )
    ).filter((node) => node.offsetParent !== null || node === document.activeElement);
    if (!focusable.length) return;
    const first = focusable[0];
    const last = focusable[focusable.length - 1];
    const inside = dialog.contains(document.activeElement);
    if (event.shiftKey && (!inside || document.activeElement === first)) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && (!inside || document.activeElement === last)) {
      event.preventDefault();
      first.focus();
    }
  }

  const handle = { close, dialog, backdrop, label };
  closeButton.addEventListener("click", () => close(undefined));
  backdrop.addEventListener("mousedown", (event) => {
    if (event.target === backdrop) close(undefined);
  });
  document.addEventListener("keydown", onKeyDown, true);
  modalStack.push(handle);
  host.append(backdrop);
  if (modalStack.length === 1 && appRoot) appRoot.inert = true;

  const target =
    dialog.querySelector(
      ".modal-body input, .modal-body textarea, .modal-body select, .modal-body button"
    ) || closeButton;
  if (target) window.setTimeout(() => target.focus(), 0);

  return handle;
}

export function closeAllModals() {
  while (modalStack.length) modalStack[modalStack.length - 1].close(undefined);
}

export function closeTopModal() {
  const top = modalStack[modalStack.length - 1];
  if (top) top.close(undefined);
  return Boolean(top);
}

export function hasModal() {
  return modalStack.length > 0;
}

export function confirmDialog(message, { title = "", confirmLabel = t("common.delete"), cancelLabel = t("common.cancel"), danger = false } = {}) {
  return new Promise((resolve) => {
    let answered = false;
    const finish = (value) => {
      if (answered) return;
      answered = true;
      resolve(value);
    };
    const cancel = el("button", { type: "button", class: "button button-secondary", text: cancelLabel });
    const confirm = el("button", {
      type: "button",
      class: `button ${danger ? "button-danger" : "button-primary"}`,
      text: confirmLabel
    });
    const modal = openModal({
      title: title || t("common.notFoundTitle"),
      size: "small",
      body: [
        el("p", { class: "modal-message", text: message }),
        el("div", { class: "form-actions" }, [cancel, confirm])
      ],
      onClose: () => finish(false)
    });
    cancel.addEventListener("click", () => {
      finish(false);
      modal.close();
    });
    confirm.addEventListener("click", () => {
      finish(true);
      modal.close();
    });
  });
}

export function openDispatchDialog({ reference, agent = "", model = "", worktree = true, onSubmit }) {
  const agentInput = el("input", {
    class: "input",
    type: "text",
    maxlength: "120",
    value: agent,
    placeholder: t("form.exampleAgent")
  });
  const modelInput = el("input", {
    class: "input",
    type: "text",
    maxlength: "200",
    value: model,
    placeholder: t("form.exampleModel")
  });
  const worktreeInput = el("input", { type: "checkbox", checked: worktree });
  const error = formError();
  const cancel = el("button", {
    type: "button",
    class: "button button-secondary",
    text: t("form.cancel")
  });
  const submit = el("button", {
    type: "submit",
    class: "button button-primary",
    text: t("panel.dispatch")
  });
  const form = el("form", { class: "form-stack" }, [
    el("p", { class: "modal-message", text: t("panel.dispatchNote", { ref: reference }) }),
    field(t("panel.dispatchAgent"), agentInput, t("panel.dispatchAgentHint")),
    field(t("panel.dispatchModel"), modelInput, t("panel.dispatchModelHint")),
    field(t("form.worktree"), worktreeInput, t("panel.dispatchWorktreeHint")),
    error,
    el("div", { class: "form-actions" }, [cancel, submit])
  ]);
  const modal = openModal({ title: t("panel.dispatch"), body: form, size: "small" });
  cancel.addEventListener("click", () => modal.close());
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    setBusy(submit, true);
    cancel.disabled = true;
    try {
      await onSubmit({
        agent: agentInput.value.trim(),
        model: modelInput.value.trim(),
        worktree: worktreeInput.checked
      });
      modal.close();
    } catch (failure) {
      if (failure.status === 404) {
        modal.close();
        toastForError(failure, t);
      } else if (form.isConnected) {
        showError(error, failure.message || t("common.error"));
      } else {
        // The dialog was closed while the request was in flight; do not lose
        // the failure on a detached form.
        toastForError(failure, t);
      }
    } finally {
      setBusy(submit, false);
      cancel.disabled = false;
    }
  });
  return modal;
}

function field(labelText, control, hint) {
  return el("label", { class: "field" }, [
    el("span", { class: "field-label", text: labelText }),
    control,
    hint ? el("span", { class: "field-hint", text: hint }) : null
  ]);
}

function select(options, value, attributes = {}) {
  const node = el("select", { class: "input", ...attributes });
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

function statusOptions() {
  return ["backlog", "todo", "in_progress", "in_review", "blocker", "done"].map((value) => ({
    value,
    label: t(`status.${value}`)
  }));
}

function priorityOptions() {
  return ["low", "normal", "high"].map((value) => ({ value, label: t(`priority.${value}`) }));
}

function formError() {
  return el("p", { class: "form-error", role: "alert", hidden: true });
}

function showError(node, message) {
  node.textContent = message;
  node.hidden = false;
}

function setBusy(button, busy) {
  button.disabled = busy;
  button.setAttribute("aria-busy", busy ? "true" : "false");
}

export function openCardForm({ project, projects = [], labels = [], initialStatus = "backlog", onCreated }) {
  const title = el("input", {
    class: "input",
    type: "text",
    required: true,
    maxlength: "200",
    placeholder: t("form.titlePlaceholder"),
    "aria-label": t("form.title")
  });
  const description = el("textarea", {
    class: "input textarea",
    rows: "5",
    placeholder: t("form.descriptionPlaceholder"),
    "aria-label": t("form.description")
  });
  const status = select(statusOptions(), initialStatus);
  const priority = select(priorityOptions(), "normal");
  const due = el("input", { class: "input", type: "date", "aria-label": t("form.due") });
  const projectSelect = select(
    projects.map((item) => ({ value: item.id, label: `${item.key} — ${item.name}` })),
    project ? project.id : projects[0] ? projects[0].id : ""
  );

  const labelBoxes = labels.map((label) => {
    const input = el("input", { type: "checkbox", value: label.id });
    return el(
      "label",
      { class: "label-option", style: { "--label-color": label.color } },
      [input, el("span", { class: "label-chip", style: { "--label-color": label.color } }, [label.name])]
    );
  });

  const error = formError();
  const cancel = el("button", { type: "button", class: "button button-secondary", text: t("form.cancel") });
  const submit = el("button", { type: "submit", class: "button button-primary", text: t("form.create") });

  const form = el("form", { class: "form-stack" }, [
    field(t("form.title"), title),
    field(t("form.description"), description),
    projects.length > 1 ? field(t("form.project"), projectSelect) : null,
    el("div", { class: "form-row" }, [
      field(t("form.status"), status),
      field(t("form.priority"), priority),
      field(t("form.due"), due)
    ]),
    labels.length
      ? el("fieldset", { class: "field" }, [
          el("legend", { class: "field-label", text: t("form.labels") }),
          el("div", { class: "label-options" }, labelBoxes)
        ])
      : el("p", { class: "field-hint", text: t("panel.noLabels") }),
    error,
    el("div", { class: "form-actions" }, [cancel, submit])
  ]);

  const modal = openModal({ title: t("form.newCard"), body: form, size: "medium" });
  cancel.addEventListener("click", () => modal.close());

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    if (!title.value.trim()) {
      showError(error, t("form.titleRequired"));
      title.focus();
      return;
    }
    const projectId = projects.length > 1 ? projectSelect.value : project ? project.id : projectSelect.value;
    const selectedLabels = labelBoxes
      .map((node) => node.querySelector("input"))
      .filter((input) => input.checked)
      .map((input) => input.value);
    const payload = {
      project_id: projectId,
      title: title.value.trim(),
      description: description.value,
      status: status.value,
      priority: priority.value,
      due_date: due.value || null
    };
    if (selectedLabels.length) payload.labels = selectedLabels;
    setBusy(submit, true);
    try {
      const detail = await api.createTask(payload);
      modal.close();
      toast(t("toast.created"), "success");
      if (onCreated) onCreated(detail);
    } catch (failure) {
      showError(error, failure.message || t("common.error"));
      toastForError(failure, t);
    } finally {
      setBusy(submit, false);
    }
  });
  return modal;
}

export function openProjectForm({ project = null, onSaved }) {
  const editing = Boolean(project);
  const key = el("input", {
    class: "input",
    type: "text",
    required: true,
    maxlength: "6",
    value: project ? project.key : "",
    placeholder: t("form.exampleKey"),
    autocapitalize: "characters"
  });
  const name = el("input", {
    class: "input",
    type: "text",
    required: true,
    maxlength: "120",
    value: project ? project.name : ""
  });
  const description = el("textarea", {
    class: "input textarea",
    rows: "3",
    value: project ? project.description : ""
  });
  const repoPath = el("input", {
    class: "input",
    type: "text",
    value: project && project.repo_path ? project.repo_path : "",
    placeholder: t("form.exampleRepoPath")
  });
  const color = el("input", {
    class: "input input-color",
    type: "color",
    value: project ? project.color : "#64748b"
  });
  const agent = el("input", { class: "input", type: "text", value: (project && project.agent) || "" });
  const model = el("input", {
    class: "input",
    type: "text",
    value: (project && project.model) || "",
    placeholder: t("form.exampleModel")
  });
  const baseBranch = el("input", {
    class: "input",
    type: "text",
    value: (project && project.base_branch) || "",
    placeholder: t("form.exampleBranch")
  });
  const worktree = el("input", { type: "checkbox", checked: project ? Boolean(project.worktree) : true });
  const promptTemplate = el("textarea", {
    class: "input textarea",
    rows: "4",
    value: (project && project.prompt_template) || ""
  });

  const error = formError();
  const cancel = el("button", { type: "button", class: "button button-secondary", text: t("form.cancel") });
  const submit = el("button", {
    type: "submit",
    class: "button button-primary",
    text: editing ? t("form.save") : t("form.create")
  });

  const form = el("form", { class: "form-stack" }, [
    el("div", { class: "form-row" }, [
      field(t("form.projectKey"), key, t("form.projectKeyHint")),
      field(t("form.projectName"), name)
    ]),
    field(t("form.description"), description),
    el("div", { class: "form-row" }, [
      field(t("form.repoPath"), repoPath),
      field(t("form.color"), color)
    ]),
    el("section", { class: "form-section" }, [
      el("h3", { class: "form-section-title", text: t("form.agentSettings") }),
      el("p", { class: "field-hint", text: t("form.agentHint") }),
      el("div", { class: "form-row" }, [
        field(t("form.agent"), agent),
        field(t("form.model"), model, t("form.modelHint"))
      ]),
      el("div", { class: "form-row" }, [
        field(t("form.baseBranch"), baseBranch),
        field(t("form.worktree"), worktree)
      ]),
      field(t("form.promptTemplate"), promptTemplate)
    ]),
    error,
    el("div", { class: "form-actions" }, [cancel, submit])
  ]);

  const modal = openModal({
    title: editing ? t("form.editProject") : t("form.newProject"),
    body: form,
    size: "large"
  });
  cancel.addEventListener("click", () => modal.close());

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    const base = {
      key: key.value.trim().toUpperCase(),
      name: name.value.trim(),
      description: description.value,
      color: color.value,
      repo_path: repoPath.value.trim() || null,
      model: model.value.trim() || null
    };
    if (!base.key || !base.name) {
      showError(error, t("form.requiredFields"));
      return;
    }
    setBusy(submit, true);
    const agentFields = {
      agent: agent.value.trim() || null,
      model: model.value.trim() || null,
      base_branch: baseBranch.value.trim() || null,
      worktree: worktree.checked,
      prompt_template: promptTemplate.value || null
    };
    try {
      let saved;
      if (editing) {
        saved = await api.updateProject(project.id, { ...base, ...agentFields });
      } else {
        const created = await api.createProject(base);
        try {
          saved = await api.updateProject(created.id, agentFields);
        } catch (patchError) {
          // The project exists; do not leave the form retrying the same key.
          modal.close();
          toast(t("toast.projectPartial"), "warning");
          if (onSaved) onSaved(created);
          return;
        }
      }
      modal.close();
      toast(t("toast.projectSaved"), "success");
      if (onSaved) onSaved(saved);
    } catch (failure) {
      showError(error, failure.message || t("common.error"));
      toastForError(failure, t);
    } finally {
      setBusy(submit, false);
    }
  });
  return modal;
}

export function openMoveMenu(task, onChoose) {
  const buttons = [];
  for (const value of ["backlog", "todo", "in_progress", "in_review", "blocker", "done"]) {
    if (value === task.status) continue;
    buttons.push(
      el(
        "button",
        {
          type: "button",
          class: "menu-item",
          dataset: { status: value }
        },
        [el("span", { class: `status-dot status-${value}` }), el("span", { text: t(`status.${value}`) })]
      )
    );
  }

  const body = el("div", { class: "menu-list" }, buttons);
  if (task.status === "blocker" && task.blocked_from) {
    body.prepend(
      el(
        "button",
        { type: "button", class: "menu-item menu-restore" },
        [
          icon("arrowUp", { size: 14 }),
          el("span", { text: t("panel.restoreTo", { status: t(`status.${task.blocked_from}`) }) })
        ]
      )
    );
  }
  const modal = openModal({ title: t("form.moveCardTitle", { ref: task.reference }), body, size: "small" });
  body.addEventListener("click", (event) => {
    const button = event.target.closest("button");
    if (!button) return;
    modal.close();
    if (button.classList.contains("menu-restore")) {
      onChoose({ restore: true });
      return;
    }
    if (button.dataset.status) onChoose({ status: button.dataset.status });
  });
  return modal;
}

export function openTokenForm({ onCreated }) {
  const name = el("input", {
    class: "input",
    type: "text",
    required: true,
    maxlength: "64",
    placeholder: t("form.exampleTokenName"),
    "aria-label": t("form.tokenName")
  });
  const scope = select(
    [
      { value: "agent", label: t("form.scopeAgent") },
      { value: "human", label: t("form.scopeHuman") }
    ],
    "agent"
  );
  const error = formError();
  const cancel = el("button", { type: "button", class: "button button-secondary", text: t("form.cancel") });
  const submit = el("button", { type: "submit", class: "button button-primary", text: t("form.create") });
  const form = el("form", { class: "form-stack" }, [
    field(t("form.tokenName"), name, t("form.tokenNameHint")),
    field(t("form.tokenScope"), scope),
    error,
    el("div", { class: "form-actions" }, [cancel, submit])
  ]);
  const modal = openModal({ title: t("settings.createToken"), body: form, size: "small" });
  cancel.addEventListener("click", () => modal.close());

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    if (!name.value.trim()) {
      showError(error, t("form.requiredFields"));
      return;
    }
    setBusy(submit, true);
    try {
      const created = await api.createToken({ name: name.value.trim(), scope: scope.value });
      modal.close();
      if (onCreated) onCreated(created);
      showTokenOnce(created);
    } catch (failure) {
      showError(error, failure.message || t("common.error"));
      toastForError(failure, t);
    } finally {
      setBusy(submit, false);
    }
  });
  return modal;
}

export async function copyText(text) {
  try {
    if (navigator.clipboard && navigator.clipboard.writeText) {
      await navigator.clipboard.writeText(text);
      return true;
    }
  } catch {
    // Fall through to the selection-based fallback.
  }
  const area = el("textarea", { class: "copy-fallback", value: text });
  document.body.append(area);
  area.select();
  let copied = false;
  try {
    copied = document.execCommand("copy");
  } catch {
    copied = false;
  }
  area.remove();
  return copied;
}

export function showTokenOnce(created) {
  const value = el("input", {
    class: "input token-value",
    type: "text",
    readonly: true,
    value: created.token,
    "aria-label": t("form.showTokenTitle")
  });
  const copy = el("button", { type: "button", class: "button button-secondary" }, [
    icon("copy", { size: 15 }),
    el("span", { text: t("form.copy") })
  ]);
  const done = el("button", { type: "button", class: "button button-primary", text: t("form.close") });
  const body = el("div", { class: "form-stack" }, [
    el("p", { text: t("form.showTokenBody") }),
    value,
    el("div", { class: "form-actions" }, [copy, done])
  ]);
  const modal = openModal({ title: t("form.showTokenTitle"), body, size: "small" });
  copy.addEventListener("click", async () => {
    const copied = await copyText(created.token);
    toast(copied ? t("form.copied") : t("form.copyFailed"), copied ? "success" : "warning");
  });
  done.addEventListener("click", () => modal.close());
  return modal;
}

export function openLabelForm({ onCreated }) {
  const name = el("input", { class: "input", type: "text", required: true, maxlength: "32" });
  const color = el("input", { class: "input input-color", type: "color", value: "#2563eb" });
  const error = formError();
  const cancel = el("button", { type: "button", class: "button button-secondary", text: t("form.cancel") });
  const submit = el("button", { type: "submit", class: "button button-primary", text: t("form.create") });
  const form = el("form", { class: "form-stack" }, [
    el("div", { class: "form-row" }, [
      field(t("form.name"), name),
      field(t("form.color"), color)
    ]),
    error,
    el("div", { class: "form-actions" }, [cancel, submit])
  ]);
  const modal = openModal({ title: t("panel.newLabel"), body: form, size: "small" });
  cancel.addEventListener("click", () => modal.close());
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    error.hidden = true;
    if (!name.value.trim()) {
      showError(error, t("form.requiredFields"));
      return;
    }
    setBusy(submit, true);
    try {
      const label = await api.createLabel({ name: name.value.trim(), color: color.value });
      modal.close();
      if (onCreated) onCreated(label);
    } catch (failure) {
      showError(error, failure.message || t("common.error"));
      toastForError(failure, t);
    } finally {
      setBusy(submit, false);
    }
  });
  return modal;
}
