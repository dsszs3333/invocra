use std::collections::BTreeSet;
use std::io::{self, Write};

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use config::{
    ensure_config_exists, load_config, save_config, AppConfig, InstanceConfig, ProjectEntry,
};
use gitlab::{AccessibleProject, GitLabClient};
use storage::TokenStore;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "invocra",
    about = "Invocra",
    version,
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Try the full dashboard without a GitLab account; no config, credentials or network.
    #[arg(long)]
    demo: bool,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Auth(AuthCommand),
    Config(ConfigCommand),
    Doctor,
    /// Run the interactive setup wizard, preserving other configured instances.
    Setup,
}

#[derive(Debug, Args)]
struct AuthCommand {
    #[command(subcommand)]
    command: AuthSubcommand,
}

#[derive(Debug, Subcommand)]
enum AuthSubcommand {
    Login {
        /// GitLab instance url to store the token for. Optional when the config has a single
        /// instance; otherwise you are prompted to pick one.
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Debug, Args)]
struct ConfigCommand {
    #[command(subcommand)]
    command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    Path,
    Discover {
        /// GitLab instance url to discover projects for. Optional when the config has a single
        /// instance; otherwise you are prompted to pick one.
        #[arg(long)]
        url: Option<String>,
    },
}

// dhat heap profiler, active only under `--features dhat-heap`. Writes dhat-heap.json on drop.
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() -> Result<()> {
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    init_tracing();

    // Build the runtime explicitly so we can force-stop it on exit. The TUI's key reader runs as
    // a blocking task in an endless `event::poll` loop; with `#[tokio::main]` the runtime would
    // wait on that thread forever and the process would not exit after `q`.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to build async runtime")?;
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(std::time::Duration::from_millis(100));
    result
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    if cli.demo {
        return tui::run_dashboard(vec![tui::InstanceRuntime::demo().await?], false).await;
    }

    match cli.command {
        Some(Commands::Auth(auth)) => match auth.command {
            AuthSubcommand::Login { url } => auth_login(url)?,
        },
        Some(Commands::Config(config)) => match config.command {
            ConfigSubcommand::Path => print_config_path()?,
            ConfigSubcommand::Discover { url } => discover_projects(url).await?,
        },
        Some(Commands::Doctor) => doctor().await?,
        Some(Commands::Setup) => {
            let bootstrap = ensure_config_exists()?;
            launch_setup(load_config()?, bootstrap.created).await?;
        }
        None => launch_tui().await?,
    }

    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

/// Resolves which instance url a command targets: an explicit `--url`, the sole configured
/// instance, or an interactive pick when several are configured.
fn pick_instance_url(config: &AppConfig, url: Option<String>) -> Result<String> {
    let target = match url {
        Some(url) => url,
        None => match config.instances.as_slice() {
            [] => bail!("no GitLab instances configured"),
            [only] => only.url.clone(),
            many => many[prompt_instance(many)?].url.clone(),
        },
    };
    config::check_url_scheme(&target, config.allow_insecure_http_for(&target))?;
    Ok(target)
}

/// Prints a numbered instance list and reads the user's selection from stdin.
fn prompt_instance(instances: &[InstanceConfig]) -> Result<usize> {
    println!("Multiple GitLab instances are configured:");
    for (idx, instance) in instances.iter().enumerate() {
        println!("  [{}] {}", idx + 1, instance.url);
    }
    print!("Select instance [1-{}]: ", instances.len());
    io::stdout().flush().context("failed to flush stdout")?;

    let mut line = String::new();
    io::stdin()
        .read_line(&mut line)
        .context("failed to read selection from stdin")?;
    let choice: usize = line.trim().parse().context("invalid selection")?;
    if choice < 1 || choice > instances.len() {
        bail!("selection out of range");
    }
    Ok(choice - 1)
}

fn auth_login(url: Option<String>) -> Result<()> {
    let target_url = match url {
        Some(url) => url,
        None => {
            let config =
                load_config().context("could not load config to pick an instance; pass --url")?;
            pick_instance_url(&config, None)?
        }
    };

    // Read without echo so the token doesn't land on screen, in scrollback, or in a session
    // recording. Requires a TTY; headless sessions can use INVOCRA_GITLAB_TOKEN instead.
    let token =
        rpassword::prompt_password(format!("GitLab Personal Access Token for {target_url}: "))
            .context("failed to read token")?;
    let token = token.trim();

    if token.is_empty() {
        bail!("token is empty");
    }

    TokenStore::for_instance(&target_url).save_token(token)?;
    println!("Token saved in system keyring for {target_url}.");
    Ok(())
}

fn print_config_path() -> Result<()> {
    let bootstrap = ensure_config_exists()?;
    println!("{}", bootstrap.path.display());
    Ok(())
}

async fn doctor() -> Result<()> {
    let bootstrap = ensure_config_exists()?;
    println!("Config: {}", bootstrap.path.display());
    println!(
        "Config created: {}",
        if bootstrap.created { "yes" } else { "no" }
    );

    let config = load_config()?;
    println!("Instances: {}", config.instances.len());

    let env_token = std::env::var(storage::TOKEN_ENV_VAR).is_ok();
    if env_token {
        println!(
            "Token source: {} env var (applies to all instances)",
            storage::TOKEN_ENV_VAR
        );
    }

    for instance in &config.instances {
        println!();
        println!("[{}]", instance.url);
        println!("  Projects: {}", instance.projects.len());
        println!("  Groups: {}", instance.groups.len());

        let token = TokenStore::for_instance(&instance.url).load_token()?;
        if !env_token {
            println!(
                "  Token in keyring: {}",
                if token.is_some() { "yes" } else { "no" }
            );
        }

        if instance.projects.is_empty() && instance.groups.is_empty() {
            println!("  API check: skipped (no projects or groups)");
            continue;
        }

        let Some(token) = token else {
            println!("  API check: skipped (no token)");
            continue;
        };
        let client = GitLabClient::new(&instance.url, token, instance.proxy.as_deref())?;
        if let Some(project) = instance.projects.first() {
            match client.get_project(project.id).await {
                Ok(project) => println!("  API check: ok ({})", project.name),
                Err(err) => println!("  API check: failed ({err})"),
            }
        } else if let Some(group) = instance.groups.first() {
            let reference = group.reference()?;
            match client
                .list_group_projects(&reference, group.include_subgroups)
                .await
            {
                Ok(projects) => println!(
                    "  API check: ok (group {reference}: {} project(s))",
                    projects.len()
                ),
                Err(err) => println!("  API check: failed ({err})"),
            }
        }
    }

    if !env_token {
        println!();
        println!(
            "(tip: set {} to skip the keychain prompt)",
            storage::TOKEN_ENV_VAR
        );
    }

    Ok(())
}

async fn discover_projects(url: Option<String>) -> Result<()> {
    let bootstrap = ensure_config_exists()?;
    let mut config = load_config()?;

    let target_url = pick_instance_url(&config, url)?;
    let token = TokenStore::for_instance(&target_url)
        .load_token()?
        .with_context(|| {
            format!("no token for {target_url}; run `invocra auth login --url {target_url}`")
        })?;
    // Reuse the instance's configured proxy (if any) so discovery works through the same tunnel.
    let proxy = config.proxy_for(&target_url);
    let client = GitLabClient::new(&target_url, token, proxy.as_deref())?;

    let projects = client.list_projects().await?;
    if projects.is_empty() {
        println!("No accessible GitLab projects found.");
        return Ok(());
    }

    let items: Vec<(u64, String)> = projects
        .iter()
        .map(|project| (project.id, project.path_with_namespace.clone()))
        .collect();
    let preselected: Vec<u64> = config
        .instances
        .iter()
        .find(|instance| instance.url == target_url)
        .map(|instance| instance.projects.iter().map(|project| project.id).collect())
        .unwrap_or_default();
    let title = format!("GitLab project discovery for {target_url}");

    let selected_ids = tui::run_project_picker(&title, &items, &preselected)?.unwrap_or_default();
    if selected_ids.is_empty() {
        println!("No projects selected. Config left unchanged.");
        return Ok(());
    }

    let selected: BTreeSet<u64> = selected_ids.into_iter().collect();
    let discovered_projects = projects
        .iter()
        .filter(|project| selected.contains(&project.id))
        .map(project_to_config_entry)
        .collect::<Vec<_>>();
    let count = discovered_projects.len();

    // Preserve the instance's groups if it already exists; only its project list is discovered.
    let groups = config
        .instances
        .iter()
        .find(|instance| instance.url == target_url)
        .map(|instance| instance.groups.clone())
        .unwrap_or_default();
    let allow_insecure_http = config.allow_insecure_http_for(&target_url);
    config::upsert_instance(
        &mut config,
        InstanceConfig {
            url: target_url.clone(),
            proxy,
            allow_insecure_http,
            projects: discovered_projects,
            groups,
        },
    );
    let path = save_config(&config)?;

    println!("Saved {count} project(s) to {}", path.display());
    if bootstrap.created {
        println!("Replaced the sample config with discovered projects.");
    }

    Ok(())
}

async fn launch_tui() -> Result<()> {
    let bootstrap = ensure_config_exists()?;
    let config = load_config()?;

    // Build a runtime per instance that has a token; instances without one are skipped with a hint.
    let mut runtimes = Vec::new();
    for instance in &config.instances {
        match TokenStore::for_instance(&instance.url).load_token()? {
            Some(token) => {
                let client = GitLabClient::new(&instance.url, token, instance.proxy.as_deref())?;
                runtimes.push(tui::InstanceRuntime {
                    url: instance.url.clone(),
                    client,
                    sources: tui::RefreshSources {
                        projects: instance.projects.clone(),
                        groups: instance.groups.clone(),
                    },
                });
            }
            None => eprintln!(
                "Warning: no token for {url}; run `invocra auth login --url {url}`. Skipping.",
                url = instance.url
            ),
        }
    }

    // Losing a token must never restart setup or replace the saved project selection.
    if bootstrap.created {
        return launch_setup(config, true).await;
    }

    if runtimes.is_empty() {
        println!("No configured instance has a token. Run `invocra auth login --url <url>`.");
        return Ok(());
    }

    tui::run_dashboard(runtimes, config.settings.notifications).await
}

async fn launch_setup(mut config: AppConfig, replace_sample: bool) -> Result<()> {
    let default_url = config
        .instances
        .first()
        .map(|instance| instance.url.clone());
    let Some(instances) = tui::run_onboarding(default_url).await? else {
        println!("Setup cancelled. Run `invocra setup` to retry.");
        return Ok(());
    };
    merge_setup_instances(&mut config, &instances, replace_sample);
    save_config(&config)?;
    tui::run_dashboard(instances, config.settings.notifications).await
}

fn merge_setup_instances(
    config: &mut AppConfig,
    instances: &[tui::InstanceRuntime],
    replace_sample: bool,
) {
    if replace_sample {
        config.instances.clear();
    }
    for rt in instances {
        let instance = InstanceConfig {
            url: rt.url.clone(),
            proxy: config.proxy_for(&rt.url),
            allow_insecure_http: config.allow_insecure_http_for(&rt.url),
            projects: rt.sources.projects.clone(),
            groups: rt.sources.groups.clone(),
        };
        config::upsert_instance(config, instance);
    }
}

fn project_to_config_entry(project: &AccessibleProject) -> ProjectEntry {
    ProjectEntry {
        name: project.path_with_namespace.clone(),
        id: project.id,
    }
}

#[cfg(test)]
mod tests {
    use super::pick_instance_url;

    #[test]
    fn cli_uses_invocra_name_and_keeps_existing_subcommands() {
        use clap::CommandFactory;
        let mut command = super::Cli::command();
        assert_eq!(command.get_name(), "invocra");
        let help = command.render_long_help().to_string();
        assert!(help.contains("Invocra"));
        assert!(help.contains("--demo"));
        assert!(help.contains("without a GitLab account"));
        for name in ["auth", "config", "doctor", "setup"] {
            assert!(command.find_subcommand(name).is_some(), "{name}");
        }
    }

    #[test]
    fn demo_flag_cannot_be_combined_with_commands_that_access_credentials() {
        use clap::Parser;
        assert!(
            super::Cli::try_parse_from(["invocra", "--demo"])
                .expect("demo CLI")
                .demo
        );
        for args in [
            vec!["--demo", "setup"],
            vec!["--demo", "auth", "login"],
            vec!["doctor", "--demo"],
        ] {
            assert!(super::Cli::try_parse_from(std::iter::once("invocra").chain(args)).is_err());
        }
    }

    #[test]
    fn discovery_url_enforces_instance_transport_policy_before_loading_token() {
        let mut cfg = config::AppConfig {
            instances: vec![config::InstanceConfig {
                url: "http://git.example".into(),
                proxy: None,
                allow_insecure_http: false,
                projects: Vec::new(),
                groups: Vec::new(),
            }],
            settings: config::Settings::default(),
        };
        assert!(pick_instance_url(&cfg, Some("http://git.example".into())).is_err());
        assert!(pick_instance_url(&cfg, None).is_err());
        cfg.instances[0].allow_insecure_http = true;
        assert_eq!(
            pick_instance_url(&cfg, Some("http://git.example".into())).expect("explicit opt-in"),
            "http://git.example"
        );
        assert!(pick_instance_url(&cfg, Some("http://other.example".into())).is_err());
        assert!(pick_instance_url(&cfg, Some("file:///etc/passwd".into())).is_err());
        assert!(pick_instance_url(&cfg, Some("https://git.example".into())).is_ok());
    }
    #[test]
    fn explicit_setup_updates_one_instance_without_losing_others() {
        let make = |url: &str| config::InstanceConfig {
            url: url.into(),
            proxy: Some("socks5://localhost:1080".into()),
            allow_insecure_http: false,
            projects: vec![],
            groups: vec![],
        };
        let mut cfg = config::AppConfig {
            instances: vec![
                make("https://first.example"),
                make("https://second.example"),
            ],
            settings: config::Settings::default(),
        };
        let second = cfg.instances[1].clone();
        let selected = [tui::InstanceRuntime {
            url: "https://first.example".into(),
            client: gitlab::GitLabClient::new("https://first.example", "test", None)
                .expect("client"),
            sources: tui::RefreshSources {
                projects: vec![config::ProjectEntry {
                    id: 10,
                    name: "selected".into(),
                }],
                groups: vec![],
            },
        }];
        super::merge_setup_instances(&mut cfg, &selected, false);
        assert_eq!(cfg.instances.len(), 2);
        assert_eq!(cfg.instances[1], second);
        assert_eq!(cfg.instances[0].projects[0].id, 10);
        assert_eq!(
            cfg.instances[0].proxy.as_deref(),
            Some("socks5://localhost:1080")
        );
        super::merge_setup_instances(&mut cfg, &selected, true);
        assert_eq!(cfg.instances.len(), 1);
        assert_eq!(cfg.instances[0].projects[0].id, 10);
    }
}
