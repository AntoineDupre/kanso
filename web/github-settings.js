// Device material is held only in this screen's DOM and transient closures.
import { api } from "./api.js";
import { el } from "./dom.js";
import { t } from "./i18n/index.js";

const fallback = t("github.fallback");
const descriptions = {
  connected: t("github.connected"),
  not_connected: t("github.notConnected"),
  credential_error: t("github.credentialError"),
  missing_cli: t("github.missingCli"),
  unavailable: t("github.unavailable")
};
const terminal = {
  completed: t("github.completed"),
  cancelled: t("github.cancelled"),
  expired: t("github.expired"),
  failed: t("github.failed", { fallback })
};

export function githubSettings(isCurrent) {
  const root = el("section", { class: "settings-list", "aria-label": t("github.title") });
  let status = null;
  let statusError = false;
  let progress = null;
  let busy = false;
  let timer = null;
  let previous = "idle";
  const button = (label, action, disabled = false) => {
    const b = el("button", { type: "button", class: "button button-secondary", text: label, disabled });
    b.addEventListener("click", action);
    return b;
  };
  async function refresh(force = false) {
    try { status = await (force ? api.githubRefresh() : api.githubStatus()); statusError = false; }
    catch { statusError = true; if (!status) status = { state: "unavailable", compatible: false }; }
  }
  async function update() {
    window.clearTimeout(timer);
    if (!isCurrent()) return;
    const before = progress;
    try {
      progress = await api.githubProgress();
      if ((previous === "pending" || before?.state === "unavailable") && progress.state !== "pending") await refresh(true);
      previous = progress.state;
    } catch { progress = { state: "unavailable", owner: false }; }
    if (!isCurrent()) return;
    if (before?.state === "pending" && progress.state === "pending" && before.owner === progress.owner && before.code === progress.code) {
      const message = root.querySelector("[data-github-progress]");
      if (message) message.textContent = progress.owner ? t("github.waiting", { seconds: progress.seconds_remaining ?? 0 }) : t("github.otherPending");
      timer = window.setTimeout(update, 1500);
      return;
    }
    draw();
    if (["pending", "unavailable"].includes(progress.state)) timer = window.setTimeout(update, 1500);
  }
  function draw() {
    const pending = progress && progress.state === "pending";
    const children = [
      el("h2", { text: t("github.title") }),
      el("p", { class: "field-hint", text: t("github.warning") }),
      el("p", { text: `${descriptions[status?.state] || t("github.checking")}${status?.account ? `: ${status.account}` : ""}` }),
      el("p", { class: "field-hint", text: t("github.repositoryAccess") })
    ];
    if (statusError) children.push(el("p", { class: "field-hint", text: t("github.statusFetchFailed") }));
    if (status?.operator_managed) children.push(el("p", { class: "field-hint", text: t("github.operatorManaged") }));
    if (status && (!status.compatible || ["missing_cli", "unavailable"].includes(status.state))) children.push(el("p", { class: "field-hint", text: fallback }));
    if (progress?.state in terminal) children.push(el("p", { role: "status", text: terminal[progress.state] }));
    if (pending) {
      children.push(el("p", { role: "status", "data-github-progress": "", text: progress.owner ? t("github.waiting", { seconds: progress.seconds_remaining ?? 0 }) : t("github.otherPending") }));
      if (progress.owner && progress.code) {
        const code = progress.code;
        const copy = button(t("github.copy"), async () => { try { await navigator.clipboard.writeText(code); copy.textContent = t("github.copied"); } catch { copy.textContent = t("github.copyFailed"); } });
        children.push(el("p", {}, [el("code", { text: code }), document.createTextNode(" "), copy]));
      }
      if (progress.owner) {
        const actions = el("div", { class: "github-auth-actions" });
        if (progress.code) actions.append(el("a", { href: "https://github.com/login/device", target: "_blank", rel: "noopener noreferrer", text: t("github.deviceLink") }));
        actions.append(button(t("github.cancel"), async () => { if (busy) return; busy = true; try { await api.githubCancel(); } catch { /* Completion can race cancellation. */ } finally { busy = false; await update(); } }, busy));
        children.push(actions);
      }
    } else {
      if (progress?.state === "unavailable") children.push(el("p", { role: "status", text: t("github.progressUnavailable") }));
      const actions = el("div", { class: "settings-row-actions" });
      actions.append(button(t("github.refresh"), async () => { if (busy) return; busy = true; await refresh(true); busy = false; draw(); }));
      const connected = ["connected", "credential_error"].includes(status?.state);
      actions.append(button(connected ? t("github.reconnect") : t("github.connect"), async () => {
        if (busy) return;
        if (connected && !window.confirm(t("github.confirmReconnect"))) return;
        busy = true;
        try { await api.githubStart(connected); previous = "pending"; await update(); }
        catch { await update(); }
        finally { busy = false; draw(); }
      }, busy || progress?.state === "unavailable" || statusError || !status?.compatible || status?.operator_managed || !["connected", "not_connected", "credential_error"].includes(status?.state)));
      children.push(actions);
    }
    root.replaceChildren(el("article", { class: "settings-row" }, [el("div", { class: "settings-row-main" }, children)]));
  }
  draw();
  Promise.all([refresh(), api.githubProgress().then((p) => { progress = p; previous = p.state; }).catch(() => { progress = { state: "unavailable" }; })]).then(() => {
    if (!isCurrent()) return;
    draw();
    if (["pending", "unavailable"].includes(progress.state)) timer = window.setTimeout(update, 1500);
  });
  return root;
}
