use std::{
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, bail, ensure};
use clap::{Parser, ValueEnum};
use msg_amrex::{AmrexField, AmrexImportOptions, InvalidValuePolicy, PlotFile, import_amrex};
use msg_vti::{VtiImportOptions, import_vti};
use multires_rs::{
    MsgChunkCodec, MsgChunkCodecSelection, MsgDataset, MsgErrorTolerance, MsgFieldCodec,
    MsgNormalization, MsgScalarType, MsgWriteOptions, write_msg,
};
use serde::Deserialize;

fn main() -> Result<()> {
    let resolved = Resolved::resolve(Cli::parse())?;
    reject_same_file(&resolved.input, &resolved.output)?;

    let (mut dataset, import_description) = match resolved.input_format {
        InputFormat::Amrex => import_from_amrex(&resolved)?,
        InputFormat::Vti => import_from_vti(&resolved)?,
    };
    normalize_fields(&mut dataset, &resolved.config)?;
    let codecs = codec_policies(&dataset, &resolved.config)?;
    let file = File::create(&resolved.output)
        .with_context(|| format!("creating MSG archive {}", resolved.output.display()))?;
    let mut writer = BufWriter::new(file);
    let archive_start = Instant::now();
    write_msg(&dataset, &MsgWriteOptions { codecs }, &mut writer)
        .with_context(|| format!("writing MSG archive {}", resolved.output.display()))?;
    let archive_time = archive_start.elapsed();
    let flush_start = Instant::now();
    writer.flush()?;
    let flush_time = flush_start.elapsed();

    eprintln!("{import_description}");
    eprintln!(
        "MSG output: fields {}, levels {}, archive {:.6}s, final flush {:.6}s",
        dataset.fields().len(),
        dataset.levels().len(),
        archive_time.as_secs_f64(),
        flush_time.as_secs_f64()
    );
    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    name = "msg-convert",
    about = "Convert supported scientific data formats into MSG archives",
    after_help = "Formats are inferred from the input and output paths when possible. With --config, positional paths may override both configured paths as a pair."
)]
struct Cli {
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
    #[arg(long, value_enum)]
    input_format: Option<InputFormat>,
    #[arg(long, value_enum)]
    output_format: Option<OutputFormat>,
    /// Select an input field or array. Repeat to select more than one.
    #[arg(short, long, value_name = "NAME", action = clap::ArgAction::Append, conflicts_with = "config")]
    field: Vec<String>,
    /// Replace invalid AMReX values with zero.
    #[arg(long, conflicts_with = "config")]
    amrex_replace_invalid: bool,
    input: Option<PathBuf>,
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum InputFormat {
    Amrex,
    Vti,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    Msg,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    input: Option<PathBuf>,
    output: Option<PathBuf>,
    input_format: Option<InputFormat>,
    output_format: Option<OutputFormat>,
    amrex: AmrexConfig,
    vti: VtiConfig,
    msg: MsgConfig,
    // Compatibility with the former amrex-to-msg configuration shape.
    variables: Vec<LegacyVariableConfig>,
    replace_invalid: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AmrexConfig {
    fields: Vec<AmrexFieldSelection>,
    replace_invalid: bool,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum AmrexFieldSelection {
    Name(String),
    Detailed(AmrexFieldConfig),
}

impl AmrexFieldSelection {
    fn name(&self) -> &str {
        match self {
            Self::Name(name) => name,
            Self::Detailed(field) => &field.name,
        }
    }

    fn scalar_type(&self) -> ConfigScalarType {
        match self {
            Self::Name(_) => ConfigScalarType::F32,
            Self::Detailed(field) => field.scalar_type.unwrap_or(ConfigScalarType::F32),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AmrexFieldConfig {
    name: String,
    scalar_type: Option<ConfigScalarType>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct VtiConfig {
    fields: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct MsgConfig {
    codec: Option<String>,
    error: Option<ErrorConfig>,
    fields: Vec<MsgFieldConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MsgFieldConfig {
    name: String,
    codec: Option<String>,
    error: Option<ErrorConfig>,
    normalize: Option<[f64; 2]>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyVariableConfig {
    name: String,
    scalar_type: Option<ConfigScalarType>,
    codec: Option<String>,
    error: Option<ErrorConfig>,
    normalize: Option<[f64; 2]>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
struct ErrorConfig {
    absolute: Option<f64>,
    relative_percent: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum ConfigScalarType {
    F32,
    F64,
}

impl From<ConfigScalarType> for MsgScalarType {
    fn from(value: ConfigScalarType) -> Self {
        match value {
            ConfigScalarType::F32 => Self::F32,
            ConfigScalarType::F64 => Self::F64,
        }
    }
}

struct Resolved {
    input: PathBuf,
    output: PathBuf,
    input_format: InputFormat,
    config: Config,
    cli_fields: Vec<String>,
    cli_replace_invalid: bool,
}

impl Resolved {
    fn resolve(cli: Cli) -> Result<Self> {
        let mut config = if let Some(path) = &cli.config {
            let source = fs::read_to_string(path)
                .with_context(|| format!("reading config {}", path.display()))?;
            toml::from_str(&source).with_context(|| format!("parsing config {}", path.display()))?
        } else {
            Config::default()
        };

        let (input, output) = match (cli.input, cli.output) {
            (Some(input), Some(output)) => (input, output),
            (None, None) => (
                config
                    .input
                    .take()
                    .context("missing INPUT path on the command line and in the config")?,
                config
                    .output
                    .take()
                    .context("missing OUTPUT path on the command line and in the config")?,
            ),
            _ => bail!("input and output path overrides must be provided together"),
        };

        let input_format = cli
            .input_format
            .or(config.input_format)
            .map(Ok)
            .unwrap_or_else(|| infer_input_format(&input))?;
        let output_format = cli
            .output_format
            .or(config.output_format)
            .map(Ok)
            .unwrap_or_else(|| infer_output_format(&output))?;
        ensure!(
            output_format == OutputFormat::Msg,
            "only MSG output is currently supported"
        );
        validate_config_for_format(&config, input_format)?;

        Ok(Self {
            input,
            output,
            input_format,
            config,
            cli_fields: cli.field,
            cli_replace_invalid: cli.amrex_replace_invalid,
        })
    }
}

fn infer_input_format(path: &Path) -> Result<InputFormat> {
    if extension_is(path, "vti") {
        return Ok(InputFormat::Vti);
    }
    if path.is_dir() && path.join("Header").is_file() {
        return Ok(InputFormat::Amrex);
    }
    bail!(
        "cannot infer the input format from {}; use --input-format or input_format in TOML",
        path.display()
    )
}

fn infer_output_format(path: &Path) -> Result<OutputFormat> {
    if extension_is(path, "msg") {
        Ok(OutputFormat::Msg)
    } else {
        bail!(
            "cannot infer the output format from {}; use --output-format or output_format in TOML",
            path.display()
        )
    }
}

fn extension_is(path: &Path, expected: &str) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(expected))
}

fn validate_config_for_format(config: &Config, format: InputFormat) -> Result<()> {
    match format {
        InputFormat::Amrex => ensure!(
            config.vti.fields.is_empty(),
            "[vti] options cannot be used with AMReX input"
        ),
        InputFormat::Vti => {
            ensure!(
                config.amrex.fields.is_empty()
                    && !config.amrex.replace_invalid
                    && config.variables.is_empty()
                    && config.replace_invalid.is_none(),
                "AMReX options cannot be used with VTI input"
            );
        }
    }
    ensure!(
        config.variables.is_empty() || config.amrex.fields.is_empty(),
        "legacy [[variables]] and [amrex].fields cannot both be configured"
    );
    Ok(())
}

fn import_from_amrex(resolved: &Resolved) -> Result<(MsgDataset, String)> {
    let plotfile = PlotFile::open(&resolved.input)
        .with_context(|| format!("opening AMReX plotfile {}", resolved.input.display()))?;
    let configured = if resolved.config.variables.is_empty() {
        resolved
            .config
            .amrex
            .fields
            .iter()
            .map(|field| (field.name(), field.scalar_type()))
            .collect::<Vec<_>>()
    } else {
        resolved
            .config
            .variables
            .iter()
            .map(|field| {
                (
                    field.name.as_str(),
                    field.scalar_type.unwrap_or(ConfigScalarType::F32),
                )
            })
            .collect::<Vec<_>>()
    };
    let requested = if resolved.cli_fields.is_empty() {
        configured
    } else {
        resolved
            .cli_fields
            .iter()
            .map(|name| (name.as_str(), ConfigScalarType::F32))
            .collect()
    };
    let mut fields = Vec::with_capacity(requested.len());
    for (index, (name, scalar_type)) in requested.iter().enumerate() {
        ensure!(
            !requested[..index]
                .iter()
                .any(|(earlier, _)| earlier == name),
            "AMReX variable {name:?} was requested more than once"
        );
        let variable = plotfile
            .variable(name)
            .with_context(|| format!("AMReX variable {name:?} was not found"))?;
        fields.push(AmrexField {
            component: variable.index,
            scalar_type: (*scalar_type).into(),
            normalization: configured_normalization(&resolved.config, name)?,
        });
    }
    let replace_invalid = resolved.cli_replace_invalid
        || resolved.config.amrex.replace_invalid
        || resolved.config.replace_invalid.unwrap_or(false);
    let (dataset, report) = import_amrex(
        &plotfile,
        &AmrexImportOptions {
            fields,
            invalid_value_policy: if replace_invalid {
                InvalidValuePolicy::ReplaceWithZero
            } else {
                InvalidValuePolicy::Reject
            },
        },
    )?;
    for (field, replacements) in dataset.fields().iter().zip(&report.replaced_values) {
        if *replacements != 0 {
            eprintln!(
                "warning: replaced {replacements} invalid values with zero in {:?}",
                field.name()
            );
        }
    }
    Ok((
        dataset,
        format!(
            "AMReX import: {:.6}s, patches {}, samples {}",
            report.elapsed.as_secs_f64(),
            report.patch_count,
            report.sample_count
        ),
    ))
}

fn import_from_vti(resolved: &Resolved) -> Result<(MsgDataset, String)> {
    let fields = if resolved.cli_fields.is_empty() {
        resolved.config.vti.fields.clone()
    } else {
        resolved.cli_fields.clone()
    };
    let (dataset, timings) = import_vti(&resolved.input, &VtiImportOptions { fields })?;
    Ok((
        dataset,
        format!(
            "VTI import: mmap {:.6}s, UTF-8 preparation {:.6}s, XML parse {:.6}s",
            timings.mmap.as_secs_f64(),
            timings.parse_utf8.as_secs_f64(),
            timings.parse_document.as_secs_f64()
        ),
    ))
}

fn normalize_fields(dataset: &mut MsgDataset, config: &Config) -> Result<()> {
    let configured = config
        .msg
        .fields
        .iter()
        .filter_map(|field| field.normalize.map(|bounds| (field.name.as_str(), bounds)))
        .chain(
            config
                .variables
                .iter()
                .filter_map(|field| field.normalize.map(|bounds| (field.name.as_str(), bounds))),
        )
        .collect::<Vec<_>>();
    for (index, &(name, bounds)) in configured.iter().enumerate() {
        ensure!(
            !configured[..index]
                .iter()
                .any(|(earlier, _)| *earlier == name),
            "MSG output field {name:?} is normalized more than once"
        );
        let normalization = resolve_normalization(bounds)?;
        let field = dataset
            .field_mut(name)
            .with_context(|| format!("MSG dataset has no output field {name:?}"))?;
        if let Some(existing) = field.normalization() {
            ensure!(
                existing == normalization,
                "MSG output field {name:?} has conflicting normalization bounds"
            );
            continue;
        }
        for level in field.levels_mut() {
            level
                .values_mut()
                .map_values(|value| normalize_value(value, normalization));
        }
        field.set_normalization(Some(normalization));
    }
    Ok(())
}

fn normalize_value(value: f64, normalization: MsgNormalization) -> f64 {
    ((value - normalization.min) / (normalization.max - normalization.min)).clamp(0.0, 1.0)
}

fn configured_normalization(config: &Config, name: &str) -> Result<Option<MsgNormalization>> {
    let bounds = config
        .msg
        .fields
        .iter()
        .filter(|field| field.name == name)
        .filter_map(|field| field.normalize)
        .chain(
            config
                .variables
                .iter()
                .filter(|field| field.name == name)
                .filter_map(|field| field.normalize),
        )
        .collect::<Vec<_>>();
    ensure!(
        bounds.len() <= 1,
        "MSG output field {name:?} is normalized more than once"
    );
    bounds
        .first()
        .copied()
        .map(resolve_normalization)
        .transpose()
}

fn resolve_normalization(bounds: [f64; 2]) -> Result<MsgNormalization> {
    let [min, max] = bounds;
    ensure!(
        min.is_finite() && max.is_finite() && min < max,
        "normalization bounds must be finite and strictly increasing"
    );
    Ok(MsgNormalization { min, max })
}

fn codec_policies(dataset: &MsgDataset, config: &Config) -> Result<Vec<MsgFieldCodec>> {
    let default = match (&config.msg.codec, config.msg.error) {
        (None, None) => None,
        (None, Some(_)) => bail!("[msg].error requires [msg].codec"),
        (Some(codec), error) => Some((parse_codec(codec)?, resolve_error(error)?)),
    };
    let mut policies = dataset
        .fields()
        .iter()
        .filter_map(|field| {
            default.map(|(selection, error_tolerance)| MsgFieldCodec {
                field: field.name().to_owned(),
                selection,
                error_tolerance,
            })
        })
        .collect::<Vec<_>>();

    for (index, field) in config.msg.fields.iter().enumerate() {
        ensure!(
            !config.msg.fields[..index]
                .iter()
                .any(|earlier| earlier.name == field.name),
            "MSG output field {:?} is configured more than once",
            field.name
        );
        ensure!(
            field.codec.is_some() || field.normalize.is_some(),
            "MSG output field {:?} must configure codec or normalize",
            field.name
        );
        match (&field.codec, field.error) {
            (None, None) => {}
            (None, Some(_)) => bail!(
                "MSG output field {:?} has an error tolerance but no codec",
                field.name
            ),
            (Some(codec), error) => {
                set_field_policy(dataset, &mut policies, &field.name, codec, error)?;
            }
        }
    }
    for field in &config.variables {
        match (&field.codec, field.error) {
            (None, None) => {}
            (None, Some(_)) => bail!(
                "legacy variable {:?} has an error tolerance but no codec",
                field.name
            ),
            (Some(codec), error) => {
                set_field_policy(dataset, &mut policies, &field.name, codec, error)?;
            }
        }
    }
    Ok(policies)
}

fn set_field_policy(
    dataset: &MsgDataset,
    policies: &mut Vec<MsgFieldCodec>,
    name: &str,
    codec: &str,
    error: Option<ErrorConfig>,
) -> Result<()> {
    ensure!(
        dataset.field(name).is_some(),
        "MSG dataset has no output field {name:?}"
    );
    let policy = MsgFieldCodec {
        field: name.to_owned(),
        selection: parse_codec(codec)?,
        error_tolerance: resolve_error(error)?,
    };
    if let Some(existing) = policies.iter_mut().find(|item| item.field == name) {
        *existing = policy;
    } else {
        policies.push(policy);
    }
    Ok(())
}

fn resolve_error(error: Option<ErrorConfig>) -> Result<Option<MsgErrorTolerance>> {
    error
        .map(|error| {
            ensure!(
                error.absolute.is_some() || error.relative_percent.is_some(),
                "error must specify absolute, relative_percent, or both"
            );
            let absolute = error.absolute.unwrap_or(0.0);
            let relative_percent = error.relative_percent.unwrap_or(0.0);
            ensure!(
                absolute.is_finite() && absolute >= 0.0,
                "absolute error must be finite and non-negative"
            );
            ensure!(
                relative_percent.is_finite() && (0.0..=100.0).contains(&relative_percent),
                "relative_percent must be finite and between 0 and 100"
            );
            Ok(MsgErrorTolerance {
                absolute,
                relative_percent,
            })
        })
        .transpose()
}

fn parse_codec(value: &str) -> Result<MsgChunkCodecSelection> {
    Ok(match value {
        "auto:lossless" => MsgChunkCodecSelection::AutoLossless,
        "auto:smallest" => MsgChunkCodecSelection::AutoSmallest,
        "uniform:f64" => MsgChunkCodecSelection::Forced(MsgChunkCodec::UniformF64),
        "dense:f32" => MsgChunkCodecSelection::Forced(MsgChunkCodec::DenseF32),
        "dense:f64" => MsgChunkCodecSelection::Forced(MsgChunkCodec::DenseF64),
        "quantized:u8" => MsgChunkCodecSelection::Forced(MsgChunkCodec::QuantizedU8),
        "quantized:u16" => MsgChunkCodecSelection::Forced(MsgChunkCodec::QuantizedU16),
        "quantized:u32" => MsgChunkCodecSelection::Forced(MsgChunkCodec::QuantizedU32),
        "affine:f64" => MsgChunkCodecSelection::Forced(MsgChunkCodec::AffineF64),
        "affine-residual:u8" => MsgChunkCodecSelection::Forced(MsgChunkCodec::AffineResidualU8),
        "affine-residual:u16" => MsgChunkCodecSelection::Forced(MsgChunkCodec::AffineResidualU16),
        "affine-residual:u32" => MsgChunkCodecSelection::Forced(MsgChunkCodec::AffineResidualU32),
        _ => bail!("unsupported MSG codec {value:?}; expected family:value"),
    })
}

fn reject_same_file(input: &Path, output: &Path) -> Result<()> {
    if input.is_file() && output.exists() {
        ensure!(
            fs::canonicalize(input)? != fs::canonicalize(output)?,
            "input and output must be different files"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infers_extensions_case_insensitively() {
        assert_eq!(
            infer_input_format(Path::new("sample.VTI")).unwrap(),
            InputFormat::Vti
        );
        assert_eq!(
            infer_output_format(Path::new("sample.MSG")).unwrap(),
            OutputFormat::Msg
        );
    }

    #[test]
    fn parses_modern_vti_configuration() {
        let config: Config = toml::from_str(
            r#"
input = "sample.vti"
output = "sample.msg"

[vti]
fields = ["density", "velocity"]

[msg]
codec = "auto:lossless"

[[msg.fields]]
name = "velocity_x"
codec = "quantized:u16"
error = { relative_percent = 0.1 }
"#,
        )
        .unwrap();
        assert_eq!(config.vti.fields, ["density", "velocity"]);
        assert_eq!(config.msg.fields.len(), 1);
    }

    #[test]
    fn parses_legacy_amrex_configuration() {
        let config: Config = toml::from_str(
            r#"
input = "plt00010"
output = "plt00010.msg"
replace_invalid = false

[[variables]]
name = "density"
scalar_type = "f64"
codec = "auto:smallest"
normalize = [1.0e20, 1.000000001e20]
error = { absolute = 0.0, relative_percent = 0.01 }
"#,
        )
        .unwrap();
        assert_eq!(config.variables.len(), 1);
        assert_eq!(config.replace_invalid, Some(false));
        assert_eq!(
            configured_normalization(&config, "density").unwrap(),
            Some(MsgNormalization {
                min: 1.0e20,
                max: 1.000000001e20
            })
        );
    }

    #[test]
    fn parses_annotated_example_configurations() {
        let examples = [
            (include_str!("../examples/amrex.toml"), InputFormat::Amrex),
            (include_str!("../examples/vti.toml"), InputFormat::Vti),
            (include_str!("../examples/codecs.toml"), InputFormat::Vti),
        ];

        for (source, input_format) in examples {
            let config: Config = toml::from_str(source).unwrap();
            validate_config_for_format(&config, input_format).unwrap();

            if let Some(codec) = &config.msg.codec {
                parse_codec(codec).unwrap();
                resolve_error(config.msg.error).unwrap();
            }
            for field in &config.msg.fields {
                if let Some(codec) = &field.codec {
                    parse_codec(codec).unwrap();
                }
                resolve_error(field.error).unwrap();
            }
        }
    }

    #[test]
    fn normalizes_values_and_preserves_original_bounds() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/assets/simple.vti");
        let (mut dataset, _) = import_vti(&path, &VtiImportOptions::default()).unwrap();
        let config: Config = toml::from_str(
            r#"
[[msg.fields]]
name = "density"
normalize = [0.0, 2.0]
"#,
        )
        .unwrap();

        normalize_fields(&mut dataset, &config).unwrap();
        let normalization = MsgNormalization { min: 0.0, max: 2.0 };
        assert_eq!(normalize_value(-1.0, normalization), 0.0);
        assert_eq!(normalize_value(1.0, normalization), 0.5);
        assert_eq!(normalize_value(3.0, normalization), 1.0);
        assert_eq!(
            dataset.field("density").unwrap().normalization(),
            Some(normalization)
        );
        dataset.validate().unwrap();

        let mut bytes = Vec::new();
        write_msg(&dataset, &MsgWriteOptions::default(), &mut bytes).unwrap();
        assert_eq!(
            multires_rs::view_msg(&bytes)
                .unwrap()
                .field("density")
                .unwrap()
                .normalization(),
            Some(normalization)
        );
    }
}
