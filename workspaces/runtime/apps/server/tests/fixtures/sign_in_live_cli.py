#!/usr/bin/env python3
"""A sign-in command for sign-in-live.py: an OAuth client that prints its link and never opens a
browser, as many tools do with a "no browser" option.

  sign_in_live_cli.py device ISSUER     device code flow (RFC 8628)
  sign_in_live_cli.py loopback ISSUER   authorization code flow with PKCE and a loopback redirect

It prints `LOGIN_OK` and exits 0 when the provider issues a token, and exits 1 after 300 seconds.
"""
import base64
import hashlib
import http.server
import json
import secrets
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

CLIENT = "studio-live"
DEADLINE = time.monotonic() + 300


def post(url, fields):
    data = urllib.parse.urlencode(fields).encode()
    try:
        with urllib.request.urlopen(urllib.request.Request(url, data=data), timeout=10) as response:
            return json.load(response)
    except urllib.error.HTTPError as error:
        with error:
            return json.load(error)


def device(issuer):
    started = post(issuer + "/device/code", {"client_id": CLIENT, "scope": "openid"})
    print(f"To sign in, open {started['verification_uri_complete']} and confirm the code {started['user_code']}.",
          flush=True)
    while time.monotonic() < DEADLINE:
        time.sleep(started["interval"])
        token = post(issuer + "/token", {"grant_type": "urn:ietf:params:oauth:grant-type:device_code",
                                         "device_code": started["device_code"], "client_id": CLIENT})
        if "access_token" in token:
            return True
        if token.get("error") != "authorization_pending":
            print("Sign-in failed: " + str(token.get("error")), flush=True)
            return False
    return False


def loopback(issuer):
    verifier = secrets.token_urlsafe(32)
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    state = secrets.token_urlsafe(8)
    result = {}
    done = threading.Event()

    class Callback(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            query = urllib.parse.parse_qs(urllib.parse.urlsplit(self.path).query)
            ok = query.get("state") == [state] and "code" in query
            if ok:
                result["code"] = query["code"][0]
            self.send_response(200 if ok else 400)
            self.send_header("Content-Type", "text/plain")
            self.end_headers()
            self.wfile.write(b"Signed in. You can close this page." if ok else b"Bad sign-in result")
            if ok:
                done.set()

        def log_message(self, *_args):
            pass

    server = http.server.HTTPServer(("127.0.0.1", 0), Callback)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    redirect = f"http://127.0.0.1:{server.server_address[1]}/callback"
    link = issuer + "/authorize?" + urllib.parse.urlencode({
        "response_type": "code", "client_id": CLIENT, "redirect_uri": redirect, "scope": "openid",
        "state": state, "code_challenge": challenge, "code_challenge_method": "S256"})
    print("Open this link to sign in: " + link, flush=True)
    try:
        if not done.wait(max(0.0, DEADLINE - time.monotonic())):
            return False
        token = post(issuer + "/token", {"grant_type": "authorization_code", "code": result["code"],
                                         "redirect_uri": redirect, "client_id": CLIENT, "code_verifier": verifier})
        return "access_token" in token
    finally:
        server.shutdown()


if __name__ == "__main__":
    flow, issuer = sys.argv[1], sys.argv[2].rstrip("/")
    ok = device(issuer) if flow == "device" else loopback(issuer)
    print("LOGIN_OK" if ok else "LOGIN_FAILED", flush=True)
    sys.exit(0 if ok else 1)
