"""Check the production pairing boundary's monitor export header contract."""
from test_isolation import isolate_supervisor_environment

isolate_supervisor_environment()

import ast
from pathlib import Path
import sys

source = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).parents[1] / "scripts/studio_api/multi_server/boundary.py"
module = ast.parse(source.read_text())
assignment = next(item for item in module.body if isinstance(item, ast.Assign) and any(isinstance(target, ast.Name) and target.id == "CORS_HEADERS" for target in item.targets))
headers = eval(compile(ast.Expression(assignment.value), str(source), "eval"), {"API_SCHEMA_HASH_HEADER": "X-Studio-API-Schema", "API_SCHEMA_MISMATCH_HEADER": "X-Studio-API-Schema-Mismatch"})
exposed = {name.strip().lower() for name in dict(headers)[b"access-control-expose-headers"].decode().split(",")}
required = {"x-studio-server", "x-studio-api-schema", "x-studio-api-schema-mismatch", "etag", "content-disposition", "x-log-truncated"}
assert required <= exposed, f"Hidden remote response headers: {sorted(required - exposed)}"
print("PASS: monitor export response headers are exposed")
