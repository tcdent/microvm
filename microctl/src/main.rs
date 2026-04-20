//! microctl — CLI frontend for microd (research/15 §3).
//!
//! Subcommands:
//!   start --name NAME --memory MB [--bucket B --kernel K ...]
//!   start --spec FILE.ini
//!   stop NAME
//!   list [--json]
//!   inspect NAME [--json]
//!   logs NAME [--follow]

use std::path::PathBuf;
use std::process::ExitCode;

use orchestrator::ipc::proto::{Command, Response, VmSpec, VmInfo};

mod client;
mod spec;

const DEFAULT_SOCKET: &str = "/tmp/microd.sock";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        return ExitCode::FAILURE;
    }

    let socket = socket_path(&args);
    let json_mode = args.iter().any(|a| a == "--json");

    match args[1].as_str() {
        "start" => cmd_start(&args[2..], &socket, json_mode),
        "stop" => cmd_stop(&args[2..], &socket, json_mode),
        "list" | "ls" => cmd_list(&socket, json_mode),
        "inspect" => cmd_inspect(&args[2..], &socket, json_mode),
        "logs" => cmd_logs(&args[2..], &socket),
        "help" | "--help" | "-h" => { usage(); ExitCode::SUCCESS }
        other => {
            eprintln!("unknown command: {other}");
            usage();
            ExitCode::FAILURE
        }
    }
}

fn socket_path(args: &[String]) -> PathBuf {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--socket" {
            if let Some(path) = args.get(i + 1) {
                return PathBuf::from(path);
            }
        }
    }
    PathBuf::from(DEFAULT_SOCKET)
}

fn usage() {
    eprintln!("Usage: microctl <command> [options]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  start   --name NAME --memory MB [--bucket B --kernel K --initrd I]");
    eprintln!("  start   --spec FILE.ini");
    eprintln!("  stop    NAME");
    eprintln!("  list    [--json]");
    eprintln!("  inspect NAME [--json]");
    eprintln!("  logs    NAME [--follow]");
}

fn cmd_start(args: &[String], socket: &PathBuf, json_mode: bool) -> ExitCode {
    // Check for --spec mode.
    let spec = if let Some(pos) = args.iter().position(|a| a == "--spec") {
        match args.get(pos + 1) {
            Some(path) => match spec::from_ini(path) {
                Ok(s) => s,
                Err(e) => { eprintln!("error: {e}"); return ExitCode::FAILURE; }
            },
            None => { eprintln!("--spec requires a path"); return ExitCode::FAILURE; }
        }
    } else {
        // Parse flags.
        let name = flag_val(args, "--name");
        let Some(name) = name else {
            eprintln!("--name is required");
            return ExitCode::FAILURE;
        };
        VmSpec {
            name,
            mem_mb: flag_val(args, "--memory")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024),
            kernel: flag_val(args, "--kernel").map(PathBuf::from),
            initrd: flag_val(args, "--initrd").map(PathBuf::from),
            bucket: flag_val(args, "--bucket"),
            golden_bucket: flag_val(args, "--golden-bucket"),
            ruleset: flag_val(args, "--ruleset").map(PathBuf::from),
            ephemeral_dir: flag_val(args, "--ephemeral-dir").map(PathBuf::from),
            net_ifname: flag_val(args, "--net-ifname"),
        }
    };

    match client::send(socket, Command::Start(spec)) {
        Ok(resp) => print_response(&resp, json_mode),
        Err(e) => { eprintln!("error: {e}"); ExitCode::FAILURE }
    }
}

fn cmd_stop(args: &[String], socket: &PathBuf, json_mode: bool) -> ExitCode {
    let name = match args.first() {
        Some(n) if !n.starts_with('-') => n.clone(),
        _ => { eprintln!("stop requires a VM name"); return ExitCode::FAILURE; }
    };
    match client::send(socket, Command::Stop { name }) {
        Ok(resp) => print_response(&resp, json_mode),
        Err(e) => { eprintln!("error: {e}"); ExitCode::FAILURE }
    }
}

fn cmd_list(socket: &PathBuf, json_mode: bool) -> ExitCode {
    match client::send(socket, Command::List) {
        Ok(resp) => print_response(&resp, json_mode),
        Err(e) => { eprintln!("error: {e}"); ExitCode::FAILURE }
    }
}

fn cmd_inspect(args: &[String], socket: &PathBuf, json_mode: bool) -> ExitCode {
    let name = match args.iter().find(|a| !a.starts_with('-')) {
        Some(n) => n.clone(),
        None => { eprintln!("inspect requires a VM name"); return ExitCode::FAILURE; }
    };
    match client::send(socket, Command::Inspect { name }) {
        Ok(resp) => print_response(&resp, json_mode),
        Err(e) => { eprintln!("error: {e}"); ExitCode::FAILURE }
    }
}

fn cmd_logs(args: &[String], socket: &PathBuf) -> ExitCode {
    let name = match args.iter().find(|a| !a.starts_with('-')) {
        Some(n) => n.clone(),
        None => { eprintln!("logs requires a VM name"); return ExitCode::FAILURE; }
    };
    let follow = args.iter().any(|a| a == "--follow" || a == "-f");

    if follow {
        let result = client::stream(socket, Command::Logs { name, follow: true }, |resp| {
            if let Response::LogChunk(chunk) = resp {
                print!("{chunk}");
            }
        });
        if let Err(e) = result {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    } else {
        match client::send(socket, Command::Logs { name, follow: false }) {
            Ok(Response::LogChunk(content)) => print!("{content}"),
            Ok(resp) => { print_response(&resp, false); }
            Err(e) => { eprintln!("error: {e}"); return ExitCode::FAILURE; }
        }
    }
    ExitCode::SUCCESS
}

fn print_response(resp: &Response, json_mode: bool) -> ExitCode {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(resp).unwrap());
        return match resp {
            Response::Error(_) => ExitCode::FAILURE,
            _ => ExitCode::SUCCESS,
        };
    }
    match resp {
        Response::Ok => {
            println!("ok");
            ExitCode::SUCCESS
        }
        Response::VmInfo(info) => {
            print_info(info);
            ExitCode::SUCCESS
        }
        Response::VmList(vms) => {
            if vms.is_empty() {
                println!("no VMs");
            } else {
                println!("{:<20} {:<12} {:>6}  {}", "NAME", "STATE", "MEM", "UPTIME");
                for vm in vms {
                    println!("{:<20} {:<12} {:>4}MB  {}s",
                        vm.name, format!("{:?}", vm.state), vm.spec.mem_mb, vm.uptime_secs);
                }
            }
            ExitCode::SUCCESS
        }
        Response::LogChunk(chunk) => {
            print!("{chunk}");
            ExitCode::SUCCESS
        }
        Response::Error(msg) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
    }
}

fn print_info(info: &VmInfo) {
    println!("name:       {}", info.name);
    println!("hash:       {}", info.name_hash);
    println!("state:      {:?}", info.state);
    println!("memory:     {}MB", info.spec.mem_mb);
    println!("uptime:     {}s", info.uptime_secs);
    if let Some(ref b) = info.spec.bucket {
        println!("bucket:     {b}");
    }
    if let Some(ref n) = info.spec.net_ifname {
        println!("net:        {n}");
    }
}

fn flag_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}
