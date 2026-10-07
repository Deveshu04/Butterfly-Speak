"""Command-line sign-in for the latency harness's Cloud mode.

Runs the same PKCE flow the app runs (`auth::session` + `auth::pkce`), against
the same Supabase project, and writes ONLY the access token to a file the
harness reads:

    python tools/latency/cloud_token.py
    python tools/latency/e2e_stress.py --relay https://<relay> \\
        --token-file tools/latency/.cloud_token --config incremental --mode manual --n 3

The differences from the app are the two the command line forces: the browser
comes back to a one-shot listener on http://127.0.0.1:8765/callback instead of
the `butterflylabs://` deep link, and the token lands in a git-ignored file
instead of process memory. Everything else — the 32-byte verifier, the S256
challenge, `POST /auth/v1/token?grant_type=pkce` with `auth_code` and
`code_verifier` beside it — is what the app does, so a token this writes is a
token the relay accepts.

It needs a Supabase project whose Authentication -> URL Configuration ->
Redirect URLs list `http://127.0.0.1:8765/callback`, with Google sign-in
enabled, and an account that can sign in to it. By default that is the project
the app uses; BS_SUPABASE_URL and BS_SUPABASE_ANON_KEY point it at another.
The redirect is PKCE-bound: a code handed to that address is worthless without
the verifier, which never leaves this process.

The token is short-lived (an hour) and there is deliberately no `--refresh`:
this writes no refresh token anywhere, so renewing is rerunning this script.
Nothing here ever prints or logs the token, the authorization code, the
verifier or a refresh token — the HTTP server's request logging is turned off
for exactly that reason (the request line carries the code).

Standard library only. `--self-test` checks the RFC 7636 appendix B vector and
needs no network, no browser and no sign-in.
"""
import argparse
import base64
import hashlib
import http.server
import json
import os
import secrets
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser

# The app's Supabase project, copied from src-tauri/src/auth/session.rs, unless
# the environment names another. Both are public by design: the anon key is a
# signed claim of the `anon` role and opens nothing RLS has not already opened.
# Another project needs both variables: one alone would pair the app's key with
# another project's address, or the other way round.
_ENV_URL = os.environ.get("BS_SUPABASE_URL", "").strip()
_ENV_KEY = os.environ.get("BS_SUPABASE_ANON_KEY", "").strip()
if bool(_ENV_URL) != bool(_ENV_KEY):
    sys.exit("Set both BS_SUPABASE_URL and BS_SUPABASE_ANON_KEY, or neither.")
SUPABASE_URL = (_ENV_URL or "https://iassqjnfvdffocyxptis.supabase.co").rstrip("/")
SUPABASE_ANON_KEY = _ENV_KEY or (
    "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9."
    "eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6Imlhc3Nxam5mdmRmZm9jeXhwdGlzIiwicm9sZSI6"
    "ImFub24iLCJpYXQiOjE3ODk3MTY5MjQsImV4cCI6MjEwNTI5MjkyNH0."
    "HTef2ALn-SmsyFll1vT9r8ZuRFnUiKNbHPW-3b9tkH4"
)

REDIRECT_HOST = "127.0.0.1"
REDIRECT_PORT = 8765
REDIRECT_PATH = "/callback"
REDIRECT_URI = f"http://{REDIRECT_HOST}:{REDIRECT_PORT}{REDIRECT_PATH}"

HERE = os.path.dirname(os.path.abspath(__file__))
# Git-ignored (see .gitignore) and per-user: a token file in the repo that
# could be committed is the one way this flow could leak.
DEFAULT_OUT = os.path.join(HERE, ".cloud_token")

BROWSER_REPLY = b"You can close this tab"
BROWSER_REPLY_FAILED = b"Sign-in failed. You can close this tab"


# --- PKCE (RFC 7636), the same two functions as auth::pkce -------------------


def b64url(raw):
    """base64url without padding — the encoding both PKCE fields use."""
    return base64.urlsafe_b64encode(raw).decode("ascii").rstrip("=")


def make_verifier():
    """A fresh code verifier: 32 random bytes, base64url without padding.

    43 characters, the shortest RFC 7636 4.1 allows, and 256 bits of entropy,
    which is the number that matters. `secrets` reads the OS CSPRNG."""
    return b64url(secrets.token_bytes(32))


def challenge(verifier):
    """The S256 challenge for `verifier`: base64url-nopad(SHA-256(ASCII(v)))."""
    return b64url(hashlib.sha256(verifier.encode("ascii")).digest())


def authorize_url(verifier):
    """Google's consent screen, through GoTrue, for this verifier.

    `auth::session::authorize_url` with one substitution: the loopback
    listener below instead of the app's deep link. `code_challenge` is left
    unescaped for the reason the Rust leaves it unescaped — base64url's
    alphabet is a subset of RFC 3986's unreserved set."""
    redirect = urllib.parse.quote(REDIRECT_URI, safe="")
    return (
        f"{SUPABASE_URL}/auth/v1/authorize?provider=google&redirect_to={redirect}"
        f"&code_challenge={challenge(verifier)}&code_challenge_method=s256"
    )


# --- the one-shot loopback listener -----------------------------------------


def callback_handler(result):
    """A request handler that parks the callback's outcome in `result`."""

    class Handler(http.server.BaseHTTPRequestHandler):
        # The browser asks for /favicon.ico and anything else it fancies;
        # only the callback path is a sign-in.
        def do_GET(self):
            parsed = urllib.parse.urlsplit(self.path)
            if parsed.path != REDIRECT_PATH:
                self.send_response(404)
                self.end_headers()
                return
            query = urllib.parse.parse_qs(parsed.query)
            code = (query.get("code") or [""])[0]
            error = (query.get("error") or [""])[0]
            if code:
                result["code"] = code
                self.reply(200, BROWSER_REPLY)
            elif error:
                # The name only. What GoTrue says about it is the browser's
                # business, and an error description can carry an address.
                result["error"] = error
                self.reply(400, BROWSER_REPLY_FAILED)
            else:
                result["error"] = "no_code"
                self.reply(400, BROWSER_REPLY_FAILED)

        def reply(self, status, text):
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(text)))
            self.end_headers()
            self.wfile.write(text)

        def log_message(self, *args):
            """Silence. The default writes the request line to stderr, and the
            request line is `GET /callback?code=... HTTP/1.1`."""

    return Handler


def wait_for_callback(timeout_s):
    """Serve until the callback arrives or `timeout_s` runs out. Returns
    `{"code": ...}`, `{"error": name}` or `{}` on a timeout."""
    result = {}
    server = http.server.HTTPServer(
        (REDIRECT_HOST, REDIRECT_PORT), callback_handler(result)
    )
    server.timeout = 1.0
    deadline = time.monotonic() + timeout_s
    try:
        while not result and time.monotonic() < deadline:
            server.handle_request()
    finally:
        server.server_close()
    return result


# --- the token exchange ------------------------------------------------------


def exchange(code, verifier):
    """`POST /auth/v1/token?grant_type=pkce`, exactly as `auth::session` sends
    it: `auth_code` (not `code`) and `code_verifier` in the body, the anon key
    in `apikey`. Returns the access token."""
    body = json.dumps({"auth_code": code, "code_verifier": verifier}).encode("utf-8")
    request = urllib.request.Request(
        f"{SUPABASE_URL}/auth/v1/token?grant_type=pkce",
        data=body,
        method="POST",
        headers={
            "apikey": SUPABASE_ANON_KEY,
            "Content-Type": "application/json",
            "Accept": "application/json",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as e:
        # The status, never the body: a 4xx from this endpoint echoes the
        # request it refused, code and verifier included.
        sys.exit(f"sign-in failed: the token endpoint answered HTTP {e.code}")
    except urllib.error.URLError:
        sys.exit("sign-in failed: could not reach Supabase")
    except ValueError:
        sys.exit("sign-in failed: the token endpoint's reply would not parse")
    token = payload.get("access_token")
    if not isinstance(token, str) or not token:
        sys.exit("sign-in failed: no access token in the reply")
    return token


# --- writing it down ---------------------------------------------------------


def harden(path):
    """Best-effort owner-only ACL on Windows, where the mode bits `os.open`
    took are almost entirely decorative. Failure is not fatal: the file is
    already inside this user's profile-scoped checkout and git-ignored, and
    nothing here prints what went wrong (the path is the only thing it could
    name, and the path is not the secret)."""
    if os.name != "nt":
        return False
    user = os.environ.get("USERNAME")
    if not user:
        return False
    try:
        done = subprocess.run(
            ["icacls", path, "/inheritance:r", "/grant:r", f"{user}:F"],
            capture_output=True,
            timeout=20,
            check=False,
        )
        return done.returncode == 0
    except (OSError, subprocess.SubprocessError):
        return False


def write_token(path, token):
    """The access token and nothing else, owner-only from the moment the file
    exists (`os.open` with 0o600, so there is no window where it is 0o644)."""
    directory = os.path.dirname(os.path.abspath(path))
    if directory and not os.path.isdir(directory):
        os.makedirs(directory, exist_ok=True)
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="ascii") as f:
        f.write(token)
    return harden(path)


# --- the flow ----------------------------------------------------------------


def sign_in(args):
    verifier = make_verifier()
    url = authorize_url(verifier)
    # Bound before the browser opens: on a fast machine the callback can
    # arrive while `webbrowser.open` is still returning.
    print(f"listening on {REDIRECT_URI}")
    print("opening your browser at:")
    print(f"  {url}")
    print("(if it did not open, paste that URL in yourself)")
    opened = webbrowser.open(url)
    if not opened:
        print("no default browser could be opened - paste the URL above")
    result = wait_for_callback(args.timeout)
    if not result:
        sys.exit(f"sign-in timed out after {args.timeout}s")
    if "error" in result:
        sys.exit(f"sign-in refused: {result['error']}")
    token = exchange(result["code"], verifier)
    out = args.out
    hardened = write_token(out, token)
    print(f"wrote the access token to {out}"
          + (" (owner-only ACL set)" if hardened else ""))
    print("it lasts about an hour; rerun this script when the harness says the "
          "token was rejected")


# --- self-test ---------------------------------------------------------------


def self_test():
    """Checks the parts that can be wrong silently. No network, no browser."""
    checks = 0

    # RFC 7636 appendix B, the vector auth::pkce's own test uses.
    assert challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk") == (
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    ), "S256 challenge does not match the RFC 7636 appendix B vector"
    checks += 1

    v = make_verifier()
    assert len(v) == 43, f"verifier is {len(v)} characters, not 43"
    assert all(c.isalnum() or c in "-._~" for c in v), "verifier is not unreserved"
    assert v != make_verifier(), "two verifiers came out the same"
    checks += 3

    url = authorize_url(v)
    assert url.startswith(f"{SUPABASE_URL}/auth/v1/authorize?provider=google&"), url
    assert f"redirect_to={urllib.parse.quote(REDIRECT_URI, safe='')}" in url, url
    assert f"code_challenge={challenge(v)}" in url, "challenge missing from the URL"
    assert url.endswith("&code_challenge_method=s256"), url
    assert v not in url, "the verifier must never travel to the browser"
    checks += 5

    # The written file: the token alone, and owner-only where the platform
    # means it. A `.token` that has picked up a newline is a 401 nobody can
    # see, so the round trip is checked rather than assumed.
    with tempfile.TemporaryDirectory() as tmp:
        path = os.path.join(tmp, "nested", "token")
        write_token(path, "not-a-real-token")
        with open(path, encoding="ascii") as f:
            assert f.read() == "not-a-real-token", "the token file round trip changed it"
        checks += 1
        if os.name != "nt":
            mode = os.stat(path).st_mode & 0o777
            assert mode == 0o600, f"token file mode is {oct(mode)}, not 0o600"
            checks += 1

    print(f"self-test: ok ({checks} checks)")


if __name__ == "__main__":
    ap = argparse.ArgumentParser(
        description="Sign in to Butterfly Labs and write the access token the "
                    "latency harness's --relay mode reads.")
    ap.add_argument("--out", default=DEFAULT_OUT,
                    help=f"where to write the access token (default {DEFAULT_OUT})")
    ap.add_argument("--timeout", type=int, default=300,
                    help="seconds to wait for the browser to come back (default 300)")
    ap.add_argument("--self-test", action="store_true",
                    help="check the PKCE vector and the file writing; no network")
    ap.add_argument("--refresh", action="store_true",
                    help="not supported: no refresh token is ever written, so "
                         "renewing is rerunning this script")
    a = ap.parse_args()
    if a.refresh:
        sys.exit("--refresh is not supported: a token lasts about an hour and no "
                 "refresh token is written anywhere. Rerun this script.")
    if a.self_test:
        self_test()
    else:
        sign_in(a)
