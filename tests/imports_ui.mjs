// Lightweight DOM exercise for the import workflow; no cloud service needed.
import assert from "node:assert/strict";

class FakeNode {
  constructor(tag = "#text") { this.tag = tag; this.children = []; this.listeners = {}; this.value = ""; this.textContent = ""; this.disabled = false; this.checked = false; this.dataset = {}; this.style = {}; }
  append(...children) { for (const child of children) child.parent = this; this.children.push(...children); }
  replaceChildren(...children) { this.children = []; if (this.tag === "select") this.value = ""; this.append(...children); }
  remove() { if (this.parent) this.parent.children = this.parent.children.filter((child) => child !== this); }
  replaceWith(node) { if (this.parent) { this.parent.children = this.parent.children.map((child) => child === this ? node : child); node.parent = this.parent; } }
  setAttribute(key, value) { this[key] = value; }
  addEventListener(name, fn) { (this.listeners[name] ||= []).push(fn); }
  async fire(name) { for (const fn of this.listeners[name] || []) await fn({ preventDefault() {} }); }
}
globalThis.Node = FakeNode;
globalThis.document = { createElement: (tag) => new FakeNode(tag), createTextNode: (text) => { const node = new FakeNode(); node.textContent = text; return node; } };
const { renderImports, sourceLink } = await import("../web/imports.js");
const walk = (node) => [node, ...node.children.flatMap(walk)];
const find = (root, predicate) => walk(root).find(predicate);
const calls = [];
let providers = [];
let pendingPreview = null;
let pendingConfirm = null;
let failure = false;
globalThis.fetch = async (path, options) => {
  calls.push({ path, payload: options.body && JSON.parse(options.body) });
  if (path.endsWith("preview") && pendingPreview) await pendingPreview;
  if (path.endsWith("confirm") && pendingConfirm) await pendingConfirm;
  if (failure) return { ok: false, status: 503, headers: { get: () => "application/json" }, json: async () => ({ error: { code: "provider_error", message: "Provider unavailable" } }) };
  const body = path.endsWith("providers") ? providers : path.endsWith("preview")
    ? { preview_id: "snapshot", candidates: [{ index: 0, valid: true, existing: null, candidate: { title: "<img onerror=alert(1)>", kind: "issue", source_url: "https://example.org/a" } }] }
    : [{ index: 0, status: "created", reference: "TST-1", message: null }];
  return { ok: true, status: 200, headers: { get: () => "application/json" }, json: async () => body };
};
const region = new FakeNode("main");
await renderImports(region, [], () => true, () => {});
assert.ok(find(region, (n) => n.textContent.includes("not yet available")));
assert.equal(sourceLink("javascript:alert(1)"), null);
assert.equal(sourceLink("https://user:password@example.org/"), null);
assert.equal(sourceLink("data:text/html,<script>alert(1)</script>"), null);
assert.equal(sourceLink("https://example.org/a").rel, "noopener noreferrer");
providers = [{ id: "fixture", name: "Fixture", sources: [{ id: "source", name: "Source" }], filters: [{ id: "query", label: "Query" }] }];
await renderImports(region, [{ id: "project", key: "TST", name: "Test" }], () => true, () => {});
const project = find(region, (n) => n["aria-label"] === "Destination project");
const provider = find(region, (n) => n["aria-label"] === "Provider");
const source = find(region, (n) => n["aria-label"] === "Source");
const preview = find(region, (n) => n.textContent === "Preview" && n.tag === "button");
assert.equal(preview.disabled, true);
project.value = "project"; await project.fire("change");
provider.value = "fixture"; await provider.fire("change");
source.value = "source"; await source.fire("change");
await preview.fire("click");
const checkbox = find(region, (n) => n.tag === "input" && n.type === "checkbox");
const confirm = find(region, (n) => n.textContent === "Import selected");
assert.equal(checkbox.checked, false);
assert.equal(confirm.disabled, true);
assert.ok(find(region, (n) => n.textContent === "<img onerror=alert(1)>"));
checkbox.checked = true; await checkbox.fire("change");
assert.equal(confirm.disabled, false);
let releaseConfirm;
pendingConfirm = new Promise((resolve) => { releaseConfirm = resolve; });
const confirming = confirm.fire("click");
assert.equal(confirm.disabled, true);
assert.equal(checkbox.disabled, true);
await confirm.fire("click");
assert.equal(calls.filter((call) => call.path.endsWith("confirm")).length, 1);
releaseConfirm(); await confirming; pendingConfirm = null;
assert.deepEqual(calls.at(-1).payload.selected, [0]);
assert.ok(find(region, (n) => n.textContent === "TST-1"));
failure = true;
await confirm.fire("click");
assert.ok(find(region, (n) => n.textContent === "Provider unavailable"));
failure = false;
await confirm.fire("click");
assert.equal(find(region, (n) => n.role === "alert").textContent, "");
const filter = find(region, (n) => n["aria-label"] === "Query");
filter.value = "changed"; await filter.fire("input");
assert.equal(find(region, (n) => n.textContent === "Import selected"), undefined);
let releasePreview;
pendingPreview = new Promise((resolve) => { releasePreview = resolve; });
const previewing = preview.fire("click");
source.value = ""; await source.fire("change");
releasePreview(); await previewing; pendingPreview = null;
assert.equal(find(region, (n) => n.textContent === "Import selected"), undefined);
assert.equal(preview.disabled, true);
source.value = "source"; await source.fire("change");
failure = true;
await preview.fire("click");
assert.ok(find(region, (n) => n.textContent === "Provider unavailable"));
await renderImports(region, [], () => true, () => {});
assert.ok(find(region, (n) => n.textContent === "Provider unavailable"));
failure = false;
await renderImports(region, [], () => true, () => {});
assert.ok(find(region, (n) => n.textContent.includes("Create an active project")));
console.log("imports UI workflow passed");
