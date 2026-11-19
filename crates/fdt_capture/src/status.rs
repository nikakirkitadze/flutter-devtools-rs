use fdt_store::run::AppInfo;
use fdt_store::time::RunId;
use fdt_vm::discovery::AppKey;

#[derive(Clone, Debug, PartialEq)]
pub enum ConnStatus {
    Idle,
    Connecting {
        label: String,
    },
    Connected {
        run: RunId,
        app: AppInfo,
        recording: bool,
    },
    Disconnected {
        app: Option<AppInfo>,
        reason: String,
        waiting_for: Option<AppKey>,
    },
    Failed {
        message: String,
        details: String,
    },
}
