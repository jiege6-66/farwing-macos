//! Local device facts for registration and the resident's reporting snapshot.
use serde::{Deserialize, Serialize};

/// The native hardware report, with memory in MiB. Keep the JSON shape stable
/// for the Windows resident service; each platform implements `read` natively.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Hardware {
    pub name: String,
    pub system_uuid: String,
    pub machine_guid: String,
    pub os: String,
    pub base_board: String,
    pub cpu: String,
    pub video: Vec<String>,
    pub mac: String,
    pub memory: i64,
    pub screen: String,
    pub errors: Vec<String>,
}
