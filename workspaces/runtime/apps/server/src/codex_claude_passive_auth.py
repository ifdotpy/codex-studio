"""Read native Claude subscription metadata without displaying Keychain dialogs."""
from __future__ import annotations

import ctypes
import json
import os
from pathlib import Path
import pwd
import re
import unicodedata
from typing import Any

from codex_linux_vm_credentials import MAX_CREDENTIAL_BYTES, claude_keychain_service


class KeychainInteractionError(PermissionError):
    """Only the Security framework can authorize the native-proof fallback."""


def read_keychain(config_dir: str | None) -> dict[str, Any] | None:
    security = ctypes.CDLL('/System/Library/Frameworks/Security.framework/Security')
    security.SecKeychainSetUserInteractionAllowed.argtypes = [ctypes.c_ubyte]
    security.SecKeychainSetUserInteractionAllowed.restype = ctypes.c_int32
    security.SecKeychainFindGenericPassword.argtypes = [
        ctypes.c_void_p, ctypes.c_uint32, ctypes.c_char_p,
        ctypes.c_uint32, ctypes.c_char_p, ctypes.POINTER(ctypes.c_uint32),
        ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p,
    ]
    security.SecKeychainFindGenericPassword.restype = ctypes.c_int32
    security.SecKeychainItemFreeContent.argtypes = [ctypes.c_void_p, ctypes.c_void_p]
    security.SecKeychainItemFreeContent.restype = ctypes.c_int32
    # This setting is process-wide. This isolated helper never runs sign-in.
    if security.SecKeychainSetUserInteractionAllowed(False) != 0:
        raise OSError('Cannot disable Keychain interaction')
    storage_dir = os.environ.get('CLAUDE_SECURESTORAGE_CONFIG_DIR', config_dir)
    service = claude_keychain_service(
        unicodedata.normalize('NFC', storage_dir) if storage_dir else None,
    ).encode()
    try:
        username = os.environ.get('USER') or pwd.getpwuid(os.getuid()).pw_name
    except (KeyError, OSError):
        username = 'claude-code-user'
    account = (username if re.fullmatch(r'[a-zA-Z0-9._-]+', username)
               else 'claude-code-user').encode()
    size, data = ctypes.c_uint32(), ctypes.c_void_p()
    status = security.SecKeychainFindGenericPassword(
        None, len(service), service, len(account), account,
        ctypes.byref(size), ctypes.byref(data), None,
    )
    try:
        if status == -25300:  # errSecItemNotFound, not a denied or locked entry.
            return None
        if status in (-25308, -25293, -128, -25291):
            raise KeychainInteractionError('Cannot read Claude Keychain credentials without interaction')
        if status != 0:
            raise OSError('Claude Keychain metadata read failed')
        if not data.value or size.value > MAX_CREDENTIAL_BYTES:
            raise ValueError('Invalid Claude credential size')
        value = json.loads(ctypes.string_at(data, size.value))
        if not isinstance(value, dict):
            raise ValueError('Invalid Claude credential object')
        return value
    finally:
        if data.value:
            security.SecKeychainItemFreeContent(None, data)


def read_json(path: Path) -> dict[str, Any]:
    with path.open('rb') as stream:
        data = stream.read(MAX_CREDENTIAL_BYTES + 1)
    if len(data) > MAX_CREDENTIAL_BYTES:
        raise ValueError('Invalid Claude metadata size')
    value = json.loads(data)
    if not isinstance(value, dict):
        raise ValueError('Invalid Claude metadata object')
    return value


def metadata() -> dict[str, Any]:
    result: dict[str, Any] = {'status': 'signedOut', 'accountId': None, 'email': None, 'plan': None}
    config_dir = os.environ.get('CLAUDE_CONFIG_DIR') or None
    directory = Path(config_dir) if config_dir else Path.home() / '.claude'
    credentials = read_keychain(config_dir)
    if credentials is None:
        try:
            credentials = read_json(directory / '.credentials.json')
        except FileNotFoundError:
            return result
    oauth = credentials.get('claudeAiOauth')
    if not isinstance(oauth, dict) or not oauth.get('accessToken'):
        return result
    if (not isinstance(oauth['accessToken'], str)
            or not isinstance(oauth.get('scopes'), list)
            or 'user:inference' not in oauth['scopes']):
        raise ValueError('Invalid Claude subscription metadata')
    config_path = directory / '.claude.json' if config_dir else Path.home() / '.claude.json'
    if (directory / '.config.json').is_file():
        config_path = directory / '.config.json'
    config = read_json(config_path)
    identity = config.get('oauthAccount', {}).get('emailAddress')
    if not isinstance(identity, str) or not identity:
        raise ValueError('Missing Claude account identity')
    plan = oauth.get('subscriptionType')
    if plan is not None and not isinstance(plan, str):
        raise ValueError('Invalid Claude subscription type')
    result.update(status='ready', accountId='claude:' + identity, email=identity,
                  plan=plan, _credentialIdentity='claude:' + identity)
    return result


def main() -> None:
    try:
        result = metadata()
    except KeychainInteractionError:
        result = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  'error': 'Cannot read Claude Code sign-in status without Keychain interaction',
                  '_authErrorKind': 'keychain'}
    except (OSError, ValueError, TypeError, AttributeError):
        result = {'status': 'error', 'accountId': None, 'email': None, 'plan': None,
                  'error': 'Cannot read Claude Code sign-in status', '_authErrorKind': 'parser'}
    print(json.dumps(result))


if __name__ == '__main__':
    main()
