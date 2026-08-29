//! Wire-level protocol data and transport limits.

pub const MAX_FRAME_BYTES: u32 = 1024 * 1024;
pub const MAX_INBOX_REQUESTS: usize = 128;
pub const MAX_OUTBOX_RESPONSES: usize = 256;

pub struct WireRequest {
    pub id: serde_json::Value,
    pub method: String,
    pub params: serde_json::Value,
    pub generation: u64,
}

pub struct WireResponse {
    pub body: serde_json::Value,
    pub generation: u64,
}
