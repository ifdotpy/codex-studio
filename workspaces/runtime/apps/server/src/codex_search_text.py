"""Resolve complete searchable message text from its durable sources."""
import json


def _table(db, name):
    return db.execute(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?", (name,)
    ).fetchone() is not None


def search_texts(db, refs):
    """Return complete text for the requested runtime item identities.

    During the old-index migration, legacy FTS remains a fallback for truncated
    records until their body has been copied to runtime_item_fulltext.
    """
    refs = list(dict.fromkeys(ref for ref in refs if isinstance(ref, str) and ref))
    if not refs:
        return {}
    found = {}
    fulltext = _table(db, "runtime_item_fulltext")
    legacy = _table(db, "runtime_search")
    addresses = _table(db, "runtime_search_rows")
    for offset in range(0, len(refs), 400):
        batch = refs[offset:offset + 400]
        marks = ",".join("?" for _ in batch)
        rows = db.execute(
            f"SELECT id,agent,record FROM runtime_items WHERE id IN ({marks})", batch
        ).fetchall()
        records = {row["id"]: (row["agent"], json.loads(row["record"])) for row in rows}
        if fulltext:
            for row in db.execute(
                f"SELECT id,body FROM runtime_item_fulltext WHERE id IN ({marks})", batch
            ):
                if row["body"] is not None:
                    found[row["id"]] = row["body"]
        truncated = {ref for ref, (_, record) in records.items() if record.get("truncated")}
        if legacy and truncated:
            legacy_batch = [ref for ref in batch if ref in truncated and ref not in found]
            marks_old = ",".join("?" for _ in legacy_batch)
            if addresses:
                old_rows = db.execute(
                    "SELECT a.id,s.body FROM runtime_search_rows a "
                    "JOIN runtime_search s ON s.rowid=a.search_rowid "
                    f"WHERE a.id IN ({marks_old})", legacy_batch
                )
            else:
                old_rows = db.execute(
                    "SELECT id,body FROM runtime_search WHERE id IN (" + marks_old + ")", legacy_batch
                )
            for row in old_rows:
                if row["id"] not in found:
                    found[row["id"]] = row["body"] or ""
        for ref, (agent, record) in records.items():
            if ref not in found:
                text = record.get("text")
                if isinstance(text, str) and (text or not record.get("inputs")) and not record.get("truncated"):
                    found[ref] = text
        # Inputs are normally represented by the parent item text. Resolve
        # event or embedded text for legacy records that have no parent text.
        event_refs = {}
        input_text = {}
        for ref, (agent, record) in records.items():
            if ref in found or not isinstance(record.get("inputs"), list):
                continue
            for entry in record["inputs"]:
                identity = entry.get("id")
                if isinstance(identity, str) and identity:
                    event_refs[(identity, agent)] = ref
                elif isinstance(entry.get("text"), str):
                    input_text.setdefault(ref, []).append(entry["text"])
        event_ids = list(event_refs)
        for event_offset in range(0, len(event_ids), 200):
            pairs = event_ids[event_offset:event_offset + 200]
            event_marks = ",".join("(?,?)" for _ in pairs)
            if event_marks:
                args = [value for pair in pairs for value in pair]
                for row in db.execute(
                    "SELECT id,agent,text FROM runtime_events WHERE (id,agent) IN (" + event_marks + ")",
                    args,
                ):
                    input_text.setdefault(event_refs[(row["id"], row["agent"])], []).append(row["text"])
        for ref, pieces in input_text.items():
            if ref and ref not in found:
                found[ref] = "\n\n".join(pieces)
    return found


def search_text(db, ref):
    """Return the complete searchable body for one runtime item."""
    return search_texts(db, (ref,)).get(ref, "")
