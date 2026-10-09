"""Choose a worker account from the requested model before writing agent state."""

from codex_catalog import CatalogPending, CatalogUnavailable


def account_order(runtime, parent_account):
    rows = {row['id']: row for row in runtime.accounts.list()}
    preferred = [parent_account, runtime.accounts.default(), *rows]
    return list(dict.fromkeys(key for key in preferred if key in rows and
                not rows[key].get('disconnected') and not rows[key].get('deleted') and not rows[key].get('duplicateOf') and
                (key == parent_account or rows[key].get('status') == 'ready')))


def resolve(runtime, parent, data, *, catalogs=None):
    """Return one account and its catalog. Never retry a model request elsewhere."""
    root = runtime.agent(parent['rootId'])
    defaults = runtime.worker_defaults(root)
    model = data.get('model')
    if model is None and data.get('profile_id'):
        import json
        with runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_profiles WHERE id=?', (data['profile_id'],)).fetchone()
        if row is None:
            raise ValueError('Unknown worker profile')
        model = json.loads(row[0]).get('model')
    model = model or defaults['model'] or root['model']
    if not isinstance(model, str) or not model.strip():
        raise ValueError('Select an available model')
    project_accounts = None
    if root.get('projectId') and root.get('projectServerId'):
        from codex_project_locations import settings_key
        import json
        with runtime.read_db() as db:
            project_key = settings_key(runtime, db, root['cwd'], root)
            row = db.execute('SELECT record FROM runtime_projects WHERE id=?', (project_key,)).fetchone()
            project_accounts = json.loads(row[0]).get('accountKeys') if row else None
    parent_account = parent.get('accountKey', 'default')
    selected = defaults.get('accountKey')
    explicit = data.get('account_key', selected)
    if selected and explicit != selected:
        raise ValueError('Use the subagent account selected in chat settings')
    if explicit is not None:
        if not isinstance(explicit, str) or not explicit:
            raise ValueError('Select an available worker account')
        if project_accounts and explicit not in project_accounts:
            raise ValueError('Select a worker account of this project')
        row = runtime.accounts.get(explicit)
        if row.get('disconnected') or row.get('deleted') or (explicit != parent_account and row.get('status') != 'ready'):
            raise ValueError('Sign in to the worker account before creating a worker')
        candidates = [explicit]
    else:
        candidates = account_order(runtime, parent_account)
        if project_accounts:
            candidates = [key for key in candidates if key in project_accounts]
    # Native catalogs are provider-specific. Do not depend on the parent's
    # provider when the requested model identifies the other provider.
    provider = ('codex' if model.startswith('gpt-') else
                'claude' if model in {'default', 'sonnet', 'opus', 'haiku'} or model.startswith('claude-') else None)
    if explicit is None and provider:
        candidates = [key for key in candidates
                      if runtime.accounts.get(key).get('provider', 'codex') == provider]
    catalogs = {} if catalogs is None else catalogs
    unavailable = []
    for key in candidates:
        if key not in catalogs:
            try:
                catalogs[key] = runtime.catalog(key)
            except (CatalogPending, CatalogUnavailable) as error:
                # An explicitly selected account is an instruction, not a
                # preference. Do not silently move its request elsewhere.
                if explicit is not None:
                    raise
                unavailable.append({'accountKey': key, 'error': str(error)})
                continue
        catalog = catalogs[key]
        if any(model in (row.get('model'), row.get('resolvedModel')) and not row.get('hidden')
               for row in catalog.get('data', [])):
            return key, catalog
    # Agents have no catalog tool; name the valid choices so the next call can succeed.
    names = []
    for key in candidates:
        for row in (catalogs.get(key) or {}).get('data', []):
            label = row.get('model')
            if row.get('resolvedModel') and row['resolvedModel'] != label:
                label = f"{label} ({row['resolvedModel']})"
            if label and not row.get('hidden') and label not in names:
                names.append(label)
    message = 'This model is not available for the selected worker accounts'
    if names:
        message += '. Available: ' + ', '.join(names[:40])
    if unavailable:
        message += '. Unavailable accounts: ' + '; '.join(
            f"{item['accountKey']}: {item['error']}" for item in unavailable)
    raise ValueError(message)


def catalog(runtime, parent_account):
    """Return selectable models in the same account order used by spawn."""
    from codex_catalog import DISPLAY_READ

    models, seen, unavailable = [], set(), []
    pending = False
    for key in account_order(runtime, parent_account):
        account = runtime.accounts.get(key)
        try:
            value = runtime.catalog(key)
        except (CatalogPending, CatalogUnavailable, ValueError, RuntimeError) as error:
            item = {'accountKey': key, 'error': str(error)}
            if DISPLAY_READ.get() and isinstance(error, CatalogPending):
                item['catalogPending'] = True
                pending = True
            unavailable.append(item)
            continue
        for row in value.get('data', []):
            if row.get('model') and not row.get('hidden') and row['model'] not in seen:
                seen.add(row['model'])
                models.append({**row, 'accountKey': key, 'provider': account.get('provider', 'codex')})
    if not models:
        if pending:
            raise CatalogPending('Model catalog is pending; no worker model catalog is available yet')
        raise ValueError('No worker model catalog is available')
    return {'data': models, 'unavailableAccounts': unavailable,
            **({'catalogPending': True} if pending else {})}


def selected_account(runtime, value):
    if not isinstance(value, dict):
        raise ValueError('worker_defaults needs model, effort and fast_mode')
    key = value.get('account_key')
    if key is None:
        return None
    if not isinstance(key, str) or not key:
        raise ValueError('Select an available subagent account')
    account = runtime.accounts.get(key)
    if account.get('disconnected') or account.get('deleted') or account.get('status') != 'ready':
        raise ValueError('Sign in to the subagent account before selecting it')
    return key


def settings_catalog(runtime, parent_account, value):
    key = selected_account(runtime, value)
    return runtime.catalog(key) if key is not None else catalog(runtime, parent_account)
