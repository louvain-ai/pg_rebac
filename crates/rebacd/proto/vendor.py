#!/usr/bin/env python3
"""Vendor the SpiceDB v1 API protos, without their validation and HTTP-gateway annotations.

    python3 vendor.py <path to a checkout of github.com/authzed/api> <path to github.com/googleapis/googleapis>

The annotations (buf.validate, validate, google.api.http, openapiv2) are options a client never reads; stripping them
leaves the messages and services byte-compatible on the wire and removes four dependency trees from the build. The
originals are Apache-2.0 (see LICENSE-authzed-api); the source commit is written into each file's header.
"""
import pathlib, re, subprocess, sys

FILES = ["core", "permission_service", "schema_service", "watch_service", "debug"]
DROP_IMPORTS = ("buf/validate/", "validate/validate.proto", "google/api/", "protoc-gen-openapiv2/")


def strip(text: str) -> str:
    out, i, n = [], 0, len(text)
    while i < n:
        if text.startswith("//", i):
            j = text.find("\n", i); j = n if j < 0 else j
            out.append(text[i:j]); i = j; continue
        if text.startswith("/*", i):
            j = text.find("*/", i) + 2
            out.append(text[i:j]); i = j; continue
        if text[i] == '"':
            j = i + 1
            while text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i:j + 1]); i = j + 1; continue
        m = re.match(r"option\s*\(", text[i:])
        if m and (i == 0 or not text[i - 1].isalnum()):
            depth, j = 0, i
            while True:
                c = text[j]
                if c in "{[": depth += 1
                elif c in "}]": depth -= 1
                elif c == ";" and depth == 0: break
                elif c == '"':
                    j += 1
                    while text[j] != '"': j += 2 if text[j] == "\\" else 1
                j += 1
            i = j + 1; continue
        if text[i] == "[":
            depth, j = 0, i
            while True:
                c = text[j]
                if c in "[{": depth += 1
                elif c in "]}":
                    depth -= 1
                    if depth == 0: break
                elif c == '"':
                    j += 1
                    while text[j] != '"': j += 2 if text[j] == "\\" else 1
                j += 1
            body = text[i:j + 1]
            if "(" in body:
                i = j + 1; continue
            out.append(body); i = j + 1; continue
        out.append(text[i]); i += 1
    lines = [l for l in "".join(out).split("\n") if not any(f'import "{d}' in l for d in DROP_IMPORTS)]
    return re.sub(r"\n{3,}", "\n\n", "\n".join(lines))


def main():
    api, google = pathlib.Path(sys.argv[1]), pathlib.Path(sys.argv[2])
    rev = subprocess.run(["git", "-C", str(api), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip()
    here = pathlib.Path(__file__).parent
    for f in FILES:
        src = (api / "authzed/api/v1" / f"{f}.proto").read_text()
        dst = here / "authzed/api/v1" / f"{f}.proto"
        dst.parent.mkdir(parents=True, exist_ok=True)
        dst.write_text(f"// Vendored from github.com/authzed/api@{rev}, annotations stripped by vendor.py. Apache-2.0.\n"
                       + strip(src))
    status = google / "google/rpc/status.proto"
    dst = here / "google/rpc/status.proto"
    dst.parent.mkdir(parents=True, exist_ok=True)
    dst.write_text(status.read_text())
    (here / "LICENSE-authzed-api").write_text((api / "LICENSE").read_text())


if __name__ == "__main__":
    main()
