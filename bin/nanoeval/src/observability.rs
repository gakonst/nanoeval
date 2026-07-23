use std::path::PathBuf;

use clap::{Args, ValueEnum, builder::NonEmptyStringValueParser};
use nanocodex_observability::{LogFormat, LogOutput, ObservabilityBuilder, ObservabilityGuard};

const DEFAULT_FILTER: &str = "warn,nanoeval=info,nanocodex_vm=info,nanocodex=info,nanocodex_service=info,nanocodex_tools=info,nanocodex_mcp=info";

#[derive(Args)]
pub(crate) struct ObservabilityArgs {
    /// Tracing filter directive. Defaults to Nanoeval and Nanocodex lifecycle spans at info.
    #[arg(
        long,
        env = "RUST_LOG",
        default_value = DEFAULT_FILTER,
        value_parser = NonEmptyStringValueParser::new()
    )]
    log_filter: String,

    /// Tracing filter applied only to exported OpenTelemetry spans.
    #[arg(
        long,
        env = "OTEL_LEVEL",
        default_value = DEFAULT_FILTER,
        value_parser = NonEmptyStringValueParser::new()
    )]
    otel_filter: String,

    /// Local tracing output format.
    #[arg(long, env = "NANOEVAL_LOG_FORMAT", default_value_t, value_enum)]
    log_format: LogFormatArg,

    /// Append local tracing output to this file instead of stderr.
    #[arg(long, env = "NANOEVAL_LOG_FILE")]
    log_file: Option<PathBuf>,

    /// Export spans through OTLP/HTTP protobuf.
    #[arg(
        long,
        env = "OTEL_EXPORTER_OTLP_ENDPOINT",
        value_parser = NonEmptyStringValueParser::new()
    )]
    otel_endpoint: Option<String>,

    /// Deployment environment attached to exported spans.
    #[arg(
        long,
        env = "OTEL_DEPLOYMENT_ENVIRONMENT",
        default_value = "development",
        value_parser = NonEmptyStringValueParser::new()
    )]
    otel_environment: String,
}

#[derive(Clone, Copy, Default, ValueEnum)]
enum LogFormatArg {
    Pretty,
    #[default]
    Compact,
    Json,
}

impl ObservabilityArgs {
    pub(crate) fn install(
        self,
    ) -> Result<ObservabilityGuard, nanocodex_observability::ObservabilityError> {
        let output = self.log_file.map_or(LogOutput::Stderr, LogOutput::File);
        let mut builder = ObservabilityBuilder::new("nanoeval", env!("CARGO_PKG_VERSION"))
            .filter(self.log_filter)
            .otel_filter(self.otel_filter)
            .format(self.log_format.into())
            .output(output)
            .environment(self.otel_environment);
        if let Some(endpoint) = self.otel_endpoint {
            builder = builder.otlp_endpoint(endpoint);
        }
        builder.install()
    }
}

impl From<LogFormatArg> for LogFormat {
    fn from(format: LogFormatArg) -> Self {
        match format {
            LogFormatArg::Pretty => Self::Pretty,
            LogFormatArg::Compact => Self::Compact,
            LogFormatArg::Json => Self::Json,
        }
    }
}
