// The active language lives behind this one module.
//
// To add French: copy `en.js` to `fr.js`, translate its `messages` values, and
// re-export `t` from `fr.js` here. Components import only this entry point.

export { t } from "./en.js";
