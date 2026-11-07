// Fetch wrapper for the Kanso API. Every failure surfaces the server's error
// envelope as an ApiError with `status` and `code`.

import { t } from "./i18n/index.js";

export class ApiError extends Error {
  constructor(message, { code = "error", status = 0, body = null } = {}) {
    super(message);
    this.name = "ApiError";
    this.code = code;
    this.status = status;
    this.body = body;
  }
}

let unauthorizedHandler = null;

export function setUnauthorizedHandler(handler) {
  unauthorizedHandler = handler;
}

function query(params) {
  if (!params) return "";
  const search = new URLSearchParams();
  for (const [key, value] of Object.entries(params)) {
    if (value === undefined || value === null || value === "") continue;
    search.set(key, String(value));
  }
  const text = search.toString();
  return text ? `?${text}` : "";
}

async function requestResponse(
  path,
  { method = "GET", body, allowUnauthorized = false, signal, secret = false } = {}
) {
  const headers = { Accept: "application/json" };
  let payload;
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    payload = JSON.stringify(body);
  }

  let response;
  try {
    response = await fetch(path, {
      method,
      headers,
      body: payload,
      credentials: "same-origin",
      redirect: secret ? "error" : "follow",
      signal
    });
  } catch (error) {
    if (error && error.name === "AbortError") throw error;
    throw new ApiError(t("error.network"), { code: "network", status: 0 });
  }

  if (response.status === 401 && !allowUnauthorized) {
    if (unauthorizedHandler) unauthorizedHandler();
  }

  if (!response.ok) {
    let envelope = null;
    let message = response.statusText || t("error.generic");
    try {
      envelope = await response.json();
      if (envelope && envelope.error) {
        message = envelope.error.message || message;
      }
    } catch {
      // Non-JSON error body: keep the status text.
    }
    const code = (envelope && envelope.error && envelope.error.code) || "error";
    throw new ApiError(message, { code, status: response.status, body: envelope });
  }

  return response;
}

async function request(path, options = {}) {
  const response = await requestResponse(path, options);
  if (response.status === 204) return null;
  const type = response.headers.get("content-type") || "";
  if (!type.includes("application/json")) return null;
  return response.json();
}

async function download(path) {
  const response = await requestResponse(path);
  const disposition = response.headers.get("content-disposition") || "";
  const match = disposition.match(/filename="([^"]+)"/);
  const blob = await response.blob();
  return {
    blob,
    filename: match ? match[1] : "kanso-export.json",
    text: await blob.text()
  };
}

const enc = encodeURIComponent;

export const api = {
  me: () => request("/api/me", { allowUnauthorized: true }),

  async login(password) {
    return request("/api/auth/login", {
      method: "POST",
      body: { password },
      allowUnauthorized: true
    });
  },

  logout: () => request("/api/auth/logout", { method: "POST" }),

  integration: () => request("/api/integration").catch(() => null),

  projects: (options = {}) => request(`/api/projects${query({ archived: options.archived ? 1 : undefined })}`),

  project: (reference) => request(`/api/projects/${enc(reference)}`),

  createProject: (payload) => request("/api/projects", { method: "POST", body: payload }),

  updateProject: (id, payload) => request(`/api/projects/${enc(id)}`, { method: "PATCH", body: payload }),

  archiveProject: (id) => request(`/api/projects/${enc(id)}/archive`, { method: "POST" }),

  board: (projectReference) =>
    request(`/api/board${query({ project: projectReference || undefined })}`),

  projectBoard: (reference) => request(`/api/projects/${enc(reference)}/board`),

  tasks: (params) => request(`/api/tasks${query(params)}`),

  task: (reference) => request(`/api/tasks/${enc(reference)}`),

  createTask: (payload) => request("/api/tasks", { method: "POST", body: payload }),

  updateTask: (reference, payload) =>
    request(`/api/tasks/${enc(reference)}`, { method: "PATCH", body: payload }),

  moveTask: (reference, payload) =>
    request(`/api/tasks/${enc(reference)}/move`, { method: "POST", body: payload }),

  claimTask: (reference) => request(`/api/tasks/${enc(reference)}/claim`, { method: "POST" }),

  releaseTask: (reference) => request(`/api/tasks/${enc(reference)}/release`, { method: "POST" }),

  exportDump: () => download("/api/export.json"),

  dispatchTask: (reference, payload) =>
    request(`/api/tasks/${enc(reference)}/dispatch`, { method: "POST", body: payload }),

  linkSession: (reference, payload) =>
    request(`/api/tasks/${enc(reference)}/session`, { method: "POST", body: payload }),

  comment: (reference, body) =>
    request(`/api/tasks/${enc(reference)}/comment`, { method: "POST", body: { body } }),

  replyTask: (reference, body) =>
    request(`/api/tasks/${enc(reference)}/reply`, { method: "POST", body: { body } }),

  retryReply: (reference, commentId) =>
    request(`/api/tasks/${enc(reference)}/reply/${enc(commentId)}`, { method: "POST" }),

  secretRequests: (reference) => request(`/api/tasks/${enc(reference)}/secrets`),
  submitSecret: (id, value) => request(`/api/secrets/${enc(id)}/submit`, { method: "POST", body: { value }, secret: true }),
  declineSecret: (id, explanation) => request(`/api/secrets/${enc(id)}/decline`, { method: "POST", body: { explanation: explanation || null } }),
  cancelSecret: (id) => request(`/api/secrets/${enc(id)}/cancel`, { method: "POST" }),
  retrySecret: (id) => request(`/api/secrets/${enc(id)}/retry`, { method: "POST" }),

  addChecklist: (reference, text) =>
    request(`/api/tasks/${enc(reference)}/checklist`, { method: "POST", body: { text } }),

  updateChecklist: (reference, item, payload) =>
    request(`/api/tasks/${enc(reference)}/checklist/${enc(item)}`, {
      method: "PATCH",
      body: payload
    }),

  deleteChecklist: (reference, item) =>
    request(`/api/tasks/${enc(reference)}/checklist/${enc(item)}`, { method: "DELETE" }),

  setDependencies: (reference, blockedBy) =>
    request(`/api/tasks/${enc(reference)}/dependencies`, {
      method: "POST",
      body: { blocked_by: blockedBy }
    }),

  archiveTask: (reference) =>
    request(`/api/tasks/${enc(reference)}/archive`, { method: "POST" }),

  restoreTask: (reference) =>
    request(`/api/tasks/${enc(reference)}/restore`, { method: "POST" }),

  deleteTask: (reference) =>
    request(`/api/tasks/${enc(reference)}${query({ hard: "true" })}`, { method: "DELETE" }),

  labels: () => request("/api/labels"),

  createLabel: (payload) => request("/api/labels", { method: "POST", body: payload }),

  updateLabel: (id, payload) =>
    request(`/api/labels/${enc(id)}`, { method: "PATCH", body: payload }),

  deleteLabel: (id) => request(`/api/labels/${enc(id)}`, { method: "DELETE" }),

  inbox: () => request("/api/inbox"),

  acceptProposal: (reference, status) =>
    request(`/api/inbox/${enc(reference)}/accept`, {
      method: "POST",
      body: status ? { status } : {}
    }),

  rejectProposal: (reference) =>
    request(`/api/inbox/${enc(reference)}/reject`, { method: "POST" }),

  tokens: () => request("/api/tokens"),

  createToken: (payload) => request("/api/tokens", { method: "POST", body: payload }),

  revokeToken: (id) => request(`/api/tokens/${enc(id)}`, { method: "DELETE" })
};
