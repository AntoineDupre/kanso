// Normalise the CycloneDX document produced by `npm sbom` for the frontend.
//
// npm names the root component after the directory it runs in ("web"); Kanso's
// frontend package is `kanso-web`. The vendored `marked.min.js` is the npm
// package's own build file, so record its SHA-256 as the component hash used
// when the SBOM is scanned.
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";

const vendor = process.argv[2];
if (!vendor) {
  console.error("usage: npm sbom ... | node scripts/sbom-frontend.mjs <vendor-file>");
  process.exit(2);
}

let input = "";
process.stdin.setEncoding("utf8");
for await (const chunk of process.stdin) {
  input += chunk;
}

const bom = JSON.parse(input);
if (bom.bomFormat !== "CycloneDX") {
  console.error("sbom: npm did not produce a CycloneDX document");
  process.exit(1);
}

const root = bom.metadata?.component;
if (!root) {
  console.error("sbom: the frontend document has no root component");
  process.exit(1);
}
root.name = "kanso-web";
root.type = "application";

const marked = (bom.components ?? []).find((component) => component.name === "marked");
if (!marked) {
  console.error("sbom: the frontend document does not list marked");
  process.exit(1);
}
marked.hashes = [
  {
    alg: "SHA-256",
    content: createHash("sha256").update(readFileSync(vendor)).digest("hex"),
  },
];

process.stdout.write(`${JSON.stringify(bom, null, 2)}\n`);
