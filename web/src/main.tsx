/**
 * Entry point.
 *
 * Route registration happens as a side effect of importing each page module, so
 * import order is route-matching order. The catch-all repo page must come last:
 * `compilePattern("*")` matches everything, and `matchRoute` returns the first
 * entry that matches.
 */
import { app } from "@toyz/loom";
import { LoomRouter } from "@toyz/loom/router";

// Built-ins are opt-in: importing them is what defines the custom elements, so
// nothing you do not use is shipped.
import "@toyz/loom/element/icon";
import "@toyz/loom/element/virtual";
import "./components/fkit-dialog";
import "./components/fkit-toggle";
import "./components/fkit-settings";
import "./components/fkit-tags";
import "./components/fkit-avatar";
import "./components/fkit-discussion";
import "./components/fkit-modal";
import "./components/fkit-label";
import "./components/fkit-notice";
import "./components/fkit-to-top";
import "./components/fkit-file-tree";
import "./icons";

import "./session";
import "./app";

// Fixed-arity routes, most specific first.
import "./pages/auth";
import "./pages/new-repo";
import "./pages/settings";
import "./pages/admin";
import "./pages/repos";

// `/:owner` must come after every fixed single-segment route above, or it
// swallows /settings and /admin. Those are reserved usernames too.
import "./pages/profile";

// Catch-all: /:owner/:repo and everything beneath it.
import "./pages/repo";

const router = new LoomRouter({ mode: "history" });
app.use(router);
app.start();

// Ask who this is, immediately.
//
// Deliberately not awaited: blocking here would hold back the shell as well,
// and a blank page is not an improvement on a header that says "…" for a
// moment. What matters is that the request is in flight before anything reads
// the answer, and that nothing draws a signed-in or signed-out state until it
// arrives -- "not known yet" is a third state, and every component that cares
// has to render it as one rather than treating it as signed out.
//
// The comment here used to claim this resolved before first paint. It did not,
// and the page that trusted it drew its signed-out front door at signed-in
// visitors.
const session = app.get<import("./session").Session>("session");
void session.load();
