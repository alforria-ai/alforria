#!/usr/bin/env python3
"""A scripted JSON-RPC language-server double for the LSP test suite.

Usage: lsp_double.py <log-path> [--die] [--no-pull]

Speaks LSP base-protocol framing (Content-Length headers) over stdio.
Every received message is appended to <log-path> as one JSON line.
The client's responses to server-initiated requests are logged too.
"""

import json
import sys

DIE = "--die" in sys.argv
NO_PULL = "--no-pull" in sys.argv

log_path = None
for arg in sys.argv[1:]:
    if not arg.startswith("--"):
        log_path = arg

LOG = open(log_path, "a", encoding="utf-8") if log_path else None
OUT = sys.stdout.buffer


def log(message):
    if LOG is not None:
        LOG.write(json.dumps(message) + "\n")
        LOG.flush()


def send(message):
    body = json.dumps(message).encode("utf-8")
    OUT.write(f"Content-Length: {len(body)}\r\n\r\n".encode("utf-8"))
    OUT.write(body)
    OUT.flush()


def respond(id, result):
    send({"jsonrpc": "2.0", "id": id, "result": result})


def request(method, params):
    send({"jsonrpc": "2.0", "id": "double", "method": method, "params": params})


def notify(method, params):
    send({"jsonrpc": "2.0", "method": method, "params": params})


def zero_range():
    return {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 0}}


def location(uri, line=0, character=0):
    return {
        "uri": uri,
        "range": {
            "start": {"line": line, "character": character},
            "end": {"line": line, "character": character + 4},
        },
    }


def publish_diagnostics(uri):
    notify(
        "textDocument/publishDiagnostics",
        {
            "uri": uri,
            "diagnostics": [
                {
                    "range": zero_range(),
                    "severity": 1,
                    "message": "double error",
                    "source": "double",
                }
            ],
        },
    )


def handle_request(method, params):
    if method == "initialize":
        capabilities = {
            "textDocumentSync": 1,
            "hoverProvider": True,
            "definitionProvider": True,
            "referencesProvider": True,
            "implementationProvider": True,
            "documentSymbolProvider": True,
            "workspaceSymbolProvider": True,
            "callHierarchyProvider": True,
        }
        if not NO_PULL:
            capabilities["diagnosticProvider"] = {"interFileDependencies": True}
        # Server-initiated requests exercising the client dispatch.
        request("workspace/configuration", {"items": [{"section": "a.b"}]})
        request("workspace/workspaceFolders", None)
        return {"capabilities": capabilities}
    if method == "textDocument/hover":
        return {"contents": "hover text"}
    if method == "textDocument/definition":
        return location("file:///double/target.rs", line=3)
    if method == "textDocument/references":
        return [location("file:///double/ref1.rs")]
    if method == "textDocument/implementation":
        return location("file:///double/impl.rs")
    if method == "textDocument/documentSymbol":
        return [{"name": "main", "kind": 12, "range": zero_range()}]
    if method == "workspace/symbol":
        return [
            {"name": "Good", "kind": 5, "location": location("file:///double/good.rs")},
            {"name": "Bad", "kind": 1, "location": location("file:///double/bad.rs")},
            {"name": "Fn", "kind": 12, "location": location("file:///double/fn.rs")},
        ]
    if method == "textDocument/prepareCallHierarchy":
        return [{"name": "item"}]
    if method == "callHierarchy/incomingCalls":
        return [{"from": {"name": "caller"}}]
    if method == "callHierarchy/outgoingCalls":
        return [{"to": {"name": "callee"}}]
    if method == "textDocument/diagnostic":
        return {
            "kind": "full",
            "items": [
                {
                    "range": zero_range(),
                    "severity": 4,
                    "message": "pull diagnostic",
                    "source": "double",
                }
            ],
        }
    return None


def handle_notification(method, params):
    if method == "textDocument/didOpen":
        publish_diagnostics(params["textDocument"]["uri"])
    if method == "textDocument/didChange":
        publish_diagnostics(params["textDocument"]["uri"])


def main():
    if DIE:
        sys.exit(0)
    while True:
        headers = {}
        line = sys.stdin.buffer.readline()
        if not line:
            return
        while line not in (b"\r\n", b"\n", b""):
            name, _, value = line.decode("utf-8").partition(":")
            headers[name.strip().lower()] = value.strip()
            line = sys.stdin.buffer.readline()
        body = sys.stdin.buffer.read(int(headers["content-length"]))
        message = json.loads(body)
        log(message)
        method = message.get("method")
        if method is None:
            continue
        if "id" in message:
            respond(message["id"], handle_request(method, message.get("params")))
        else:
            handle_notification(method, message.get("params") or {})


main()
