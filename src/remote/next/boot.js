// Starts the page. A file of its own rather than inline, so the page's
// Content-Security-Policy needs no exception for inline scripts.
import init from "./arko-web.js";

init({ module_or_path: new URL("arko-web_bg.wasm", import.meta.url) });
