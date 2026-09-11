"""Local Codex profiles. Store identity metadata, never copies of credentials."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import tempfile
import threading
import uuid
import time

from codex_state import codex_home


def auth_metadata(home):
    """Unverified token claims are display metadata, not authentication proof."""
    path = Path(home) / "auth.json"
    try:
        if path.stat().st_size > 1024 * 1024:
            raise ValueError("The Codex auth file is too large")
        auth = json.loads(path.read_text())
        if not isinstance(auth, dict):
            raise ValueError("Invalid Codex auth file")
        tokens = auth.get("tokens") or {}
        claims = {}
        token = tokens.get("id_token") or tokens.get("access_token")
        if token:
            try:
                part = token.split(".")[1]
                claims = json.loads(
                    base64.urlsafe_b64decode(part + "=" * (-len(part) % 4))
                )
            except (ValueError, IndexError, TypeError):
                claims = {}
        details = claims.get("https://api.openai.com/auth") or {}
        email = claims.get("email") or (
            claims.get("https://api.openai.com/profile") or {}
        ).get("email")
        account = tokens.get("account_id") or details.get("chatgpt_account_id")
        api_key = bool(auth.get("OPENAI_API_KEY"))
        return {
            "accountId": account,
            "email": email,
            "plan": details.get("chatgpt_plan_type")
            or ("API key" if api_key else None),
            "status": (
                "ready"
                if account and tokens.get("access_token") or api_key
                else "signedOut"
            ),
            "_credentialIdentity": (
                "chatgpt:" + str(account)
                if account
                else (
                    "api:" + hashlib.sha256(auth["OPENAI_API_KEY"].encode()).hexdigest()
                    if api_key
                    else None
                )
            ),
        }
    except FileNotFoundError:
        return {"accountId": None, "email": None, "plan": None, "status": "signedOut"}
    except (OSError, ValueError, TypeError, AttributeError):
        # Parser errors must not include the auth file contents.
        return {
            "accountId": None,
            "email": None,
            "plan": None,
            "status": "error",
            "error": "Cannot read this Codex profile's authentication",
        }


class AccountStore:
    def __init__(self, root):
        self.root = Path(root) / "accounts"
        self.path = self.root / "registry.json"
        self.lock = threading.RLock()
        self.login_lock = threading.Lock()
        self.discovered = False
        self.base_home = codex_home()
        if self.path.exists():
            try:
                self.data = json.loads(self.path.read_text())
                if self.data.get("version") != 1 or not isinstance(
                    self.data.get("accounts"), dict
                ):
                    raise ValueError()
            except (OSError, ValueError, AttributeError):
                raise RuntimeError("Cannot read the account registry") from None
        else:
            self.data = {
                "version": 1,
                "defaultAccountKey": "default",
                "accounts": {
                    "default": {
                        "id": "default",
                        "home": str(self.base_home),
                        "label": "Codex",
                        "source": "Codex CLI",
                        **auth_metadata(self.base_home),
                    }
                },
                "logins": {},
            }
            self._save()

    def _save(self):
        self.root.mkdir(parents=True, exist_ok=True, mode=0o700)
        fd, name = tempfile.mkstemp(prefix="registry-", dir=self.root)
        try:
            with os.fdopen(fd, "w") as handle:
                json.dump(self.data, handle)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(name, self.path)
        finally:
            if os.path.exists(name):
                os.unlink(name)

    def _row(self, key):
        if (
            not isinstance(key, str)
            or not re.fullmatch(r"[A-Za-z0-9-]{1,100}", key)
            or key not in self.data["accounts"]
        ):
            raise ValueError("Unknown Codex account")
        return self.data["accounts"][key]

    def refresh(self, key):
        with self.lock:
            row = self._row(key)
            if row.get("duplicateOf"):
                return {k: v for k, v in row.items() if not k.startswith("_") and k != "projectRules"}
            metadata = auth_metadata(row["home"])
            expected = row.get("accountId")
            credential_identity = row.get("_credentialIdentity")
            if (expected and metadata.get("accountId") != expected) or (
                credential_identity
                and metadata.get("_credentialIdentity") != credential_identity
            ):
                row.update(
                    status="changed",
                    error="This profile's account changed. Restore its original login or add a separate profile.",
                )
            elif (
                row.get("source") == "Codex Agents"
                and row.get("status") in {"pending", "error"}
                and metadata["status"] == "signedOut"
            ):
                pass
            else:
                row.update(metadata)
                if metadata["status"] != "error":
                    row.pop("error", None)
            return {k: v for k, v in row.items() if not k.startswith("_") and k != "projectRules"}

    def get(self, key):
        with self.lock:
            return self.refresh(key)

    def legacy_project_defaults(self):
        """Read old directory rules only as migration hints, never permissions."""
        with self.lock:
            candidates = {}
            for key, row in self.data["accounts"].items():
                rules = row.get("projectRules") or {}
                allowed = rules.get("allowedProjects")
                if not isinstance(allowed, list):
                    continue
                identity = row.get("accountId") or row.get("_credentialIdentity") or key
                for value in allowed:
                    if isinstance(value, str) and value.strip():
                        path = str(Path(value).expanduser().resolve())
                        candidates.setdefault(path, {}).setdefault(identity, key)
            return {
                path: next(iter(owners.values()))
                for path, owners in candidates.items() if len(owners) == 1
            }

    def home(self, key):
        row = self.get(key)
        if row["status"] in {"changed", "error"}:
            raise ValueError(row["error"])
        home = Path(row["home"])
        if key == "default":
            home.mkdir(parents=True, exist_ok=True)
        if not home.is_dir():
            raise ValueError("The Codex profile directory is missing")
        return home

    def list(self):
        with self.lock:
            return [self.get(key) for key, row in self.data["accounts"].items()
                    if not key.startswith("login-") or row.get("status") == "ready"]

    def snapshot(self):
        if not self.discovered:
            return self.discover()
        with self.lock:
            logins = self.login_receipts()
            return {
                "accounts": self.list(),
                "defaultAccountKey": self.data["defaultAccountKey"],
                "logins": logins,
                "supportsDisconnect": True,
            }

    def default(self, key=None):
        with self.lock:
            if key is not None:
                row = self.get(key)
                if row["status"] != "ready" or row.get("disconnected"):
                    raise ValueError("Sign in to this account first")
                self.data["defaultAccountKey"] = key
                self._save()
            return self.data["defaultAccountKey"]

    def disconnect(self, key):
        """Remove a profile from new choices; existing native identities still work."""
        with self.lock:
            row = self._row(key)
            if row.get("disconnected"):
                return self.snapshot()
            if self.data["defaultAccountKey"] == key:
                replacement = next((other for other in self.data["accounts"]
                                    if other != key
                                    and not self.data["accounts"][other].get("disconnected")
                                    and not self.data["accounts"][other].get("duplicateOf")
                                    and self.get(other).get("status") == "ready"), None)
                if replacement is None:
                    raise ValueError("Connect another account before disconnecting the application default.")
                self.data["defaultAccountKey"] = replacement
            row["disconnected"] = True
            self._save()
            return self.snapshot()

    def reconnect(self, key):
        with self.lock:
            row = self.get(key)
            if row.get("status") != "ready":
                raise ValueError("Restore this profile's original login before reconnecting it.")
            self._row(key).pop("disconnected", None)
            self._save()
            return self.snapshot()

    def register(self, home):
        if not isinstance(home, str) or not home.strip():
            raise ValueError("Supply a Codex profile directory")
        path = Path(home).expanduser().resolve()
        metadata = auth_metadata(path)
        if metadata["status"] != "ready":
            raise ValueError(
                "This directory has no readable Codex auth.json. Use Sign in for a new account"
            )
        with self.lock:
            for row in self.data["accounts"].values():
                if (
                    row["home"] == str(path)
                    or metadata.get("accountId")
                    and row.get("accountId") == metadata["accountId"]
                ):
                    return row["id"]
            key = "profile-" + hashlib.sha256(str(path).encode()).hexdigest()[:20]
            label = (
                path.parent.name
                if path.name in {".codex", ".codex-profile"}
                else path.name
            )
            self.data["accounts"][key] = {
                "id": key,
                "home": str(path),
                "label": label,
                "source": "Local profile",
                **metadata,
            }
            self._save()
            return key

    def discover(self):
        """Bounded known profile locations; no recursive credential search."""
        home = Path.home()
        candidates = [self.base_home, home / ".codex"]
        for base, pattern in [
            (home / "Projects", "*/.codex-profile"),
            (home / "Projects", "*/.codex"),
            (home / ".codex/profiles", "*"),
            (home / ".codex-profiles", "*"),
        ]:
            if base.is_dir():
                candidates.extend(sorted(base.glob(pattern))[:1000])
        for path in candidates:
            if (path / "auth.json").is_file():
                try:
                    self.register(str(path))
                except ValueError:
                    continue
        with self.lock:
            self.discovered = True
            self._save()
        return self.snapshot()

    def _login_profile(self, request):
        key = "login-" + request
        home = self.root / key
        home.mkdir(parents=True, exist_ok=True, mode=0o700)
        source = self.base_home / "config.toml"
        if source.exists() and not (home / "config.toml").exists():
            # Auth restrictions from the original account must not bind a new login.
            config = re.sub(
                r"(?m)^\s*(?:forced_chatgpt_account_id|forced_login_method|cli_auth_credentials_store)\s*=.*\n?",
                "",
                source.read_text(),
            )
            fd = os.open(
                home / "config.toml", os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600
            )
            with os.fdopen(fd, "w") as handle:
                handle.write(config)
        for name in ("skills", "plugins", "rules", "AGENTS.md"):
            source = self.base_home / name
            target = home / name
            if source.exists() and not target.exists() and not target.is_symlink():
                target.symlink_to(source)
        self.data["accounts"][key] = {
            "id": key,
            "home": str(home),
            "label": "New account",
            "source": "Codex Agents",
            "accountId": None,
            "email": None,
            "plan": None,
            "status": "pending",
        }
        return key

    def login_receipts(self):
        """Reconcile saved sign-ins from their isolated homes after a lost reply."""
        with self.lock:
            before = json.dumps(self.data, sort_keys=True)
            for request, receipt in self.data.setdefault("logins", {}).items():
                receipt.setdefault("requestId", request)
                if receipt.get("status") in {"ready", "duplicate", "cancelled"}:
                    continue
                if receipt.get("status") == "starting" and time.time() - receipt.get("createdAt", 0) > 30:
                    receipt.update(status="uncertain", error="Sign-in response is unconfirmed. Check status or start a separate attempt.")
                key = receipt["accountKey"]
                row = self.refresh(key)
                if row.get("status") != "ready":
                    continue
                duplicate = next((other for other, value in self.data["accounts"].items()
                                  if other != key and not value.get("duplicateOf")
                                  and value.get("status") == "ready" and row.get("accountId")
                                  and value.get("accountId") == row["accountId"]), None)
                receipt.update(status="duplicate" if duplicate else "ready",
                               accountKey=key, resolvedAccountKey=duplicate or key)
                receipt.pop("error", None)
                for field in ("userCode", "verificationUrl"):
                    receipt.pop(field, None)
                if duplicate:
                    self.data["accounts"][key].update(status="duplicate", duplicateOf=duplicate)
                else:
                    self.data["accounts"][key]["label"] = row.get("email") or "Codex account"
            if json.dumps(self.data, sort_keys=True) != before:
                self._save()
            return [dict(r) for r in self.data["logins"].values()]

    def start_login(self, runtime, request):
        try:
            request = str(uuid.UUID(request))
        except (ValueError, TypeError, AttributeError):
            raise ValueError("Supply a UUID request_id") from None
        # Reserve once, then release the registry lock before native I/O.
        with self.lock:
            self.login_receipts()
            previous = self.data["logins"].get(request)
            if previous:
                return dict(previous)
            key = self._login_profile(request)
            self.data["logins"][request] = {
                "requestId": request, "accountKey": key,
                "status": "starting", "createdAt": time.time(),
            }
            self._save()
        submitted = False
        try:
            server = runtime.connect(key)
            submitted = True
            response = server.call("account/login/start", {"type": "chatgptDeviceCode"}, timeout=20)
            if response.get("type") != "chatgptDeviceCode" or not all(
                isinstance(response.get(k), str) and response[k]
                for k in ("loginId", "verificationUrl", "userCode")
            ):
                raise RuntimeError("Invalid device sign-in response")
            with self.lock:
                receipt = self.data["logins"][request]
                # Completion may arrive before the request's acknowledgement.
                if receipt["status"] not in {"ready", "duplicate", "cancelled", "error"}:
                    receipt.update(status="pending", **{k: response[k] for k in ("loginId", "verificationUrl", "userCode")})
                self._save()
        except Exception:
            with self.lock:
                receipt = self.data["logins"][request]
                if receipt["status"] not in {"ready", "duplicate", "cancelled", "error"}:
                    receipt.update(status="uncertain" if submitted else "error",
                                   error="Sign-in response is unconfirmed. Check its status before starting again." if submitted
                                   else "Could not start sign-in. Try again.")
                    if not submitted:
                        self.data["accounts"][key].update(status="error", error=receipt["error"])
                    self._save()
        with self.lock:
            self.login_receipts()
            return dict(self.data["logins"][request])

    def cancel_login(self, runtime, request):
        with self.lock:
            self.login_receipts()
            receipt = self.data["logins"].get(request)
            if not receipt:
                raise ValueError("Unknown sign-in request")
            if receipt["status"] in {"ready", "duplicate", "cancelled", "error"}:
                return dict(receipt)
            if not receipt.get("loginId"):
                raise ValueError("The sign-in response is still unknown. Check its status first.")
            key, login_id = receipt["accountKey"], receipt["loginId"]
        try:
            result = runtime.connect(key).call("account/login/cancel", {"loginId": login_id}, timeout=10)
            if result.get("status") not in {"canceled", "notFound"}:
                raise RuntimeError("Invalid cancellation response")
        except Exception:
            with self.lock:
                receipt["error"] = "Cancellation is unconfirmed. Check status or retry cancellation."
                self._save()
                return dict(receipt)
        with self.lock:
            self.login_receipts()
            if receipt["status"] not in {"ready", "duplicate"}:
                receipt.update(status="cancelled")
                receipt.pop("error", None)
                receipt.pop("userCode", None)
                receipt.pop("verificationUrl", None)
                self.data["accounts"][key]["status"] = "signedOut"
                self._save()
            return dict(receipt)

    def login_completed(self, key, params):
        with self.lock:
            row = self._row(key)
            receipts = [r for r in self.data.setdefault("logins", {}).values() if r.get("accountKey") == key]
            for receipt in receipts:
                if receipt.get("loginId") and params.get("loginId") and params["loginId"] != receipt["loginId"]:
                    return
                if receipt.get("status") in {"ready", "duplicate", "cancelled"}:
                    return
                if params.get("loginId"):
                    receipt["loginId"] = params["loginId"]
                if not params.get("success"):
                    receipt.update(status="error", error="Sign-in failed or expired. Try again.")
                    receipt.pop("userCode", None)
                    receipt.pop("verificationUrl", None)
                    row.update(status="error", error=receipt["error"])
            if params.get("success"):
                self.refresh(key)
                self.login_receipts()
            self._save()
