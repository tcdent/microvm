//! IPC command dispatch: Command → Orchestrator method → Response.
//!
//! Pure dispatch logic; transport-agnostic. Unix-socket server and
//! future HTTP adapter both call into this.

use crate::ipc::proto::{Command, Response};
use crate::Orchestrator;

pub fn dispatch(orch: &Orchestrator, cmd: Command) -> Response {
    match cmd {
        Command::Start(spec) => match orch.start_vm(spec) {
            Ok(info) => Response::VmInfo(info),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::Stop { name } => match orch.stop_vm(&name) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error(e.to_string()),
        },
        Command::List => Response::VmList(orch.list_vms()),
        Command::Inspect { name } => match orch.inspect_vm(&name) {
            Ok(info) => Response::VmInfo(info),
            Err(e) => Response::Error(e.to_string()),
        },
        Command::Logs { name, follow } => {
            if follow {
                // Streaming handled by server.rs directly — shouldn't
                // reach here. Return error if it does.
                Response::Error("follow mode must use streaming handler".into())
            } else {
                let content = crate::log::read_log(&orch.config.log_dir, &name);
                Response::LogChunk(content)
            }
        }
    }
}
