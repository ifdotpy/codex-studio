"""Bounded, resumable tree uploads. An upload identity is never applied twice."""
from __future__ import annotations

import hashlib
import fcntl
import json
import os
from contextlib import contextmanager
from pathlib import Path, PurePosixPath
import shutil
import tarfile

from common import GuestError, bounded_lock, decode, integer, private_dir, require

MAX_TREE = 256 * 1024**3
MAX_MEMBERS = 1_000_000


def sha(value):
    require(isinstance(value, str) and len(value) == 64
            and all(char in "0123456789abcdef" for char in value), "sha256 must contain 64 lowercase hexadecimal characters")
    return value


def upload_id(value):
    require(isinstance(value, str) and 0 < len(value) <= 128
            and all(char.isascii() and (char.isalnum() or char in "-_") for char in value),
            "uploadId must contain ASCII letters, digits, hyphens, or underscores")
    return value


def tree_space(path, required=0):
    floor = int(os.environ.get("CODEX_WORKSPACE_MIN_FREE_BYTES", 20 * 1024**3))
    if floor < 0 or shutil.disk_usage(path).free < floor + required:
        raise GuestError("busy", "The data disk has insufficient free space")


@contextmanager
def real_directory(root, parts=(), *, create=False, missing_ok=False):
    """Open each parent without following a link, including links inside the root."""
    descriptor = None
    try:
        descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        for part in parts:
            require(part not in {"", ".", ".."} and "/" not in part, "The path component is invalid")
            if create:
                try:
                    os.mkdir(part, mode=0o700, dir_fd=descriptor)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
    except FileNotFoundError as exc:
        if descriptor is not None:
            os.close(descriptor)
        descriptor = None
        if not missing_ok:
            raise GuestError("invalid_params", "The path parent is not a real directory") from exc
    except OSError as exc:
        if descriptor is not None:
            os.close(descriptor)
        descriptor = None
        raise GuestError("invalid_params", "The path parent is not a real directory") from exc
    except GuestError:
        if descriptor is not None:
            os.close(descriptor)
        raise
    try:
        yield descriptor
    finally:
        if descriptor is not None:
            os.close(descriptor)


def validate_destination(staging, root):
    """Reject writes below existing links before changing the project tree."""
    links = []
    for parent, directories, files in os.walk(staging, followlinks=False):
        for name in directories + files:
            path = Path(parent) / name
            relative = path.relative_to(staging)
            is_link = path.is_symlink()
            parts = relative.parts if not is_link and path.is_dir() else relative.parts[:-1]
            with real_directory(root, parts, missing_ok=True):
                pass
            if is_link:
                links.append((relative, os.readlink(path)))
    return links


def extract_tree(archive: Path, destination: Path):
    """Treat link targets as text. Create links after all files and directories."""
    expanded = 0
    members = 0
    links = []
    link_paths = set()
    with tarfile.open(archive, "r|*") as source:
        for item in source:
            members += 1
            require(members <= MAX_MEMBERS, "The archive has too many entries")
            name = PurePosixPath(item.name)
            require(not name.is_absolute() and ".." not in name.parts, "The archive path is outside the tree")
            if str(name) == ".":
                require(item.isdir(), "The archive root must be a directory")
                continue
            target = destination.joinpath(*name.parts)
            require(target.is_relative_to(destination), "The archive path is outside the tree")
            require(item.isfile() or item.isdir() or item.issym(), "The archive entry type is not supported")
            require(not any(parent in link_paths for parent in name.parents), "The archive path has a symlink parent")
            if item.issym():
                require(name not in link_paths and not target.exists(), "The archive path occurs more than once")
                link_paths.add(name)
                links.append((target, item.linkname))
                continue
            require(name not in link_paths, "The archive path occurs more than once")
            expanded += item.size
            require(expanded <= MAX_TREE, "The expanded tree exceeds the size limit")
            tree_space(destination, item.size)
            with real_directory(destination, name.parts[:-1], create=True) as parent:
                if item.isdir():
                    try:
                        os.mkdir(name.name, mode=0o700, dir_fd=parent)
                    except FileExistsError:
                        pass
                    with real_directory(destination, name.parts) as directory:
                        os.fchmod(directory, 0o700 | (item.mode & 0o077))
                else:
                    descriptor = os.open(name.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                                         0o600, dir_fd=parent)
                    with os.fdopen(descriptor, "wb") as output, source.extractfile(item) as input_file:
                        while block := input_file.read(1024 * 1024):
                            tree_space(destination, len(block))
                            output.write(block)
                        output.flush()
                        os.fsync(output.fileno())
                        os.fchmod(output.fileno(), 0o600 | (item.mode & 0o177))
    for target, link in links:
        with real_directory(destination, target.relative_to(destination).parts[:-1], create=True) as parent:
            os.symlink(link, target.name, dir_fd=parent)
    return expanded


def prepare_tree(archive, destination, expected):
    """Restart extraction only after taking the surviving worker's file lease."""
    require(destination == archive.parent / "expanded", "The extraction destination differs from the upload directory")
    with (archive.parent / "extract.lock").open("a+") as lease:
        try:
            fcntl.flock(lease, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as exc:
            raise GuestError("busy", "The upload extraction is still in progress") from exc
        with archive.open("rb") as source:
            actual = hashlib.file_digest(source, "sha256").hexdigest()
        require(actual == sha(expected), "The archive checksum differs")
        if destination.is_symlink() or destination.is_file():
            destination.unlink()
        elif destination.exists():
            shutil.rmtree(destination)
        destination.mkdir(mode=0o700)
        try:
            return extract_tree(archive, destination)
        except BaseException:
            shutil.rmtree(destination, ignore_errors=True)
            raise


class Uploads:
    def __init__(self, state, projects, database):
        self.path = state / "uploads"
        private_dir(self.path)
        self.projects = projects
        self.db = database
        self.locks = {}
        self.db.executescript("""
            CREATE TABLE IF NOT EXISTS uploads (id TEXT PRIMARY KEY, root TEXT, total INTEGER,
                digest TEXT, received INTEGER DEFAULT 0, next_seq INTEGER DEFAULT 0,
                state TEXT DEFAULT 'receiving', result TEXT, mode TEXT, deletes TEXT);
            CREATE TABLE IF NOT EXISTS upload_chunks (id TEXT, seq INTEGER, digest TEXT,
                size INTEGER, PRIMARY KEY(id,seq));
        """)
        self.db.commit()

    def row(self, identity):
        row = self.db.execute("SELECT root,total,digest,received,next_seq,state,result,mode,deletes FROM uploads WHERE id=?", (identity,)).fetchone()
        if row is None:
            raise GuestError("not_found", "The upload does not exist")
        return row

    def status(self, identity, row):
        return {"uploadId": identity, "root": row[0], "totalBytes": row[1],
                "receivedBytes": row[3], "nextSeq": row[4], "state": row[5], "mode": row[7]}

    async def run(self, method, params, apply):
        identity = upload_id(params.get("uploadId"))
        async with bounded_lock(self.locks, identity):
            if method == "upload.begin":
                root = Path(params.get("root", ""))
                require(root.is_absolute() and root.parent.resolve() == self.projects,
                        "The upload root must be directly under projects")
                require(not root.is_symlink(), "The project root must not be a symlink")
                total = integer(params.get("totalBytes"), None, 0, MAX_TREE)
                expected = sha(params.get("sha256"))
                mode = params.get("mode", "full")
                require(mode in {"full", "delta"}, "The upload mode must be full or delta")
                deletes = params.get("deletePaths", [])
                require(isinstance(deletes, list) and len(deletes) <= MAX_MEMBERS, "deletePaths must be a bounded list")
                for item in deletes:
                    require(isinstance(item, str) and item and str(PurePosixPath(item)) != "."
                            and not PurePosixPath(item).is_absolute() and ".." not in PurePosixPath(item).parts,
                            "A deletion path is outside the project root")
                deletes_json = json.dumps(sorted(set(deletes)))
                require(mode == "delta" or not deletes, "A full upload must not have deletePaths")
                row = self.db.execute("SELECT root,total,digest,received,next_seq,state,result,mode,deletes FROM uploads WHERE id=?", (identity,)).fetchone()
                if row:
                    if (row[0], row[1], row[2], row[7], row[8]) != (str(root), total, expected, mode, deletes_json):
                        raise GuestError("id_conflict", "The upload ID has different content")
                    return self.status(identity, row)
                require(self.db.execute("SELECT count(*) FROM uploads WHERE state='receiving'").fetchone()[0] < 32,
                        "The upload count exceeds the limit")
                tree_space(self.path, total)
                directory = self.path / identity
                private_dir(directory)
                with (directory / "tree.tar").open("xb"):
                    pass
                self.db.execute("INSERT INTO uploads(id,root,total,digest,mode,deletes) VALUES (?,?,?,?,?,?)",
                                (identity, str(root), total, expected, mode, deletes_json))
                self.db.commit()
                return self.status(identity, self.row(identity))
            row = self.row(identity)
            if method == "upload.chunk":
                sequence = integer(params.get("seq"), None, 0, MAX_TREE)
                data = decode(params.get("data"))
                require(len(data) > 0, "The upload chunk must contain data")
                chunk_digest = hashlib.sha256(data).hexdigest()
                prior = self.db.execute("SELECT digest,size FROM upload_chunks WHERE id=? AND seq=?",
                                        (identity, sequence)).fetchone()
                if prior:
                    if prior != (chunk_digest, len(data)):
                        raise GuestError("id_conflict", "The upload chunk has different content")
                    return self.status(identity, row)
                require(row[5] == "receiving" and sequence == row[4], "The upload chunk is out of order")
                require(row[3] + len(data) <= row[1], "The upload exceeds totalBytes")
                tree_space(self.path, len(data))
                with (self.path / identity / "tree.tar").open("r+b") as output:
                    output.seek(row[3])
                    output.write(data)
                    output.truncate()
                    output.flush()
                    os.fsync(output.fileno())
                with self.db:
                    self.db.execute("INSERT INTO upload_chunks VALUES (?,?,?,?)", (identity, sequence, chunk_digest, len(data)))
                    self.db.execute("UPDATE uploads SET received=received+?,next_seq=next_seq+1 WHERE id=?", (len(data), identity))
                return self.status(identity, self.row(identity))
            if method == "upload.commit":
                if row[5] == "applied":
                    return json.loads(row[6])
                if row[5] != "receiving":
                    raise GuestError("outcome_unknown", "The upload application has no proven result")
                require(row[3] == row[1], "The upload is incomplete")
                archive = self.path / identity / "tree.tar"
                # Extraction and hash checks run in a bounded worker, before the apply point.
                result = await apply(identity, Path(row[0]), archive, row[2])
                return result
            raise GuestError("invalid_params", "Unknown upload method")


if __name__ == "__main__":
    import sys
    archive, destination, expected = sys.argv[1:4]
    try:
        archive = Path(archive)
        destination = Path(destination)
        expanded = prepare_tree(archive, destination, expected)
        if len(sys.argv) == 5:
            validate_destination(destination, Path(sys.argv[4]))
        print(json.dumps({"bytes": expanded}))
    except GuestError as exc:
        print(json.dumps({"error": exc.object()}))
        sys.exit(1)
    except (OSError, tarfile.TarError):
        print(json.dumps({"error": {"code": "invalid_params", "message": "The archive cannot be extracted"}}))
        sys.exit(1)
