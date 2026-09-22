"""Daybreak selection uses account-native model grants, separate from the model."""


def resolve_program(catalog, model, enabled, provider="codex"):
    if type(enabled) is not bool:
        raise ValueError("daybreak_enabled must be a boolean")
    if provider == "claude":
        if enabled:
            raise ValueError("Daybreak is available only for Codex models")
        return "standard"
    info = next((row for row in catalog.get("data", [])
                 if row.get("model") == model and not row.get("hidden")), None)
    programs = (info or {}).get("availableAccessPrograms")
    cyber = programs.get("cyber") if isinstance(programs, dict) else None
    if not isinstance(cyber, list) or any(not isinstance(value, str) for value in cyber):
        cyber = None
    if enabled:
        for program in ("daybreakBlue", "daybreakRed"):
            if cyber and program in cyber:
                return program
        raise ValueError("Daybreak is not available for this model on this account. Choose another model or turn off Daybreak")
    if cyber is not None and "standard" not in cyber:
        raise ValueError("This model requires Daybreak. Turn on Daybreak or choose another model")
    return "standard"


def turn_program(runtime, agent):
    """Read grants outside runtime locks. The provider still owns authorization."""
    enabled = agent.get("daybreakEnabled", False)
    catalog = runtime.catalog(agent.get("accountKey", "default")) if enabled else {}
    return resolve_program(catalog, agent["model"], enabled, agent.get("provider", "codex"))


def turn_params(agent, program=None):
    if agent.get("provider") == "claude":
        return {}
    if program is None:
        program = agent.get("cyberAccessProgram", "standard")
        if agent.get("daybreakEnabled") and program not in {"daybreakBlue", "daybreakRed"}:
            raise ValueError("Select an available Daybreak model before sending")
    return {"cyberAccessProgram": program if agent.get("daybreakEnabled") else "standard"}
