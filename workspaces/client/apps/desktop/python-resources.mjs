import { execFileSync } from "node:child_process";

// The Python programs that run from Contents/Resources of a package: the recovery
// job (recover_backend.py) and the update installer (install_update.py).
export const pythonEntryPoints = ["recover_backend.py", "install_update.py"];

// Every Python file that package.mjs copies into Contents/Resources: the entry
// points and the local modules that they import. check-package.mjs derives the
// required modules from the imports of the packaged entry points, so a module
// missing here fails that check.
export const pythonResources = [
  ...pythonEntryPoints,
  "desktop_process.py",
  "desktop_updates.py",
  "desktop_vm.py",
];

// Reads the imports of each entry point and of each local module that it reaches.
// A module is local when `<name>.py` is in the directory (the runtime puts the
// script directory first on sys.path). Any other name must come from the standard
// library: `-I -S` leaves out the user and site packages.
const missingModulesScript = String.raw`
import ast, importlib.util, json, sys
from pathlib import Path
root = Path(sys.argv[1])
pending, seen, missing = list(sys.argv[2:]), set(), set()
while pending:
    name = pending.pop()
    if name in seen:
        continue
    seen.add(name)
    tree = ast.parse((root / name).read_text(encoding="utf-8"), name)
    for node in ast.walk(tree):
        if isinstance(node, ast.Import):
            modules = [alias.name for alias in node.names]
        elif isinstance(node, ast.ImportFrom) and not node.level:
            modules = [node.module]
        else:
            continue
        for module in modules:
            top = module.partition(".")[0]
            if (root / f"{top}.py").is_file():
                pending.append(f"{top}.py")
            elif importlib.util.find_spec(top) is None:
                missing.add(top)
print(json.dumps(sorted(missing)))
`;

// Names of the modules that the entry points in `directory` import, directly or
// through a local module, and that neither the directory nor the standard library
// supplies. An empty list means that the Python programs can start.
export function missingPythonModules(
  directory,
  entryPoints = pythonEntryPoints,
) {
  return JSON.parse(
    execFileSync(
      "python3",
      ["-I", "-S", "-c", missingModulesScript, directory, ...entryPoints],
      { encoding: "utf8" },
    ),
  );
}
