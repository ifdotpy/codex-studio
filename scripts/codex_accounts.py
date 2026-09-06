"""Local Codex profiles. Store identity metadata, never copies of credentials."""

import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import threading
import uuid

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
            return {k: v for k, v in row.items() if not k.startswith("_")}

    def get(self, key):
        with self.lock:
            result = self.refresh(key)
            result["projectRules"] = self._rule_owner(key).setdefault(
                "projectRules", {"allowedProjects": None, "revision": 0}
            )
            return result

    def _rule_owner(self, key):
        row = self._row(key)
        identity = row.get("accountId") or row.get("_credentialIdentity")
        if identity:
            for candidate in self.data["accounts"].values():
                if (
                    candidate.get("accountId") or candidate.get("_credentialIdentity")
                ) == identity:
                    return candidate
        return row

    def set_project_rules(self, key, allowed, expected_revision):
        if type(expected_revision) is not int or expected_revision < 0:
            raise ValueError("Supply the current project rule revision")
        if allowed is not None:
            if not isinstance(allowed, list) or len(allowed) > 128:
                raise ValueError("Supply up to 128 project directories")
            normalized = []
            for value in allowed:
                if not isinstance(value, str) or not value.strip():
                    raise ValueError("Each project needs a directory path")
                path = Path(value).expanduser().resolve()
                if not path.is_dir():
                    raise ValueError("A project directory does not exist: " + str(path))
                if str(path) not in normalized:
                    normalized.append(str(path))
            allowed = normalized
        with self.lock:
            previous = self.get(key)["projectRules"]
            if previous["revision"] != expected_revision:
                if previous["allowedProjects"] == allowed:
                    return dict(previous)
                raise ValueError("Project rules changed. Reload them before saving")
            if previous["allowedProjects"] == allowed:
                return dict(previous)
            rules = {"allowedProjects": allowed, "revision": expected_revision + 1}
            owner = self._rule_owner(key)
            owner["projectRules"] = rules
            try:
                self._save()
            except Exception:
                owner["projectRules"] = previous
                raise
            return dict(rules)

    @staticmethod
    def _git(path, *args):
        try:
            result = subprocess.run(
                ["git", "-C", str(path), *args],
                capture_output=True,
                text=True,
                timeout=3,
                check=False,
                env={k: v for k, v in os.environ.items() if not k.startswith("GIT_")},
            )
            return result.stdout.strip() if result.returncode == 0 else None
        except (OSError, subprocess.TimeoutExpired):
            return None

    @classmethod
    def _linked_worktree(cls, cwd, allowed):
        target_top = cls._git(cwd, "rev-parse", "--show-toplevel")
        source_top = cls._git(allowed, "rev-parse", "--show-toplevel")
        if not target_top or not source_top:
            return False
        target_top, source_top = Path(target_top).resolve(), Path(source_top).resolve()
        target_common = cls._git(
            cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"
        )
        source_common = cls._git(
            allowed, "rev-parse", "--path-format=absolute", "--git-common-dir"
        )
        if (
            not target_common
            or not source_common
            or Path(target_common).resolve() != Path(source_common).resolve()
        ):
            return False
        entries = cls._git(allowed, "worktree", "list", "--porcelain", "-z")
        registered = {
            Path(part[9:]).resolve()
            for part in (entries or "").split("\0")
            if part.startswith("worktree ")
        }
        if target_top not in registered:
            return False
        try:
            # A rule for a subdirectory grants the matching subtree, not the whole repository.
            subtree = allowed.relative_to(source_top)
            cwd.relative_to(target_top / subtree)
            return True
        except ValueError:
            return False

    def project_allowed(self, key, cwd):
        allowed = self.get(key)["projectRules"]["allowedProjects"]
        if allowed is None:
            return True
        if not isinstance(cwd, (str, Path)) or not str(cwd).strip():
            return False
        directory = Path(cwd).expanduser().resolve()
        for value in allowed:
            root = Path(value)
            # Replacing a saved root with a symlink does not grant a different project.
            if root.resolve() != root or not root.is_dir():
                continue
            if directory == root or root in directory.parents:
                return True
            if self._linked_worktree(directory, root):
                return True
        return False

    def check_project(self, key, cwd, skip=False):
        if type(skip) is not bool:
            raise ValueError("Dangerously skip rules must be true or false")
        account = self.get(key)
        if skip or self.project_allowed(key, cwd):
            return
        allowed = account["projectRules"]["allowedProjects"]
        roots = ", ".join(allowed) if allowed else "none"
        raise ValueError(
            f"Account {account.get('email') or account['label']} cannot use project {cwd}. "
            f"Allowed projects: {roots}. Choose another account or enable Dangerously skip rules for this team"
        )

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
            return [self.get(key) for key in self.data["accounts"]]

    def snapshot(self):
        if not self.discovered:
            return self.discover()
        with self.lock:
            return {
                "accounts": self.list(),
                "defaultAccountKey": self.data["defaultAccountKey"],
            }

    def default(self, key=None):
        with self.lock:
            if key is not None:
                row = self.get(key)
                if row["status"] != "ready":
                    raise ValueError("Sign in to this account first")
                self.data["defaultAccountKey"] = key
                self._save()
            return self.data["defaultAccountKey"]

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

    def start_login(self, runtime, request):
        try:
            request = str(uuid.UUID(request))
        except (ValueError, TypeError, AttributeError):
            raise ValueError("Supply a UUID request_id") from None
        with self.login_lock:
            with self.lock:
                previous = self.data["logins"].get(request)
                if previous:
                    return dict(previous)
                key = self._login_profile(request)
                self.data["logins"][request] = {
                    "accountKey": key,
                    "status": "uncertain",
                    "error": "The login response was not recorded. Start a new sign-in attempt",
                }
                self._save()
            try:
                response = runtime.connect(key).call(
                    "account/login/start", {"type": "chatgptDeviceCode"}, timeout=20
                )
                if response.get("type") != "chatgptDeviceCode" or not all(
                    isinstance(response.get(k), str) and response[k]
                    for k in ("loginId", "verificationUrl", "userCode")
                ):
                    raise RuntimeError("The Codex device sign-in response is invalid")
                result = {
                    "accountKey": key,
                    "status": "pending",
                    **{
                        k: response[k]
                        for k in ("loginId", "verificationUrl", "userCode")
                    },
                }
            except Exception:
                result = {
                    "accountKey": key,
                    "status": "error",
                    "error": "Codex could not start device sign-in. Check the connection and start a new attempt",
                }
            with self.lock:
                self.data["logins"][request] = result
                if result["status"] == "error":
                    self.data["accounts"][key].update(
                        status="error", error=result["error"]
                    )
                self._save()
            return result

    def login_completed(self, key, params):
        with self.lock:
            row = self._row(key)
            if params.get("success"):
                self.refresh(key)
            else:
                row.update(
                    status="error",
                    error="Codex sign-in failed or expired. Start a new attempt",
                )
            self._save()
