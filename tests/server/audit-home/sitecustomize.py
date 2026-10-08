"""Audit and block Python access to real per-user Codex/Claude state."""

import os
import sys


if os.environ.get("CODEX_SERVER_TEST_AUDIT_HOME") == "1":
    real_home = os.path.abspath(os.environ["CODEX_SERVER_TEST_REAL_HOME"])
    protected = tuple(os.path.normcase(os.path.abspath(os.path.join(real_home, relative)))
                      for relative in (".codex", ".claude", ".local/state/codex-agents"))
    allowed_executable = os.path.normcase(os.path.realpath(
        os.environ.get("CODEX_SERVER_TEST_AUDIT_ALLOW_EXECUTABLE", "")
    ))
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
                         "os.rename", "os.rmdir", "os.stat", "sqlite3.connect",
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
            argv0 = argv[0] if isinstance(argv, (tuple, list)) and argv else None
            argv0_path = os.path.normcase(os.path.realpath(os.fsdecode(argv0))) if argv0 else ""
            argv_values = [os.fsdecode(item) for item in argv if isinstance(item, (str, bytes, os.PathLike))] if isinstance(argv, (tuple, list)) else []
            after_separator = next((argv_values[index + 1] for index, item in enumerate(argv_values[:-1])
                                    if item == "--"), None)
            indirect_path = (os.path.normcase(os.path.realpath(after_separator))
                             if after_separator else "")
            if allowed_executable and allowed_executable in {executable_path, argv0_path, indirect_path}:
                record = f"ALLOWED_EXEC\t{allowed_executable}\n".encode("utf-8", "backslashreplace")
                os.write(audit_fd, record)
                values = [value for value in strings(args[:3])
                          if os.path.normcase(os.path.realpath(value)) != allowed_executable]
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
                if filtered_keys:
                    os.write(audit_fd, f"ALLOWED_CONFIG\t{allowed_executable}\n".encode())
                    if is_allowed_codex_value(environment.get("CODEX_BIN")):
                        os.write(audit_fd, b"ALLOWED_ISOLATED_ENV\tCodex child homes outside protected state\n")
                values.extend(value for key, value in environment.items() if key not in filtered_keys)
            else:
                values.extend(strings(environment))
        for value in values:
            path = os.path.normcase(os.path.abspath(value))
            for root in protected:
                if path == root or path.startswith(root + os.sep):
                    record = f"{event}\t{path}\n".encode("utf-8", "backslashreplace")
                    os.write(audit_fd, record)
                    raise PermissionError(f"server test real-home audit blocked {event}: {path}")

    sys.addaudithook(reject_real_home)
