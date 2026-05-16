# Studio &middot; Python (PyIceberg) cell reference

The Python cells in Studio's notebook editor execute against PyIceberg
inside a managed Python venv. **The Computeza wrapper pre-loads your
registered Iceberg-REST catalogs into the cell scope**, so cell code
diverges from a stock PyIceberg script in a few small but important
ways.

This document is the reference for cell-time usage. Everything outside
those small differences is plain upstream PyIceberg -- see
<https://py.iceberg.apache.org/api/> for the full API.

## What the wrapper does for you

Every PyIceberg cell runs inside a wrapper that, before your code
executes:

1. Reads the catalog list from the metadata store
   (`sail/catalog-names` plus per-catalog config in the secrets vault).
2. Exports S3 credentials to `AWS_*` environment variables (so PyArrow's
   S3 client and `s3fs` both find them).
3. Calls `load_catalog(...)` once per registered catalog with the
   right `type=rest`, `uri`, `warehouse`, and `s3.*` properties.
4. Strips Lakekeeper's remote-signing flags from each catalog's FileIO
   (`s3.remote-signing-enabled`, `s3.signer.*`) so reads go through
   the static S3 keys directly.
5. Wraps `catalog.load_table` so the same sanitisation happens every
   time you open a fresh table, and clears the `s3fs` filesystem
   cache so the new properties take effect on the next I/O.
6. Stashes the loaded catalogs in your cell's globals:

| Name        | Type   | What it is                                        |
| ----------- | ------ | ------------------------------------------------- |
| `catalogs`  | `dict` | `{ "<name>": Catalog }` -- every registered catalog |
| `cat`       | `Catalog` | The **first** entry in `catalogs`, as a shortcut |
| `cat_<name>` | `Catalog` | One alias per registered catalog name (sanitised: `default` -> `cat_default`) |

That means your first cell can be one line:

```python
cat.list_namespaces()
```

...and the wrapper has handled everything from credentials to remote
signing for you.

## What NOT to write

Anything that re-imports or re-loads a catalog will fight the wrapper.

```python
# DON'T -- the wrapper already did this for you, and this form needs
# config in ~/.pyiceberg.yaml or PYICEBERG_CATALOG__DEFAULT__URI which
# the wrapper does not set. You'll get:
#   ValueError: URI missing, please provide using --uri, the config or
#   environment variable PYICEBERG_CATALOG__DEFAULT__URI
from pyiceberg.catalog import load_catalog
cat = load_catalog("default")
```

```python
# DON'T -- this rebuilds a fresh Catalog object that LACKS the
# remote-signing strip + s3fs cache clear our wrapper does. Reads
# may 403 against Garage even though the env vars are set.
from pyiceberg.catalog import load_catalog
cat = load_catalog("default", uri="http://...", warehouse="...")
```

Always use the pre-bound `cat` / `catalogs[<name>]`. If you need to
swap to a different catalog mid-cell:

```python
cat = catalogs["default_test"]
```

## Identifier shapes

Tables and namespaces are addressed by **tuples** of name parts, or
by dot-joined strings. Both work:

```python
cat.load_table(("analytics", "events"))   # tuple form
cat.load_table("analytics.events")        # string form

cat.list_tables("analytics")              # namespace as string
cat.list_tables(("analytics",))           # namespace as 1-tuple
```

What you **can't** do is mix the two for one identifier:

```python
# WRONG -- "default" is the catalog (already bound as `cat`); the
# table identifier is (namespace, table), not (catalog, "ns.table").
cat.load_table(("default", "analytics.events"))

# WRONG -- "analytics.events" passed as the *namespace* gets encoded
# as a nested namespace `analytics\x1fevents` per the REST spec, and
# Lakekeeper rejects it with NamespacePartContainsDot.
cat.load_table(("analytics.events",))
```

Catalog -> namespace -> table. Never include the catalog in the table
identifier; switch catalogs via `catalogs[<name>]` instead.

## Discovery

Drop this in any cell to see what your wrapper bound and what data
is reachable:

```python
print("catalogs :", list(catalogs.keys()))
print("warehouse:", cat.properties.get("warehouse", "(unknown)"))

for ns in cat.list_namespaces():
    label = ".".join(ns)
    try:
        tables = cat.list_tables(ns)
        print(f"  /{label}/  ({len(tables)} tables)")
        for t in tables[:10]:
            print(f"      {'.'.join(t)}")
    except Exception as e:
        print(f"  /{label}/  (list_tables failed: {e})")
```

## Reading a table

The simplest case:

```python
tbl = cat.load_table(("analytics", "events"))
tbl.scan().to_pandas()        # returns the result as the cell's
                              # interactive table widget
```

Field selection and row filtering use PyIceberg's expression types
for type safety:

```python
from pyiceberg.expressions import GreaterThanOrEqual, EqualTo, And

scan = tbl.scan(
    row_filter=And(
        GreaterThanOrEqual("event_date", "2026-05-01"),
        EqualTo("event_type", "signup"),
    ),
    selected_fields=("user_id", "event_type", "event_date"),
    limit=1000,
)
scan.to_pandas()              # or .to_arrow() / .to_duckdb("t")
```

Time travel:

```python
snaps = tbl.inspect.snapshots()       # NOTE: inspect.snapshots(),
                                      # not tbl.snapshots()
prior_id = int(snaps["snapshot_id"][-2])
tbl.scan(snapshot_id=prior_id).to_pandas().head(50)

tbl.inspect.entries(snapshot_id=prior_id)   # file-level metadata
```

## Schemas and required fields

When PyIceberg writes data, it strictly validates that the PyArrow
schema you hand it matches the Iceberg schema. The common pitfall:
Iceberg fields declared `required=True` must have PyArrow fields with
`nullable=False`, or the append refuses:

```
ValueError: Mismatch in fields:
  x 1: user_id: required long      | 1: user_id: optional long
```

The fix is on the PyArrow side:

```python
import pyarrow as pa

# WRONG -- default nullable=True for every field, won't match
# required=True on the Iceberg side.
arrow_schema = pa.schema([
    ("user_id",    pa.int64()),
    ("event_type", pa.string()),
])

# RIGHT -- nullable=False matches Iceberg's required=True.
arrow_schema = pa.schema([
    pa.field("user_id",    pa.int64(),  nullable=False),
    pa.field("event_type", pa.string(), nullable=False),
])
```

If you'd rather not worry about it, declare the Iceberg schema with
`required=False` and any PyArrow schema will accept.

## Creating a table + appending rows

```python
import pyarrow as pa
import datetime as dt
from pyiceberg.exceptions import (
    NamespaceAlreadyExistsError,
    TableAlreadyExistsError,
)
from pyiceberg.schema import Schema
from pyiceberg.types import NestedField, LongType, StringType, TimestampType
from pyiceberg.partitioning import PartitionSpec, PartitionField

# 1) namespace
try:
    cat.create_namespace("playground")
except NamespaceAlreadyExistsError:
    pass

# 2) table
iceberg_schema = Schema(
    NestedField(1, "user_id",    LongType(),      required=True),
    NestedField(2, "event_type", StringType(),    required=True),
    NestedField(3, "ts",         TimestampType(), required=True),
)
spec = PartitionSpec(
    PartitionField(source_id=3, field_id=1000, transform="day", name="ts_day"),
)

ident = ("playground", "demo")
try:
    tbl = cat.create_table(identifier=ident, schema=iceberg_schema, partition_spec=spec)
except TableAlreadyExistsError:
    tbl = cat.load_table(ident)

# 3) insert -- PyArrow schema mirrors required=True with nullable=False
arrow_schema = pa.schema([
    pa.field("user_id",    pa.int64(),         nullable=False),
    pa.field("event_type", pa.string(),        nullable=False),
    pa.field("ts",         pa.timestamp("us"), nullable=False),
])

batch = pa.Table.from_pylist(
    [
        {"user_id": 1, "event_type": "signup",   "ts": dt.datetime(2026, 5, 16,  9, 0)},
        {"user_id": 2, "event_type": "login",    "ts": dt.datetime(2026, 5, 16, 10, 0)},
        {"user_id": 3, "event_type": "purchase", "ts": dt.datetime(2026, 5, 16, 11, 0)},
    ],
    schema=arrow_schema,
)

tbl.append(batch)                  # or tbl.overwrite(batch) to replace
tbl.scan().to_pandas()
```

## Output rendering rules

The cell renderer treats the **last expression** of the cell as its
output, the same way Jupyter does. Make the last line bare:

```python
df = tbl.scan().to_pandas()
df                                 # -> renders as an interactive table
```

vs.

```python
df = tbl.scan().to_pandas()
print(df)                          # -> renders as raw stdout text
```

What it renders interactively:

| Type returned                  | Rendered as              |
| ------------------------------ | ------------------------ |
| `pandas.DataFrame`             | interactive table widget |
| `pyarrow.Table`                | interactive table widget |
| `pyspark.sql.connect.DataFrame` (Sail) | interactive table widget |
| `list[dict]` (uniform keys)    | interactive table widget |
| anything else                  | `repr()` in stdout       |

Output rows are capped at 1 000; the widget shows
`Showing N of total &middot; pandas.DataFrame` so you know it's truncated.
For wider tables, scroll horizontally inside the widget.

## Multi-catalog usage

If you've wired more than one Iceberg-REST catalog (each backed by
its own Lakekeeper warehouse), the wrapper binds them all:

```python
list(catalogs.keys())              # ['default', 'default_test', ...]
cat                                # alias for catalogs[<first>]
cat_default                        # alias for catalogs['default']
cat_default_test                   # alias for catalogs['default_test']
```

Switching is just rebinding `cat`:

```python
cat = catalogs["default_test"]
cat.list_namespaces()
```

Copying a fully-qualified name out of the catalog sidebar (the copy
icon on each row) gives you `catalog.namespace.table`. To load that
in code, split off the catalog part:

```python
qualified = "default.analytics.events"   # what the copy button gives you
catalog, ns_and_table = qualified.split(".", 1)
namespace, table = ns_and_table.rsplit(".", 1)
tbl = catalogs[catalog].load_table((namespace, table))
```

## Sanity check (paste into a fresh cell)

```python
import pyiceberg, pyarrow, sys, os
print("pyiceberg:", pyiceberg.__version__, "pyarrow:", pyarrow.__version__,
      "python:", sys.version.split()[0])
print("catalogs:", list(catalogs.keys()))
print("AWS env :", {
    k: (v[:6] + "..." if v else "(empty)")
    for k, v in os.environ.items()
    if k.startswith("AWS_")
})
print("cat.io  :", type(cat._file_io).__name__,
      "remote-signing-enabled?",
      cat._file_io.properties.get("s3.remote-signing-enabled", "(stripped)"))
print("namespaces:", cat.list_namespaces())
```

Expected:

- `pyiceberg` ≥ `0.10.0`
- `python` `3.10`-`3.14`
- `catalogs` is non-empty (if it's `[]`, the warehouse isn't wired --
  open the warehouse page and click **Connect to SQL**)
- All `AWS_*` vars populated
- `remote-signing-enabled?` is `(stripped)` (this is the wrapper's
  fix; if it shows `true`, you're on an older build -- pull and
  rebuild)

## Troubleshooting

### `URI missing, please provide using --uri...`

You called `load_catalog("default")` yourself. Don't -- use the
pre-bound `cat` instead. See [What NOT to write](#what-not-to-write).

### `NoSuchNamespaceError: Namespace with name 'X' does not exist`

The namespace literally isn't in this warehouse. Run the
[discovery snippet](#discovery) to see what's actually there. Common
slip: passing the catalog name as the namespace
(`cat.list_tables("default")` -- `default` is the *catalog*, not a
namespace inside it).

### `NamespacePartContainsDot: Namespace parts cannot contain '.'`

You passed a dotted string as a tuple element:
`cat.load_table(("default.analytics", "events"))`. The REST client
encodes a tuple element as one namespace part; a dot inside one part
turns into a literal byte (`%1F` in the URL). Use the right shape:
`cat.load_table(("analytics", "events"))`.

### `PermissionError: Forbidden: Garage does not support anonymous access yet`

The S3 write call landed without credentials. Verify:

```python
import os
print({k: v[:6] + "..." if v else "(empty)"
       for k, v in os.environ.items()
       if k.startswith("AWS_")})
```

Every `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_REGION` /
`AWS_ENDPOINT_URL` should be populated. If any are empty, you're on
an older build of the wrapper -- pull and rebuild
(`git pull && cargo build --bin computeza && sudo systemctl restart
computeza`).

### `ClientError: 403 Forbidden` on `HeadObject` (read path)

Lakekeeper's REST `/v1/config` is injecting remote-signing flags
that point fsspec at a signer endpoint instead of using the static
keys we provisioned. The wrapper strips these automatically after
each `load_catalog` and each `cat.load_table()`. Verify with:

```python
print(tbl.io.properties.get("s3.remote-signing-enabled", "(stripped)"))
```

If this prints `true`, you're on a pre-strip wrapper -- pull and
rebuild.

### `ValueError: Mismatch in fields`

PyArrow schema `nullable=True` doesn't match Iceberg `required=True`.
See [Schemas and required fields](#schemas-and-required-fields).

### `AST node line range (M, N) is not valid`

Long-running concern from older builds; if you see this on a current
build, the cell wrapper has gotten out of sync with the running Sail
venv. Restart `computeza serve` so the cell wrapper is re-emitted.

### Cell output is empty even though no error fired

Likely a `DataFrame` with `object`-dtype list/dict columns; pre-pandas
2.x the wrapper's `pd.isna(v)` check raised on those and silently
dropped the marker line. The current wrapper is scalar-safe -- pull and
rebuild if you still see this.
