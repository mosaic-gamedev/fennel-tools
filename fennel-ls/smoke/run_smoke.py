#!/usr/bin/env python3
"""
Automated smoke test for fennel-ls.

Starts the LSP binary, opens smoke files, and verifies:
  - workspace/symbol
  - textDocument/references (cross-file)
  - textDocument/rename (cross-file)

Usage:
  python3 smoke/run_smoke.py [--binary PATH]

Exit code 0 = all checks passed, non-zero = failures.
"""

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time

SMOKE_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_ROOT = os.path.dirname(SMOKE_DIR)
DEFAULT_BINARY = os.path.join(REPO_ROOT, "target", "release", "fennel-ls")


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
        self._responses = {}  # id -> response
        self._lock = threading.Lock()
        self._cond = threading.Condition(self._lock)
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()

    def _next_id(self):
        self._id += 1
        return self._id

    def _read_loop(self):
        stdout = self.proc.stdout
        while True:
            # Read MIME-style headers until blank line
            headers = {}
            while True:
                line = stdout.readline()
                if not line:  # EOF
                    return
                line = line.rstrip(b"\r\n")
                if not line:  # blank line = end of headers
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
            if "id" in msg and "method" not in msg:
                with self._cond:
                    self._responses[msg["id"]] = msg
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
    utils_uri = file_uri(utils_path)
    consumer_uri = file_uri(consumer_path)
    utils_text = read_file(utils_path)
    consumer_text = read_file(consumer_path)

    # ── initialize ────────────────────────────────────────────────────────────
    init = lsp.request("initialize", {
        "processId": os.getpid(),
        "rootUri": file_uri(SMOKE_DIR),
        "capabilities": {},
    })
    lsp.notify("initialized", {})

    # ── open both files ───────────────────────────────────────────────────────
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": utils_uri, "languageId": "fennel", "version": 1, "text": utils_text}
    })
    lsp.notify("textDocument/didOpen", {
        "textDocument": {"uri": consumer_uri, "languageId": "fennel", "version": 1, "text": consumer_text}
    })

    # Sync barrier: documentSymbol on utils to ensure didOpen processing is done.
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": utils_uri}})
    lsp.request("textDocument/documentSymbol", {"textDocument": {"uri": consumer_uri}})

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
