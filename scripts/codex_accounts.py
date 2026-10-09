"""Local Codex profiles. Store identity metadata, never copies of credentials."""

import base64
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import tempfile
import threading
import uuid
import time
from typing import TYPE_CHECKING

from codex_state import codex_home
from codex_private_paths import ensure_private_dir, protect_temp_file
from studio_api.accounts.events import publish_account_change

if TYPE_CHECKING:
    from codex_records import AccountDataRecord, AccountSnapshotRecord


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
                "deleteReceipts": {},
            }
            self._save()

    def _save(self):
        ensure_private_dir(self.root)
        fd, name = tempfile.mkstemp(prefix="registry-", dir=self.root)
        try:
            protect_temp_file(name)
            with os.fdopen(fd, "w") as handle:
                fd = -1
                json.dump(self.data, handle)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(name, self.path)
        finally:
            if fd >= 0:
                os.close(fd)
            if os.path.exists(name):
                os.unlink(name)

    def _row(self, key: object) -> "AccountDataRecord":
        if (
            not isinstance(key, str)
            or not re.fullmatch(r"[A-Za-z0-9-]{1,100}", key)
            or key not in self.data["accounts"]
        ):
            raise ValueError("Unknown Codex account")
        return self.data["accounts"][key]

    def refresh(self, key, verified_metadata=None):
        import copy
        for _ in range(2):
            with self.lock:
                row = self._row(key)
                if row.get("duplicateOf"):
                    return {k: v for k, v in row.items() if not k.startswith("_")}
                provider, home = row.get("provider"), row.get("home")
                claude_options = copy.deepcopy(row.get("claudeOptions"))
            if provider == "claude" and verified_metadata is not None:
                metadata = verified_metadata
            elif provider == "claude":
                from codex_claude import auth_metadata as claude_auth
                metadata = claude_auth(claude_options)
            else:
                metadata = auth_metadata(home)
            with self.lock:
                row = self._row(key)
                if row.get("duplicateOf"):
                    return {k: v for k, v in row.items() if not k.startswith("_")}
                if (row.get("provider"), row.get("home"), row.get("claudeOptions")) != (provider, home, claude_options):
                    continue
                expected = row.get("accountId")
                credential_identity = row.get("_credentialIdentity")
                observed = metadata.get("accountId")
                observed_credential = metadata.get("_credentialIdentity")
                if metadata["status"] != "error":
                    row.pop("_authErrorKind", None)
                if (expected and observed and observed != expected) or (
                    credential_identity and observed_credential
                    and observed_credential != credential_identity
                ):
                    row.update(
                        status="changed",
                        error="This profile's account changed. Restore its original login or add a separate profile.",
                    )
                elif metadata["status"] == "ready" and (
                    (expected and not observed)
                    or (credential_identity and not observed_credential)
                    or (not observed and not observed_credential)
                ):
                    row.update(
                        status="error",
                        error="Cannot verify this profile's account. Sign in to its original account again.",
                    )
                elif (
                    row.get("source") == "Codex Agents"
                    and row.get("status") in {"pending", "error"}
                    and metadata["status"] == "signedOut"
                ):
                    pass
                else:
                    # Missing authentication does not erase the original identity.
                    row.update({k: v for k, v in metadata.items()
                                if v or k not in {"accountId", "_credentialIdentity", "email", "plan"}})
                    if metadata["status"] != "error":
                        row.pop("error", None)
                return {k: v for k, v in row.items() if not k.startswith("_")}
        with self.lock:
            row = self._row(key)
            row.update(status="error", error="This profile changed during authentication. Check its settings and try again.")
            return {k: v for k, v in row.items() if not k.startswith("_")}

    def get(self, key):
        return self.refresh(key)

    def home(self, key, *, for_login=False):
        row = self.get(key)
        if not for_login and row["status"] in {"changed", "error"}:
            raise ValueError(row["error"])
        home = Path(row["home"])
        if key == "default":
            home.mkdir(parents=True, exist_ok=True)
        if not home.is_dir():
            raise ValueError("The Codex profile directory is missing")
        return home

    def list(self):
        with self.lock:
            keys = [key for key, row in self.data["accounts"].items()
                    if (not key.startswith("login-") or row.get("status") == "ready") and not row.get("deleted")]
        for key in keys:
            self.refresh(key)
        with self.lock:
            return [{k: v for k, v in row.items() if not k.startswith("_")}
                    for key in keys if not (row := self._row(key)).get("deleted")
                    and (not key.startswith("login-") or row.get("status") == "ready")]

    def snapshot(self, *, refresh=True):
        if refresh:
            if not self.discovered:
                return self.discover()
            logins = self.login_receipts()
            with self.lock:
                keys = list(self.data["accounts"])
            for key in keys:
                self.refresh(key)
        else:
            import copy
        with self.lock:
            if not refresh:
                keys = list(self.data["accounts"])
                logins = [{"requestId": request, **receipt}
                          for request, receipt in self.data.get("logins", {}).items()]
            snapshot = {
                "accounts": [{k: v for k, v in row.items() if not k.startswith("_")}
                             for key in keys if not (row := self._row(key)).get("deleted")
                             and (not key.startswith("login-") or row.get("status") == "ready")],
                "archivedAccounts": [{k: v for k, v in row.items() if not k.startswith("_")}
                                     for key in keys if (row := self._row(key)).get("deleted")],
                "defaultAccountKey": self.data["defaultAccountKey"],
                "logins": logins,
                "supportsDisconnect": True,
                "supportsDelete": True,
            }
            return snapshot if refresh else copy.deepcopy(snapshot)

    def default(self, key=None):
        if key is not None:
            self.get(key)
        with self.lock:
            if key is not None:
                row = self._row(key)
                if row["status"] != "ready" or row.get("disconnected") or row.get("deleted"):
                    raise ValueError("Sign in to this account first")
                self.data["defaultAccountKey"] = key
                self._save()
            return self.data["defaultAccountKey"]

    def disconnect(self, key):
        """Remove a profile from new choices; existing native identities still work."""
        candidates = {row["id"] for row in self.list() if row.get("status") == "ready"}
        with self.lock:
            row = self._row(key)
            if not row.get("disconnected") and self.data["defaultAccountKey"] == key:
                replacement = next((other for other in self.data["accounts"]
                                    if other != key and other in candidates
                                    and not self.data["accounts"][other].get("disconnected")
                                    and not self.data["accounts"][other].get("deleted")
                                    and not self.data["accounts"][other].get("duplicateOf")
                                    and self.data["accounts"][other].get("status") == "ready"), None)
                if replacement is None:
                    raise ValueError("Connect another account before disconnecting the application default.")
                self.data["defaultAccountKey"] = replacement
            if not row.get("disconnected"):
                row["disconnected"] = True
                self._save()
        return self.snapshot()

    def delete(self, key, request_id):
        """Hide an account from new choices while retaining its native identity for old chats."""
        try:
            request = str(uuid.UUID(request_id))
        except (ValueError, TypeError, AttributeError):
            raise ValueError("Supply a UUID request_id") from None
        candidates = {row["id"] for row in self.list() if row.get("status") == "ready"}
        with self.lock:
            receipts = self.data.setdefault("deleteReceipts", {})
            previous = receipts.get(request)
            if previous:
                if previous.get("accountKey") != key:
                    raise ValueError("This delete request id has different content")
            else:
                row = self._row(key)
                if self.data["defaultAccountKey"] == key:
                    replacement = next((other for other, value in self.data["accounts"].items()
                                        if other != key and other in candidates and not value.get("deleted")
                                        and not value.get("disconnected") and not value.get("duplicateOf")
                                        and value.get("status") == "ready"), None)
                    if replacement is None:
                        raise ValueError("Add another connected account before deleting the application default.")
                    self.data["defaultAccountKey"] = replacement
                row["deleted"] = True
                receipts[request] = {"accountKey": key, "deletedAt": time.time()}
                self._save()
        return self.snapshot()

    def set_name(self, key, label, request_id):
        """Save a short account label once for an exact request."""
        if not isinstance(label, str) or len(label.strip()) > 32:
            raise ValueError("The account name must contain at most 32 characters")
        try:
            request = str(uuid.UUID(request_id))
        except (ValueError, TypeError, AttributeError):
            raise ValueError("Supply a UUID request_id") from None
        with self.lock:
            receipts = self.data.setdefault("nameReceipts", {})
            previous = receipts.get(request)
            content = {"accountKey": key, "label": label}
            if previous is not None:
                if previous != content:
                    raise ValueError("This name request id has different content")
            else:
                row = self._row(key)
                if row.get("deleted"):
                    raise ValueError("This account was deleted")
                row["label"] = label.strip()
                receipts[request] = content
                self._save()
        return self.snapshot()

    def reconnect(self, key):
        self.get(key)
        with self.lock:
            row = self._row(key)
            if row.get("status") != "ready":
                raise ValueError("Restore this profile's original login before reconnecting it.")
            self._row(key).pop("disconnected", None)
            self._save()
        return self.snapshot()

    def register(self, home, *, restore_deleted=True):
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
                    if row.get("deleted") and restore_deleted:
                        row.pop("deleted", None)
                        self._save()
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

    def register_claude(self, options=None, label=None, verified_metadata=None):
        """Register native config without copying credentials or starting sign-in."""
        from codex_claude import profile_options, auth_metadata as claude_auth, installed
        options = profile_options(options)
        if not installed(options):
            raise ValueError("The Claude Code executable is missing")
        metadata = verified_metadata if verified_metadata is not None else claude_auth(options, force=True)
        if metadata["status"] != "ready":
            raise ValueError("Sign in to this Claude Code configuration with a subscription first")
        identity = json.dumps({k: options.get(k, "") for k in ("binaryPath", "configDir")}, sort_keys=True)
        key = "claude-profile-" + hashlib.sha256(identity.encode()).hexdigest()[:20]
        if label is not None and (not isinstance(label, str) or not label.strip() or len(label) > 200):
            raise ValueError("Invalid Claude profile label")
        with self.lock:
            existing_key = key in self.data["accounts"]
        if existing_key:
            self.get(key)
        with self.lock:
            if key in self.data["accounts"]:
                existing = self._row(key)
                if (existing.get("status") != "ready"
                        or any(existing.get(field) and existing[field] != metadata.get(field)
                               for field in ("accountId", "_credentialIdentity"))):
                    raise ValueError("Restore this profile's original Claude login")
                if existing.get("claudeOptions") != options:
                    raise ValueError("This Claude configuration already exists. Update its settings")
                if existing.get("deleted"):
                    self.data["accounts"][key].pop("deleted", None)
                    self._save()
                return key
            # The discovered default Claude account predates explicit profile
            # registration and uses the stable key ``claude-local``. Reuse that
            # native identity when Add targets the same default config, including
            # after it was tombstoned; otherwise Add would leave the deleted row
            # hidden and create a second account for the same credentials.
            local = self.data["accounts"].get("claude-local")
            if local and local.get("provider") == "claude":
                local_options = profile_options(local.get("claudeOptions"))
                same_paths = all(
                    local_options.get(field, "") == options.get(field, "")
                    for field in ("binaryPath", "configDir")
                )
                same_login = (
                    not local.get("accountId")
                    or local.get("accountId") == metadata.get("accountId")
                )
                if same_paths and same_login:
                    default_options = profile_options({})
                    if options != default_options:
                        if local_options != default_options and local_options != options:
                            raise ValueError(
                                "This Claude configuration already exists. Update its settings"
                            )
                        local["claudeOptions"] = options
                    local.update(metadata)
                    if label is not None:
                        local["label"] = label.strip()
                    local.pop("deleted", None)
                    self._save()
                    return "claude-local"
            self.data["accounts"][key] = {
                "id": key, "provider": "claude", "claudeOptions": options,
                "home": options.get("configDir") or os.environ.get("CLAUDE_CONFIG_DIR") or str(Path.home() / ".claude"),
                "label": (label or "Claude Code").strip(), "source": "Claude Code", **metadata,
            }
            self._save()
            return key

    def find_claude_config(self, config_dir):
        """Return the registered account for this exact native config path."""
        from codex_claude import profile_options
        target = profile_options({'configDir': config_dir}).get('configDir')
        with self.lock:
            for key, row in self.data['accounts'].items():
                if row.get('provider') != 'claude':
                    continue
                options = profile_options(row.get('claudeOptions'))
                if options.get('configDir') == target:
                    return {**row, 'id': key}
        return None

    def update_claude(self, key, options, label=None):
        """Update launch settings; native identity paths need a separate profile."""
        from codex_claude import profile_options
        options = profile_options(options)
        with self.lock:
            row = self._row(key)
            if row.get("provider") != "claude":
                raise ValueError("Select a Claude Code profile")
            previous = profile_options(row.get("claudeOptions"))
            if any(options.get(k) != previous.get(k) for k in ("binaryPath", "configDir")):
                raise ValueError("Add a separate Claude profile to change the executable or config directory")
            if label is not None and (not isinstance(label, str) or not label.strip() or len(label) > 200):
                raise ValueError("Invalid Claude profile label")
            row["claudeOptions"] = options
            if label is not None:
                row["label"] = label.strip()
            self._save()
        return self.get(key)

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
                    self.register(str(path), restore_deleted=False)
                except ValueError:
                    continue
        from codex_claude import installed, auth_metadata as claude_auth
        if installed():
            metadata = claude_auth()
            with self.lock:
                self.data["accounts"].setdefault("claude-local", {
                    "id": "claude-local", "provider": "claude",
                    "home": str(Path.home() / ".claude"),
                    "label": "Claude Code", "source": "Claude Code", **metadata,
                })
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
            pending = {request: receipt["accountKey"]
                       for request, receipt in self.data.setdefault("logins", {}).items()
                       if receipt.get("status") not in {"ready", "duplicate", "cancelled", "error"}}
        for key in dict.fromkeys(pending.values()):
            self.refresh(key)
        with self.lock:
            before = json.dumps(self.data, sort_keys=True)
            for request, receipt in self.data.setdefault("logins", {}).items():
                receipt.setdefault("requestId", request)
                if receipt.get("status") in {"ready", "duplicate", "cancelled"}:
                    continue
                if pending.get(request) != receipt["accountKey"]:
                    continue
                if receipt.get("status") == "starting" and time.time() - receipt.get("createdAt", 0) > 30:
                    receipt.update(status="uncertain", error="Sign-in response is unconfirmed. Check status or start a separate attempt.")
                key = receipt["accountKey"]
                row = self._row(key)
                if receipt.get("reauthAccountKey"):
                    # Cached credentials do not prove that this sign-in completed.
                    if not receipt.get("nativeCompleted"):
                        continue
                    if row.get("status") != "ready" or row.get("accountId") != receipt["expectedAccountId"]:
                        receipt.update(status="error", error="Sign in with this profile's original account.")
                        continue
                    receipt.update(status="ready", resolvedAccountKey=key)
                    receipt.pop("error", None)
                    receipt.pop("userCode", None)
                    receipt.pop("verificationUrl", None)
                    continue
                if row.get("status") != "ready":
                    continue
                expected_email = receipt.get("emailHint")
                actual_email = row.get("email")
                if expected_email and actual_email and actual_email.casefold() != expected_email.casefold():
                    receipt.update(
                        status="error",
                        email=actual_email,
                        error=f"A different account signed in. Expected {expected_email}.",
                    )
                    home = Path(row.get("home", "")).resolve()
                    if home.parent == self.root.resolve() and home.name == "login-" + request:
                        shutil.rmtree(home, ignore_errors=True)
                    self.data["accounts"].pop(key, None)
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
                    self.data["accounts"][duplicate].pop("deleted", None)
                    self.data["accounts"][key].update(status="duplicate", duplicateOf=duplicate)
                else:
                    self.data["accounts"][key]["label"] = (
                        receipt.get("requestedLabel") or row.get("email") or "Codex account"
                    )
            if json.dumps(self.data, sort_keys=True) != before:
                self._save()
            return [dict(r) for r in self.data["logins"].values()]

    def start_login(self, runtime, request, account_key=None, email=None, label=None):
        try:
            request = str(uuid.UUID(request))
        except (ValueError, TypeError, AttributeError):
            raise ValueError("Supply a UUID request_id") from None
        # Reserve once, then release the registry lock before native I/O.
        self.login_receipts()
        with self.lock:
            previous = self.data["logins"].get(request)
            if previous:
                if (previous.get("reauthAccountKey") != account_key
                        or previous.get("emailHint") != email
                        or previous.get("requestedLabel") != label):
                    raise ValueError("This sign-in request belongs to a different account")
                return dict(previous)
            if account_key is not None:
                row = self._row(account_key)
                if row.get("provider", "codex") != "codex" or not row.get("accountId") or row.get("duplicateOf"):
                    raise ValueError("Select a saved Codex subscription account")
                if any(r.get("accountKey") == account_key and r.get("status") in {"starting", "pending", "uncertain"}
                       for r in self.data["logins"].values()):
                    raise ValueError("Sign-in is already active for this account. Check its status first.")
                key = account_key
            else:
                key = self._login_profile(request)
                if label:
                    self.data["accounts"][key]["label"] = label
            self.data["logins"][request] = {
                "requestId": request, "accountKey": key,
                "status": "starting", "createdAt": time.time(),
            }
            if account_key is None:
                self.data["logins"][request].update(
                    emailHint=email, requestedLabel=label,
                )
            if account_key is not None:
                self.data["logins"][request].update(reauthAccountKey=key, expectedAccountId=row["accountId"], email=row.get("email"))
            self._save()
        submitted = False
        try:
            server = runtime.connect(key, for_login=True) if account_key is not None else runtime.connect(key)
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
                    expiry = response.get("expiresAt")
                    expires_in = response.get("expiresIn")
                    if (
                        expiry is None
                        and isinstance(expires_in, (int, float))
                        and math.isfinite(expires_in)
                        and expires_in > 0
                    ):
                        expiry = time.time() + expires_in
                    if (
                        isinstance(expiry, (int, float))
                        and not isinstance(expiry, bool)
                        and math.isfinite(expiry)
                        and expiry > time.time()
                    ):
                        receipt["expiresAt"] = expiry
                self._save()
        except Exception:
            with self.lock:
                receipt = self.data["logins"][request]
                if receipt["status"] not in {"ready", "duplicate", "cancelled", "error"}:
                    receipt.update(status="uncertain" if submitted else "error",
                                   error="Sign-in response is unconfirmed. Check its status before starting again." if submitted
                                   else "Could not start sign-in. Try again.")
                    if not submitted and account_key is None:
                        self.data["accounts"][key].update(status="error", error=receipt["error"])
                    self._save()
        self.login_receipts()
        with self.lock:
            return dict(self.data["logins"][request])

    def cancel_login(self, runtime, request):
        self.login_receipts()
        with self.lock:
            receipt = self.data["logins"].get(request)
            if not receipt:
                raise ValueError("Unknown sign-in request")
            if receipt["status"] in {"ready", "duplicate", "cancelled", "error"}:
                return dict(receipt)
            if not receipt.get("loginId"):
                raise ValueError("The sign-in response is still unknown. Check its status first.")
            key, login_id = receipt["accountKey"], receipt["loginId"]
        try:
            server = runtime.connect(key, for_login=True) if receipt.get("reauthAccountKey") else runtime.connect(key)
            result = server.call("account/login/cancel", {"loginId": login_id}, timeout=10)
            if result.get("status") not in {"canceled", "notFound"}:
                raise RuntimeError("Invalid cancellation response")
        except Exception:
            with self.lock:
                receipt["error"] = "Cancellation is unconfirmed. Check status or retry cancellation."
                self._save()
                return dict(receipt)
        self.login_receipts()
        with self.lock:
            if receipt["status"] not in {"ready", "duplicate"}:
                receipt.update(status="cancelled")
                receipt.pop("error", None)
                receipt.pop("userCode", None)
                receipt.pop("verificationUrl", None)
                if not receipt.get("reauthAccountKey"):
                    self.data["accounts"][key]["status"] = "signedOut"
                self._save()
            return dict(receipt)

    def login_completed(self, key, params):
        with self.lock:
            before = json.dumps(self.data, sort_keys=True)
            row = self._row(key)
            receipts = [r for r in self.data.setdefault("logins", {}).values() if r.get("accountKey") == key]
            for receipt in receipts:
                if receipt.get("loginId") and params.get("loginId") and params["loginId"] != receipt["loginId"]:
                    continue
                if receipt.get("status") in {"ready", "duplicate", "cancelled"}:
                    continue
                if params.get("loginId"):
                    receipt["loginId"] = params["loginId"]
                if not params.get("success"):
                    receipt.update(status="error", error="Sign-in failed or expired. Try again.")
                    receipt.pop("userCode", None)
                    receipt.pop("verificationUrl", None)
                    if not receipt.get("reauthAccountKey"):
                        row.update(status="error", error=receipt["error"])
                elif receipt.get("reauthAccountKey"):
                    receipt["nativeCompleted"] = True
        if params.get("success"):
            self.refresh(key)
            self.login_receipts()
        with self.lock:
            self._save()
            changed = json.dumps(self.data, sort_keys=True) != before
        if changed:
            publish_account_change(self.root.parent)
