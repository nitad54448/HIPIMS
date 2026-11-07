//! `hipims-qcm`: starts the instrument actor and the local web service.

use hipims_qcm::actor;
use hipims_qcm::config::Config;
use hipims_qcm::project::Project;
use hipims_qcm::server::{self, AppState, LogLevelSetter};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{reload, EnvFilter};

const HELP: &str = "hipims-qcm - QCM tracking / Q-control / HiPIMS acquisition on a Zurich Instruments UHF

USAGE: hipims-qcm [OPTIONS]

  --config <file>     TOML configuration (default: ./config.toml, ./config/default.toml, or next to the executable)
  --data-dir <dir>    where projects are stored (default: from the config, `data`)
  --port <n>          web UI port (default 8765, bound to 127.0.0.1 only)
  --sim               preselect the built-in simulator (no LabOne/hardware needed)
  --no-browser        do not open the browser automatically
  --print-config      print the effective configuration and exit
  -h, --help          show this help

The UI is served at http://127.0.0.1:<port>/ ; the URL printed at start-up carries the
session token. Use the Exit button (or Ctrl-C) to stop: outputs are switched off first.";

struct Args {
    config: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    port: Option<u16>,
    sim: bool,
    no_browser: bool,
    print_config: bool,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args { config: None, data_dir: None, port: None, sim: false, no_browser: false, print_config: false };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "--config" => a.config = Some(it.next().ok_or("--config needs a path")?.into()),
            "--data-dir" => a.data_dir = Some(it.next().ok_or("--data-dir needs a path")?.into()),
            "--port" => a.port = Some(it.next().ok_or("--port needs a number")?.parse().map_err(|_| "invalid port")?),
            "--sim" => a.sim = true,
            "--no-browser" => a.no_browser = true,
            "--print-config" => a.print_config = true,
            other => return Err(format!("unknown option `{other}` (see --help)")),
        }
    }
    Ok(Some(a))
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if let Err(e) = r {
        eprintln!("could not open the browser ({e}); open the URL above manually");
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let (filter, reload_handle) = reload::Layer::new(EnvFilter::new("warn,hipims_qcm=info,bridge=info"));
    tracing_subscriber::registry().with(filter).with(tracing_subscriber::fmt::layer()).init();
    let setter: LogLevelSetter = Box::new(move |lvl| {
        if !["error", "warn", "info", "debug", "trace"].contains(&lvl) {
            return Err(format!("unknown level `{lvl}`"));
        }
        let f = EnvFilter::try_new(format!("warn,hipims_qcm={lvl},bridge={lvl}")).map_err(|e| e.to_string())?;
        reload_handle.reload(f).map_err(|e| e.to_string())
    });

    let exe_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from));
    let cfg_path = args.config.clone().or_else(|| {
        let mut cands = vec![PathBuf::from("config.toml"), PathBuf::from("config/default.toml")];
        if let Some(d) = &exe_dir {
            cands.push(d.join("config.toml"));
            cands.push(d.join("config/default.toml"));
        }
        cands.into_iter().find(|p| p.is_file())
    });
    let mut cfg = match &cfg_path {
        Some(p) => match Config::load(p) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(2);
            }
        },
        None => Config::default(),
    };
    if let Some(p) = args.port {
        cfg.server.port = p;
    }
    if let Some(d) = args.data_dir {
        cfg.data_dir = d;
    }
    if args.no_browser {
        cfg.server.open_browser = false;
    }
    if let Err(e) = cfg.validate() {
        eprintln!("error: {e}");
        return ExitCode::from(2);
    }
    if args.print_config {
        println!("{}", cfg.to_toml());
        return ExitCode::SUCCESS;
    }
    let cfg = Arc::new(cfg);

    let project = match Project::new(&cfg.data_dir, &Project::default_name(), cfg.to_toml()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (handle, actor_thread) = actor::spawn(cfg.clone());

    // A panic anywhere must still drive the instrument to its safe state.
    let panic_handle = handle.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_handle.emergency_stop();
        default_hook(info);
        std::thread::sleep(std::time::Duration::from_millis(800));
    }));

    let token = server::new_token();
    let state = AppState::new(cfg.clone(), handle.clone(), project, token.clone(), args.sim, setter);
    let code = rt.block_on(async {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.server.port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                eprintln!("error: cannot listen on {addr}: {e} (another instance running? use --port)");
                return ExitCode::FAILURE;
            }
        };
        let url = format!("http://127.0.0.1:{}/#token={}{}", cfg.server.port, token, if args.sim { "&sim=1" } else { "" });
        println!(
            "hipims-qcm {} ready:\n\n    {url}\n\nUse the Exit button in the UI (or Ctrl-C) to stop; outputs are switched off first.",
            env!("CARGO_PKG_VERSION")
        );
        if cfg.server.open_browser {
            open_browser(&url);
        }
        match server::serve(state, listener).await {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("server error: {e}");
                ExitCode::FAILURE
            }
        }
    });
    // Whatever happened: stop the actor (safe state) before leaving.
    actor::shutdown_and_join(&handle, actor_thread);
    code
}
