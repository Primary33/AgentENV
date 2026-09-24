use super::build::{self, Build, BuildContext, ImageBuild, ImagePush};
use crate::client::{sandboxes::NewComposeSandbox, Client};
use anyhow::{ensure, Context, Result};
use clap::{Args as ClapArgs, Subcommand};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;

#[cfg(target_os = "linux")]
mod runtime;

const MAX_COMPOSE_BYTES: u64 = 1024 * 1024;

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    cmd: Sub,
}

#[derive(Subcommand)]
enum Sub {
    /// Create a sandbox, wait for Compose readiness, and print its ID
    Up(UpArgs),
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    GuestInit,
    #[cfg(target_os = "linux")]
    #[command(hide = true)]
    GuestStart { timeout: String },
}

#[derive(ClapArgs)]
#[command(after_help = "Examples:
  aenv compose up -f compose.yaml --cpu 2 --memory 2048
  aenv compose up -f compose.yaml --env TAG=stable --profile worker
  cat compose.yaml | aenv compose up -f -

Each invocation creates a new sandbox. Manage it with aenv exec, connect, pause,
resume, snapshot, and delete. Only explicit --env values are used for interpolation;
the local environment and .env files are not loaded.")]
struct UpArgs {
    /// Compose YAML or JSON file; use - to read stdin
    #[arg(short = 'f', long, default_value = "compose.yaml", value_name = "PATH")]
    file: PathBuf,
    /// Compose interpolation variable (repeatable; last value wins)
    #[arg(long = "env", value_name = "KEY=VALUE", value_parser = parse_env)]
    environment: Vec<(String, String)>,
    /// Enable an optional Compose profile (repeatable)
    #[arg(long = "profile", value_name = "NAME")]
    profiles: Vec<String>,
    /// Sandbox TTL in seconds, starting after Compose is ready
    #[arg(long, default_value_t = super::DEFAULT_TIMEOUT_SECS)]
    timeout: u32,
    /// Startup budget in seconds, including image resolution and health checks
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..=300))]
    startup_timeout: u32,
    #[command(flatten)]
    resources: super::CpuMemoryArgs,
    /// Root filesystem size in MiB (must be divisible by 1024)
    #[arg(long = "disk-size-mb", alias = "disk-mb", value_parser = super::parse_disk_size_mb)]
    disk_size_mb: Option<u32>,
}

pub(super) fn parse_env(value: &str) -> std::result::Result<(String, String), String> {
    let (key, value) = value
        .split_once('=')
        .filter(|(key, _)| !key.is_empty())
        .ok_or_else(|| "expected KEY=VALUE with a non-empty key".to_owned())?;
    Ok((key.to_owned(), value.to_owned()))
}

pub(super) fn read_compose(reader: impl Read) -> Result<String> {
    let mut compose = String::new();
    reader
        .take(MAX_COMPOSE_BYTES + 1)
        .read_to_string(&mut compose)
        .context("reading Compose source as UTF-8")?;
    anyhow::ensure!(
        compose.len() as u64 <= MAX_COMPOSE_BYTES,
        "Compose source exceeds 1 MiB"
    );
    anyhow::ensure!(!compose.trim().is_empty(), "Compose source is empty");
    Ok(compose)
}

pub fn run(args: Args) -> Result<()> {
    match args.cmd {
        Sub::Up(args) => up(args),
        #[cfg(target_os = "linux")]
        Sub::GuestInit => runtime::run(None),
        #[cfg(target_os = "linux")]
        Sub::GuestStart { timeout } => runtime::run(Some(&timeout)),
    }
}

fn up(args: UpArgs) -> Result<()> {
    let compose = if args.file == std::path::Path::new("-") {
        read_compose(std::io::stdin().lock())?
    } else {
        let file = std::fs::File::open(&args.file)
            .with_context(|| format!("opening Compose file {}", args.file.display()))?;
        read_compose(file)?
    };
    let body = NewComposeSandbox {
        compose: &compose,
        compose_env: args.environment.into_iter().collect(),
        profiles: args.profiles,
        timeout: args.timeout,
        startup_timeout: args.startup_timeout,
        cpu_count: args.resources.cpu_count,
        memory_mb: args.resources.memory_mb,
        disk_size_mb: args.disk_size_mb,
    };
    let client = Client::from_env()?;
    eprintln!("Starting Compose sandbox; waiting for services to become ready...");
    let sandbox = client.create_compose_sandbox(&body)?;
    println!("{}", sandbox.sandbox_id);
    Ok(())
}

#[derive(Clone, Default, ClapArgs)]
#[group(id = "compose-build")]
pub(super) struct BuildArgs {
    /// Build service images from a Compose file instead of creating a VM template
    #[arg(long, value_name = "PATH")]
    pub compose: Option<PathBuf>,
    /// Apply Harbor task defaults: build main from ./Dockerfile and keep it alive unless overridden
    #[arg(long, requires = "compose", conflicts_with_all = ["context", "name"])]
    harbor: bool,
    /// Registry repository for unique service image tags, e.g. registry.example.com/team/images;
    /// optional, only needed to distribute images through a registry
    #[arg(long, requires = "compose", conflicts_with_all = ["context", "name"])]
    image_repository: Option<String>,
    /// Write an image-only Compose file (default: compose.built.yaml beside the input); must not exist
    #[arg(long, requires = "compose", conflicts_with_all = ["context", "name"], value_name = "PATH")]
    output: Option<PathBuf>,
    /// Compose interpolation variable; repeatable, last value wins; .env is not loaded
    #[arg(long = "env", requires = "compose", conflicts_with_all = ["context", "name"], value_name = "KEY=VALUE", value_parser = parse_env)]
    environment: Vec<(String, String)>,
    /// Enable an optional Compose profile (repeatable)
    #[arg(long = "profile", requires = "compose", conflicts_with_all = ["context", "name"], value_name = "NAME")]
    profiles: Vec<String>,
    /// Allow HTTP or untrusted TLS for image pushes (development registries only)
    #[arg(long, requires = "compose", conflicts_with_all = ["context", "name"])]
    registry_insecure: bool,
}

#[derive(Deserialize)]
struct Plan {
    compose: Value,
    services: Vec<Service>,
}

#[derive(Deserialize)]
struct Service {
    name: String,
    context: PathBuf,
    dockerfile: PathBuf,
    args: BTreeMap<String, String>,
    target: Option<String>,
    #[serde(default, rename = "noCache")]
    no_cache: bool,
}

fn validate_repository(repository: &str) -> Result<()> {
    let (host, path) = repository
        .split_once('/')
        .context("--image-repository must include a registry host and repository path")?;
    ensure!(
        !host.is_empty()
            && (host.contains('.') || host.contains(':') || host == "localhost")
            && host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
            && !path.is_empty()
            && path.len() <= 200
            && path.split('/').all(|part| !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))),
        "invalid --image-repository; use REGISTRY/REPOSITORY without a scheme, tag, or digest"
    );
    Ok(())
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        digest
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())),
        "server returned an invalid image digest; upgrade the AgentENV server"
    );
    Ok(())
}

pub(super) fn run_build(client: Client, args: build::Args) -> Result<()> {
    let options = &args.compose;
    let repository = options.image_repository.as_deref();
    if let Some(repository) = repository {
        validate_repository(repository)?;
    }
    let source = options
        .compose
        .as_ref()
        .context("missing Compose file")?
        .canonicalize()
        .context("locate Compose file")?;
    let base = source
        .parent()
        .context("Compose file has no parent directory")?;
    let output = options
        .output
        .clone()
        .unwrap_or_else(|| base.join("compose.built.yaml"));
    // Refuse an existing destination before allocating remote resources. Persist
    // without clobbering also closes the race with another writer during builds.
    ensure!(
        !output.try_exists()? && output.symlink_metadata().is_err(),
        "output {} already exists; choose another --output",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let mut destination =
        tempfile::NamedTempFile::new_in(parent).context("prepare Compose output directory")?;
    let compose = read_compose(std::fs::File::open(&source)?)?;
    let request = json!({
        "compose": compose, "harbor": options.harbor,
        "composeEnv": options.environment.iter().cloned().collect::<BTreeMap<_, _>>(),
        "profiles": options.profiles,
    });
    ensure!(
        serde_json::to_vec(&request)?.len() <= 2 * 1024 * 1024,
        "Compose planner request exceeds 2 MiB"
    );
    let response = super::tokio_rt()?
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(60),
                client.build_request(Method::POST, "/sandboxes-compose/plan", Some(request)),
            )
            .await
            .context("Compose planning timed out")?
        })
        .context("plan Compose build on the server")?;
    let mut plan: Plan = serde_json::from_slice(&response).context("decode Compose build plan")?;
    ensure!(
        !plan.services.is_empty(),
        "Compose file selects no services with build; use aenv compose up directly"
    );

    // Validate every path before the first build, resolving context relative to
    // the Compose file and Dockerfile relative to that context, per Compose.
    let mut builds = Vec::new();
    let id = uuid::Uuid::new_v4().simple().to_string();
    for (index, service) in plan.services.into_iter().enumerate() {
        let context = base.join(service.context);
        let build = Build {
            name: format!("compose-{id}-{index}"),
            context: BuildContext::prepare(&context, Some(&context.join(service.dockerfile)))
                .with_context(|| format!("service {}", service.name))?,
            build_args: service
                .args
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect(),
            no_cache: args.no_cache || service.no_cache,
            image: Some(ImageBuild {
                target: service.target,
                push: repository.map(|repository| ImagePush {
                    image: format!("{repository}:aenv-{id}-{index}"),
                    insecure: options.registry_insecure,
                }),
            }),
        };
        builds.push((service.name, build));
    }
    // Carry logical image dependencies; placement belongs to the server.
    let images = crate::commands::tokio_rt()?.block_on(async {
        let mut dependencies = Vec::new();
        let mut images = Vec::new();
        for (name, build) in builds {
            let pushed = build.image.as_ref().and_then(|image| image.push.as_ref());
            match pushed {
                Some(push) => eprintln!("Building Compose service {name} -> {}", push.image),
                None => eprintln!("Building Compose service {name}"),
            }
            let info = build::run_async(&client, &args, &build, &dependencies)
                .await
                .with_context(|| format!("building Compose service {name}"))?;
            let reference = match pushed {
                Some(push) => push.image.clone(),
                None => {
                    let digest = info.image_digest.as_deref().context(
                        "server does not report the built image digest; upgrade the AgentENV \
                         server to build without --image-repository",
                    )?;
                    validate_digest(digest)?;
                    digest.to_owned()
                }
            };
            if repository.is_none() {
                dependencies.push(reference.clone());
            }
            images.push((name, reference));
        }
        Ok::<_, anyhow::Error>(images)
    })?;
    for (name, image) in images {
        plan.compose["services"][&name]["image"] = Value::String(image);
    }
    plan.compose
        .as_object_mut()
        .context("invalid Compose plan")?
        .remove("x-aenv-build");
    let mut encoded = serde_json::to_vec_pretty(&plan.compose)?;
    encoded.push(b'\n');
    // compose up accepts YAML and JSON, both bounded to 1 MiB.
    ensure!(
        encoded.len() <= 1024 * 1024,
        "generated Compose file exceeds 1 MiB"
    );
    destination.write_all(&encoded)?;
    destination.as_file().sync_all()?;
    destination
        .persist_noclobber(&output)
        .with_context(|| format!("publish Compose output {}", output.display()))?;
    eprintln!("Built Compose file: {}", output.display());
    eprintln!("Start it with: aenv compose up -f {}", output.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    fn parse(arguments: &[&str]) -> UpArgs {
        let crate::Cmd::Compose(Args { cmd: Sub::Up(args) }) =
            crate::Cli::try_parse_from(arguments).unwrap().cmd
        else {
            panic!("expected compose up");
        };
        args
    }

    #[test]
    fn accepts_profiles_explicit_environment_and_resource_aliases() {
        crate::Cli::command().debug_assert();
        let args = parse(&[
            "aenv",
            "compose",
            "up",
            "-f",
            "-",
            "--profile",
            "web",
            "--profile",
            "worker",
            "--env",
            "VALUE=a=b",
            "--env",
            "EMPTY=",
            "--cpu-count",
            "2",
            "--mem",
            "2048",
            "--disk-mb",
            "8192",
            "--timeout",
            "600",
            "--startup-timeout",
            "60",
        ]);
        assert_eq!(args.file, PathBuf::from("-"));
        assert_eq!(args.profiles, ["web", "worker"]);
        assert_eq!(
            args.environment,
            [("VALUE".into(), "a=b".into()), ("EMPTY".into(), "".into())]
        );
        assert_eq!(args.resources.cpu_count, Some(2));
        assert_eq!(args.resources.memory_mb, Some(2048));
        assert_eq!(args.disk_size_mb, Some(8192));
        assert_eq!((args.timeout, args.startup_timeout), (600, 60));
        let defaults = parse(&["aenv", "compose", "up"]);
        assert_eq!(defaults.file, PathBuf::from("compose.yaml"));
        assert!(defaults.environment.is_empty());
        assert!(defaults.profiles.is_empty());
        assert!(!defaults.resources.is_set());
        assert_eq!((defaults.timeout, defaults.startup_timeout), (300, 300));
    }

    #[test]
    fn rejects_invalid_environment_disk_and_startup_budgets() {
        for (flag, value) in [
            ("--env", "MISSING_VALUE"),
            ("--env", "=value"),
            ("--disk-size-mb", "0"),
            ("--disk-size-mb", "1025"),
            ("--startup-timeout", "0"),
            ("--startup-timeout", "301"),
        ] {
            assert!(crate::Cli::try_parse_from(["aenv", "compose", "up", flag, value]).is_err());
        }
    }

    #[test]
    fn preserves_compose_source_and_bounds_input() {
        let source = "services: {app: {image: '${IMAGE}', command: ['echo', '$$HOME']}}\n";
        assert_eq!(read_compose(source.as_bytes()).unwrap(), source);
        assert!(read_compose(" \n".as_bytes()).is_err());
        assert!(read_compose(&[0xff][..]).is_err());
        let maximum = vec![b'x'; MAX_COMPOSE_BYTES as usize];
        assert!(read_compose(maximum.as_slice()).is_ok());
        assert!(read_compose(std::io::repeat(b'x')).is_err());
    }

    #[test]
    fn compose_build_flags_do_not_change_template_builds() {
        crate::Cli::command().debug_assert();
        for (args, valid) in [
            ("aenv build --compose compose.yaml", true),
            ("aenv build --compose compose.yaml --image-repository example.com/team/images --env TAG=a=b --profile worker", true),
            ("aenv build . --name demo --image-repository example.com/team/images", false),
            ("aenv build . --name demo --harbor", false),
            ("aenv build . --name demo --output out.yaml", false),
        ] {
            assert_eq!(crate::Cli::try_parse_from(args.split_whitespace()).is_ok(), valid, "{args}");
        }
        for extra in [
            "--start-cmd",
            "--ready-cmd",
            "--name",
            "--file",
            "--build-arg",
            "--secret",
        ] {
            let args = format!("aenv build --compose compose.yaml {extra} value");
            assert!(
                crate::Cli::try_parse_from(args.split_whitespace()).is_err(),
                "{args}"
            );
        }
    }

    #[test]
    fn repository_cannot_inject_exporter_options_or_reuse_a_tag() {
        for value in [
            "example.com/team/images",
            "localhost:5000/images",
            "192.0.2.10:5000/build",
        ] {
            assert!(validate_repository(value).is_ok(), "{value}");
        }
        for value in [
            "images",
            "https://example.com/images",
            "example.com/images:latest",
            "example.com/images,push=false",
            "example.com//images",
            "example.com/images@sha256:abc",
            "example.com/Upper",
        ] {
            assert!(validate_repository(value).is_err(), "{value}");
        }
    }

    #[test]
    fn image_digest_must_be_a_sha256_reference() {
        assert!(validate_digest(&format!("sha256:{}", "ab".repeat(32))).is_ok());
        for value in [
            "ab".repeat(32),
            "sha256:".to_owned(),
            format!("sha256:{}", "ab".repeat(31)),
            format!("sha256:{}", "ag".repeat(32)),
            format!("sha256:{}", "ab".repeat(33)),
        ] {
            assert!(validate_digest(&value).is_err(), "{value}");
        }
    }
}
