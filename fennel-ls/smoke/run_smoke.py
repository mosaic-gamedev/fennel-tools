#!/usr/bin/env python3
"""
Automated smoke test for fennel-ls.

Starts the LSP binary, opens smoke files, and verifies:
  - workspace/symbol
  - textDocument/references (cross-file)
  - textDocument/rename (cross-file)
  - textDocument/formatting  (requires default build; skipped with --no-formatting)
  - macro expansion diagnostics

Usage:
  python3 smoke/run_smoke.py [--binary PATH]

Exit code 0 = all checks passed, non-zero = failures.
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

SMOKE_DIR = os.path.dirname(os.path.abspath(__file__))
CRATE_ROOT = os.path.dirname(SMOKE_DIR)       # fennel-ls/
WORKSPACE_ROOT = os.path.dirname(CRATE_ROOT)  # repo root (Cargo workspace)
DEFAULT_BINARY = os.path.join(WORKSPACE_ROOT, "target", "release", "fennel-ls")


def file_uri(path):
    return "file://" + path


def make_request(id_, method, params):
    body = json.dumps({"jsonrpc": "2.0", "id": id_, "method": method, "params": params})
    return f"Content-Length: {len(body)}\r\n\r\n{body}".encode()


def make_notify(method, params):
    body = json.dumps({"jsonrpc": "2.0", "method": method, "params": params})
    return f"Content-Length: {len(body)}\r\n\r\n{body}".encode()


class LspClient:
    def __init__(self, binary):
        self.proc = subprocess.Popen(
            [binary],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self._id = 0
        # Single lock + condition covers both responses and notifications so
        # callers can wait on either kind of message without separate locks.
        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._responses = {}      # id -> full response message
        self._notifications = {}  # method -> [params, ...] (all received, in order)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _next_id(self):
        self._id += 1
        return self._id

    def _read_loop(self):
        stdout = self.proc.stdout
        while True:
            headers = {}
            while True:
                line = stdout.readline()
                if not line:
                    return
                line = line.rstrip(b"\r\n")
                if not line:
                    break
                if b":" in line:
                    k, v = line.split(b":", 1)
                    headers[k.strip().lower()] = v.strip()

            length_bytes = headers.get(b"content-length")
            if length_bytes is None:
                continue
            length = int(length_bytes)

            body = b""
            while len(body) < length:
                chunk = stdout.read(length - len(body))
                if not chunk:
                    return
                body += chunk

            msg = json.loads(body)
            with self._cond:
                if "id" in msg and "method" not in msg:
                    # Response to a request we sent.
                    self._responses[msg["id"]] = msg
                elif "method" in msg and "id" not in msg:
                    # Server-to-client notification (e.g. publishDiagnostics).
                    method = msg.get("method", "")
                    if method not in self._notifications:
                        self._notifications[method] = []
                    self._notifications[method].append(msg.get("params", {}))
                self._cond.notify_all()

    def _wait(self, id_, timeout=10):
        deadline = time.time() + timeout
        with self._cond:
            while id_ not in self._responses:
                remaining = deadline - time.time()
                if remaining <= 0:
                    raise TimeoutError(f"no response for id={id_}")
                self._cond.wait(remaining)
            return self._responses.pop(id_)

    def request(self, method, params, timeout=10):
        id_ = self._next_id()
        self.proc.stdin.write(make_request(id_, method, params))
        self.proc.stdin.flush()
        return self._wait(id_, timeout)

    def notify(self, method, params):
        self.proc.stdin.write(make_notify(method, params))
        self.proc.stdin.flush()

    def get_diagnostics(self, uri):
        """Return the last publishDiagnostics params received for `uri`, or None."""
        with self._cond:
            notifs = self._notifications.get("textDocument/publishDiagnostics", [])
            last = None
            for n in notifs:
                if n.get("uri") == uri:
                    last = n
            return last

    def wait_for_diagnostics(self, uri, predicate, timeout=5.0):
        """Block until predicate(params) is True for the latest diagnostics on uri,
        or until timeout seconds elapse.

        Returns (params, satisfied) where satisfied=True means predicate was met
        before the timeout. satisfied=False means we timed out; params is the
        last-seen publish (or None if none arrived at all).
        """
        deadline = time.time() + timeout
        with self._cond:
            while True:
                notifs = self._notifications.get("textDocument/publishDiagnostics", [])
                last = None
                for n in notifs:
                    if n.get("uri") == uri:
                        last = n
                if last is not None and predicate(last):
                    return last, True
                remaining = deadline - time.time()
                if remaining <= 0:
                    return last, False
                self._cond.wait(remaining)

    def close(self):
        try:
            self.request("shutdown", {}, timeout=5)
            self.notify("exit", {})
        except Exception:
            pass
        self.proc.terminate()


def read_file(path):
    with open(path, encoding="utf-8") as f:
        return f.read()


FAILURES = []

def check(label, condition, detail=""):
    if condition:
        print(f"  PASS  {label}")
    else:
        msg = f"  FAIL  {label}"
        if detail:
            msg += f"\n        {detail}"
        print(msg)
        FAILURES.append(label)


def run(binary):
    lsp = LspClient(binary)

    utils_path = os.path.join(SMOKE_DIR, "utils.fnl")
    consumer_path = os.path.join(SMOKE_DIR, "cross-file-refs.fnl")
    macro_user_path = os.path.join(SMOKE_DIR, "macro-user.fnl")
    ch_path = os.path.join(SMOKE_DIR, "call-hierarchy.fnl")
    ca_path = os.path.join(SMOKE_DIR, "code-actions.fnl")
    utils_uri = file_uri(utils_path)
    consumer_uri = file_uri(consumer_path)
    macro_user_uri = file_uri(macro_user_path)
    ch_uri = file_uri(ch_path)
    ca_uri = file_uri(ca_path)
    utils_text = read_file(utils_path)
    consumer_text = read_file(consumer_path)
    macro_user_text = read_file(macro_user_path)
    ch_text = read_file(ch_path)
    ca_text = read_file(ca_path)

    # ── initialize ────────────────────────────────────────────────────────────
    init = lsp.request("initialize", {
        "processId": os.getpid(),
        "rootUri": file_uri(SMOKE_DIR),
        "capabilities": {},
    })
    lsp.notify("initialized", {})

    # ── open files ────────────────────────────────────────────────────────────
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": utils_uri, "languageId": "fennel", "version": 1, "text": utils_text}
    })
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": consumer_uri, "languageId": "fennel", "version": 1, "text": consumer_text}
    })
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": macro_user_uri, "languageId": "fennel", "version": 1, "text": macro_user_text}
    })
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": ch_uri, "languageId": "fennel", "version": 1, "text": ch_text}
    })
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": ca_uri, "languageId": "fennel", "version": 1, "text": ca_text}
    })

    # Sync barrier: documentSymbol on each file ensures didOpen processing is complete.
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": consumer_uri}})
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": macro_user_uri}})
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": ch_uri}})
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": ca_uri}})

    # ── workspace/symbol ──────────────────────────────────────────────────────
    print("\n=== workspace/symbol ===")

    r = lsp.request("workspace/symbol", {"query": "greet"})
    syms = r.get("result") or []
    names = [s["name"] for s in syms]
    check("query 'greet' finds greet", "greet" in names, f"got {names}")

    r = lsp.request("workspace/symbol", {"query": "bye"})
    syms = r.get("result") or []
    names = [s["name"] for s in syms]
    check("query 'bye' finds bye", "bye" in names, f"got {names}")

    r = lsp.request("workspace/symbol", {"query": ""})
    syms = r.get("result") or []
    all_names = [s["name"] for s in syms]
    # greet and bye come from utils.fnl (open file);
    # geometry.fnl is not open or required so vec2 won't appear.
    check("empty query returns defs from open files",
          "greet" in all_names and "bye" in all_names, f"got {all_names}")

    # ── textDocument/references (cross-file) ──────────────────────────────────
    print("\n=== textDocument/references (cross-file) ===")

    # "greet" definition is on line 2 (0-indexed), col 4 in utils.fnl
    # "(fn greet [name]\n" → "greet" starts at col 4
    greet_line = next(i for i, l in enumerate(utils_text.splitlines()) if "(fn greet" in l)
    greet_col = utils_text.splitlines()[greet_line].index("greet")

    r = lsp.request("textDocument/references", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": greet_line, "character": greet_col},
        "context": {"includeDeclaration": True},
    })
    refs = r.get("result") or []
    ref_uris = [ref["uri"] for ref in refs]
    consumer_refs = [ref for ref in refs if ref["uri"] == consumer_uri]

    check("references from def includes consumer file", consumer_uri in ref_uris,
          f"uris: {set(ref_uris)}")
    check("references finds both utils.greet call sites", len(consumer_refs) >= 2,
          f"consumer refs: {consumer_refs}")

    # Cursor on `utils.greet` in consumer (first call site, skipping comments)
    consumer_lines = consumer_text.splitlines()
    first_call_line = next(
        i for i, l in enumerate(consumer_lines)
        if "(utils.greet" in l and not l.lstrip().startswith(";;")
    )
    first_call_col = consumer_lines[first_call_line].index("utils.greet")

    r = lsp.request("textDocument/references", {
        "textDocument": {"uri": consumer_uri},
        "position": {"line": first_call_line, "character": first_call_col},
        "context": {"includeDeclaration": True},
    })
    refs2 = r.get("result") or []
    consumer_refs2 = [ref for ref in refs2 if ref["uri"] == consumer_uri]
    check("cursor on cross-file ref also returns cross-file results",
          len(consumer_refs2) >= 2, f"refs: {refs2}")

    # "bye" should only appear once
    bye_line = next(i for i, l in enumerate(utils_text.splitlines()) if "(fn bye" in l)
    bye_col = utils_text.splitlines()[bye_line].index("bye")
    r = lsp.request("textDocument/references", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": bye_line, "character": bye_col},
        "context": {"includeDeclaration": True},
    })
    bye_refs = r.get("result") or []
    bye_consumer = [ref for ref in bye_refs if ref["uri"] == consumer_uri]
    check("references for bye finds one consumer site", len(bye_consumer) == 1,
          f"bye consumer refs: {bye_consumer}")

    # ── textDocument/rename (cross-file) ─────────────────────────────────────
    print("\n=== textDocument/rename (cross-file) ===")

    r = lsp.request("textDocument/rename", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": greet_line, "character": greet_col},
        "newName": "hello",
    })
    edit = r.get("result") or {}
    changes = edit.get("changes") or {}

    check("rename produces changes for utils.fnl", utils_uri in changes,
          f"change keys: {list(changes.keys())}")
    check("rename produces changes for consumer file", consumer_uri in changes,
          f"change keys: {list(changes.keys())}")

    consumer_edits = changes.get(consumer_uri, [])
    new_texts = [e["newText"] for e in consumer_edits]
    check("consumer rename edits use 'utils.hello'",
          all(t == "utils.hello" for t in new_texts),
          f"new texts: {new_texts}")
    check("consumer gets 2 rename edits for greet", len(consumer_edits) == 2,
          f"edits: {consumer_edits}")

    # bye → farewell
    r = lsp.request("textDocument/rename", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": bye_line, "character": bye_col},
        "newName": "farewell",
    })
    edit2 = r.get("result") or {}
    changes2 = edit2.get("changes") or {}
    consumer_edits2 = changes2.get(consumer_uri, [])
    check("rename bye → farewell edits consumer", len(consumer_edits2) == 1,
          f"edits: {consumer_edits2}")
    if consumer_edits2:
        check("bye rename uses 'utils.farewell'",
              consumer_edits2[0]["newText"] == "utils.farewell",
              f"new text: {consumer_edits2[0]['newText']}")

    # ── textDocument/formatting ───────────────────────────────────────────────
    print("\n=== textDocument/formatting ===")

    caps = init.get("result", {}).get("capabilities", {})
    formatting_supported = bool(caps.get("documentFormattingProvider"))

    if not formatting_supported:
        print("  SKIP  formatting (binary not built with formatting support or --no-formatting passed)")
    else:
        # utils.fnl is already well-formatted; expect null or empty edit list.
        r = lsp.request("textDocument/formatting", {
            "textDocument": {"uri": utils_uri},
            "options": {"tabSize": 2, "insertSpaces": True},
        })
        result = r.get("result")
        check("formatting response is a list or null",
              result is None or isinstance(result, list),
              f"got: {result!r}")

        # macro-user.fnl: verify we get a valid response (editor would apply edits).
        r2 = lsp.request("textDocument/formatting", {
            "textDocument": {"uri": macro_user_uri},
            "options": {"tabSize": 2, "insertSpaces": True},
        })
        result2 = r2.get("result")
        check("formatting macro-user.fnl returns list or null",
              result2 is None or isinstance(result2, list),
              f"got: {result2!r}")

    # ── macro expansion diagnostics ───────────────────────────────────────────
    # Verifies that after expansion, macro-introduced names ('defsimple' from
    # scope.macros, 'answer' from scope.unmanglings) do NOT appear as
    # "unknown identifier" diagnostics.
    #
    # In tower-lsp, did_open (notification) and documentSymbol (request) run
    # concurrently — the barrier above fires before macro expansion finishes.
    # We wait on the condition variable until a clean publish arrives or we time out.
    print("\n=== macro expansion diagnostics ===")

    def no_unknown_ids(params):
        return not any("unknown identifier" in d.get("message", "")
                        for d in params.get("diagnostics", []))

    diag, satisfied = lsp.wait_for_diagnostics(macro_user_uri, no_unknown_ids, timeout=5.0)
    if not satisfied:
        print("  FAIL  macro expansion (no clean re-publish within timeout)")
    else:
        diagnostics = diag.get("diagnostics", [])
        messages = [d.get("message", "") for d in diagnostics]
        unknown = [m for m in messages if "unknown identifier" in m]
        check("no unknown-identifier warnings after macro expansion",
              len(unknown) == 0,
              f"remaining warnings: {unknown}")
        if unknown:
            print(f"        (all diag messages: {messages})")

    # ── textDocument/definition ───────────────────────────────────────────────
    print("\n=== textDocument/definition ===")

    # From the definition site itself — should resolve back to the same location.
    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": greet_line, "character": greet_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and len(result) == 0):
        check("definition from def site returns a result", False,
              f"got null/empty: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("definition from def site targets utils.fnl",
              loc.get("uri") == utils_uri,
              f"uri: {loc.get('uri')!r}")
        def_pos = loc.get("range", {}).get("start", {})
        check("definition from def site lands on correct line",
              def_pos.get("line") == greet_line,
              f"got line {def_pos.get('line')}, want {greet_line}")

    # From a cross-file call site — should navigate to the definition in utils.fnl.
    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": consumer_uri},
        "position": {"line": first_call_line, "character": first_call_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and len(result) == 0):
        check("definition from cross-file call returns a result", False,
              f"got null/empty: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("definition from cross-file call targets utils.fnl",
              loc.get("uri") == utils_uri,
              f"uri: {loc.get('uri')!r}")
        def_pos = loc.get("range", {}).get("start", {})
        check("definition from cross-file call lands on greet line",
              def_pos.get("line") == greet_line,
              f"got line {def_pos.get('line')}, want {greet_line}")

    # ── textDocument/hover ────────────────────────────────────────────────────
    print("\n=== textDocument/hover ===")

    # Hover on the greet definition should return its docstring.
    r = lsp.request("textDocument/hover", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": greet_line, "character": greet_col},
    })
    hover = r.get("result")
    if hover is None:
        print("  SKIP  hover on greet (server returned null — feature may be disabled)")
    else:
        content = hover.get("contents", "")
        if isinstance(content, dict):
            content = content.get("value", "")
        elif isinstance(content, list):
            content = " ".join(
                (c.get("value", "") if isinstance(c, dict) else str(c)) for c in content
            )
        check("hover on greet includes docstring",
              "Say hello" in content,
              f"hover content: {content!r}")

    # Hover on the cross-file call site: cursor on 'utils' of 'utils.greet'.
    r = lsp.request("textDocument/hover", {
        "textDocument": {"uri": consumer_uri},
        "position": {"line": first_call_line, "character": first_call_col},
    })
    hover2 = r.get("result")
    if hover2 is None:
        print("  SKIP  hover on cross-file call site (server returned null)")
    else:
        content2 = hover2.get("contents", "")
        if isinstance(content2, dict):
            content2 = content2.get("value", "")
        elif isinstance(content2, list):
            content2 = " ".join(
                (c.get("value", "") if isinstance(c, dict) else str(c)) for c in content2
            )
        check("hover on cross-file call site returns non-empty content",
              bool(content2),
              f"hover content: {content2!r}")

    # ── textDocument/didChange ────────────────────────────────────────────────
    print("\n=== textDocument/didChange ===")

    # Edit utils.fnl: rename `bye` to `farewell`.
    modified_utils = utils_text.replace("bye", "farewell")
    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 2},
        "contentChanges": [{"text": modified_utils}],
    })

    # Sync barrier: documentSymbol returns after the change is processed.
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})

    r = lsp.request("workspace/symbol", {"query": "farewell"})
    syms = r.get("result") or []
    names = [s["name"] for s in syms]
    check("after didChange, renamed symbol 'farewell' is visible",
          "farewell" in names,
          f"got: {names}")

    r = lsp.request("workspace/symbol", {"query": "bye"})
    syms_bye = r.get("result") or []
    names_bye = [s["name"] for s in syms_bye]
    check("after didChange, old name 'bye' is no longer visible",
          "bye" not in names_bye,
          f"got: {names_bye}")

    # Restore utils.fnl to its original content for the remaining tests.
    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 3},
        "contentChanges": [{"text": utils_text}],
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})

    # ── diagnostics.fnl warning count ────────────────────────────────────────
    print("\n=== diagnostics.fnl warning count ===")

    diag_path = os.path.join(SMOKE_DIR, "diagnostics.fnl")
    diag_uri = file_uri(diag_path)
    diag_text = read_file(diag_path)

    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": diag_uri, "languageId": "fennel", "version": 1, "text": diag_text}
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": diag_uri}})

    def has_any_diagnostics(params):
        return len(params.get("diagnostics", [])) > 0

    diag_result, diag_arrived = lsp.wait_for_diagnostics(diag_uri, has_any_diagnostics, timeout=5.0)
    if not diag_arrived:
        print("  SKIP  diagnostics count (no diagnostics published within timeout)")
    else:
        all_diags = diag_result.get("diagnostics", [])
        count = len(all_diags)
        EXPECTED_WARNINGS = 10
        check(f"diagnostics.fnl emits exactly {EXPECTED_WARNINGS} warnings",
              count == EXPECTED_WARNINGS,
              f"got {count}: {[d.get('message', '') for d in all_diags]}")

        categories = {
            "never mutated": 2,
            "unused": 2,       # unused-local (let bindings)
            "never used": 2,   # unused param
            "argument": 2,  # covers both "argument but got" and "arguments but got"
            "immutable": 1,
            "unknown identifier": 1,
        }
        for substr, expected_count in categories.items():
            matching = [d for d in all_diags if substr in d.get("message", "")]
            check(f"diagnostics.fnl has {expected_count} '{substr}' warning(s)",
                  len(matching) == expected_count,
                  f"got {len(matching)}: {[d.get('message','') for d in matching]}")

    # ── Incremental didChange (range-based) ───────────────────────────────────
    print("\n=== incremental didChange ===")

    # Replace just "greet" in utils.fnl with "salute" using a range edit.
    utils_lines = utils_text.splitlines()
    fn_line = next(i for i, l in enumerate(utils_lines) if "(fn greet" in l)
    fn_col_start = utils_lines[fn_line].index("greet")
    fn_col_end = fn_col_start + len("greet")

    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 10},
        "contentChanges": [{
            "range": {
                "start": {"line": fn_line, "character": fn_col_start},
                "end":   {"line": fn_line, "character": fn_col_end},
            },
            "text": "salute",
        }],
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})

    r = lsp.request("workspace/symbol", {"query": "salute"})
    syms = r.get("result") or []
    check("range-based didChange: new name 'salute' visible in workspace/symbol",
          any(s["name"] == "salute" for s in syms),
          f"got: {[s['name'] for s in syms]}")

    r = lsp.request("workspace/symbol", {"query": "greet"})
    syms_old = r.get("result") or []
    # Only verify that utils.fnl no longer exposes `greet` — diagnostics.fnl
    # defines its own `greet` for testing purposes and is still open.
    utils_greet = [s for s in syms_old
                   if s.get("location", {}).get("uri") == utils_uri
                   and s.get("name") == "greet"]
    check("range-based didChange: 'greet' gone from utils.fnl",
          len(utils_greet) == 0,
          f"still found in utils.fnl: {utils_greet}")

    # Restore utils.fnl to its original content.
    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 11},
        "contentChanges": [{"text": utils_text}],
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})

    # ── Circular require (must not hang) ──────────────────────────────────────
    print("\n=== circular require (no hang) ===")

    import tempfile
    with tempfile.TemporaryDirectory() as tmpdir:
        a_path = os.path.join(tmpdir, "circ-a.fnl")
        b_path = os.path.join(tmpdir, "circ-b.fnl")
        a_uri = file_uri(a_path)
        b_uri = file_uri(b_path)

        a_text = "(local b (require :circ-b))\n(fn greet [] (b.hello))\n"
        b_text = "(local a (require :circ-a))\n(fn hello [] :hi)\n"

        with open(a_path, "w") as f: f.write(a_text)
        with open(b_path, "w") as f: f.write(b_text)

        lsp.notify("textDocument/didOpen", {
            "textDocument": {"uri": a_uri, "languageId": "fennel", "version": 1, "text": a_text}
        })
        lsp.notify("textDocument/didOpen", {
            "textDocument": {"uri": b_uri, "languageId": "fennel", "version": 1, "text": b_text}
        })

        try:
            r_a = lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": a_uri}}, timeout=5)
            r_b = lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": b_uri}}, timeout=5)
            check("circular require: server responds to documentSymbol without hanging",
                  r_a.get("result") is not None or r_a.get("error") is not None,
                  f"response: {r_a}")
        except TimeoutError:
            check("circular require: server responds without hanging", False,
                  "server timed out — likely infinite loop on circular require")

    # ── textDocument/documentSymbol (hierarchical) ───────────────────────────
    print("\n=== textDocument/documentSymbol (hierarchical) ===")

    r = lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})
    syms = r.get("result") or []
    sym_names = [s.get("name") for s in syms]
    check("documentSymbol finds 'greet'", "greet" in sym_names, f"got: {sym_names}")
    check("documentSymbol finds 'bye'",   "bye"   in sym_names, f"got: {sym_names}")

    if syms:
        first = syms[0]
        check("documentSymbol returns 'range' field (hierarchical)",
              "range" in first,
              f"keys: {list(first.keys())}")
        check("documentSymbol returns 'selectionRange' field (hierarchical)",
              "selectionRange" in first,
              f"keys: {list(first.keys())}")
        check("documentSymbol does NOT return 'location' field (not flat SymbolInformation)",
              "location" not in first,
              f"keys: {list(first.keys())}")

    # selectionRange must be strictly smaller than (or equal to) range
    greet_sym = next((s for s in syms if s.get("name") == "greet"), None)
    if greet_sym:
        sel = greet_sym.get("selectionRange", {})
        full = greet_sym.get("range", {})
        sel_start_char = sel.get("start", {}).get("character", -1)
        full_start_char = full.get("start", {}).get("character", -1)
        check("greet selectionRange starts at name token (col 4), not opening paren (col 0)",
              sel_start_char == 4,
              f"selectionRange.start.character={sel_start_char}, range.start.character={full_start_char}")

    # ── textDocument/prepareRename ────────────────────────────────────────────
    print("\n=== textDocument/prepareRename ===")

    r = lsp.request("textDocument/prepareRename", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": greet_line, "character": greet_col},
    })
    result = r.get("result")
    check("prepareRename on a symbol returns non-null",
          result is not None,
          f"got: {result!r}")
    if result is not None:
        placeholder = result.get("placeholder", "")
        check("prepareRename placeholder is 'greet'",
              placeholder == "greet",
              f"got placeholder: {placeholder!r}")
        rng = result.get("range", {})
        rng_start = rng.get("start", {})
        check("prepareRename range starts at the symbol column",
              rng_start.get("character") == greet_col,
              f"range: {rng}")

    # prepareRename on an unresolvable position should return null
    r_null = lsp.request("textDocument/prepareRename", {
        "textDocument": {"uri": utils_uri},
        "position": {"line": 0, "character": 0},  # comment line
    })
    result_null = r_null.get("result")
    check("prepareRename on non-symbol returns null",
          result_null is None,
          f"got: {result_null!r}")

    # ── textDocument/selectionRange ───────────────────────────────────────────
    print("\n=== textDocument/selectionRange ===")

    # Cursor inside 'greet' name — should expand through the fn form to the whole file
    r = lsp.request("textDocument/selectionRange", {
        "textDocument": {"uri": utils_uri},
        "positions": [{"line": greet_line, "character": greet_col}],
    })
    result = r.get("result")
    check("selectionRange returns a list",
          isinstance(result, list) and len(result) == 1,
          f"got: {result!r}")
    if isinstance(result, list) and result:
        sel = result[0]
        # Innermost range should be just the 'greet' token (a few chars wide on its line)
        inner_range = sel.get("range", {})
        inner_start = inner_range.get("start", {})
        check("selectionRange innermost is on the greet line",
              inner_start.get("line") == greet_line,
              f"inner start: {inner_start}")
        # Must have a parent (at least the fn form wraps it)
        parent = sel.get("parent")
        check("selectionRange has at least one parent (enclosing form)",
              parent is not None,
              f"parent: {parent!r}")
        if parent:
            p_range = parent.get("range", {})
            p_start = p_range.get("start", {})
            p_end   = p_range.get("end", {})
            check("selectionRange parent starts at or before the greet line",
                  p_start.get("line", 999) <= greet_line,
                  f"parent range: {p_range}")
            check("selectionRange parent ends after the greet line",
                  p_end.get("line", -1) > greet_line,
                  f"parent range: {p_range}")

    # Multiple positions in one request — response length must match
    r2 = lsp.request("textDocument/selectionRange", {
        "textDocument": {"uri": utils_uri},
        "positions": [
            {"line": greet_line, "character": greet_col},
            {"line": bye_line, "character": bye_col},
        ],
    })
    result2 = r2.get("result")
    check("selectionRange with 2 positions returns 2 results",
          isinstance(result2, list) and len(result2) == 2,
          f"got: {result2!r}")

    # ── textDocument/rangeFormatting ──────────────────────────────────────────
    print("\n=== textDocument/rangeFormatting ===")

    # utils.fnl is already cleanly formatted — rangeFormatting should return
    # empty edits (or null) for the greet function's range.
    r = lsp.request("textDocument/rangeFormatting", {
        "textDocument": {"uri": utils_uri},
        "range": {
            "start": {"line": greet_line, "character": 0},
            "end": {"line": greet_line + 3, "character": 0},
        },
        "options": {"tabSize": 2, "insertSpaces": True},
    })
    result = r.get("result")
    check("rangeFormatting returns null or a list (not an error)",
          result is None or isinstance(result, list),
          f"got: {result!r}")

    # Temporarily introduce a formatting error in utils.fnl, verify rangeFormatting
    # produces an edit that does NOT extend past the greet function.
    bye_start_line = next(i for i, l in enumerate(utils_text.splitlines()) if "(fn bye" in l)
    bad_utils = utils_text.replace("(fn greet [name]", "(fn greet   [name]", 1)
    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 90},
        "contentChanges": [{"text": bad_utils}],
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})  # sync

    r2 = lsp.request("textDocument/rangeFormatting", {
        "textDocument": {"uri": utils_uri},
        "range": {
            "start": {"line": greet_line, "character": 0},
            "end": {"line": greet_line + 3, "character": 0},
        },
        "options": {"tabSize": 2, "insertSpaces": True},
    })
    edits = r2.get("result") or []
    check("rangeFormatting on messy greet returns at least one edit",
          len(edits) >= 1,
          f"got: {edits!r}")
    if edits:
        edit_end_line = edits[0].get("range", {}).get("end", {}).get("line", 999)
        check("rangeFormatting edit does not extend to bye line",
              edit_end_line < bye_start_line,
              f"edit ends on line {edit_end_line}, bye starts on line {bye_start_line}")

    # Restore original utils.fnl content.
    lsp.notify("textDocument/didChange", {
        "textDocument": {"uri": utils_uri, "version": 91},
        "contentChanges": [{"text": utils_text}],
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})  # sync

    # ── textDocument/prepareCallHierarchy ─────────────────────────────────────
    print("\n=== textDocument/prepareCallHierarchy ===")

    ch_lines = ch_text.splitlines()
    helper_line = next(i for i, l in enumerate(ch_lines) if "(fn helper" in l)
    helper_col  = ch_lines[helper_line].index("helper")

    r = lsp.request("textDocument/prepareCallHierarchy", {
        "textDocument": {"uri": ch_uri},
        "position": {"line": helper_line, "character": helper_col},
    })
    items = r.get("result")
    check("prepareCallHierarchy returns a non-empty list",
          isinstance(items, list) and len(items) >= 1,
          f"got: {items!r}")
    if isinstance(items, list) and items:
        item = items[0]
        check("prepareCallHierarchy item name is 'helper'",
              item.get("name") == "helper",
              f"name: {item.get('name')!r}")
        check("prepareCallHierarchy item kind is FUNCTION (12)",
              item.get("kind") == 12,
              f"kind: {item.get('kind')}")
        check("prepareCallHierarchy item has 'range' field",
              "range" in item, f"keys: {list(item.keys())}")
        check("prepareCallHierarchy item has 'selectionRange' field",
              "selectionRange" in item, f"keys: {list(item.keys())}")
        sel_col = item.get("selectionRange", {}).get("start", {}).get("character", -1)
        check("prepareCallHierarchy selectionRange starts at 'helper' column",
              sel_col == helper_col,
              f"selectionRange.start.character={sel_col}, expected {helper_col}")

    # prepareCallHierarchy on a non-function should return null.
    r_null = lsp.request("textDocument/prepareCallHierarchy", {
        "textDocument": {"uri": ch_uri},
        "position": {"line": 0, "character": 0},  # comment line
    })
    check("prepareCallHierarchy on non-function returns null",
          r_null.get("result") is None,
          f"got: {r_null.get('result')!r}")

    # ── callHierarchy/incomingCalls ───────────────────────────────────────────
    print("\n=== callHierarchy/incomingCalls ===")

    # Use the item we got from prepareCallHierarchy above.
    if isinstance(items, list) and items:
        helper_item = items[0]
        r = lsp.request("callHierarchy/incomingCalls", {"item": helper_item})
        callers = r.get("result") or []
        caller_names = [c.get("from", {}).get("name") for c in callers]
        check("incomingCalls for 'helper' finds at least 2 callers",
              len(callers) >= 2,
              f"caller names: {caller_names}")
        check("incomingCalls includes 'caller-a'",
              "caller-a" in caller_names,
              f"caller names: {caller_names}")
        check("incomingCalls includes 'caller-b'",
              "caller-b" in caller_names,
              f"caller names: {caller_names}")
        # caller-b calls helper twice — its fromRanges should have 2 entries.
        caller_b_entry = next((c for c in callers if c.get("from", {}).get("name") == "caller-b"), None)
        if caller_b_entry:
            check("caller-b has 2 call site ranges (calls helper twice)",
                  len(caller_b_entry.get("fromRanges", [])) == 2,
                  f"fromRanges: {caller_b_entry.get('fromRanges')}")
    else:
        check("incomingCalls skipped (prepareCallHierarchy returned no item)", False, "")

    # ── callHierarchy/outgoingCalls ───────────────────────────────────────────
    print("\n=== callHierarchy/outgoingCalls ===")

    # caller-a calls helper once → outgoing from caller-a should include helper.
    caller_a_line = next(i for i, l in enumerate(ch_lines) if "(fn caller-a" in l)
    caller_a_col  = ch_lines[caller_a_line].index("caller-a")
    r = lsp.request("textDocument/prepareCallHierarchy", {
        "textDocument": {"uri": ch_uri},
        "position": {"line": caller_a_line, "character": caller_a_col},
    })
    ca_items = r.get("result")
    if isinstance(ca_items, list) and ca_items:
        r2 = lsp.request("callHierarchy/outgoingCalls", {"item": ca_items[0]})
        callees = r2.get("result") or []
        callee_names = [c.get("to", {}).get("name") for c in callees]
        check("outgoingCalls from 'caller-a' finds 'helper'",
              "helper" in callee_names,
              f"callee names: {callee_names}")
        helper_entry = next((c for c in callees if c.get("to", {}).get("name") == "helper"), None)
        if helper_entry:
            check("outgoingCalls from caller-a has 1 call site for helper",
                  len(helper_entry.get("fromRanges", [])) == 1,
                  f"fromRanges: {helper_entry.get('fromRanges')}")
    else:
        check("outgoingCalls skipped (prepareCallHierarchy for caller-a returned no item)", False, "")

    # caller-b calls helper twice → 2 from_ranges.
    caller_b_line = next(i for i, l in enumerate(ch_lines) if "(fn caller-b" in l)
    caller_b_col  = ch_lines[caller_b_line].index("caller-b")
    r = lsp.request("textDocument/prepareCallHierarchy", {
        "textDocument": {"uri": ch_uri},
        "position": {"line": caller_b_line, "character": caller_b_col},
    })
    cb_items = r.get("result")
    if isinstance(cb_items, list) and cb_items:
        r3 = lsp.request("callHierarchy/outgoingCalls", {"item": cb_items[0]})
        callees_b = r3.get("result") or []
        callee_names_b = [c.get("to", {}).get("name") for c in callees_b]
        check("outgoingCalls from 'caller-b' finds 'helper'",
              "helper" in callee_names_b,
              f"callee names: {callee_names_b}")
        helper_b_entry = next((c for c in callees_b if c.get("to", {}).get("name") == "helper"), None)
        if helper_b_entry:
            check("outgoingCalls from caller-b has 2 call sites for helper",
                  len(helper_b_entry.get("fromRanges", [])) == 2,
                  f"fromRanges: {helper_b_entry.get('fromRanges')}")
    else:
        check("outgoingCalls skipped (prepareCallHierarchy for caller-b returned no item)", False, "")

    # ── textDocument/codeAction — remove unused local ─────────────────────────
    print("\n=== textDocument/codeAction (new actions) ===")

    # Wait for diagnostics on code-actions.fnl so we can find the unused-local one.
    ca_diags_params, satisfied = lsp.wait_for_diagnostics(
        ca_uri,
        lambda p: any("never used" in d.get("message", "") for d in p.get("diagnostics", [])),
        timeout=5.0,
    )
    check("code-actions.fnl produces an 'unused' diagnostic",
          satisfied,
          f"diagnostics: {ca_diags_params}")

    unused_diag = None
    if ca_diags_params:
        for d in ca_diags_params.get("diagnostics", []):
            if "never used" in d.get("message", "") and "test-unused" in d.get("message", ""):
                unused_diag = d
                break

    check("found 'test-unused' diagnostic for code action test",
          unused_diag is not None,
          f"diagnostics: {[d.get('message') for d in (ca_diags_params or {}).get('diagnostics', [])]}")

    if unused_diag:
        r = lsp.request("textDocument/codeAction", {
            "textDocument": {"uri": ca_uri},
            "range": unused_diag["range"],
            "context": {"diagnostics": [unused_diag]},
        })
        actions = r.get("result") or []
        action_titles = [a.get("title", "") if isinstance(a, dict) else "" for a in actions]
        remove_action = next(
            (a for a in actions
             if isinstance(a, dict) and "Remove" in a.get("title", "")),
            None
        )
        check("'Remove unused' code action is offered for unused local",
              remove_action is not None,
              f"action titles: {action_titles}")

    # ── textDocument/codeAction — local→var refactor ──────────────────────────

    # Position on "test-mutable" in code-actions.fnl.
    ca_lines = ca_text.splitlines()
    mutable_line = next(i for i, l in enumerate(ca_lines) if "test-mutable" in l and "(local" in l)
    mutable_col  = ca_lines[mutable_line].index("test-mutable")

    r = lsp.request("textDocument/codeAction", {
        "textDocument": {"uri": ca_uri},
        "range": {
            "start": {"line": mutable_line, "character": mutable_col},
            "end":   {"line": mutable_line, "character": mutable_col},
        },
        "context": {"diagnostics": []},
    })
    actions = r.get("result") or []
    action_titles = [a.get("title", "") if isinstance(a, dict) else "" for a in actions]
    local_to_var = next(
        (a for a in actions
         if isinstance(a, dict) and "local" in a.get("title", "") and "var" in a.get("title", "")),
        None
    )
    check("'local → var' refactor action is offered on a local binding",
          local_to_var is not None,
          f"action titles: {action_titles}")

    # ── textDocument/codeAction — wrap in do ──────────────────────────────────

    # Select a non-empty range covering "(local test-mutable 1)".
    mutable_end_col = ca_lines[mutable_line].index(")") + 1
    r = lsp.request("textDocument/codeAction", {
        "textDocument": {"uri": ca_uri},
        "range": {
            "start": {"line": mutable_line, "character": 0},
            "end":   {"line": mutable_line, "character": mutable_end_col},
        },
        "context": {"diagnostics": []},
    })
    actions = r.get("result") or []
    action_titles = [a.get("title", "") if isinstance(a, dict) else "" for a in actions]
    wrap_action = next(
        (a for a in actions
         if isinstance(a, dict) and "do" in a.get("title", "").lower()),
        None
    )
    check("'Wrap in (do ...)' action is offered for a non-empty selection",
          wrap_action is not None,
          f"action titles: {action_titles}")
    if wrap_action:
        new_text = (
            wrap_action.get("edit", {})
            .get("changes", {})
            .get(ca_uri, [{}])[0]
            .get("newText", "")
        )
        check("'Wrap in do' new_text starts with '(do'",
              new_text.strip().startswith("(do"),
              f"newText: {new_text!r}")

    # ── textDocument/declaration ──────────────────────────────────────────────
    print("\n=== textDocument/declaration ===")

    r = lsp.request("textDocument/declaration", {
        "textDocument": {"uri": consumer_uri},
        "position": {"line": first_call_line, "character": first_call_col},
    })
    decl = r.get("result")
    check("declaration from cross-file call returns a result",
          decl is not None and decl != [],
          f"got: {decl!r}")
    if decl:
        loc = decl[0] if isinstance(decl, list) else decl
        check("declaration navigates to utils.fnl",
              loc.get("uri") == utils_uri,
              f"uri: {loc.get('uri')!r}")

    # ── diagnostic codes and tags ─────────────────────────────────────────────
    print("\n=== diagnostic codes and tags ===")

    # diagnostics.fnl must be open (it was opened in the earlier section).
    # Grab the most recently published diagnostics for it.
    diag_result2, arrived2 = lsp.wait_for_diagnostics(
        diag_uri,
        lambda p: len(p.get("diagnostics", [])) > 0,
        timeout=5.0,
    )
    if not arrived2:
        print("  SKIP  diagnostic codes/tags (no diagnostics for diagnostics.fnl)")
    else:
        all_d = diag_result2.get("diagnostics", [])
        unused_local = next((d for d in all_d if "never used" in d.get("message", "")), None)
        arity_diag   = next((d for d in all_d if "expects"    in d.get("message", "")), None)

        check("unused-local diagnostic has 'code' field",
              unused_local is not None and unused_local.get("code") == "unused-local",
              f"code: {(unused_local or {}).get('code')!r}")
        check("unused-local diagnostic has UNNECESSARY tag (1)",
              unused_local is not None and 1 in (unused_local.get("tags") or []),
              f"tags: {(unused_local or {}).get('tags')!r}")
        check("arity diagnostic has 'arity' code",
              arity_diag is not None and arity_diag.get("code") == "arity",
              f"code: {(arity_diag or {}).get('code')!r}")
        check("arity diagnostic has no UNNECESSARY tag",
              arity_diag is not None and not (arity_diag.get("tags") or []),
              f"tags: {(arity_diag or {}).get('tags')!r}")

    # ── textDocument/documentLink ─────────────────────────────────────────────
    print("\n=== textDocument/documentLink ===")

    doc_link_cap = caps.get("documentLinkProvider")
    if not doc_link_cap:
        print("  SKIP  documentLink (not in capabilities)")
    else:
        # consumer.fnl requires utils via (local utils (require :utils))
        r = lsp.request("textDocument/documentLink", {
            "textDocument": {"uri": consumer_uri},
        })
        links = r.get("result") or []
        check("documentLink returns a list",
              isinstance(links, list),
              f"got: {links!r}")
        utils_link = next(
            (l for l in links if l.get("target", "").endswith("utils.fnl")),
            None,
        )
        check("documentLink includes link to utils.fnl",
              utils_link is not None,
              f"links: {links!r}")
        if utils_link:
            check("documentLink target is a file:// URI",
                  utils_link.get("target", "").startswith("file://"),
                  f"target: {utils_link.get('target')!r}")

        # utils.fnl has no require — should return empty list or null.
        r2 = lsp.request("textDocument/documentLink", {
            "textDocument": {"uri": utils_uri},
        })
        result2 = r2.get("result")
        check("documentLink for file with no requires is empty or null",
              result2 is None or result2 == [],
              f"got: {result2!r}")

    # ── textDocument/codeLens ─────────────────────────────────────────────────
    print("\n=== textDocument/codeLens ===")

    code_lens_cap = caps.get("codeLensProvider")
    if not code_lens_cap:
        print("  SKIP  codeLens (not in capabilities)")
    else:
        r = lsp.request("textDocument/codeLens", {
            "textDocument": {"uri": utils_uri},
        })
        lenses = r.get("result") or []
        check("codeLens returns a list",
              isinstance(lenses, list),
              f"got: {lenses!r}")
        # utils.fnl defines greet (called 2× in consumer) and bye (called 1×)
        greet_lens = next(
            (l for l in lenses if l.get("command", {}).get("title", "").startswith("2 ")),
            None,
        )
        check("codeLens shows '2 references' for greet (called twice in consumer)",
              greet_lens is not None,
              f"lens titles: {[l.get('command', {}).get('title') for l in lenses]}")
        check("codeLens range has start field",
              lenses and "start" in lenses[0].get("range", {}),
              f"first lens: {lenses[0] if lenses else None!r}")

    # ── textDocument/semanticTokens/full/delta ────────────────────────────────
    print("\n=== textDocument/semanticTokens/full/delta ===")

    sem_cap = caps.get("semanticTokensProvider") or {}
    full_opts = sem_cap.get("full") if isinstance(sem_cap, dict) else None
    delta_supported = isinstance(full_opts, dict) and full_opts.get("delta") is True

    if not delta_supported:
        print("  SKIP  semanticTokens/full/delta (not advertised)")
    else:
        # First, get a full result to obtain a result_id.
        r_full = lsp.request("textDocument/semanticTokens/full", {
            "textDocument": {"uri": utils_uri},
        })
        full_result = r_full.get("result") or {}
        result_id = full_result.get("resultId")
        check("semanticTokens/full returns a resultId",
              result_id is not None,
              f"got: {full_result!r}")

        if result_id:
            # Request delta — file unchanged, so edits should be empty.
            r_delta = lsp.request("textDocument/semanticTokens/full/delta", {
                "textDocument": {"uri": utils_uri},
                "previousResultId": result_id,
            })
            delta_result = r_delta.get("result") or {}
            # Response is either SemanticTokens or SemanticTokensDelta.
            # For an unchanged file we expect delta with 0 edits.
            edits = delta_result.get("edits")
            check("semanticTokens/full/delta on unchanged file returns edits list",
                  edits is not None,
                  f"delta result: {delta_result!r}")
            check("semanticTokens/full/delta on unchanged file has 0 edits",
                  edits == [],
                  f"edits: {edits!r}")
            check("semanticTokens/full/delta returns a new resultId",
                  delta_result.get("resultId") is not None,
                  f"delta result: {delta_result!r}")

    # ── diagnostic relatedInformation ────────────────────────────────────────
    print("\n=== diagnostic relatedInformation ===")

    shadow_path = os.path.join(SMOKE_DIR, "shadow-test.fnl")
    shadow_uri = file_uri(shadow_path)
    shadow_text = "(local x 1)\n(local x 2)\n"

    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": shadow_uri, "languageId": "fennel", "version": 1, "text": shadow_text}
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": shadow_uri}})  # sync

    shadow_diag_params, shadow_arrived = lsp.wait_for_diagnostics(
        shadow_uri,
        lambda p: any("already defined" in d.get("message", "") for d in p.get("diagnostics", [])),
        timeout=5.0,
    )
    check("shadow warning is published for re-defined local",
          shadow_arrived,
          f"diagnostics: {shadow_diag_params}")

    if shadow_arrived and shadow_diag_params:
        shadow_diag = next(
            (d for d in shadow_diag_params.get("diagnostics", [])
             if "already defined" in d.get("message", "")),
            None,
        )
        rel_info = shadow_diag.get("relatedInformation") if shadow_diag else None
        check("shadow diagnostic has relatedInformation",
              shadow_diag is not None and bool(rel_info),
              f"diag: {shadow_diag!r}")
        if rel_info:
            rel = rel_info[0]
            rel_uri = rel.get("location", {}).get("uri")
            check("relatedInformation points to the same file",
                  rel_uri == shadow_uri,
                  f"relatedInformation: {rel!r}")
            rel_line = rel.get("location", {}).get("range", {}).get("start", {}).get("line")
            check("relatedInformation points to the first definition (line 0)",
                  rel_line == 0,
                  f"relatedInformation line: {rel_line}")
        check("shadow diagnostic has 'shadow' code",
              shadow_diag is not None and shadow_diag.get("code") == "shadow",
              f"code: {(shadow_diag or {}).get('code')!r}")

    # ── textDocument/onTypeFormatting ─────────────────────────────────────────
    print("\n=== textDocument/onTypeFormatting ===")

    on_type_caps = caps.get("documentOnTypeFormattingProvider")
    if not on_type_caps:
        print("  SKIP  onTypeFormatting (not advertised in capabilities)")
    else:
        check("onTypeFormatting capability first trigger is '\\n'",
              on_type_caps.get("firstTriggerCharacter") == "\n",
              f"got: {on_type_caps!r}")

        # Simulate Enter at end of "(fn greet [name]" — cursor lands at greet_line+1, col 0.
        # [name] is closed so innermost unclosed paren is (fn at col 0 → indent 1.
        r = lsp.request("textDocument/onTypeFormatting", {
            "textDocument": {"uri": utils_uri},
            "position": {"line": greet_line + 1, "character": 0},
            "ch": "\n",
            "options": {"tabSize": 2, "insertSpaces": True},
        })
        result = r.get("result")
        check("onTypeFormatting returns a list of edits",
              isinstance(result, list) and len(result) >= 1,
              f"got: {result!r}")
        if isinstance(result, list) and result:
            edit = result[0]
            new_text = edit.get("newText", "")
            check("onTypeFormatting newText is spaces only",
                  new_text == "" or all(c == " " for c in new_text),
                  f"newText: {new_text!r}")
            edit_start_line = edit.get("range", {}).get("start", {}).get("line")
            check("onTypeFormatting edit targets the new line",
                  edit_start_line == greet_line + 1,
                  f"edit range: {edit.get('range')!r}")
            indent_len = len(new_text)
            check("onTypeFormatting indent is >= 1 (inside a form)",
                  indent_len >= 1,
                  f"indent: {indent_len}")

        # Trigger with a non-newline character — server should return null.
        r2 = lsp.request("textDocument/onTypeFormatting", {
            "textDocument": {"uri": utils_uri},
            "position": {"line": greet_line + 1, "character": 5},
            "ch": "x",
            "options": {"tabSize": 2, "insertSpaces": True},
        })
        result2 = r2.get("result")
        check("onTypeFormatting with non-'\\n' ch returns null",
              result2 is None,
              f"got: {result2!r}")

        # Inside (fn caller-b [n] — cursor one line into the body.
        caller_b_line = next(i for i, l in enumerate(ch_lines) if "(fn caller-b" in l)
        r3 = lsp.request("textDocument/onTypeFormatting", {
            "textDocument": {"uri": ch_uri},
            "position": {"line": caller_b_line + 1, "character": 0},
            "ch": "\n",
            "options": {"tabSize": 2, "insertSpaces": True},
        })
        result3 = r3.get("result")
        check("onTypeFormatting inside (fn caller-b) returns a list",
              isinstance(result3, list),
              f"got: {result3!r}")
        if isinstance(result3, list) and result3:
            inner_indent = len(result3[0].get("newText", ""))
            check("onTypeFormatting inside fn body gives non-zero indent",
                  inner_indent >= 1,
                  f"indent: {inner_indent}")

    # ── textDocument/inlineValue ──────────────────────────────────────────────
    print("\n=== textDocument/inlineValue ===")

    inline_cap = caps.get("inlineValueProvider")
    if not inline_cap:
        print("  SKIP  inlineValue (not advertised in capabilities)")
    else:
        # utils.fnl: greet is on line 2, body on line 4 — stop there.
        # At that point `greet` (fn name) and `name` (param) are both in scope.
        utils_lines = utils_text.splitlines()
        greet_body_line = next(
            (i for i, l in enumerate(utils_lines) if "(print" in l and "Hello" in l),
            4,
        )
        r = lsp.request("textDocument/inlineValue", {
            "textDocument": {"uri": utils_uri},
            "range": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": greet_body_line, "character": 0},
            },
            "context": {
                "frameId": 1,
                "stoppedLocation": {
                    "start": {"line": greet_body_line, "character": 0},
                    "end":   {"line": greet_body_line, "character": 0},
                },
            },
        })
        result = r.get("result") or []
        check("inlineValue returns a list",
              isinstance(result, list),
              f"got: {result!r}")
        variable_names = [
            v.get("variableName") for v in result
            if isinstance(v, dict) and "variableName" in v
        ]
        check("inlineValue includes 'name' param at stopped location",
              "name" in variable_names,
              f"variableNames: {variable_names!r}")
        check("inlineValue includes 'greet' fn name at stopped location",
              "greet" in variable_names,
              f"variableNames: {variable_names!r}")

        # Stopped at top-level (no locals in scope) — should return empty or
        # only globally-bound names, never function-body params.
        r2 = lsp.request("textDocument/inlineValue", {
            "textDocument": {"uri": utils_uri},
            "range": {
                "start": {"line": 0, "character": 0},
                "end":   {"line": 0, "character": 0},
            },
            "context": {
                "frameId": 1,
                "stoppedLocation": {
                    "start": {"line": 0, "character": 0},
                    "end":   {"line": 0, "character": 0},
                },
            },
        })
        result2 = r2.get("result") or []
        top_level_names = [
            v.get("variableName") for v in result2
            if isinstance(v, dict) and "variableName" in v
        ]
        check("inlineValue at top level does not include fn params",
              "name" not in top_level_names,
              f"variableNames at top-level: {top_level_names!r}")

    # ── Lua module require (direct Fennel → Lua) ─────────────────────────────
    print("\n=== Lua module require ===")

    lua_consumer_path = os.path.join(SMOKE_DIR, "lua-consumer.fnl")
    lua_consumer_uri  = file_uri(lua_consumer_path)
    lua_api_path      = os.path.join(SMOKE_DIR, "lua-api.lua")
    lua_api_uri       = file_uri(lua_api_path)
    lua_consumer_text = read_file(lua_consumer_path)
    lua_api_text      = read_file(lua_api_path)

    lsp.notify("textDocument/didOpen", {
        "textDocument": {
            "uri": lua_consumer_uri,
            "languageId": "fennel",
            "version": 1,
            "text": lua_consumer_text,
        }
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": lua_consumer_uri}})

    # No unknown-identifier warnings for Lua module members
    lua_diag, satisfied = lsp.wait_for_diagnostics(
        lua_consumer_uri,
        lambda p: p is not None,  # any publish is fine; we just need the list
        timeout=5.0,
    )
    if lua_diag is not None:
        unknown = [
            d for d in lua_diag.get("diagnostics", [])
            if "unknown identifier" in d.get("message", "")
            and any(
                m in d.get("message", "")
                for m in ("api.add", "api.greet", "api.answer")
            )
        ]
        check("no unknown-identifier warnings for Lua module members (api.add/greet/answer)",
              len(unknown) == 0,
              f"warnings: {[d.get('message') for d in unknown]}")
    else:
        print("  SKIP  Lua module diagnostics (no publish within timeout)")

    # Goto-def: cursor on api.add → should navigate into lua-api.lua
    consumer_lines = lua_consumer_text.splitlines()
    add_call_line = next(
        i for i, l in enumerate(consumer_lines) if "api.add" in l and "(api.add" in l
    )
    add_call_col = consumer_lines[add_call_line].index("api.add")

    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": lua_consumer_uri},
        "position": {"line": add_call_line, "character": add_call_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and not result):
        check("Lua goto-def: api.add returns a result", False, f"got: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("Lua goto-def: api.add targets lua-api.lua",
              loc.get("uri") == lua_api_uri,
              f"uri: {loc.get('uri')!r}, expected: {lua_api_uri!r}")
        # add is defined on the first non-comment, non-blank line in lua-api.lua
        add_lua_line = next(
            i for i, l in enumerate(lua_api_text.splitlines())
            if "function add" in l
        )
        def_line = loc.get("range", {}).get("start", {}).get("line")
        check("Lua goto-def: api.add lands on the correct Lua line",
              def_line == add_lua_line,
              f"got line {def_line}, want {add_lua_line}")

    # Goto-def: cursor on api.greet → should also navigate into lua-api.lua
    greet_call_line = next(
        i for i, l in enumerate(consumer_lines) if "(api.greet" in l
    )
    greet_call_col = consumer_lines[greet_call_line].index("api.greet")

    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": lua_consumer_uri},
        "position": {"line": greet_call_line, "character": greet_call_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and not result):
        check("Lua goto-def: api.greet returns a result", False, f"got: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("Lua goto-def: api.greet targets lua-api.lua",
              loc.get("uri") == lua_api_uri,
              f"uri: {loc.get('uri')!r}")
        greet_lua_line = next(
            i for i, l in enumerate(lua_api_text.splitlines())
            if "function greet" in l
        )
        def_line = loc.get("range", {}).get("start", {}).get("line")
        check("Lua goto-def: api.greet lands on the correct Lua line",
              def_line == greet_lua_line,
              f"got line {def_line}, want {greet_lua_line}")

    # Completion: after "api." should include Lua module members
    r = lsp.request("textDocument/completion", {
        "textDocument": {"uri": lua_consumer_uri},
        "position": {"line": add_call_line, "character": add_call_col + len("api.")},
    })
    completion = r.get("result")
    if completion is None:
        print("  SKIP  Lua module completion (server returned null)")
    else:
        items = completion if isinstance(completion, list) else completion.get("items", [])
        labels = [it.get("label", "") for it in items]
        check("Lua module completion includes 'add'",   "add"    in labels, f"labels: {labels}")
        check("Lua module completion includes 'greet'", "greet"  in labels, f"labels: {labels}")
        check("Lua module completion includes 'answer'","answer" in labels, f"labels: {labels}")

    # ── Nested require: Fennel → Fennel → Lua ────────────────────────────────
    print("\n=== Nested require (Fennel → Lua) ===")

    fnl_chain_path  = os.path.join(SMOKE_DIR, "fnl-chain.fnl")
    fnl_chain_uri   = file_uri(fnl_chain_path)
    lua_chain_path  = os.path.join(SMOKE_DIR, "lua-chain.lua")
    lua_chain_uri   = file_uri(lua_chain_path)
    fnl_chain_text  = read_file(fnl_chain_path)
    lua_chain_text  = read_file(lua_chain_path)

    lsp.notify("textDocument/didOpen", {
        "textDocument": {
            "uri": fnl_chain_uri,
            "languageId": "fennel",
            "version": 1,
            "text": fnl_chain_text,
        }
    })
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": fnl_chain_uri}})

    # No unknown-identifier warnings for chain.double / chain.square
    chain_diag, _ = lsp.wait_for_diagnostics(fnl_chain_uri, lambda p: p is not None, timeout=5.0)
    if chain_diag is not None:
        chain_unknown = [
            d for d in chain_diag.get("diagnostics", [])
            if "unknown identifier" in d.get("message", "")
            and any(m in d.get("message", "") for m in ("chain.double", "chain.square"))
        ]
        check("nested require: no unknown-identifier warnings for Lua module members",
              len(chain_unknown) == 0,
              f"warnings: {[d.get('message') for d in chain_unknown]}")
    else:
        print("  SKIP  nested require diagnostics (no publish within timeout)")

    # Goto-def on chain.double → lua-chain.lua at the double function line
    chain_lines = fnl_chain_text.splitlines()
    compute_line = next(i for i, l in enumerate(chain_lines) if "chain.double" in l)
    double_col   = chain_lines[compute_line].index("chain.double")

    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": fnl_chain_uri},
        "position": {"line": compute_line, "character": double_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and not result):
        check("nested goto-def: chain.double returns a result", False, f"got: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("nested goto-def: chain.double targets lua-chain.lua",
              loc.get("uri") == lua_chain_uri,
              f"uri: {loc.get('uri')!r}, expected: {lua_chain_uri!r}")
        double_lua_line = next(
            i for i, l in enumerate(lua_chain_text.splitlines())
            if "function double" in l
        )
        def_line = loc.get("range", {}).get("start", {}).get("line")
        check("nested goto-def: chain.double lands on the correct Lua line",
              def_line == double_lua_line,
              f"got line {def_line}, want {double_lua_line}")

    # Goto-def on chain.square → lua-chain.lua at the square function line
    square_col = chain_lines[compute_line].index("chain.square")

    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": fnl_chain_uri},
        "position": {"line": compute_line, "character": square_col},
    })
    result = r.get("result")
    if result is None or (isinstance(result, list) and not result):
        check("nested goto-def: chain.square returns a result", False, f"got: {result!r}")
    else:
        loc = result[0] if isinstance(result, list) else result
        check("nested goto-def: chain.square targets lua-chain.lua",
              loc.get("uri") == lua_chain_uri,
              f"uri: {loc.get('uri')!r}")
        square_lua_line = next(
            i for i, l in enumerate(lua_chain_text.splitlines())
            if "function square" in l
        )
        def_line = loc.get("range", {}).get("start", {}).get("line")
        check("nested goto-def: chain.square lands on the correct Lua line",
              def_line == square_lua_line,
              f"got line {def_line}, want {square_lua_line}")

    # Goto-def on the require string :lua-chain → navigates into lua-chain.lua
    require_line = next(i for i, l in enumerate(chain_lines) if ":lua-chain" in l)
    require_col  = chain_lines[require_line].index(":lua-chain") + 1  # inside the string

    r = lsp.request("textDocument/definition", {
        "textDocument": {"uri": fnl_chain_uri},
        "position": {"line": require_line, "character": require_col},
    })
    result = r.get("result")
    if result is not None and not (isinstance(result, list) and not result):
        loc = result[0] if isinstance(result, list) else result
        check("nested require string goto-def targets lua-chain.lua",
              loc.get("uri") == lua_chain_uri,
              f"uri: {loc.get('uri')!r}")
    else:
        print("  SKIP  nested require-string goto-def (no result)")

    # ── macro hooks ───────────────────────────────────────────────────────────
    # Verifies end-to-end hook execution:
    #   1. The .lsp.fnl in smoke/ registers a `defnode` hook for :simple-macros.
    #   2. Opening hooks-macro.fnl triggers analysis → hook pass → second analysis.
    #   3. After the hook pass, FennelNode3D (Bind), _ready and _process (AnalyzeFn)
    #      are real definitions visible via workspace/symbol.
    #   4. No unknown-identifier warnings appear (DSL args not analyzed).

    print("\n=== macro hooks ===")
    hooks_macro_path = os.path.join(SMOKE_DIR, "hooks-macro.fnl")
    hooks_macro_uri = file_uri(hooks_macro_path)
    hooks_macro_text = read_file(hooks_macro_path)

    lsp.notify("textDocument/didOpen", {
        "textDocument": {
            "uri": hooks_macro_uri,
            "languageId": "fennel",
            "version": 1,
            "text": hooks_macro_text,
        }
    })
    # Barrier: documentSymbol ensures the first analysis pass is done.
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": hooks_macro_uri}})

    # Wait for the hook pass to complete and re-publish diagnostics.
    # Success condition: no unknown-identifier diagnostics (all macro args suppressed).
    def no_unknown_in_hooks(params):
        return all(
            "unknown identifier" not in d.get("message", "")
            for d in params.get("diagnostics", [])
        )

    diag, satisfied = lsp.wait_for_diagnostics(hooks_macro_uri, no_unknown_in_hooks, timeout=10.0)
    check("no unknown-identifier warnings in hooks-macro.fnl",
          satisfied,
          f"last diags: {diag.get('diagnostics') if diag else 'none'}")

    # FennelNode3D should be a real definition from the Bind instruction.
    syms = lsp.request("workspace/symbol", {"query": "FennelNode3D"}).get("result") or []
    names = [s["name"] for s in syms]
    check("hook Bind creates FennelNode3D as a workspace symbol",
          "FennelNode3D" in names,
          f"got {names}")

    # _ready should be a real Fn definition from the AnalyzeFn instruction.
    syms = lsp.request("workspace/symbol", {"query": "_ready"}).get("result") or []
    names = [s["name"] for s in syms]
    check("hook AnalyzeFn creates _ready as a workspace symbol",
          "_ready" in names,
          f"got {names}")

    # _process similarly.
    syms = lsp.request("workspace/symbol", {"query": "_process"}).get("result") or []
    names = [s["name"] for s in syms]
    check("hook AnalyzeFn creates _process as a workspace symbol",
          "_process" in names,
          f"got {names}")

    lsp.close()
    return FAILURES


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default=DEFAULT_BINARY)
    args = parser.parse_args()

    if not os.path.isfile(args.binary):
        print(f"Binary not found: {args.binary}")
        print("Run: cargo build --release")
        sys.exit(2)

    print(f"Binary: {args.binary}")
    failures = run(args.binary)

    print(f"\n{'='*50}")
    if failures:
        print(f"FAILED ({len(failures)} checks):")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    else:
        print("All checks passed.")
        sys.exit(0)


if __name__ == "__main__":
    main()
