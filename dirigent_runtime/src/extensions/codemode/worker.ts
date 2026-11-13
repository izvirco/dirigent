// The codemode sandbox's worker, for the compiled runtime. A compiled pi spawns it by this
// path from the bundle's root (`getCodemodeWorkerSpecifier` in pi's config), so build.ts
// embeds this file as an extra entrypoint at that place. Not exported, hence the path.
import "../../../node_modules/@earendil-works/pi-coding-agent/dist/extensions/codemode/worker.js";
