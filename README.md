# msg_convert

`msg-convert` is a command-line frontend for converting AMReX plotfiles and
VTK XML ImageData (`.vti`) files into MSG archives. Input and output formats are
inferred when possible and can be supplied explicitly when paths are ambiguous.

```sh
msg-convert plt00010 plt00010.msg
msg-convert simulation.vti simulation.msg
msg-convert --input-format amrex INPUT OUTPUT
msg-convert --config conversion.toml
```

Source-specific settings select imported data. `[msg]` settings control archive
encoding after import.

```toml
input = "simulation.vti"
output = "simulation.msg"

[vti]
fields = ["density", "velocity"]

[msg]
codec = "auto:lossless"

[[msg.fields]]
name = "velocity_x"
codec = "quantized:u16"
error = { relative_percent = 0.1 }
```

Multi-component VTI arrays are selected by their source name and become scalar
MSG fields with suffixes such as `_x`, `_y`, and `_z`.

AMReX fields may be names or inline tables when a logical scalar type is needed:

```toml
input = "plt00010"
output = "plt00010.msg"

[amrex]
replace_invalid = false
fields = [
    "temperature",
    { name = "density", scalar_type = "f64" },
]

[[msg.fields]]
name = "density"
codec = "auto:smallest"
normalize = [0.0, 1.0]
error = { absolute = 0.0, relative_percent = 0.01 }
```

`normalize = [min, max]` maps and clamps a final MSG field with
`(value - min) / (max - min)`. Stored values are in `[0, 1]`, while the
original finite, strictly increasing bounds are retained in archive metadata.
Codec error tolerances for normalized fields are still interpreted in the
original physical units. Normalization can be configured with or without a
per-field codec override.

The former `amrex-to-msg` keys `replace_invalid` and `[[variables]]` remain
accepted for migration, including `normalize` on a legacy variable.

## Examples

Annotated configurations are available in [`examples/`](examples):

- [`amrex.toml`](examples/amrex.toml) covers AMReX field selection, scalar
  types, invalid-value handling, normalization, and per-field codec overrides.
- [`vti.toml`](examples/vti.toml) covers VTI array selection, vector component
  naming, normalization, and absolute or relative error budgets.
- [`codecs.toml`](examples/codecs.toml) lists every supported codec and its
  error-tolerance requirements.

Run one after replacing its sample paths and field names:

```sh
msg-convert --config examples/amrex.toml
```
