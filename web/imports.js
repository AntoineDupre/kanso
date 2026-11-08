import { api } from "./api.js";
import { el } from "./dom.js";
import { t } from "./i18n/index.js";

// Source links are external, untrusted input, even when a provider is trusted.
export function sourceLink(url) {
  try {
    const parsed = new URL(url);
    if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password) return null;
    return el("a", {
      href: parsed.href,
      target: "_blank",
      rel: "noopener noreferrer",
      text: t("imports.openSource")
    });
  } catch {
    return null;
  }
}

function taskLink(reference, navigate) {
  const match = reference?.match(/^([A-Z0-9]{2,6})-(\d+)$/);
  if (!match) return null;
  const path = `/p/${encodeURIComponent(match[1])}/task/${match[2]}`;
  return el("a", {
    href: path,
    text: reference,
    onClick: (event) => {
      if (event.ctrlKey || event.metaKey || event.shiftKey || event.altKey) return;
      event.preventDefault();
      navigate(path);
    }
  });
}

export async function renderImports(region, projects, current, navigate) {
  const loading = el("p", { class: "muted", role: "status", text: t("imports.loading") });
  const page = el("div", { class: "page imports-page" }, [
    el("h1", { class: "page-title", text: t("imports.title") }),
    loading
  ]);
  region.replaceChildren(page);
  let providers;
  try {
    providers = await api.importProviders();
  } catch (error) {
    if (current()) loading.replaceWith(el("p", { class: "form-error", role: "alert", text: error.message }));
    return;
  }
  if (!current()) return;
  loading.remove();
  if (!providers.length) {
    page.append(el("p", { class: "muted", text: t("imports.unavailable") }));
    return;
  }
  const activeProjects = projects.filter((project) => !project.archived_at);
  if (!activeProjects.length) {
    page.append(el("p", { class: "muted", text: t("imports.noProjects") }));
    return;
  }
  const select = (label, placeholder, options) => {
    const input = el("select", { class: "input", "aria-label": label }, [
      el("option", { value: "", text: placeholder }), ...options
    ]);
    page.append(el("label", { class: "field" }, [el("span", { class: "field-label", text: label }), input]));
    return input;
  };
  const project = select(t("imports.project"), t("imports.selectProject"), activeProjects.map((project) =>
    el("option", { value: project.id, text: `${project.key} — ${project.name}` })
  ));
  const provider = select(t("imports.provider"), t("imports.selectProvider"), providers.map((provider) =>
    el("option", { value: provider.id, text: provider.name })
  ));
  const source = select(t("imports.source"), t("imports.selectSource"), []);
  const filtersBox = el("div");
  const results = el("div");
  const error = el("p", { class: "form-error", role: "alert" });
  const previewButton = el("button", {
    class: "button button-primary", type: "button", text: t("imports.preview"), disabled: true
  });
  page.append(filtersBox, previewButton, error, results);
  let generation = 0;
  let filters = [];
  function invalidate() {
    generation += 1;
    results.replaceChildren();
    error.textContent = "";
    previewButton.textContent = t("imports.preview");
    previewButton.disabled = !(project.value && provider.value && source.value);
  }
  function controls() {
    const selected = providers.find((providerInfo) => providerInfo.id === provider.value);
    source.replaceChildren(
      el("option", { value: "", text: t("imports.selectSource") }),
      ...(selected?.sources || []).map((sourceInfo) => el("option", { value: sourceInfo.id, text: sourceInfo.name }))
    );
    filters = (selected?.filters || []).map((filter) => ({
      id: filter.id,
      label: filter.label,
      input: el("input", { class: "input", type: "text", "aria-label": filter.label, onInput: invalidate })
    }));
    filtersBox.replaceChildren(...filters.map((filter) => el("label", { class: "field" }, [
      el("span", { class: "field-label", text: filter.label }), filter.input
    ])));
    invalidate();
  }
  project.addEventListener("change", invalidate);
  provider.addEventListener("change", controls);
  source.addEventListener("change", invalidate);
  previewButton.addEventListener("click", async () => {
    invalidate();
    const version = generation;
    const payload = {
      project_id: project.value,
      provider: provider.value,
      source: source.value,
      filters: Object.fromEntries(filters.map((filter) => [filter.id, filter.input.value]))
    };
    previewButton.disabled = true;
    previewButton.textContent = t("imports.previewing");
    try {
      const data = await api.importPreview(payload);
      if (!current() || generation !== version) return;
      const selected = new Set();
      const checkboxes = [];
      let importing = false;
      const confirm = el("button", {
        type: "button", class: "button button-primary", text: t("imports.confirm"), disabled: true
      });
      const list = el("div", { class: "settings-list" });
      for (const item of data.candidates) {
        const candidate = item.candidate;
        const checkbox = el("input", {
          type: "checkbox", disabled: !item.valid, "aria-label": t("imports.select", { title: candidate.title })
        });
        checkbox.addEventListener("change", () => {
          if (checkbox.checked) selected.add(item.index);
          else selected.delete(item.index);
          confirm.disabled = importing || !selected.size;
        });
        checkboxes.push({ checkbox, valid: item.valid });
        list.append(el("article", { class: "settings-row import-candidate" }, [
          checkbox,
          el("div", { class: "settings-row-main" }, [
            el("strong", { text: candidate.title }),
            el("p", { text: `${payload.provider} · ${candidate.kind}` }),
            sourceLink(candidate.source_url),
            item.existing ? el("p", {}, [
              document.createTextNode(t("imports.existing", { reference: "" })), taskLink(item.existing, navigate)
            ]) : null,
            !item.valid ? el("p", { class: "form-error", text: t("imports.invalid") }) : null
          ])
        ]));
      }
      const outcome = el("div", { role: "status" });
      confirm.addEventListener("click", async () => {
        if (importing || !selected.size) return;
        importing = true;
        error.textContent = "";
        confirm.disabled = true;
        confirm.textContent = t("imports.importing");
        for (const { checkbox } of checkboxes) checkbox.disabled = true;
        try {
          const response = await api.importConfirm({
            preview_id: data.preview_id, project_id: payload.project_id,
            provider: payload.provider, source: payload.source, selected: [...selected]
          });
          if (!current() || generation !== version) return;
          const statuses = { created: t("imports.created"), skipped: t("imports.skipped"), failed: t("imports.failed") };
          outcome.replaceChildren(...response.map((result) => {
            const item = data.candidates.find((candidate) => candidate.index === result.index);
            return el("p", {}, [
              document.createTextNode(`${item.candidate.title}: ${statuses[result.status] || t("imports.unknownOutcome")} `),
              taskLink(result.reference, navigate),
              document.createTextNode(result.message ? ` — ${result.message}` : "")
            ]);
          }));
        } catch (failure) {
          if (current() && generation === version) error.textContent = failure.message;
        } finally {
          importing = false;
          if (current() && generation === version) {
            for (const { checkbox, valid } of checkboxes) checkbox.disabled = !valid;
            confirm.disabled = !selected.size;
            confirm.textContent = t("imports.confirm");
          }
        }
      });
      results.replaceChildren(
        el("h2", { text: t("imports.preview") }),
        data.candidates.length ? list : el("p", { text: t("imports.empty") }),
        confirm, outcome
      );
    } catch (failure) {
      if (current() && generation === version) error.textContent = failure.message;
    } finally {
      if (current() && generation === version) {
        previewButton.disabled = false;
        previewButton.textContent = t("imports.preview");
      }
    }
  });
}
