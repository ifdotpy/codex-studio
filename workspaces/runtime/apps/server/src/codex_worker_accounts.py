"""Choose a worker account from the requested model before writing agent state."""

from codex_catalog import CatalogPending, CatalogUnavailable


class ExcludedAccountError(ValueError):
    """A chat tried to start new work on one of its disabled accounts."""


def account_order(runtime, parent_account, *, excluded=()):
    rows = {row['id']: row for row in runtime.accounts.list()}
    preferred = [parent_account, runtime.accounts.default(), *rows]
    return list(dict.fromkeys(key for key in preferred if key in rows and key not in excluded and
                not rows[key].get('disconnected') and not rows[key].get('deleted') and not rows[key].get('duplicateOf') and
                (key == parent_account or rows[key].get('status') == 'ready' or rows[key].get('canAttemptNativeProof') is True)))


def excluded_accounts(root):
    value = root.get('excludedAccountKeys', [])
    return set(value) if isinstance(value, list) and all(isinstance(key, str) for key in value) else set()


def require_not_excluded(root, account_key):
    if account_key in excluded_accounts(root):
        raise ExcludedAccountError(f'Account {account_key} is disabled for this chat')


def model_provider(model):
    return ('codex' if model.startswith('gpt-') else
            'claude' if model in {'default', 'sonnet', 'opus', 'haiku'} or model.startswith('claude-') else None)


def _price_list(runtime):
    """The cached public price list, or None before the first download. This read never
    starts a download; the cost readers keep the cache fresh."""
    from codex_pricing import PricingCatalog
    pricing = getattr(runtime, '_worker_pricing', None)
    if pricing is None and getattr(runtime, 'root', None) is not None:
        pricing = runtime._worker_pricing = PricingCatalog(runtime.root)
    return pricing.snapshot(ttl=float('inf')) if pricing is not None else None


def _rates(snapshot, model, catalogs):
    """Input and output prices of a model per million tokens, or None when unknown."""
    from codex_pricing import lookup
    # A catalog alias such as Claude "default" is priced as the model it resolves to.
    resolved = next((row['resolvedModel'] for value in catalogs.values() for row in (value or {}).get('data', [])
                     if row.get('model') == model and row.get('resolvedModel')), model)
    provider = model_provider(resolved) or model_provider(model)
    if provider is None:
        return None
    rates = lookup(snapshot, 'openai' if provider == 'codex' else 'anthropic', resolved)
    return (rates['input'], rates['output']) if rates else None


def check_provider(requested, default):
    wanted = model_provider(default)
    if requested != default and wanted and model_provider(requested) != wanted:
        raise ValueError(f'Choose a {wanted.title()} model for subagents, as set in chat settings')


def check_choice(runtime, requested, default, catalogs):
    """An agent may choose a subagent model of the default's provider that costs no more
    per token than the default the person chose. With a price list, a model without a
    known price is refused unless it is the default; before the first list the choice stands."""
    if requested == default:
        return
    check_provider(requested, default)
    snapshot = _price_list(runtime)
    if snapshot is None:
        return
    rates, limit = _rates(snapshot, requested, catalogs), _rates(snapshot, default, catalogs)
    if rates is None or limit is None:
        raise ValueError(f'Studio cannot compare the price of {requested} with the default subagent model {default}. '
                         'Omit model to use the default')
    if rates[0] > limit[0] or rates[1] > limit[1]:
        raise ValueError(f'{requested} costs more than the default subagent model {default} '
                         f'(input {rates[0]:g} > {limit[0]:g} or output {rates[1]:g} > {limit[1]:g} per million tokens). '
                         'Choose a model that costs no more, or omit model')


def resolve(runtime, parent, data, *, catalogs=None, limit_choice=True):
    """Return one account and its catalog. Never retry a model request elsewhere.
    limit_choice holds a subagent model that an agent chose to check_choice; a native
    review has its own default (review_defaults) and passes False."""
    root = runtime.agent(parent['rootId'])
    defaults = runtime.worker_defaults(root)
    excluded = excluded_accounts(root)
    model = data.get('model')
    if model is None and data.get('profile_id'):
        import json
        with runtime.db() as db:
            row = db.execute('SELECT record FROM runtime_profiles WHERE id=?', (data['profile_id'],)).fetchone()
        if row is None:
            raise ValueError('Unknown worker profile')
        model = json.loads(row[0]).get('model')
    requested = model
    default_model = defaults['model'] or root['model']
    model = model or default_model
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
        require_not_excluded(root, explicit)
        if project_accounts and explicit not in project_accounts:
            raise ValueError('Select a worker account of this project')
        row = runtime.accounts.get(explicit)
        if row.get('disconnected') or row.get('deleted') or (explicit != parent_account and row.get('status') != 'ready' and not runtime.accounts.allow_native_auth_attempt(explicit, row)):
            raise ValueError('Sign in to the worker account before creating a worker')
        candidates = [explicit]
    else:
        candidates = account_order(runtime, parent_account, excluded=excluded)
        if project_accounts:
            candidates = [key for key in candidates if key in project_accounts]
        if not candidates:
            cause = ' and this project account selection' if project_accounts else ''
            raise ValueError(f'No worker account is available: this chat disables all accounts{cause}')
    # Native catalogs are provider-specific. Do not depend on the parent's
    # provider when the requested model identifies the other provider.
    provider = model_provider(model)
    limited = limit_choice and requested and isinstance(default_model, str) and default_model
    if limited:
        check_provider(requested, default_model)
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
            if limited:
                check_choice(runtime, requested, default_model, catalogs)
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


def catalog(runtime, parent_account, root=None):
    """Return selectable models in the same account order used by spawn."""
    from codex_catalog import DISPLAY_READ

    models, seen, unavailable = [], set(), []
    pending = False
    for key in account_order(runtime, parent_account,
                             excluded=excluded_accounts(root) if root is not None else ()):
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


def selected_account(runtime, value, root=None):
    if not isinstance(value, dict):
        raise ValueError('worker_defaults needs model, effort and fast_mode')
    key = value.get('account_key')
    if key is None:
        return None
    if not isinstance(key, str) or not key:
        raise ValueError('Select an available subagent account')
    if root is not None:
        require_not_excluded(root, key)
    account = runtime.accounts.get(key)
    if account.get('disconnected') or account.get('deleted') or (account.get('status') != 'ready' and not runtime.accounts.allow_native_auth_attempt(key, account)):
        raise ValueError('Sign in to the subagent account before selecting it')
    return key


def settings_catalog(runtime, parent_account, value, root=None):
    key = selected_account(runtime, value, root)
    return runtime.catalog(key) if key is not None else catalog(runtime, parent_account, root)
