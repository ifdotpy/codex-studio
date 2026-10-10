"""Audit and block Python access to real per-user Codex/Claude state."""

import os
import sys


if os.environ.get("CODEX_SERVER_TEST_AUDIT_HOME") == "1":
    real_home = os.path.abspath(os.environ["CODEX_SERVER_TEST_REAL_HOME"])
    protected = tuple(os.path.normcase(os.path.abspath(os.path.join(real_home, relative)))
                      for relative in (".codex", ".claude", ".local/state/codex-agents"))
    allowed_value = os.environ.get("CODEX_SERVER_TEST_AUDIT_ALLOW_EXECUTABLE", "")
    allowed_executable = (os.path.normcase(os.path.realpath(allowed_value)) if allowed_value else "")
    audit_fd = os.open(os.environ["CODEX_SERVER_TEST_AUDIT_LOG"],
                       os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)

    def strings(value):
        if isinstance(value, (str, bytes, os.PathLike)):
            try:
                yield os.fsdecode(value)
            except (TypeError, ValueError):
                return
        elif isinstance(value, dict):
            for item in value.values():
                yield from strings(item)
        elif isinstance(value, (tuple, list)):
            for item in value:
                yield from strings(item)

    def is_allowed_codex_value(value):
        if not allowed_executable or not isinstance(value, (str, bytes, os.PathLike)):
            return False
        return os.path.normcase(os.path.realpath(os.fsdecode(value))) == allowed_executable

    def is_protected_path(value):
        if not isinstance(value, (str, bytes, os.PathLike)):
            return False
        path = os.path.normcase(os.path.realpath(os.fsdecode(value)))
        return any(path == root or path.startswith(root + os.sep) for root in protected)

    def reject_real_home(event, args):
        if event not in {"open", "os.mkdir", "os.listdir", "os.scandir", "os.remove",
                         "os.rename", "os.rmdir", "sqlite3.connect",
                         "subprocess.Popen"}:
            return
        values = list(strings(args[:2]))
        if event == "open" and len(args) >= 3 and isinstance(args[0], (str, bytes, os.PathLike)):
            raw_path, mode, flags = args[:3]
            path = os.path.normcase(os.path.realpath(os.fsdecode(raw_path)))
            writing = bool(int(flags) & (os.O_WRONLY | os.O_RDWR | os.O_CREAT | os.O_TRUNC | os.O_APPEND))
            read_mode = isinstance(mode, str) and mode.startswith("r") and "+" not in mode
            if allowed_executable and path == allowed_executable and read_mode and not writing:
                record = f"ALLOWED_READ\t{path}\n".encode("utf-8", "backslashreplace")
                os.write(audit_fd, record)
                return
        if event == "subprocess.Popen":
            executable = args[0] if args else None
            argv = args[1] if len(args) > 1 else None
            executable_path = os.path.normcase(os.path.realpath(os.fsdecode(executable))) if executable else ""
            argv_items = list(argv) if isinstance(argv, (tuple, list)) else [argv]
            argv_paths = [
                os.path.normcase(os.path.realpath(os.fsdecode(value)))
                if isinstance(value, (str, bytes, os.PathLike)) else ""
                for value in argv_items
            ]
            allowed_argv_index = None
            if allowed_executable and argv_paths and argv_paths[0] == allowed_executable:
                allowed_argv_index = 0
            elif allowed_executable and "--" in argv_items:
                wrapper_end = argv_items.index("--")
                if wrapper_end + 1 < len(argv_paths) and argv_paths[wrapper_end + 1] == allowed_executable:
                    allowed_argv_index = wrapper_end + 1
            allowed_direct_executable = bool(allowed_executable and executable_path == allowed_executable)
            if allowed_direct_executable or allowed_argv_index is not None:
                record = f"ALLOWED_EXEC\t{allowed_executable}\n".encode("utf-8", "backslashreplace")
                os.write(audit_fd, record)
                values = list(strings(args[:3]))
                if allowed_direct_executable:
                    allowed_token = executable_path
                else:
                    allowed_token = allowed_executable
                allowed_occurrences = int(allowed_direct_executable) + int(
                    allowed_argv_index is not None)
                for _ in range(allowed_occurrences):
                    for index, value in enumerate(values):
                        if os.path.normcase(os.path.realpath(value)) == allowed_token:
                            del values[index]
                            break
                environment = args[3] if len(args) > 3 else None
            else:
                values = list(strings(args[:2]))
                environment = args[3] if len(args) > 3 else None
            if isinstance(environment, dict):
                isolated_home_keys = ("HOME", "CODEX_HOME", "XDG_CACHE_HOME",
                                      "XDG_STATE_HOME", "XDG_DATA_HOME", "XDG_CONFIG_HOME")
                unsafe_home = next((key for key in isolated_home_keys
                                    if is_protected_path(environment.get(key))), None)
                if unsafe_home:
                    record = f"subprocess.Popen\t{unsafe_home} points into protected home\n".encode()
                    os.write(audit_fd, record)
                    raise PermissionError(f"server test real-home audit blocked subprocess environment: {unsafe_home}")
                filtered_keys = {key for key in (
                    "CODEX_BIN", "CODEX_SERVER_TEST_AUDIT_ALLOW_EXECUTABLE"
                ) if is_allowed_codex_value(environment.get(key))}
                if is_allowed_codex_value(environment.get("CODEX_BIN")):
                    os.write(audit_fd, b"ALLOWED_ISOLATED_ENV\tCODEX_BIN passed with isolated homes\n")
                values.extend(value for key, value in environment.items() if key not in filtered_keys)
            else:
                values.extend(strings(environment))
        for value in values:
            path = os.path.normcase(os.path.realpath(value))
            for root in protected:
                if path == root or path.startswith(root + os.sep):
                    record = f"{event}\t{path}\n".encode("utf-8", "backslashreplace")
                    os.write(audit_fd, record)
                    raise PermissionError(f"server test real-home audit blocked {event}: {path}")

    sys.addaudithook(reject_real_home)
